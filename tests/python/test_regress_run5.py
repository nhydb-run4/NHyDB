"""Regression pool of RUN5 (0.28.2): fixes from the review of rounds 2-4.

Every test names its finding (register in the private RUN5 report).
Tests that guard against endless loops or growing files run in a child
process with a time and file-size limit.
"""
import os
import subprocess
import sys
import textwrap

import pytest

import nhydb


@pytest.fixture(autouse=True)
def _own_config(monkeypatch):
    """The tests write their nhydb_config.toml next to the database; a
    NHYDB_CONFIG of the environment (dir3 QA runs) would take precedence,
    so it is removed for these tests (and their child processes)."""
    monkeypatch.delenv("NHYDB_CONFIG", raising=False)


# Peak RSS of the child process in MiB on every platform (macOS: ru_maxrss in
# bytes, Linux: in KiB, Windows: PeakWorkingSetSize). Prepended to the child code.
PEAK = """
def peak_mib():
    import sys
    if sys.platform == "win32":
        import ctypes
        from ctypes import wintypes
        class PMC(ctypes.Structure):
            _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
                (n, ctypes.c_size_t) for n in ("PeakWorkingSetSize", "WorkingSetSize", "QPPP", "QPP", "QPNP", "QNP", "PU", "PPU")]
        k32 = ctypes.WinDLL("kernel32")
        k32.GetCurrentProcess.restype = wintypes.HANDLE
        k32.K32GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(PMC), wintypes.DWORD]
        pmc = PMC()
        pmc.cb = ctypes.sizeof(PMC)
        k32.K32GetProcessMemoryInfo(k32.GetCurrentProcess(), ctypes.byref(pmc), pmc.cb)
        return pmc.PeakWorkingSetSize >> 20
    import resource
    r = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return (r if sys.platform == "darwin" else r * 1024) >> 20
"""


def _child(code, timeout=120, fsize=512 << 20):
    """Runs ``code`` in a child process with a CPU-time and file-size limit."""
    # rlimits are POSIX-only; on Windows the subprocess timeout is the only bound
    pre = PEAK + ("" if sys.platform == "win32" else textwrap.dedent(f"""
        import resource
        resource.setrlimit(resource.RLIMIT_FSIZE, ({fsize}, {fsize}))
        resource.setrlimit(resource.RLIMIT_CPU, ({timeout}, {timeout}))
    """))
    r = subprocess.run([sys.executable, "-c", pre + textwrap.dedent(code)], capture_output=True, text=True, timeout=timeout + 30)
    assert r.returncode == 0, r.stdout + r.stderr
    return r.stdout


# ----------------------------------------------------------------------
# RA2-01: index trees with fence keys longer than a page terminate
# ----------------------------------------------------------------------

@pytest.mark.parametrize("n_long,klen,front", [(2, 4100, True), (20, 5000, True), (300, 5000, False)])
def test_ra2_01_long_keys_build_terminates(tmp_path, n_long, klen, front):
    out = _child(f"""
        import nhydb, os
        p = {str(tmp_path / 'x.nhydb')!r}
        db = nhydb.NHyDB.create(p)
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{{"v": 1, "name": "t", "type": "utf8"}}])
        db.create_classic_layout(1, 1, "c", shape_id=1)
        longs = [("0" if {front} else "z") + "%04d" % i + "L" * {klen} for i in range({n_long})]
        rows = [[v] for v in longs] + [["k%06d" % i] for i in range(500)]
        db.put_dense_batch(1, 1, rows, shape_id=1)
        db.create_index(1, 1, "i_t", "t")
        db.build_indexes(commit=True)
        for i, v in enumerate(longs):
            assert list(db.find_u(1, {{"i_t": v}})) == [1 + i], i
        assert list(db.find_u(1, {{"i_t": "k000007"}})) == [{n_long} + 8]
        assert db.verify(deep=True)["ok"]
        db.close()
        size = os.path.getsize(p)
        nhydb.NHyDB.compact_file(p, make_backup=False)
        db = nhydb.NHyDB(p)
        assert list(db.find_u(1, {{"i_t": longs[-1]}})) == [{n_long}]
        db.close()
        print("size", size)
    """)
    assert int(out.split()[-1]) < 64 << 20


