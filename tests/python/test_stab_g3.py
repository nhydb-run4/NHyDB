"""Regression tests for the findings of the differential review G3
(performance revision): G3-01 universe fast path checks the DATA blocks,
G3-02 batch() = single calls, G3-03 range reads over a vanished u range,
G3-04 batch flush atomic across (realm, lane) groups."""

import random
import struct

import pytest

import nhydb
from nhydb.query import Q

DATA_TAG = b"D10D"


# ----------------------------------------------------------------------
# G3-01 / G3-03: shifted u_start (same varint length, payload CRC not fixed)
# ----------------------------------------------------------------------

def _skip_vu(b, i):
    while b[i] & 0x80:
        i += 1
    return i + 1


def _shifted_db(path):
    """Two committed DATA blocks (u 1..10, u 11..23); the u_start of the 2nd
    is flipped 11 -> 41 in the file (header CRC intact, payload CRC not)."""
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 11)], shape_id=1, commit=True)
    db.put_dense_batch(1, 11, [[u] for u in range(11, 24)], shape_id=1, commit=True)
    db.close()
    b = bytearray(path.read_bytes())
    off, seen = 8192, 0
    while True:
        plen = struct.unpack_from("<Q", b, off + 16)[0]
        ps = off + 32
        if b[ps:ps + 4] == DATA_TAG:
            seen += 1
            if seen == 2:
                pos = _skip_vu(b, _skip_vu(b, ps + 4))
                assert b[pos] == 11
                b[pos] = 41
                break
        off = (ps + plen + 7) & ~7
    path.write_bytes(bytes(b))
    return path


def _payload_flip_db(path):
    """Like `_shifted_db`, but a byte at the end of the 2nd DATA payload is
    flipped (metadata intact)."""
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 11)], shape_id=1, commit=True)
    db.put_dense_batch(1, 11, [[u] for u in range(11, 24)], shape_id=1, commit=True)
    db.close()
    b = bytearray(path.read_bytes())
    off, seen = 8192, 0
    while True:
        plen = struct.unpack_from("<Q", b, off + 16)[0]
        ps = off + 32
        if b[ps:ps + 4] == DATA_TAG:
            seen += 1
            if seen == 2:
                b[ps + plen - 1] ^= 0x01
                break
        off = (ps + plen + 7) & ~7
    path.write_bytes(bytes(b))
    return path


UNIVERSE_APIS = {
    "Q.all n": lambda x: x.query(Q.all(), realm=1, return_mode="n"),
    "Q.all count": lambda x: x.query(Q.all(), realm=1, return_mode="count"),
    "Q.all record": lambda x: x.query(Q.all(), realm=1, return_mode="record"),
    "~Q.all count": lambda x: x.query(~Q.all(), realm=1, return_mode="count"),
    "sql COUNT(*)": lambda x: x.sql("SELECT COUNT(*) FROM c"),
}

RANGE_APIS = {
    "range": lambda x: list(x.range(1, 1, 30)),
    "range gap only": lambda x: list(x.range(1, 11, 20)),
    "select": lambda x: list(x.select(1, 11, 20, ["a"])),
    "read_numpy": lambda x: x.read_numpy(1, 11, 10),
    "read_numpy from 1": lambda x: x.read_numpy(1, 1, 30),
    "range across": lambda x: list(x.range(1, 1, 60)),
    "select across": lambda x: list(x.select(1, 5, 60, ["a"])),
    "read_numpy across": lambda x: x.read_numpy(1, 1, 60),
}


def _cfg(tmp_path, budget):
    p = tmp_path / f"c{budget}.toml"
    p.write_text(f"[cache]\nblock_cache_bytes = {budget}\n")
    return str(p)


@pytest.mark.parametrize("budget", [0, 64 << 20])
@pytest.mark.parametrize("make", [_shifted_db, _payload_flip_db])
@pytest.mark.parametrize("api", sorted(UNIVERSE_APIS))
def test_g3_01_universe_fast_path_checks_blocks(tmp_path, make, api, budget):
    p = make(tmp_path / "f.nhydb")
    with nhydb.NHyDB(str(p), readonly=True, config=_cfg(tmp_path, budget)) as x:
        with pytest.raises(nhydb.CorruptionError):
            UNIVERSE_APIS[api](x)


@pytest.mark.parametrize("budget", [0, 64 << 20])
@pytest.mark.parametrize("api", sorted(RANGE_APIS))
def test_g3_03_range_reads_over_vanished_u(tmp_path, api, budget):
    p = _shifted_db(tmp_path / "f.nhydb")
    with nhydb.NHyDB(str(p), readonly=True, config=_cfg(tmp_path, budget)) as x:
        with pytest.raises(nhydb.CorruptionError):
            RANGE_APIS[api](x)
        # the intact first block stays readable
        assert [r["_u"] for r in x.range(1, 1, 10)] == list(range(1, 11))


