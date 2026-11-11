"""Regression pool of the 0.28.2 review round (2026-10-27).

One test (or a group) per finding of the review; the finding id is in
the test name. The scenarios follow the reviewers' reproductions. Settings
come from a `nhydb_config.toml` next to each database (the tests are
independent of `NHYDB_CONFIG`; with that variable set they are skipped).
"""
import copy
import os
import shutil
import struct
import subprocess
import sys
import textwrap
import time
import warnings
from pathlib import Path

import pytest

import nhydb
import timing
import regress_dirtool as t

pytestmark = pytest.mark.skipif(bool(os.environ.get("NHYDB_CONFIG")), reason="settings come from nhydb_config.toml next to the database")

np = pytest.importorskip("numpy")


def _cfg(d, text):
    (Path(d) / "nhydb_config.toml").write_text(textwrap.dedent(text))


def _blocks(raw):
    out, off = [], 8192
    while off + 32 <= len(raw) and raw[off:off + 4] == b"NHBL":
        bt = raw[off + 5]
        plen = struct.unpack_from("<Q", raw, off + 16)[0]
        end = (off + 32 + plen + 7) & ~7
        out.append((off, end, bt, raw[off + 32:off + 36]))
        off = end
    return out


def _dense_db(p, cfg="[directory]\nflush_blocks = 1\n"):
    _cfg(p.parent, cfg)
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u * 3] for u in range(1, 13)], shape_id=1, commit=True)
    db.put_dense_batch(1, 1, [[u * 3 + 1] for u in range(1, 13)], shape_id=1, commit=True)
    db.put_cells_numpy(1, np.array([5], dtype=np.uint64), 1, np.array([50], dtype=np.uint64), commit=True)
    db.put_cells_numpy(1, np.array([5], dtype=np.uint64), 1, np.array([51], dtype=np.uint64), commit=True)
    db.put_dense_batch(1, 100, [[1]], shape_id=1, commit=True)
    db.put_dense_batch(1, 101, [[1]], shape_id=1, commit=True)
    db.close()


def _variant(src, dst, mutate):
    dst.parent.mkdir(parents=True, exist_ok=True)
    for s in (".layout.enc", ".layout.key"):
        shutil.copyfile(str(src) + s, str(dst) + s)
    os.chmod(str(dst) + ".layout.key", 0o600)
    shutil.copyfile(src.parent / "nhydb_config.toml", dst.parent / "nhydb_config.toml")
    raw = bytearray(src.read_bytes())
    mutate(raw)
    dst.write_bytes(bytes(raw))
    return dst


def _swap(raw, a, b):
    sa, sb = bytes(raw[a[0]:a[1]]), bytes(raw[b[0]:b[1]])
    assert len(sa) == len(sb)
    raw[a[0]:a[1]], raw[b[0]:b[1]] = sb, sa


# --------------------------------------------------------------------------
# RD-01 / RD-05: CRC-valid swapped or copied data blocks
# --------------------------------------------------------------------------

@pytest.mark.parametrize("with_dir", [True, False])
def test_rd01_rd05_swapped_data_blocks_never_give_stale_values(tmp_path, with_dir):
    src = tmp_path / "src" / "src.nhydb"
    src.parent.mkdir()
    _dense_db(src, "[directory]\nflush_blocks = 1\n" if with_dir else "[directory]\nflush_blocks = 100000\n")
    bl = _blocks(src.read_bytes())
    dense = [b for b in bl if b[3] == b"D28D"][:2]
    sparse = [b for b in bl if b[3] == b"SP28"][:2]
    cases = {
        "swap_dense": lambda r: _swap(r, *dense),
        "old_over_new_dense": lambda r: r.__setitem__(slice(dense[1][0], dense[1][1]), r[dense[0][0]:dense[0][1]]),
        "swap_sparse": lambda r: _swap(r, *sparse),
    }
    for name, fn in cases.items():
        p = _variant(src, tmp_path / name / "src.nhydb", fn)
        try:
            db = nhydb.NHyDB(p, readonly=True)
        except nhydb.CorruptionError as e:
            # without a directory the open scan sees the block ids out of order
            assert not with_dir and "not ascending" in str(e), (name, e)
            ins = nhydb.NHyDB.inspect_tail(p)
            assert not ins["clean"]
            continue
        with db:
            for fn_read in (lambda: db.get(1, 3)["a"], lambda: db.get(1, 5)["a"], lambda: db.read_numpy(1, 1, 3)["a"].tolist()):
                try:
                    v = fn_read()
                except nhydb.CorruptionError:
                    continue
                # a value that is returned is the newest one
                assert v in (10, 51, [4, 7, 10]), (name, v)
            assert not db.verify(deep=True)["ok"], name
            # RD-05: no panic in the index build
            try:
                db2 = nhydb.NHyDB(p)
            except nhydb.NHyDBError:
                continue
            with db2:
                db2.create_index(1, 1, "by_a", "a")
                with pytest.raises(nhydb.NHyDBError):
                    db2.build_indexes()