# ----------------------------------------------------------------------
# RA2-02: the first block behind the directory root is checked for
# ascending block ids (a swapped CRC-valid block is no silent "missing")
# ----------------------------------------------------------------------

def _blocks(raw):
    import struct
    out, off = [], 8192
    while off + 32 <= len(raw) and raw[off:off + 4] == b"NHBL":
        plen = struct.unpack_from("<Q", raw, off + 16)[0]
        end = (off + 32 + plen + 7) & ~7
        out.append((off, end, raw[off + 5], bytes(raw[off + 32:off + 36])))
        off = end
    return out


def _swap_first_tail(tmp_path, sparse):
    import shutil
    d0 = tmp_path / "base"
    d0.mkdir()
    (d0 / "nhydb_config.toml").write_text("[directory]\nflush_blocks = 3\nflush_bytes = 4096\n")
    db = nhydb.NHyDB.create(str(d0 / "x.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "k", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    exp = {}
    for b in range(10):
        if sparse:
            cells = [(u, 50, 1000 + u, "varuint") for u in range(b * 10 + 1, b * 10 + 11)]
            db.put_points(1, cells, commit=True)
        else:
            db.put_dense_batch(1, b * 10 + 1, [[1000 + u] for u in range(b * 10 + 1, b * 10 + 11)], shape_id=1, commit=True)
        exp.update({u: 1000 + u for u in range(b * 10 + 1, b * 10 + 11)})
    db.close()
    raw = bytearray((d0 / "x.nhydb").read_bytes())
    bl = _blocks(raw)
    roots = [i for i, b in enumerate(bl) if b[3] == b"DT17"]
    assert roots, "no directory root written"
    first = bl[roots[-1] + 1]
    n = first[1] - first[0]
    victims = [b for b in bl[:roots[-1]] if b[2] == first[2] and b[1] - b[0] == n]
    assert victims, "no listed block of the same kind and length"
    cases = []
    for v in victims[:3]:
        r = bytearray(raw)
        r[v[0]:v[0] + n], r[first[0]:first[0] + n] = raw[first[0]:first[0] + n], raw[v[0]:v[0] + n]
        d = tmp_path / f"w{v[0]}"
        d.mkdir()
        for f in d0.iterdir():
            if f.name != "x.nhydb":
                shutil.copy(f, d / f.name)
        (d / "x.nhydb").write_bytes(r)
        cases.append(d / "x.nhydb")
    return cases, exp, (50 if sparse else 1)


@pytest.mark.parametrize("sparse", [False, True])
def test_ra2_02_swapped_first_tail_block_never_silent(tmp_path, sparse):
    cases, exp, v = _swap_first_tail(tmp_path, sparse)
    for p in cases:
        try:
            db = nhydb.NHyDB(str(p), readonly=True)
        except nhydb.CorruptionError:
            continue                      # rejected on open: never silent
        for u, x in exp.items():
            try:
                got = db.get_value(1, u, v)
            except nhydb.CorruptionError:
                continue
            assert got == x, (p, u, got)
        db.close()


# ----------------------------------------------------------------------
# RA2-03: the index build budget is checked inside large blocks; overlapping
# blocks are deduplicated exactly as before
# ----------------------------------------------------------------------

def test_ra2_03_build_budget_inside_one_sparse_block(tmp_path):
    out = _child(f"""
        import nhydb
        from pathlib import Path
        d = Path({str(tmp_path)!r})
        db = nhydb.NHyDB.create(str(d / "x.nhydb"))
        db.create_realm(1, "r")
        db.create_index(30, 1, "t", 60)
        n = 300000
        db.put_points(1, [(u, 60, "value-%09d-" % u + "x" * 40, "utf8") for u in range(1, n + 1)], u_window=10**9)
        db.commit(); db.close()
        (d / "nhydb_config.toml").write_text("[index]\\nbuild_memory_bytes = 1000000\\n")
        db = nhydb.NHyDB(str(d / "x.nhydb"))
        r0 = peak_mib()
        r = db.build_indexes(commit=True)
        r1 = peak_mib()
        assert r["spill_rounds"] > 10, r["spill_rounds"]
        for u in (1, 7, n // 2, n):
            assert list(db.find_u(1, {{"t": "value-%09d-" % u + "x" * 40}})) == [u]
        print("delta", r1 - r0)
    """)
    # one block of 300 000 cells is ~18 MB; before RUN5 the build held all keys (~180 MiB)
    assert int(out.split()[-1]) < 90, out


@pytest.mark.parametrize("budget", [1, 2000, 1 << 30])
def test_ra2_03_overlapping_blocks_dedup_unchanged(tmp_path, budget):
    import random
    rnd = random.Random(budget)
    d = tmp_path / "db"
    d.mkdir()
    db = nhydb.NHyDB.create(str(d / "x.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "k", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_index(1, 1, "k", "k")
    db.create_index(2, 1, "x", 50)
    truth_k, truth_x = {}, {}
    for i in range(40):
        lo = rnd.randrange(1, 900)
        n = rnd.randrange(1, 120)
        rows = [[rnd.randrange(30)] for _ in range(n)]
        db.put_dense_batch(1, lo, rows, shape_id=1, commit=True)
        truth_k.update({lo + j: r[0] for j, r in enumerate(rows)})
        cells = [(u, 50, rnd.randrange(30), "varuint") for u in range(lo, lo + rnd.randrange(1, 80))]
        db.put_points(1, cells, commit=True)
        truth_x.update({c[0]: c[2] for c in cells})
        if i % 7 == 3:
            u = rnd.choice(sorted(truth_k))
            db.delete_record(1, u)
            db.commit()
            truth_k.pop(u)
            truth_x.pop(u, None)      # delete_record deletes every cell of u
    (d / "nhydb_config.toml").write_text(f"[index]\nbuild_memory_bytes = {budget}\n")
    db.close()
    db = nhydb.NHyDB(str(d / "x.nhydb"))
    db.build_indexes(commit=True)
    for key in range(31):
        assert list(db.find_u(1, {"k": key})) == sorted(u for u, v in truth_k.items() if v == key), key
        assert list(db.find_u(1, {"x": key})) == sorted(u for u, v in truth_x.items() if v == key), key
    assert db.verify(deep=True)["ok"]


# ----------------------------------------------------------------------
# RA2-07 / RUN7: a DATA block of format 0.16 (tag D10D, no prefix
# checksum) is no longer read at all – rejected on open
# ----------------------------------------------------------------------

def _legacy_tail_block(tmp_path):
    """0.28.2 file whose last DATA block is rewritten as a valid 0.16 block."""
    import struct
    import zlib
    for n in range(3, 60):
        p = tmp_path / f"leg{n}.nhydb"
        db = nhydb.NHyDB.create(str(p))
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{"v": 1, "name": "k", "type": "varuint"}])
        db.create_classic_layout(1, 1, "c", shape_id=1)
        db.put_dense_batch(1, 1, [[7]] * 5, shape_id=1, commit=True)
        db.put_dense_batch(1, 10, [[8]] * n, shape_id=1, commit=True)
        db.close()
        raw = bytearray(p.read_bytes())
        off, end, bt, tag = [b for b in _blocks(raw) if b[2] == 4][-1]
        plen = struct.unpack_from("<Q", raw, off + 16)[0]
        if (plen + 7) & ~7 != (plen - 4 + 7) & ~7:
            continue
        pay = bytes(raw[off + 32:off + 32 + plen])
        assert pay[:4] == b"D28D" and all(b < 128 for b in pay[4:8])
        new = b"D10D" + pay[4:8] + pay[12:]        # realm, shape, u_start, count; no prefix checksum
        raw[off + 32:end] = new + bytes(end - off - 32 - len(new))
        struct.pack_into("<Q", raw, off + 16, len(new))
        struct.pack_into("<I", raw, off + 24, zlib.crc32(new))
        struct.pack_into("<I", raw, off + 28, 0)
        struct.pack_into("<I", raw, off + 28, zlib.crc32(bytes(raw[off:off + 32])))
        p.write_bytes(raw)
        return p
    pytest.skip("no block length with unchanged alignment")


def test_ra2_07_legacy_block_rejected(tmp_path):
    p = _legacy_tail_block(tmp_path)
    for ro in (True, False):
        with pytest.raises(nhydb.CorruptionError, match="wrong tag"):
            nhydb.NHyDB(str(p), readonly=ro)


# ----------------------------------------------------------------------
# RC4-01: aggregates over a covering VALUE index keep the record types
# (schema evolution int -> float, bool fields, sparse cells)
# ----------------------------------------------------------------------

def _evo_db(p, second_type, second_vals, sparse=False, index=True):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s1", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 3, "name": "amount", "type": "varint"}])
    db.create_shape(2, 1, "s2", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 3, "name": "amount", "type": second_type}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[k, k % 3] for k in range(1, 301)], shape_id=1)
    db.put_dense_batch(1, 301, [[k, second_vals(k)] for k in range(301, 311)], shape_id=2)
    if sparse:
        db.put_points(1, [(5, 3, 2.0, "f64")])
    if index:
        db.create_index(2, 1, "i_amount", "amount")
        db.build_indexes(commit=True)
    else:
        db.commit()
    return db


