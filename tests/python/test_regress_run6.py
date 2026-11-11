"""Regression pool of RUN6 (0.28.2): ORDER BY of the streaming consumers is
bounded in every query path (sorted runs in the result directory, never an
unbounded in-memory sort), the result bound of the collecting calls
(`[query] max_result_bytes`), and the errors of the sort runs.

Each test sets its configuration explicitly (`NHYDB_CONFIG`); with
`sort_memory_bytes = 1` (= the 64 KiB minimum) every sort beyond a few
hundred rows writes runs.
"""
import math
import os
import random
import subprocess
import sys
import textwrap

import pytest

import nhydb


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


@pytest.fixture
def cfg(tmp_path, monkeypatch):
    rd = tmp_path / "results"
    rd.mkdir()

    def write(text=""):
        p = tmp_path / "cfg.toml"
        p.write_text(f'[results]\ntemp_dir = "{rd.as_posix()}"\nsort_memory_bytes = 1\n{text}')
        monkeypatch.setenv("NHYDB_CONFIG", str(p))
        return rd
    return write


def _realm(db, n, shapes=1, overlay=False, sparse=False, floatkey=False, seed=3):
    rnd = random.Random(seed)
    db.create_realm(1, "r")
    ktype = "f64" if floatkey else "varint"
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": ktype},
                                {"v": 3, "name": "t", "type": "utf8"}])
    if shapes > 1:
        db.create_shape(2, 1, "s2", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": "varint"},
                                     {"v": 3, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    half = n // 2
    mk = lambda u: [u, rnd.randrange(-50, 50) + (0.5 if floatkey else 0),
                    rnd.choice(["a", "b", "c", "ä", ""]) * rnd.randrange(1, 4)]
    db.put_dense_batch(1, 1, [mk(u) for u in range(1, half + 1)], shape_id=1)
    db.put_dense_batch(1, half + 1, [[u, int(r[1]) if isinstance(r[1], float) else r[1], r[2]] for u, r in
                                     ((u, mk(u)) for u in range(half + 1, n + 1))], shape_id=2 if shapes > 1 else 1)
    if overlay:
        db.delete_record(1, 7)
    if sparse:
        db.put_points(1, [(u, 2, rnd.randrange(-50, 50), "varint") for u in range(3, n, 97)])
    db.commit()


QUERIES = [
    "SELECT id, k FROM c ORDER BY k",
    "SELECT id, k FROM c ORDER BY k DESC",
    "SELECT id, t FROM c ORDER BY t DESC, k",
    "SELECT id FROM c ORDER BY k, t DESC",
    "SELECT id, k FROM c ORDER BY k LIMIT 50 OFFSET 30",
    "SELECT id, k FROM c WHERE k > 0 ORDER BY t, k DESC",
    "SELECT id FROM c ORDER BY k DESC LIMIT 0",
]


@pytest.mark.parametrize("variant", ["plain", "overlay", "twoshapes", "sparse", "floatkey"])
def test_run6_streaming_sort_equals_sql_in_every_path(tmp_path, cfg, variant):
    """RD4-10: every streaming consumer sorts like sql(), with runs on disk."""
    rd = cfg()
    db = nhydb.NHyDB.create(str(tmp_path / "x.nhydb"))
    _realm(db, 6000, shapes=2 if variant == "twoshapes" else 1, overlay=variant == "overlay",
           sparse=variant == "sparse", floatkey=variant == "floatkey")
    for q in QUERIES:
        want = db.sql(q)["rows"]
        assert list(db.sql_iter(q)) == want, (variant, q)
        rf = db.sql_to_file(q)
        assert rf.fetchall() == want, (variant, q)
        rf.close()
        np = db.sql_numpy(q)
        for col in np:
            assert [x.item() if hasattr(x, "item") else x for x in np[col]] == [r[col] for r in want], (variant, q, col)
    assert [p.name for p in rd.iterdir()] == []


def test_run6_bounded_memory_for_unsupported_layouts(tmp_path):
    """RD4-10: a realm with a delete (general path) sorts within the budget:
    before RUN6 3 M rows took +2.1 GB, here 400 000 rows stay far below."""
    out = subprocess.run([sys.executable, "-c", PEAK + textwrap.dedent(f"""
        import nhydb, os
        d = {str(tmp_path)!r}
        os.makedirs(d + "/res", exist_ok=True)
        open(d + "/cfg.toml", "w").write('[results]\\ntemp_dir = "%s/res"\\nsort_memory_bytes = 8388608\\n' % d.replace(os.sep, '/'))
        os.environ["NHYDB_CONFIG"] = d + "/cfg.toml"
        db = nhydb.NHyDB.create(d + "/x.nhydb")
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{{"v": 1, "name": "id", "type": "varuint"}}, {{"v": 2, "name": "k", "type": "varuint"}}, {{"v": 3, "name": "t", "type": "utf8"}}])
        db.create_classic_layout(1, 1, "c", shape_id=1)
        n = 400000
        db.put_dense_batch(1, 1, [[u, (u * 7919) % n, "text %d" % u] for u in range(1, n + 1)], shape_id=1)
        db.delete_record(1, 7); db.commit()
        r0 = peak_mib()
        prev, cnt = None, 0
        for row in db.sql_iter("SELECT id, k, t FROM c ORDER BY k DESC"):
            assert prev is None or prev >= row["k"]
            prev = row["k"]; cnt += 1
        print(cnt, peak_mib() - r0)
    """)], capture_output=True, text=True, env={k: v for k, v in os.environ.items() if k != "NHYDB_CONFIG"})
    assert out.returncode == 0, out.stderr
    cnt, mb = map(int, out.stdout.split())
    assert cnt == 399999
    assert mb < 150, mb        # in-memory sort of all rows needed ~ 290 MB


def test_run6_nan_beyond_budget_is_a_clear_error(tmp_path, cfg):
    """NaN has no total order for runs: within the budget the stream sorts
    exactly like sql(); beyond it a QueryLimitError, never another order."""
    rd = cfg()
    db = nhydb.NHyDB.create(str(tmp_path / "x.nhydb"))
    _realm(db, 50, floatkey=True)
    db.put_dense_batch(1, 51, [[51, math.nan, "n"], [52, 1.5, "m"]], shape_id=1, commit=True)
    q = "SELECT id, k FROM c ORDER BY k"
    assert [r["id"] for r in db.sql_iter(q)] == [r["id"] for r in db.sql(q)["rows"]]   # 52 rows: in memory
    db.put_dense_batch(1, 60, [[u, float(u % 7), "x" * 30] for u in range(60, 9000)], shape_id=1, commit=True)
    with pytest.raises(nhydb.QueryLimitError, match="NaN"):
        list(db.sql_iter(q))
    assert len(db.sql(q)["rows"]) == 52 + 8940      # sql() unchanged


def test_run6_incomparable_keys_give_the_error_of_sql(tmp_path, cfg):
    cfg()
    db = nhydb.NHyDB.create(str(tmp_path / "x.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": "utf8"}])
    db.create_shape(2, 1, "s2", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": "varint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u, "s%05d" % u] for u in range(1, 4000)], shape_id=1)
    db.put_dense_batch(1, 5000, [[u, u] for u in range(5000, 9000)], shape_id=2, commit=True)
    q = "SELECT id, k FROM c ORDER BY k"
    with pytest.raises(nhydb.QueryError) as a:
        db.sql(q)
    with pytest.raises(nhydb.QueryError) as b:
        list(db.sql_iter(q))
    assert type(a.value) is type(b.value) and str(a.value) == str(b.value)