# --------------------------------------------------------------------------
# RA-01 / RD-09: damaged header of a (redundant) directory block
# --------------------------------------------------------------------------

def _many_blocks_db(p, n=400, cfg="[directory]\nflush_blocks = 16\ncache_entries = 64\n"):
    _cfg(p.parent, cfg)
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    for u in range(1, n + 1):
        db.put_dense_batch(1, u, [[u]], shape_id=1, commit=(u % 20 == 0))
        if u % 3 == 0:
            db.put_cell(1, u, 5, u * 7, type="varuint")
    db.commit()
    db.close()


def _dir_offsets(p, tag):
    return [b[0] for b in _blocks(p.read_bytes()) if b[2] == 14 and b[3] == tag]


@pytest.mark.parametrize("which", ["live_root", "live_run", "dead_block", "live_page"])
def test_ra01_rd09_damaged_directory_header_is_not_data_damage(tmp_path, which):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _many_blocks_db(p)
    with nhydb.NHyDB(p, readonly=True) as db:
        st = db.stats()["directory"]
        want = {u: db.get(1, u)["a"] for u in range(1, 401)}
        cells = {u: db.get_value(1, u, 5) for u in range(3, 401, 3)}
    d = t.DB(str(p))
    root = d.sup["dir_root"]
    runs = d.root()["runs"]
    live_pages = {pg["off"] for r in runs for part in d.run(r)["parts"] for pg in part["pages"]}
    target = {
        "live_root": root,
        "live_run": runs[-1],
        "dead_block": next(o for o in _dir_offsets(p, b"DT17") if o != root),
        "live_page": sorted(live_pages)[0],
    }[which]
    raw = bytearray(p.read_bytes())
    raw[target + 10] ^= 0x10  # block id byte: header CRC fails
    p.write_bytes(bytes(raw))
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        with nhydb.NHyDB(p, readonly=True) as db:
            assert db.stats()["file_bytes"] == len(raw)  # stats() works (RD-09)
            v = db.verify()
            if which == "dead_block":
                assert v["ok"] and any("unused block directory block damaged" in w for w in v["warnings"])
            else:
                assert not v["ok"] or which != "live_page"
            if which != "live_page":
                assert {u: db.get(1, u)["a"] for u in range(1, 401)} == want
        # lossless repair and compaction
        rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False)
        assert not rep.get("discarded_uncommitted_blocks"), rep
        with nhydb.NHyDB(p) as db:
            assert {u: db.get(1, u)["a"] for u in range(1, 401)} == want
            assert {u: db.get_value(1, u, 5) for u in range(3, 401, 3)} == cells
            assert db.verify(deep=True)["ok"]
        nhydb.NHyDB.compact_file(p, make_backup=False)
        with nhydb.NHyDB(p) as db:
            assert {u: db.get(1, u)["a"] for u in range(1, 401)} == want
    assert st["runs"] >= 1