AGG_QUERIES = [
    "SELECT SUM(amount) AS s FROM records WHERE amount >= 0",
    "SELECT SUM(amount) AS s, COUNT(*) AS n, AVG(amount) AS a FROM records",
    "SELECT MAX(amount) AS mx, MIN(amount) AS mn, SUM(amount) AS s FROM records WHERE id > 300",
    "SELECT amount, COUNT(*) AS n FROM records WHERE id > 300 GROUP BY amount",
    "SELECT amount, COUNT(*) AS n FROM records GROUP BY amount",
]


def _typed(rows):
    return [{k: (type(v).__name__, v) for k, v in r.items()} for r in rows]


@pytest.mark.parametrize("case", ["f64", "bool", "f64_sparse", "int"])
def test_rc4_01_index_aggregates_keep_record_types(tmp_path, case):
    typ, vals = {"f64": ("f64", lambda k: float(k % 3)), "f64_sparse": ("f64", lambda k: float(k % 3)),
                 "bool": ("bool", lambda k: k % 2 == 0), "int": ("varint", lambda k: k % 3)}[case]
    db = _evo_db(tmp_path / "a.nhydb", typ, vals, sparse=case.endswith("sparse"))
    # reference: the same data without any index (regular aggregator)
    ref = _evo_db(tmp_path / "b.nhydb", typ, vals, sparse=case.endswith("sparse"), index=False)
    for q in AGG_QUERIES:
        got, want = db.sql(q)["rows"], ref.sql(q)["rows"]
        assert _typed(got) == _typed(want), (q, got, want)
    g = db.sql_numpy(AGG_QUERIES[3])["amount"]
    w = ref.sql_numpy(AGG_QUERIES[3])["amount"]
    assert g.dtype == w.dtype and list(g) == list(w)


