"""Persistent block directory (file format 0.17) through the Python API.

The databases here use a configuration with tiny directory thresholds, so
that commits write directory runs and merge them. Every read must give the
same answers as a database written without directory; damaged directory
blocks must never make data count as valid nor cost data; the memory-bounded
compaction must keep the logical state.
"""
import random
import struct

import pytest

import nhydb

TINY = "[directory]\nflush_blocks = 4\ncache_entries = 64\n"
NONE = "[directory]\nflush_blocks = 1000000000\nflush_bytes = 1000000000000\n"
BLOCK_HEADER = struct.Struct("<4sBBHQQII")
BT_DIRECTORY = 14


def _cfg(tmp_path, name, text):
    p = tmp_path / f"{name}.toml"
    p.write_text(text)
    return str(p)


def _schema(db):
    db.create_realm(1, "dense")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "k", "type": "varuint"}, {"v": 2, "name": "s", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_index(10, 1, "k", 1)
    db.create_realm(2, "sparse")
    db.commit()


def _workload(dbs, seed, n=400):
    rnd = random.Random(seed)
    for _ in range(n):
        op = rnd.randrange(100)
        lane = "NR" if rnd.randrange(4) == 0 else "NL"
        if op < 30:
            u0 = rnd.randrange(1, 300)
            rows = [[(u0 + i) % 11, f"r{u0}-{i}-{rnd.random():.6f}"] for i in range(rnd.randrange(1, 15))]
            for db in dbs:
                db.put_dense_batch(1, u0, rows, shape_id=1, lane=lane)
        elif op < 65:
            cells = {(rnd.randrange(1, 500) if rnd.randrange(12) else 10**30 + rnd.randrange(4), rnd.randrange(1, 20)):
                     f"c{rnd.random():.8f}" for _ in range(rnd.randrange(1, 25))}
            pts = [(u, v, val, "utf8") for (u, v), val in sorted(cells.items())]
            for db in dbs:
                db.put_points(2, pts, lane=lane)
        elif op < 75:
            u, v = rnd.randrange(1, 500), rnd.randrange(1, 20)
            for db in dbs:
                db.delete_cell(2, u, v, lane=lane)
        elif op < 82:
            u = rnd.randrange(1, 300)
            for db in dbs:
                db.delete_record(1, u, lane=lane)
        elif op < 90:
            u = rnd.randrange(1, 300)
            val = f"ov{rnd.random():.5f}"
            for db in dbs:
                db.put_points(1, [(u, 2, val)], lane=lane)
        else:
            for db in dbs:
                db.commit()
    for db in dbs:
        db.commit()


def _snapshot(db):
    out = []
    for lane in ("NL", "NR"):
        for u in range(1, 310):
            out.append(("D", lane, u, db.get(1, u, lane=lane)))
        for u in (1, 17, 499, 10**30, 10**30 + 3):
            for v in range(1, 21):
                out.append(("S", lane, u, v, db.get_value(2, u, v, lane=lane)))
    out.append(("P", [(p["realm"], p["lane"], p["u"], p["v"], p["value"]) for p in db.iter_materialized_sparse_points()]))
    for k in range(11):
        out.append(("F", k, db.find_u(1, {"k": k})))
    return out


def test_directory_matches_full_scan_and_survives_reopen_and_compaction(tmp_path):
    pa, pb = tmp_path / "a.nhydb", tmp_path / "b.nhydb"
    ca, cb = _cfg(tmp_path, "tiny", TINY), _cfg(tmp_path, "none", NONE)
    a = nhydb.NHyDB.create(pa, config=ca)
    b = nhydb.NHyDB.create(pb, config=cb)
    for db in (a, b):
        _schema(db)
    _workload([a, b], seed=11)
    for db in (a, b):
        db.build_indexes()
    d = a.stats()["directory"]
    assert d["flushes"] > 5 and d["merges"] >= 1 and d["runs"] >= 1
    assert b.stats()["directory"]["flushes"] == 0
    ref = _snapshot(b)
    assert _snapshot(a) == ref
    assert a.verify(deep=True)["ok"]
    a.close()
    b.close()
    a = nhydb.NHyDB(pa, config=ca)
    st = a.stats()["directory"]
    assert st["loaded"] and not st["fallback_full_scan"] and st["persisted_entries"] > 0
    assert _snapshot(a) == ref
    assert a.verify(deep=True)["ok"]
    a.close()
    rep = nhydb.NHyDB.compact_file(pa, make_backup=False, config=ca)
    assert rep["ok"] and rep["after_bytes"] < rep["before_bytes"]
    with nhydb.NHyDB(pa, config=ca) as a:
        assert _snapshot(a) == ref
        assert a.verify(deep=True)["ok"]
        assert a.stats()["directory"]["runs"] == 1


def _blocks(raw):
    out, off = [], 8192
    while off + 32 <= len(raw):
        magic, ver, bt, flags, bid, plen, _pc, _hc = BLOCK_HEADER.unpack_from(raw, off)
        if magic != b"NHBL":
            break
        out.append((off, bt, plen))
        off += (32 + plen + 7) & ~7
    return out


def test_damaged_directory_falls_back_or_fails_loudly_and_repair_keeps_data(tmp_path):
    p = tmp_path / "d.nhydb"
    c = _cfg(tmp_path, "tiny", TINY)
    db = nhydb.NHyDB.create(p, config=c)
    _schema(db)
    _workload([db], seed=5, n=250)
    db.build_indexes()
    db.checkpoint()
    ref = _snapshot(db)
    root = db.stats()["directory"]["root_offset"]
    db.close()
    clean = p.read_bytes()
    dir_blocks = [off for off, bt, _ in _blocks(clean) if bt == BT_DIRECTORY]
    assert root in dir_blocks
    # damaged root: the open scans every block, same answers, warning
    img = bytearray(clean)
    img[root + 32 + 5] ^= 0x20
    p.write_bytes(bytes(img))
    with pytest.warns(RuntimeWarning, match="block directory"):
        db = nhydb.NHyDB(p, config=c)
    with db:
        assert db.stats()["directory"]["fallback_full_scan"]
        assert any("block directory" in w for w in db.verify()["warnings"])
        assert _snapshot(db) == ref
    # damaged page in use: reads that need it raise CorruptionError (the
    # checkpoint merged everything into one run written right before the
    # root: [... pages, run, root])
    assert dir_blocks[-1] == root
    page = dir_blocks[-3]
    img = bytearray(clean)
    img[page + 32 + 7] ^= 0x01
    p.write_bytes(bytes(img))
    with nhydb.NHyDB(p, config=c) as db:
        with pytest.raises(nhydb.CorruptionError):
            _snapshot(db)
    info = nhydb.NHyDB.inspect_tail(p)
    assert page in info["damaged_directory_blocks"] and info["directory_damaged_in_use"]
    rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False, config=c)
    assert rep["applied"] and rep["lost_blocks"] == [] and rep["safe_end"] == len(clean)
    with nhydb.NHyDB(p, config=c) as db:
        assert _snapshot(db) == ref
        v = db.verify(deep=True)
        assert v["ok"], v