def test_g3_01_universe_fast_path_unchanged_on_intact_db(tmp_path):
    p = tmp_path / "ok.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 11)], shape_id=1, commit=True)
    db.put_dense_batch(1, 100, [[u] for u in range(5)], shape_id=1, commit=True)
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as x:
        for _ in range(2):  # 1st call verifies, 2nd uses the registry
            assert x.query(Q.all(), realm=1, return_mode="count")["count"] == 15
            assert x.sql("SELECT COUNT(*) FROM c")["rows"] == [{"count_all": 15}]
        assert x.stats()["block_cache"]["verified_blocks"] >= 2
        assert [r["_u"] for r in x.range(1, 1, 200)] == list(range(1, 11)) + list(range(100, 105))
        assert int(x.read_numpy(1, 5, 100)["_present"].sum()) == 6 + 5  # u 5..10 and 100..104


# ----------------------------------------------------------------------
# G3-02: batch() leaves the same final state as the single calls
# ----------------------------------------------------------------------

def _g302_db(path):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u, f"s{u}"] for u in range(1, 9)], commit=True)
    db.put_cell(1, 3, 5, 33, type="varint")
    db.put_cell(1, 20, 5, 7, type="varint")
    db.put_cell(1, 21, 5, None, type="varint")
    db.put_cell(1, 4, 1, None)  # null overlay of a BASE value
    db.delete_cell(1, 5, 2)
    db.commit()
    return db


def _state(db):
    cells = sorted((c["realm"], c["u"], c["v"], c["lane"], repr(c["value"])) for c in db.iter_materialized_cells(1))
    return {
        "cells": cells,
        "count": db.query(Q.all(), realm=1, return_mode="count")["count"],
        "n": db.query(Q.all(), realm=1, return_mode="n")["refs"],
        "sql": db.sql("SELECT COUNT(*) FROM c")["rows"],
        "range": [sorted(r.items()) for r in db.range(1, 1, 40)],
        "get": [(u, v, repr(db.get_value(1, u, v))) for u in range(1, 26) for v in (1, 2, 5)],
    }


_FIXED = [
    [("put", 30, 1, None), ("del", 30, 1)],                      # the G3 repro
    [("put", 30, 1, None), ("del", 30, 1), ("put", 30, 1, None)],
    [("put", 30, 1, 9), ("put", 30, 1, None), ("del", 30, 1)],
    [("put", 30, 1, 9), ("del", 30, 1)],                         # value then delete on a new cell
    [("put", 2, 1, 9), ("del", 2, 1)],                           # ... on a dense value
    [("del", 2, 1)], [("del", 2, 1), ("del", 2, 1)],
    [("del", 31, 1)],                                            # delete without put / value
    [("del", 4, 1), ("put", 4, 1, None), ("del", 4, 1)],        # null overlay stays
    [("del", 21, 5), ("put", 21, 5, 1), ("del", 21, 5), ("del", 21, 5)],
    [("put", 30, 2, None), ("del", 30, 2), ("put", 30, 5, 1), ("del", 30, 5)],
]


def _apply(target, ops):
    for op in ops:
        v = op[2]
        typ = "varint" if v == 5 else None
        if op[0] == "put":
            target.put_cell(1, op[1], v, op[3] if v != 2 or op[3] is None else f"x{op[3]}", type=typ)
        else:
            target.delete_cell(1, op[1], v)


def _random_ops(seed):
    rng = random.Random(seed)
    ops = []
    for _ in range(rng.randint(1, 40)):
        u = rng.choice([1, 2, 3, 4, 5, 9, 20, 21, 22, 23])
        v = rng.choice([1, 2, 5])
        if rng.random() < 0.45:
            ops.append(("del", u, v))
        else:
            ops.append(("put", u, v, rng.choice([None, None, 0, 1, 2, 3])))
    return ops


@pytest.mark.parametrize("case", list(range(len(_FIXED))) + [f"seed{s}" for s in range(60)])
def test_g3_02_batch_equals_single_calls(tmp_path, case):
    ops = _FIXED[case] if isinstance(case, int) else _random_ops(int(case[4:]))
    a = _g302_db(tmp_path / "a.nhydb")
    b = _g302_db(tmp_path / "b.nhydb")
    _apply(a, ops)
    with b.batch() as w:
        _apply(w, ops)
    a.commit()
    b.commit()
    sa = _state(a)
    assert sa == _state(b), ops
    a.close()
    b.close()
    with nhydb.NHyDB(str(tmp_path / "b.nhydb"), readonly=True) as r:
        assert _state(r) == sa
        assert r.verify(deep=True)["ok"]