def test_run6_max_result_bytes_bounds_collecting_calls(tmp_path, monkeypatch):
    p = tmp_path / "cfg.toml"
    p.write_text("[query]\nmax_result_bytes = 200000\n")
    monkeypatch.setenv("NHYDB_CONFIG", str(p))
    db = nhydb.NHyDB.create(str(tmp_path / "x.nhydb"))
    _realm(db, 6000)
    for q in ["SELECT id, k, t FROM c", "SELECT id, k FROM c ORDER BY k", "SELECT id, COUNT(*) AS n FROM c GROUP BY id"]:
        with pytest.raises(nhydb.QueryLimitError, match="max_result_bytes"):
            db.sql(q)
    assert len(db.sql("SELECT id FROM c LIMIT 100")["rows"]) == 100
    assert sum(1 for _ in db.sql_iter("SELECT id, k, t FROM c ORDER BY k")) == 6000   # streams are not bounded by it
    p.write_text("[query]\nmax_result_bytes = 0\n")
    db.close()
    db = nhydb.NHyDB(str(tmp_path / "x.nhydb"))
    assert len(db.sql("SELECT id, k, t FROM c")["rows"]) == 6000


@pytest.mark.skipif(os.name != "posix", reason="POSIX permissions (chmod on the result directory)")
def test_run6_sort_dir_errors_name_cause_and_remedies(tmp_path, monkeypatch):
    """RD4-06: an unwritable result directory gives an OSError with errno
    and an explanation with remedies; an exhausted budget likewise."""
    if os.geteuid() == 0:
        pytest.skip("root ignores file permissions")
    rd = tmp_path / "ro"
    rd.mkdir()
    p = tmp_path / "cfg.toml"
    p.write_text(f'[results]\ntemp_dir = "{rd.as_posix()}"\nsort_memory_bytes = 1\n')
    monkeypatch.setenv("NHYDB_CONFIG", str(p))
    db = nhydb.NHyDB.create(str(tmp_path / "x.nhydb"))
    _realm(db, 6000)
    os.chmod(rd, 0o500)
    try:
        with pytest.raises(PermissionError) as e:
            list(db.sql_iter("SELECT id, k FROM c ORDER BY k"))
        assert e.value.errno == 13
        assert "result directory" in str(e.value) and "remedies" in str(e.value)
    finally:
        os.chmod(rd, 0o700)
    db.close()
    # the budget of a result directory is fixed by its first use in the process
    rd2 = tmp_path / "small"
    rd2.mkdir()
    p.write_text(f'[results]\ntemp_dir = "{rd2.as_posix()}"\nsort_memory_bytes = 1\nbudget_bytes = 10000\n')
    db = nhydb.NHyDB(str(tmp_path / "x.nhydb"))
    with pytest.raises(nhydb.DatabaseSizeLimitError, match="remedies"):
        list(db.sql_iter("SELECT id, k FROM c ORDER BY k"))