# ----------------------------------------------------------------------
# RC4-02: sql_iter skips a record deleted during the iteration (general
# path with overlays) instead of yielding a row of None
# ----------------------------------------------------------------------

@pytest.mark.parametrize("q", ["SELECT id, k FROM records", "SELECT id FROM records WHERE id > 100",
                               "SELECT id FROM records WHERE k = 0"])
def test_rc4_02_sql_iter_no_phantom_row(tmp_path, q):
    db = nhydb.NHyDB.create(str(tmp_path / "p.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": "varuint"}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 3] for u in range(1, 601)], shape_id=1)
    db.delete_record(1, 7)                       # overlay: general path
    db.commit()
    before = [r["id"] for r in db.sql(q)["rows"]]
    it = db.sql_iter(q)
    head = [next(it) for _ in range(3)]
    db.delete_record(1, 450)
    db.commit()
    rows = head + list(it)
    after = [r["id"] for r in db.sql(q)["rows"]]
    assert all(r["id"] is not None for r in rows), [r for r in rows if r["id"] is None]
    # rows of a section built before the delete may still show u 450
    # (documented section-wise semantics), never a row of None
    ids = [r["id"] for r in rows]
    assert ids in (before, after), (len(ids), len(before), len(after))


# ----------------------------------------------------------------------
# RC4-05: SQL EXPLAIN carries the hints of explain()
# ----------------------------------------------------------------------