def test_data_block_under_directory_is_checked_on_first_use(tmp_path):
    """Blocks covered by the directory are not read by the open; their
    payload CRC is checked before first use instead (F021 with directory)."""
    p = tmp_path / "f.nhydb"
    c = _cfg(tmp_path, "tiny", TINY)
    db = nhydb.NHyDB.create(p, config=c)
    db.create_realm(2, "sparse")
    for i in range(20):
        db.put_points(2, [(i + 1, 1, f"value {i}", "utf8")], commit=True)
    db.checkpoint()
    db.close()
    raw = bytearray(p.read_bytes())
    sparse = [(off, plen) for off, bt, plen in _blocks(bytes(raw)) if bt == 10]
    off, plen = sparse[3]
    raw[off + 32 + plen - 2] ^= 0x01  # inside the value of u=4
    p.write_bytes(bytes(raw))
    with nhydb.NHyDB(p, config=c) as db:  # opens: the block is not read
        assert db.get_value(2, 1, 1) == "value 0"
        with pytest.raises(nhydb.CorruptionError, match="CRC"):
            db.get_value(2, 4, 1)
        assert not db.verify()["ok"]


def test_config_keys_and_stats(tmp_path):
    c = _cfg(tmp_path, "keys", "[directory]\nflush_blocks = 2\nflush_bytes = 1048576\ncache_entries = 10\n")
    p = tmp_path / "k.nhydb"
    with nhydb.NHyDB.create(p, config=c) as db:
        db.create_realm(2, "s")
        for i in range(10):
            db.put_points(2, [(i + 1, 1, "x", "utf8")], commit=True)
        d = db.stats()["directory"]
        assert d["flushes"] >= 4 and d["root_offset"] > 0
    with pytest.raises(ValueError):
        nhydb.NHyDB(p, config=_cfg(tmp_path, "bad", "[directory]\nflush_blocks = 0\n"))