# ----------------------------------------------------------------------
# G3-04: a failing flush writes nothing (all (realm, lane) groups)
# ----------------------------------------------------------------------

def test_g3_04_failed_flush_writes_nothing(tmp_path):
    p = tmp_path / "m.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_realm(2, "s")
    db.put_cell(1, 1, 1, 11, type="varint")
    db.commit()
    end = db.stats()["current_end"]
    db.set_max_db_size(end + 4096 + 300)
    db.commit()
    end = db.stats()["current_end"]
    blocks = db.next_block_id
    db.get_value(1, 1, 1)  # warm caches
    count = db.query(Q.all(), realm=1, return_mode="count")["count"]
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        with db.batch() as b:
            b.put_cell(1, 5, 5, 55, type="varint")
            b.put_cell(1, 1, 1, 12, type="varint")
            for u in range(1, 400):
                b.put_cell(2, u, 1, "x" * 50, type="utf8")
    assert db.stats()["current_end"] == end
    assert db.next_block_id == blocks
    assert db.get_value(1, 5, 5) is None and db.get_value(1, 1, 1) == 11 and db.get_value(2, 1, 1) is None
    assert sorted((c["u"], c["v"]) for c in db.iter_materialized_cells()) == [(1, 1)]
    assert db.query(Q.all(), realm=1, return_mode="count")["count"] == count
    # the handle keeps working: write, commit, reopen
    with db.batch() as b:
        b.put_cell(1, 6, 6, 66, type="varint")
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert r.get_value(1, 6, 6) == 66 and r.get_value(1, 5, 5) is None and r.get_value(1, 1, 1) == 11
        assert r.verify(deep=True)["ok"]


def test_g3_04_flush_error_keeps_earlier_uncommitted_writes(tmp_path):
    """Rolling back the failed flush only drops the batch, not the writes of
    the handle before it (no commit in between)."""
    p = tmp_path / "k.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_realm(2, "s")
    db.commit()
    db.put_cell(1, 9, 9, 99, type="varint")  # uncommitted, before the batch
    db.set_max_db_size(db.stats()["current_end"] + 4096 + 300)
    end = db.stats()["current_end"]
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        with db.batch() as b:
            b.put_cell(1, 5, 5, 55, type="varint")
            for u in range(1, 400):
                b.put_cell(2, u, 1, "x" * 50, type="utf8")
    assert db.stats()["current_end"] == end
    assert db.get_value(1, 9, 9) == 99 and db.get_value(1, 5, 5) is None
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert r.get_value(1, 9, 9) == 99 and r.get_value(1, 5, 5) is None
        assert r.verify(deep=True)["ok"]


def test_g3_04_realm_gone_inside_block_writes_nothing(tmp_path):
    """rollback() inside the block removes an uncommitted realm: the flush
    fails at the 2nd group and the 1st group is not left behind."""
    p = tmp_path / "g.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.commit()
    db.create_realm(2, "s")
    end = db.stats()["current_end"]
    with pytest.raises(nhydb.SchemaError):
        with db.batch() as b:
            b.put_cell(1, 5, 5, 55, type="varint")
            b.put_cell(2, 5, 5, 56, type="varint")
            db.rollback()
    assert db.stats()["current_end"] <= end
    assert db.get_value(1, 5, 5) is None
    assert list(db.iter_materialized_cells()) == []
    db.put_cell(1, 6, 6, 66, type="varint", commit=True)
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert r.get_value(1, 6, 6) == 66 and r.get_value(1, 5, 5) is None
        assert r.verify(deep=True)["ok"]


def test_g3_04_one_group_several_windows_writes_nothing(tmp_path):
    """One (realm, lane) group split into several u windows: the size limit
    hits a later window, the earlier windows are taken back as well."""
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.commit()
    db.set_max_db_size(db.stats()["current_end"] + 2000)
    db.commit()
    end, blocks = db.stats()["current_end"], db.next_block_id
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        with db.batch(u_window=10) as b:
            for u in range(1, 200):
                b.put_cell(1, u, 1, "y" * 40, type="utf8")
    assert db.stats()["current_end"] == end and db.next_block_id == blocks
    assert list(db.iter_materialized_cells()) == []
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert r.get_value(1, 1, 1) is None and r.verify(deep=True)["ok"]