def test_rc4_05_sql_explain_has_hints(tmp_path):
    from nhydb.query import Q, QueryEngine
    (tmp_path / "nhydb_config.toml").write_text("[query]\nmax_result_rows = 10\n")
    db = nhydb.NHyDB.create(str(tmp_path / "e.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 101)], shape_id=1, commit=True)
    want = QueryEngine(db).explain(Q.all(), realm=1).get("hints")
    assert want, "explain() gives no hint for 100 > max_result_rows rows"
    got = db.sql("EXPLAIN SELECT id FROM records")
    assert got.get("hints") == want


# ----------------------------------------------------------------------
# RD4-01..08: result files, sort runs and cleanup
# ----------------------------------------------------------------------

def _res_db(tmp_path, n=30000, extra=""):
    rd = tmp_path / "results"
    rd.mkdir(exist_ok=True)
    (tmp_path / "nhydb_config.toml").write_text(
        f'[results]\ntemp_dir = "{rd.as_posix()}"\nsort_memory_bytes = 65536\n{extra}')
    db = nhydb.NHyDB.create(str(tmp_path / "r.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u, "key-%07d" % ((u * 7919) % n) + "x" * 40] for u in range(1, n + 1)], shape_id=1, commit=True)
    return db, rd


def _cleaner(rd, stop, counter):
    while not stop.is_set():
        counter.append(nhydb.cleanup_results(str(rd)))


def test_rd4_01_02_cleanup_never_removes_active_files(tmp_path):
    import threading
    db, rd = _res_db(tmp_path)
    q = "SELECT id, k FROM c ORDER BY k DESC"
    want = db.sql(q)["rows"]
    stop, removed = threading.Event(), []
    t = threading.Thread(target=_cleaner, args=(rd, stop, removed))
    t.start()
    try:
        for _ in range(4):
            assert list(db.sql_iter(q)) == want
        for i in range(60):
            rf = db.sql_to_file("SELECT id FROM c WHERE id < %d" % (i + 2))
            assert rf.fetchall() == [{"id": u} for u in range(1, i + 2)]
            rf.close()
        jobs = [db.submit_sql(q) for _ in range(4)]
        for j in jobs:
            rf = j.wait(120)
            assert rf.fetchall() == want
            rf.close()
    finally:
        stop.set()
        t.join()
    assert sum(removed) == 0, sum(removed)
    assert sorted(p.name for p in rd.iterdir()) == []


def test_rd4_02_cleanup_in_another_process(tmp_path):
    """Real second process cleaning the shared result directory."""
    db, rd = _res_db(tmp_path, n=8000)
    code = textwrap.dedent(f"""
        import nhydb, time, sys
        t0 = time.time(); n = 0
        while time.time() - t0 < 6:
            n += nhydb.cleanup_results({str(rd)!r})
        print(n)
    """)
    p = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE, text=True)
    q = "SELECT id, k FROM c ORDER BY k"
    want = db.sql(q)["rows"]
    import time
    t0 = time.time()
    while time.time() - t0 < 5:
        rf = db.sql_to_file(q)
        assert rf.rows == len(want)
        rf.close()
        assert list(db.sql_iter(q))[:5] == want[:5]
    out, _ = p.communicate(timeout=60)
    assert int(out.strip()) == 0, out


def test_rd4_03_parallel_sort_jobs_no_collision(tmp_path):
    db, rd = _res_db(tmp_path, n=6000)
    q = "SELECT id FROM c ORDER BY k"
    want = db.sql(q)["rows"]
    for _ in range(4):
        jobs = [db.submit_sql(q) for _ in range(16)]
        for j in jobs:
            assert j.wait(120).fetchall() == want


def test_rd4_04_wait_timeouts(tmp_path):
    db, rd = _res_db(tmp_path, n=200)
    j = db.submit_sql("SELECT id FROM c LIMIT 3")
    assert j.wait(float("inf")).rows == 3
    assert j.wait(1e30).rows == 3
    with pytest.raises(ValueError):
        db.submit_sql("SELECT id FROM c LIMIT 3").wait(float("nan"))


@pytest.mark.skipif(sys.platform == "win32", reason="RLIMIT_NOFILE is POSIX-only")
def test_rd4_05_many_runs_with_few_file_descriptors(tmp_path):
    out = _child(f"""
        import resource, nhydb
        from pathlib import Path
        d = Path({str(tmp_path)!r})
        rd = d / "results"; rd.mkdir()
        (d / "nhydb_config.toml").write_text('[results]\\ntemp_dir = "%s"\\nsort_memory_bytes = 65536\\n' % rd.as_posix())
        db = nhydb.NHyDB.create(str(d / "r.nhydb"))
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{{"v": 1, "name": "id", "type": "varuint"}}, {{"v": 2, "name": "k", "type": "utf8"}}])
        db.create_classic_layout(1, 1, "c", shape_id=1)
        n = 200000
        db.put_dense_batch(1, 1, [[u, "%07d" % ((u * 7919) % n) + "k" * 150] for u in range(1, n + 1)], shape_id=1, commit=True)
        q = "SELECT id FROM c ORDER BY k"
        want = [r["id"] for r in db.sql(q)["rows"]]
        resource.setrlimit(resource.RLIMIT_NOFILE, (256, 256))
        got = [r["id"] for r in db.sql_iter(q)]
        assert got == want
        assert list(rd.iterdir()) == []
        print("ok", len(got))
    """, timeout=300)
    assert out.split()[-1] == "200000"


def test_rd4_07_caller_files_and_reserved_names(tmp_path):
    db, rd = _res_db(tmp_path, n=50)
    own = rd / "nhydb-export.nhyc"
    db.sql_to_file("SELECT id FROM c", path=str(own)).close()
    assert nhydb.cleanup_results(str(rd)) == 0
    assert own.exists()
    with pytest.raises(ValueError, match="reserved"):
        db.sql_to_file("SELECT id FROM c", path=str(rd / "nhydb-123-abc-0.nhyc"))


def test_rd4_08_existing_target_never_replaced(tmp_path):
    db, rd = _res_db(tmp_path, n=50)
    target = tmp_path / "out.nhyc"
    target.write_bytes(b"mine")
    with pytest.raises(FileExistsError):
        db.sql_to_file("SELECT id FROM c", path=str(target))
    assert target.read_bytes() == b"mine"
    assert not (tmp_path / "out.nhyc.part").exists()


def test_rc4_02_sparse_only_u_stays_a_row(tmp_path):
    """A u with only sparse cells (no dense record) is a row of sql() and of
    sql_iter alike (found by the RUN5 fuzz: the first RC4-02 fix skipped it)."""
    db = nhydb.NHyDB.create(str(tmp_path / "s.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": "varuint"}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 3] for u in range(1, 51)], shape_id=1)
    db.put_points(1, [(70, 9, 5, "varuint")])          # sparse-only u 70, unselected field
    db.delete_record(1, 7)
    db.commit()
    for q in ["SELECT id FROM records", "SELECT id, k FROM records WHERE k IS NULL"]:
        assert list(db.sql_iter(q)) == db.sql(q)["rows"], q


# ----------------------------------------------------------------------
# Follow-ups of the independent RUN5 review (RY-02/03/04)
# ----------------------------------------------------------------------

def _dead_pid():
    p = subprocess.Popen([sys.executable, "-c", "pass"])
    p.wait()
    return p.pid


@pytest.mark.skipif(sys.platform == "win32", reason="process check only on Unix")
def test_ry_03_cleanup_removes_leftovers_of_dead_processes(tmp_path):
    db, rd = _res_db(tmp_path, n=10)
    pid = _dead_pid()
    dead = [rd / f".nhydb-sortrun_{pid}_0_00000000.tmp", rd / f".nhydb-{pid}-abc-1.nhyc.part.init"]
    alive = [rd / f".nhydb-sortrun_{os.getpid()}_0_00000000.tmp", rd / ".nhydb-1-abc-1.nhyc.part.init", rd / ".other.tmp"]
    for f in dead + alive:
        f.write_bytes(b"")
    assert nhydb.cleanup_results(str(rd)) == 2
    assert not any(f.exists() for f in dead) and all(f.exists() for f in alive)


def test_ry_04_reserved_name_through_dotdot(tmp_path):
    db, rd = _res_db(tmp_path, n=10)
    (rd / "sub").mkdir()
    with pytest.raises(ValueError, match="reserved"):
        db.sql_to_file("SELECT id FROM c", path=str(rd / "sub" / ".." / "nhydb-99999-abc-1.nhyc"))


def test_ry_02_staged_merge_order(tmp_path):
    # more than RUN_FINAL_FANIN (128) runs: staged merge of the smallest runs
    # keeps the order of sql() (the budget effect is measured in the report)
    db, rd = _res_db(tmp_path, n=120000)
    q = "SELECT id FROM c ORDER BY k DESC"
    want = [r["id"] for r in db.sql(q)["rows"]]
    assert [r["id"] for r in db.sql_iter(q)] == want