def test_rr6_01_large_limit_window_is_bounded_too(tmp_path):
    """RR6-01: ORDER BY … LIMIT/OFFSET with a window larger than the sort
    budget sorts in runs as well (top-k windows only while they fit)."""
    out = subprocess.run([sys.executable, "-c", PEAK + textwrap.dedent(f"""
        import nhydb, os
        d = {str(tmp_path)!r}
        os.makedirs(d + "/res", exist_ok=True)
        open(d + "/cfg.toml", "w").write('[results]\\ntemp_dir = "%s/res"\\nsort_memory_bytes = 1\\n' % d.replace(os.sep, '/'))
        os.environ["NHYDB_CONFIG"] = d + "/cfg.toml"
        db = nhydb.NHyDB.create(d + "/x.nhydb")
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{{"v": 1, "name": "id", "type": "varuint"}}, {{"v": 2, "name": "k", "type": "varuint"}}])
        db.create_classic_layout(1, 1, "c", shape_id=1)
        n = 600000
        db.put_dense_batch(1, 1, [[u, (u * 7919) % n] for u in range(1, n + 1)], shape_id=1, commit=True)
        want = [r["id"] for r in db.sql("SELECT id FROM c ORDER BY k DESC LIMIT 10 OFFSET 500000")["rows"]]
        r0 = peak_mib()
        got = [r["id"] for r in db.sql_iter("SELECT id FROM c ORDER BY k DESC LIMIT 10 OFFSET 500000")]
        assert got == want
        print(peak_mib() - r0)
    """)], capture_output=True, text=True, env={k: v for k, v in os.environ.items() if k != "NHYDB_CONFIG"})
    assert out.returncode == 0, out.stderr
    assert int(out.stdout.split()[-1]) < 60, out.stdout     # window of 500 010 keys in memory: ~ 120 MB


def test_rr6_02_sql_numpy_counts_column_values(tmp_path, monkeypatch):
    """RR6-02: sql_numpy is bounded by the column values (~8 B each), not by
    the row maps (~250 B each)."""
    p = tmp_path / "cfg.toml"
    p.write_text("[query]\nmax_result_bytes = 1000000\n")
    monkeypatch.setenv("NHYDB_CONFIG", str(p))
    db = nhydb.NHyDB.create(str(tmp_path / "x.nhydb"))
    _realm(db, 30000)
    assert len(db.sql_numpy("SELECT id FROM c")["id"]) == 30000      # 30 000 × 8 B fit 1 MB
    with pytest.raises(nhydb.QueryLimitError):
        db.sql("SELECT id FROM c")                                      # 30 000 rows as dicts do not
    with pytest.raises(nhydb.QueryLimitError):
        db.sql_numpy("SELECT id, k, t, id AS a, k AS b FROM c")      # ~ 1.3 MB of column values


def test_rr6_04_index_aggregate_is_bounded(tmp_path, monkeypatch):
    p = tmp_path / "cfg.toml"
    p.write_text("[query]\nmax_result_bytes = 100000\n")
    monkeypatch.setenv("NHYDB_CONFIG", str(p))
    db = nhydb.NHyDB.create(str(tmp_path / "x.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 20001)], shape_id=1, commit=True)
    db.create_index(1, 1, "i_id", "id")
    db.build_indexes(commit=True)
    with pytest.raises(nhydb.QueryLimitError, match="groups"):
        db.sql("SELECT id, COUNT(*) AS n FROM c GROUP BY id")      # 20 000 groups from the index