def test_ra01_compaction_with_damaged_live_page(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _many_blocks_db(p)
    with nhydb.NHyDB(p, readonly=True) as db:
        want = {u: db.get(1, u)["a"] for u in range(1, 401)}
    d = t.DB(str(p))
    page = sorted(pg["off"] for r in d.root()["runs"] for part in d.run(r)["parts"] for pg in part["pages"])[0]
    raw = bytearray(p.read_bytes())
    raw[page + 40] ^= 0x01  # payload: CRC fails
    p.write_bytes(bytes(raw))
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        nhydb.NHyDB.compact_file(p, make_backup=False)
        with nhydb.NHyDB(p) as db:
            assert {u: db.get(1, u)["a"] for u in range(1, 401)} == want
            assert db.verify(deep=True)["ok"]


# --------------------------------------------------------------------------
# RA-02 / RB-06: directory growth with checkpoints and many keys
# --------------------------------------------------------------------------

def test_ra02_checkpoints_do_not_rewrite_the_whole_directory(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _cfg(p.parent, "[directory]\nflush_blocks = 1000000\n")
    db = nhydb.NHyDB.create(p)
    db.create_realm(2, "s")
    sizes = []
    for i in range(1, 16385):
        db.put_cell(2, 1, i, i, type="varuint")
        if i % 1024 == 0:
            db.checkpoint()
            st = db.stats()
            sizes.append((st["file_bytes"], st["storage"]["by_block_type"].get("DIRECTORY", {})))
    db.close()
    total = sizes[-1][0]
    dir_bytes = sizes[-1][1].get("bytes", 0) if isinstance(sizes[-1][1], dict) else 0
    # leveled merging: the directory stays a small fraction of the file (it
    # was 2.85 x the data with a full rewrite at every checkpoint)
    assert dir_bytes < 0.5 * total, (dir_bytes, total)


def test_rb06_compaction_of_many_small_keys_writes_no_directory(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    db = nhydb.NHyDB.create(p)
    for r in range(1, 2001):
        db.create_realm(r, f"r{r}")
    db.commit()
    for r in range(1, 2001):
        db.put_cell(r, 1, 1, r, type="varuint")
    db.commit()
    db.close()
    nhydb.NHyDB.compact_file(p, make_backup=False)
    # 2 000 keys with one block each: no directory (it grew the file from
    # 120 kB to 1.01 MB; 0.28.0 and now: about 257 kB, one block per realm)
    assert not [b for b in _blocks(p.read_bytes()) if b[2] == 14]
    assert p.stat().st_size < 400_000
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get_value(1234, 1, 1) == 1234


# --------------------------------------------------------------------------
# RA-03: directory blocks respect db_max_size_bytes
# --------------------------------------------------------------------------

def test_ra03_directory_respects_size_limit(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _cfg(p.parent, "[directory]\nflush_blocks = 2\n")
    db = nhydb.NHyDB.create(p)
    db.create_realm(2, "s")
    db.commit()
    limit = p.stat().st_size + 4096
    db.set_max_db_size(limit, commit=True)
    for i in range(1, 100000):
        try:
            db.put_cell(2, i, 1, i, type="varuint", commit=True)
        except nhydb.DatabaseSizeLimitError:
            break
        assert p.stat().st_size <= limit
    db.close()
    assert p.stat().st_size <= limit
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.verify(deep=True)["ok"]


# --------------------------------------------------------------------------
# RA-04 / RD-07: failed merge (damaged live page) is not retried per write
# --------------------------------------------------------------------------

def test_ra04_damaged_page_degrades_directory_without_retry_storm(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _cfg(p.parent, "[directory]\nflush_blocks = 4\n")
    db = nhydb.NHyDB.create(p)
    db.create_realm(2, "s")
    for i in range(1, 29):
        db.put_cell(2, i, 1, i, type="varuint", commit=True)
    db.close()
    d = t.DB(str(p))
    page = sorted(pg["off"] for r in d.root()["runs"] for part in d.run(r)["parts"] for pg in part["pages"])[0]
    raw = bytearray(p.read_bytes())
    raw[page + 40] ^= 0x01
    p.write_bytes(bytes(raw))
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        db = nhydb.NHyDB(p)
        t0 = time.perf_counter()
        for i in range(1000, 3000):
            db.put_cell(2, i, 1, i, type="varuint")
        dt = time.perf_counter() - t0
        db.commit()
        st = db.stats()["directory"]
        assert st["degraded"], st
        assert st["tail_entries"] < 100, st  # the tail is still written as runs
        assert any("not merged any more" in w for w in db.verify()["warnings"])
        db.close()
    assert dt < timing.limit(2.0), dt
    rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False)
    with nhydb.NHyDB(p) as db:
        assert db.get_value(2, 2999, 1) == 2999 and db.get_value(2, 5, 1) == 5
        assert db.verify(deep=True)["ok"], rep


# --------------------------------------------------------------------------
# RA-05: an I/O error of the flush after a write call
# --------------------------------------------------------------------------

@pytest.mark.skipif(os.name != "posix", reason="RLIMIT_FSIZE")
def test_ra05_flush_error_after_write_call_keeps_the_call_atomic(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _cfg(p.parent, "[directory]\nflush_blocks = 2000\n")
    code = f"""
import resource, signal, nhydb
signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
db = nhydb.NHyDB.create({str(p)!r})
db.create_realm(2, "s")
db.commit()
for i in range(1, 1990):
    db.put_cell(2, i, 1, i, type="varuint")
db.commit()
import os
size = os.path.getsize({str(p)!r})
resource.setrlimit(resource.RLIMIT_FSIZE, (size + 70000, resource.RLIM_INFINITY))
ok = 0
for i in range(5000, 5100):
    try:
        db.put_cell(2, i, 1, i, type="varuint")
        ok += 1
    except OSError:
        print("WRITE_ERROR", i, db.get_value(2, i, 1))
        break
print("OK", ok)
"""
    r = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=120)
    assert r.returncode == 0, r.stderr
    # a write call reports an error only for its own block, never for an
    # optional directory flush after it
    for line in r.stdout.splitlines():
        if line.startswith("WRITE_ERROR"):
            assert line.endswith("None"), line


# --------------------------------------------------------------------------
# RA-06: open + close after a directory fallback writes nothing
# --------------------------------------------------------------------------

def test_ra06_open_close_after_fallback_writes_nothing(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _many_blocks_db(p, n=100)
    d = t.DB(str(p))
    raw = bytearray(p.read_bytes())
    raw[d.sup["dir_root"] + 40] ^= 0x01  # root payload: fallback to the full scan
    p.write_bytes(bytes(raw))
    before = p.read_bytes()
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        db = nhydb.NHyDB(p)
        assert db.get(1, 50)["a"] == 50
        db.close()
    assert p.read_bytes() == before


# --------------------------------------------------------------------------
# RA-07 / RD-03 / RD-04 / RD-11 / RA-08: forged CRC-valid directory states
# --------------------------------------------------------------------------

def _forge_base(tmp_path):
    base = tmp_path / "base" / "db.nhydb"
    base.parent.mkdir()
    _dense_db(base, "[directory]\nflush_blocks = 7\n")
    db = nhydb.NHyDB(base)
    db.delete_record(1, 10)
    db.commit()
    db.close()
    d = t.DB(str(base))
    assert len(d.root()["runs"]) == 1
    return base


def _forged(base, tmp_path, name, fn):
    p = tmp_path / name / "db.nhydb"
    p.parent.mkdir()
    for s in ("", ".layout.enc", ".layout.key"):
        shutil.copyfile(str(base) + s, str(p) + s)
    os.chmod(str(p) + ".layout.key", 0o600)
    shutil.copyfile(base.parent / "nhydb_config.toml", p.parent / "nhydb_config.toml")
    d = t.DB(str(p))
    fn(d)
    d.save(str(p))
    return p


def _write_dir(d, root, parts, runs=None):
    run_off = d.append(t.enc_run(1, parts))
    root = copy.deepcopy(root)
    root["runs"] = [run_off] if runs is None else runs(run_off)
    d.commit(d.append(t.enc_root(root)))


def test_ra07_rd03_rd04_rd11_ra08_forged_directory(tmp_path):
    base = _forge_base(tmp_path)

    def wrong_aggregates(d):
        root = d.root()
        for k in root["keys"]:
            if k["dense_records"]:
                k["dense_records"] = 5
        _write_dir(d, root, d.run(root["runs"][0])["parts"])

    def overlay_flags_off(d):
        root = d.root()
        for k in root["keys"]:
            k["overlay"] = k["dense_overlap"] = False
        root["has_overlays"] = False
        _write_dir(d, root, d.run(root["runs"][0])["parts"])

    def wrong_max_u(d):
        root = d.root()
        for k in root["keys"]:
            if k["dense_records"]:
                k["max_u"] = 6
        _write_dir(d, root, d.run(root["runs"][0])["parts"])

    def forged_delete(d):
        root = d.root()
        parts = d.run(root["runs"][0])["parts"]
        part = next(x for x in parts if x["kind"] == 3)
        ents = [{"u": 3, "bid": 99}, {"u": 10, "bid": part["pages"][0]["max_bid"]}]
        off = d.append(t.enc_page(3, 1, 0, ents))
        part["pages"] = [dict(off=off, **{"n": 2, "first": 3, "max_hi": 10, "max_bid": max(99, ents[1]["bid"])})]
        _write_dir(d, root, parts)

    def runs_twice(d):
        root = d.root()
        _write_dir(d, root, d.run(root["runs"][0])["parts"], runs=lambda o: [o, o])

    def long_number(d):
        root = d.root()
        root["keys"][0]["max_u"] = int("9" * 2000)
        _write_dir(d, root, d.run(root["runs"][0])["parts"])

    def root_beyond_end(d):
        d.write_super(dir_root=d.sup["end"] + 8)

    detected_by_deep = {"wrong_aggregates": wrong_aggregates, "overlay_flags_off": overlay_flags_off, "forged_delete": forged_delete, "wrong_max_u": wrong_max_u}
    falls_back = {"runs_twice": runs_twice, "long_number": long_number, "root_beyond_end": root_beyond_end}
    with nhydb.NHyDB(base, readonly=True) as db:
        ref = (db.get(1, 3)["a"], db.get(1, 10), db.get(1, 5)["a"])
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        for name, fn in detected_by_deep.items():
            p = _forged(base, tmp_path, name, fn)
            with nhydb.NHyDB(p, readonly=True) as db:
                assert db.stats()["directory"]["loaded"], name
                v = db.verify(deep=True)
                assert not v["ok"], (name, v)
        for name, fn in falls_back.items():
            p = _forged(base, tmp_path, name, fn)
            t0 = time.perf_counter()
            with nhydb.NHyDB(p, readonly=True) as db:
                assert db.stats()["directory"]["fallback_full_scan"], name
                assert (db.get(1, 3)["a"], db.get(1, 10), db.get(1, 5)["a"]) == ref
                assert db.verify(deep=True)["ok"], name
            assert time.perf_counter() - t0 < 5, name


# --------------------------------------------------------------------------
# RB-01 / RB-02 / RB-03 / RC-01: external merge (> 4096 overlapping blocks)
# --------------------------------------------------------------------------

def _overlap_db(p, n=4200, size=64):
    db = nhydb.NHyDB.create(p)
    db.create_realm(2, "s")
    db.commit()
    for i in range(n):
        db.put_cell(2, 1, 1 + (i % 50), "x" * size + str(i), type="utf8")
        if i % 500 == 0:
            db.commit()
    db.commit()
    db.close()


def test_rb02_rb03_rc01_external_merge_leaves_nothing_and_needs_no_tmpdir(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _overlap_db(p)
    tmp = tmp_path / "tmp"
    tmp.mkdir()
    code = f"""
import nhydb
with nhydb.NHyDB({str(p)!r}, readonly=True) as db:
    pts = list(db.iter_materialized_sparse_points(2))
print(len(pts), pts[0])
"""
    for tmpdir in (str(tmp), str(tmp_path / "missing")):
        env = dict(os.environ, TMPDIR=tmpdir)
        r = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, env=env, timeout=300)
        assert r.returncode == 0, r.stderr
        assert r.stdout.startswith("50 ")
    assert not list(tmp.iterdir())
    assert not [f for f in p.parent.iterdir() if "merge" in f.name]
    # killed during an external merge: nothing is left behind
    killer = f"""
import nhydb
nhydb.NHyDB.compact_file({str(p)!r}, make_backup=False)
"""
    proc = subprocess.Popen([sys.executable, "-c", killer], env=dict(os.environ, TMPDIR=str(tmp)))
    time.sleep(0.3)
    proc.kill()
    proc.wait()
    leftovers = [f.name for f in p.parent.iterdir() if "merge" in f.name] + [f.name for f in tmp.iterdir()]
    assert not leftovers, leftovers
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        with nhydb.NHyDB(p) as db:
            assert db.verify(deep=True)["ok"]


# --------------------------------------------------------------------------
# RC-02: more than 2^18 blocks – queries do not re-check every block
# --------------------------------------------------------------------------

def test_rc02_queries_over_many_blocks_do_not_reload_everything(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    n = 270_000
    for u in range(1, n + 1):
        db.put_dense_batch(1, u, [[u % 7]], shape_id=1, commit=(u % 50_000 == 0))
    db.commit()
    db.close()
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.sql("SELECT COUNT(*) AS n FROM c")["rows"][0]["n"] == n
        loads = db.stats()["block_cache"]["loads"]
        assert db.sql("SELECT COUNT(*) AS n FROM c")["rows"][0]["n"] == n
        again = db.stats()["block_cache"]["loads"] - loads
        assert again < n // 10, again


# --------------------------------------------------------------------------
# G1–G4: sparse v index
# --------------------------------------------------------------------------

def _sparse_db(p, n, order, cfg=None):
    if cfg:
        _cfg(p.parent, cfg)
    db = nhydb.NHyDB.create(p)
    db.create_realm(2, "s")
    db.commit()
    for i in range(n):
        v = {"asc": i + 1, "perm": (i * 7919) % n + 1, "same": 1}[order]
        db.put_cell(2, 1, v, v if order != "same" else i, type="varuint", commit=((i + 1) % 20_000 == 0))
    db.commit()
    db.close()


def test_g1_unordered_records_build_in_n_log_n(tmp_path):
    times = {}
    for order in ("asc", "perm"):
        p = tmp_path / order / "db.nhydb"
        p.parent.mkdir()
        _sparse_db(p, 60_000, order)
        with nhydb.NHyDB(p, readonly=True) as db:
            t0 = time.perf_counter()
            assert db.get_value(2, 1, 30_001) == 30_001
            times[order] = time.perf_counter() - t0
            assert all(db.get_value(2, 1, v) == v for v in range(1, 60_001, 997))
    # quadratic before: 60 000 unordered records took ~25x as long
    assert times["perm"] < 4 * times["asc"] + timing.limit(0.2), times


def test_g2_g3_overwritten_versions_cost_no_index_memory(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _sparse_db(p, 50_000, "same")
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get_value(2, 1, 1) == 49_999
        bc = db.stats()["block_cache"]
        assert bc["vindex_bytes"] < 1 << 16, bc  # one live cell, not 50 000 records
        t0 = time.perf_counter()
        for _ in range(2000):
            assert db.get_value(2, 1, 1) == 49_999
        assert time.perf_counter() - t0 < 1.0


def test_g2_index_budget_is_respected(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _cfg(p.parent, "[cache]\nvindex_max_marks = 20000\n")
    db = nhydb.NHyDB.create(p)
    db.create_realm(2, "s")
    for u in range(1, 41):
        for v in range(1, 301):
            db.put_cell(2, u, v, v, type="varuint")
    db.commit()
    db.close()
    with nhydb.NHyDB(p, readonly=True) as db:
        for u in range(1, 41):
            assert db.get_value(2, u, 150) == 150
            bc = db.stats()["block_cache"]
            assert bc["vindex_bytes"] <= bc["vindex_budget_bytes"], bc


def test_g4_directory_flush_does_not_drop_the_v_index(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _sparse_db(p, 5000, "perm", cfg="[directory]\nflush_blocks = 64\n")
    db = nhydb.NHyDB(p)
    assert db.get_value(2, 1, 77) == 77
    builds = db.stats()["block_cache"]["vindex_builds"]
    for i in range(1, 400):
        db.put_cell(2, 1, 5000 + i, i, type="varuint")
        assert db.get_value(2, 1, 5000 + i) == i
    st = db.stats()
    assert st["directory"]["flushes"] >= 5
    assert st["block_cache"]["vindex_builds"] == builds, st["block_cache"]
    assert st["block_cache"]["vindex_catchups"] >= 300
    db.commit()
    db.close()


def test_fall_d_interleaved_single_cells_open_fast_without_directory(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _cfg(p.parent, "[directory]\nflush_blocks = 100000000\n")
    db = nhydb.NHyDB.create(p)
    db.create_realm(2, "s")
    db.commit()
    nu = 100
    for i in range(60_000):
        db.put_cell(2, i % nu + 1, i // nu + 1, i, type="varuint", commit=((i + 1) % 20_000 == 0))
    db.commit()
    db.close()
    t0 = time.perf_counter()
    with nhydb.NHyDB(p, readonly=True) as db:
        dt = time.perf_counter() - t0
        assert db.get_value(2, 7, 300) == 299 * nu + 6
    assert dt < timing.limit(3.0), dt  # the tail interval index was quadratic in this pattern


# --------------------------------------------------------------------------
# F021 residue: a DATA block of the open-scan tail with a damaged routing
# prefix never becomes a directory entry
# --------------------------------------------------------------------------

def test_f021_damaged_tail_prefix_is_rejected_on_open(tmp_path):
    """RD-08: a flipped u_start in the tail is caught by the prefix
    checksum on open (it never reaches a directory flush)."""
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    _cfg(p.parent, "[directory]\nflush_blocks = 100000000\n")
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    for u in range(1, 41):
        db.put_dense_batch(1, u * 100, [[u]], shape_id=1, commit=True)
    db.close()
    raw = bytearray(p.read_bytes())
    data = [b for b in _blocks(raw) if b[3] == b"D28D"]
    off = data[5][0]
    # D28D, realm, shape, u_start (vu, 2 bytes for 600): flip a low bit of u_start
    raw[off + 32 + 6] ^= 0x01
    p.write_bytes(bytes(raw))
    for kw in ({"readonly": True}, {}):
        with pytest.raises(nhydb.CorruptionError, match="DATA block .* prefix checksum"):
            nhydb.NHyDB(p, **kw)


GOLDEN_016 = Path(__file__).parent / "golden_016" / "golden.nhydb"


def test_run7_format_016_file_is_rejected(tmp_path):
    """RUN7 (no backward compatibility): a database of format 0.16 (NHyDB
    0.28.0) is not opened; the error names its format."""
    p = tmp_path / "l" / "golden.nhydb"
    p.parent.mkdir()
    for s_ in ("", ".layout.enc", ".layout.key"):
        shutil.copyfile(str(GOLDEN_016) + s_, str(p) + s_)
    os.chmod(str(p) + ".layout.key", 0o600)
    for ro in (False, True):
        with pytest.raises(nhydb.CorruptionError, match="NHYDB016"):
            nhydb.NHyDB(p, readonly=ro)


# --------------------------------------------------------------------------
# Round 2 (RD-08 prefix checksums, SECONDARY key trees, external-sort build)
# --------------------------------------------------------------------------

def _kv_db(p, n, cfg=""):
    """Realm 1: k (unique ints), s (unique strings), flag (3 values), t (text)."""
    p.parent.mkdir(parents=True, exist_ok=True)
    _cfg(p.parent, cfg)
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "k", "type": "varuint"}, {"v": 2, "name": "s", "type": "utf8"},
                                {"v": 3, "name": "flag", "type": "utf8"}, {"v": 4, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_index(10, 1, "k", 1)
    db.create_index(11, 1, "s", 2)
    db.create_index(12, 1, "flag", 3)
    db.create_token_index(13, 1, "tok", 4)
    db.create_index(14, 1, "tri", 4, kind="TRIGRAM")
    db.create_fulltext_index(15, 1, "ft", 4)
    words = ["alpha", "beta", "gamma", "delta"]
    rows = [[u * 3, f"key{u:07d}", ("a", "b", "c")[u % 3], f"{words[u % 4]} w{u % 997} {words[(u // 4) % 4]}"] for u in range(1, n + 1)]
    for i in range(0, n, 5000):
        db.put_dense_batch(1, i + 1, rows[i:i + 5000], shape_id=1)
    db.commit()
    return db


def _answers(db, n):
    out = [db.find_u(1, {"k": 3 * u}) for u in (1, 2, n // 2, n)]
    out += [db.find_u(1, {"k": (3 * a, 3 * a + 30)}) for a in (1, n // 3, n - 5)]
    out += [db.find_u(1, {"s": f"key{u:07d}"}) for u in (1, n // 2, n)]
    out += [db.find_u(1, {"s": (f"key{n // 2:07d}", f"key{n // 2 + 20:07d}")}), len(db.find_u(1, {"flag": "b"}))]
    out += [db.index_candidates(13, "token", w) for w in ("w5", "gamma")]
    out += [db.index_candidates(13, "tokens_all", ["alpha", "w4"]), len(db.index_candidates(13, "tokens_any", ["w1", "w2"]))]
    out += [db.index_candidates(14, "contains", "w99"), db.index_candidates(15, "fulltext", "gamma w12")]
    out += [db.index_candidates(15, "phrase", "beta w9"), db.posting_count(10, (3, 3 * 1000)), db.posting_count(12, "a")]
    out += [db.index_candidate_estimate(13, "tokens_any", ["alpha", "beta"]), db.index_candidate_estimate(15, "fulltext", "alpha", predicate_options={"mode": "any"})]
    return out


def test_r2_spilled_build_answers_like_unbounded_build(tmp_path):
    """External sort: a build under a tiny memory budget gives exactly the
    answers of an unbounded build (VALUE, TOKEN, TRIGRAM, FULLTEXT)."""
    n = 30_000
    a = _kv_db(tmp_path / "a" / "db.nhydb", n)
    ra = a.build_indexes()
    want = _answers(a, n)
    a.close()
    b = _kv_db(tmp_path / "b" / "db.nhydb", n, "[index]\nbuild_memory_bytes = 200000\n")
    rb = b.build_indexes()
    assert ra["spill_rounds"] == 0 and rb["spill_rounds"] >= 5, (ra["spill_rounds"], rb["spill_rounds"])
    strip = lambda r: [{k: v for k, v in x.items() if k not in ("block_offset", "block_id", "payload_bytes")} for x in r["indexes"]]  # noqa: E731
    assert strip(ra) == strip(rb)
    assert _answers(b, n) == want
    assert b.verify(deep=True)["ok"]
    b.close()


def test_r2_wide_range_does_not_fill_the_posting_cache(tmp_path):
    """A range over many keys decodes their postings once instead of caching
    one entry per key (posting cache budget in bytes, 0.28.2)."""
    n = 20_000
    db = _kv_db(tmp_path / "w" / "db.nhydb", n)
    db.build_indexes()
    db.close()
    with nhydb.NHyDB(tmp_path / "w" / "db.nhydb", readonly=True) as db:
        assert len(db.find_u(1, {"k": (3, 3 * n)})) == n
        assert db.stats()["posting_cache_entries"] == 0
        assert db.find_u(1, {"k": 30}) == [10]
        assert db.stats()["posting_cache_entries"] == 1


def test_r2_page_cache_stays_within_its_budget(tmp_path):
    n = 40_000
    db = _kv_db(tmp_path / "c" / "db.nhydb", n, "[cache]\nindex_page_cache_bytes = 65536\n")
    db.build_indexes()
    db.close()
    with nhydb.NHyDB(tmp_path / "c" / "db.nhydb", readonly=True) as db:
        for u in range(1, n, 37):
            assert db.find_u(1, {"s": f"key{u:07d}"}) == [u]
        bc = db.stats()["block_cache"]
        assert bc["index_page_budget_bytes"] == 65536
        assert 0 < bc["index_page_bytes"] <= 65536, bc
        assert bc["index_page_evictions"] > 0 and bc["index_page_reads"] > 100, bc


def test_r2_deep_verify_does_not_load_the_key_trees_into_the_cache(tmp_path):
    db = _kv_db(tmp_path / "v" / "db.nhydb", 5000)
    db.build_indexes()
    db.close()
    with nhydb.NHyDB(tmp_path / "v" / "db.nhydb", readonly=True) as db:
        assert db.verify(deep=True)["ok"]
        st = db.stats()
        assert not any(i["lanes"]["NL"]["directory_loaded"] for i in st["indexes"]), st["indexes"]
        assert st["block_cache"]["index_pages"] == 0
        db.find_u(1, {"k": 3})
        assert db.stats()["indexes"][0]["lanes"]["NL"]["directory_loaded"]


def test_r2_damaged_index_page_is_corruption_never_a_wrong_answer(tmp_path):
    """A flipped byte in a page of the key tree (block type 15) fails the
    page's payload CRC on first use; verify reports it; a compaction (which
    rebuilds the indexes) gives a clean file."""
    p = tmp_path / "d" / "db.nhydb"
    db = _kv_db(p, 20_000)
    db.build_indexes()
    db.close()
    raw = bytearray(p.read_bytes())
    pages = [b for b in _blocks(raw) if b[2] == 15 and b[3] == b"SL28" and raw[b[0] + 36] == 10]  # leaves of index 10 (k)
    assert len(pages) > 10
    off, end = pages[len(pages) // 2][:2]
    raw[end - 9] ^= 0x10
    p.write_bytes(bytes(raw))
    with nhydb.NHyDB(p, readonly=True) as db:
        errors = 0
        for u in range(1, 20_001, 7):
            try:
                assert db.find_u(1, {"k": 3 * u}) == [u]
            except nhydb.CorruptionError:
                errors += 1
        assert errors > 0
        assert not db.verify()["ok"]
    nhydb.NHyDB.compact_file(p, make_backup=False)
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.verify(deep=True)["ok"]
        assert db.find_u(1, {"k": 3 * 10_000}) == [10_000]


def test_r2_mixed_key_types_follow_the_scan_semantics(tmp_path):
    """Keys of every class in one deep tree (numbers incl. 1 == 1.0 == True,
    NaN, None, str, bytes, lists): eq and range give the scan answers."""
    p = tmp_path / "m" / "db.nhydb"
    p.parent.mkdir()
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_index(20, 1, "x", 50)
    vals = {}
    u = 1
    for i in range(1500):
        for v, typ in ((i, "varint"), (i + 0.5, "f64"), (f"s{i:05d}", "utf8"), (bytes([i % 256, i // 256]), "bytes")):
            vals[u] = v
            db.put_points(1, [(u, 50, v, typ)])
            u += 1
    specials = [(True, "bool"), (1.0, "f64"), (float("nan"), "f64"), (-0.0, "f64"), (None, None)]
    for v, typ in specials:
        if v is None:
            continue
        vals[u] = v
        db.put_points(1, [(u, 50, v, typ)])
        u += 1
    db.commit()
    db.build_indexes()

    def scan_eq(x):
        return sorted(k for k, v in vals.items() if type(v) is not bytes and type(x) is not bytes and v == x
                      or type(v) is bytes and type(x) is bytes and v == x)

    def num(v):
        return isinstance(v, (int, float)) and not (isinstance(v, float) and v != v)

    def scan_range(lo, hi):
        out = []
        for k, v in vals.items():
            if num(lo) and num(hi) and num(v) and lo <= v <= hi:
                out.append(k)
            elif isinstance(lo, str) and isinstance(hi, str) and isinstance(v, str) and lo <= v <= hi:
                out.append(k)
            elif isinstance(lo, bytes) and isinstance(hi, bytes) and isinstance(v, bytes) and lo <= v <= hi:
                out.append(k)
        return sorted(out)

    for x in (0, 1, 1.0, True, 2.5, 1499, -0.0, "s00007", b"\x07\x00", 10**30, float("nan"), "zzz"):
        assert sorted(db.index_candidates(20, "eq", x)) == scan_eq(x), x
    for lo, hi in ((0, 10), (0.4, 0.6), (True, 2), (-1, 1), (1490, 10**6), ("s00010", "s00020"), (b"\x00", b"\x05\x00"), ("a", 5)):
        assert sorted(db.index_candidates(20, "between", lo, hi)) == scan_range(lo, hi), (lo, hi)
    assert db.verify(deep=True)["ok"]
    db.close()


def test_r2_large_build_writes_a_directory_root_for_a_fast_reopen(tmp_path):
    """Many SECONDARY pages after a build: the commit writes a directory
    root, so the next open does not scan the pages."""
    p = tmp_path / "r" / "db.nhydb"
    db = _kv_db(p, 120_000)
    db.build_indexes()
    db.close()
    raw = p.read_bytes()
    assert sum(1 for b in _blocks(raw) if b[2] == 15) >= 1024
    with nhydb.NHyDB(p, readonly=True) as db:
        d = db.stats()["directory"]
        assert d["loaded"] and d["root_offset"] > 0, d
        assert db.find_u(1, {"k": 3 * 777}) == [777]
