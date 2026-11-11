"""Stabilization of the read/query API (findings F011, F022, F027, F028,
F056 parts; test gaps T015).
"""
import struct

import pytest

import nhydb
from nhydb import _native as native
from nhydb.query import Q, QueryEngine

DATA_START = 8192
BT_SECONDARY = 7


def _small_db(path, n=40):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "i64"},
                                {"v": 3, "name": "note", "type": "utf8"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 4, "fox %d" % u] for u in range(1, n + 1)], shape_id=1)
    db.commit()
    return db


# ---------------------------------------------------------------------------
# F027: NHySQL FROM <layout> covers only records routed to that layout
# ---------------------------------------------------------------------------

@pytest.fixture
def routed_db(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "routing.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "i64"}])
    db.create_shape(2, 1, "t", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "y", "type": "i64"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.create_layout(2, 1, "special", "CLASSIC", shape_id=2, u_start=100, u_end=199, priority=5)
    db.put_dense_batch(1, 1, [[1, 5], [2, 6], [3, 5]], shape_id=1)
    db.put_dense_batch(1, 100, [[100, 5], [101, 7]], shape_id=2)
    db.commit()
    yield db
    db.close()


def test_sql_from_layout_counts_only_its_records(routed_db):
    db = routed_db
    assert db.sql("SELECT COUNT(*) FROM special")["rows"] == [{"count_all": 2}]
    assert db.sql("SELECT COUNT(*) FROM main")["rows"] == [{"count_all": 3}]
    assert [r["id"] for r in db.sql("SELECT id, y FROM special")["rows"]] == [100, 101]
    assert db.sql("SELECT id FROM special WHERE id < 50")["rows"] == []
    assert [r["id"] for r in db.sql("SELECT id FROM main WHERE x = 5")["rows"]] == [1, 3]
    # with a fresh index on x the answer is the same
    db.create_index(1, 1, "ix", "x")
    db.build_indexes(commit=True)
    assert [r["id"] for r in db.sql("SELECT id FROM main WHERE x = 5")["rows"]] == [1, 3]
    assert db.sql("SELECT COUNT(*) FROM main WHERE x = 5")["rows"] == [{"count_all": 2}]
    rows = db.sql("SELECT x, COUNT(*) AS n FROM main GROUP BY x ORDER BY x")["rows"]
    assert rows == [{"x": 5, "n": 2}, {"x": 6, "n": 1}]


# ---------------------------------------------------------------------------
# F028 / T015: everything fails uniformly after close, also with warm caches
# ---------------------------------------------------------------------------

def _closed_error(fn):
    with pytest.raises(nhydb.NHyDBError, match="closed"):
        r = fn()
        if hasattr(r, "__next__"):
            next(r)


def test_reads_after_close_fail_with_warm_cache(tmp_path):
    db = _small_db(tmp_path / "c.nhydb")
    db.create_index(1, 1, "ix", "x")
    db.build_indexes(commit=True)
    eng = QueryEngine(db)
    # warm every cache
    assert db.sql("SELECT COUNT(*) FROM main")["rows"] == [{"count_all": 40}]
    assert db.get(1, 5)["x"] == 1
    assert db.find_u(1, {"ix": 1})
    assert eng.execute(Q.field("x") == 1, realm=1, return_mode="count")["count"] == 10
    rng = db.range(1, 1, 1000)
    sel = db.select_us(1, list(range(1, 41)) * 10, ["x"])
    cells = db.iter_materialized_cells(1)
    recs = db.iter_materialized_record_ns(1)
    fnd = db.find(1, {"ix": 1})
    open_iters = [rng, sel, cells, recs, fnd]
    for it in open_iters:
        next(it)
    db.close()
    for it in open_iters:
        _closed_error(lambda it=it: next(it))
    _closed_error(lambda: db.sql("SELECT COUNT(*) FROM main"))
    _closed_error(lambda: db.get(1, 5))
    _closed_error(lambda: db.get(1, 999))
    _closed_error(lambda: db.find_u(1, {"ix": 1}))
    _closed_error(lambda: db.build_indexes())
    _closed_error(lambda: db.query(Q.field("x") == 1, realm=1, return_mode="count"))
    _closed_error(lambda: eng.execute(Q.field("x") == 1, realm=1, return_mode="count"))
    _closed_error(lambda: eng.explain(Q.field("x") == 1, realm=1))
    _closed_error(lambda: db.range(1, 1, 10))
    _closed_error(lambda: db.select_us(1, [1, 2], ["x"]))
    _closed_error(lambda: db.record_n_for_address(1, 1, 1))


def test_iterator_after_close_and_compact(tmp_path):
    path = tmp_path / "k.nhydb"
    db = _small_db(path, n=600)
    it = db.range(1, 1, 1000)
    assert next(it)["x"] == 1
    db.close()
    nhydb.NHyDB.compact_file(str(path)) if hasattr(nhydb.NHyDB, "compact_file") else nhydb.compact_file(str(path))
    _closed_error(lambda: next(it))
    db2 = nhydb.NHyDB(str(path))
    assert sum(1 for _ in db2.range(1, 1, 1000)) == 600
    db2.close()


def test_select_us_overlay_tombstone_sparse_only(tmp_path):
    db = _small_db(tmp_path / "s.nhydb", n=5)
    db.put_cell(1, 2, 2, 99)          # overlay
    db.delete_cell(1, 3, 2)           # tombstone
    db.put_cell(1, 50, 2, 7)          # sparse-only record
    db.delete_record(1, 4)
    db.commit()
    rows = db.select_us(1, [1, 2, 3, 4, 50, 60], ["id", "x"])
    got = {r["_u"]: (r["id"], r["x"]) for r in rows}
    # select_us returns rows only for u with a live dense record
    assert got == {1: (1, 1), 2: (2, 99), 3: (3, None)}
    db.close()


# ---------------------------------------------------------------------------
# F011: SECONDARY blocks are CRC-checked before use
# ---------------------------------------------------------------------------

def _secondary_blocks(raw):
    off, out = DATA_START, []
    while off + 32 <= len(raw):
        magic, _ver, bt, _flags, _bid, plen, _pcrc, _hcrc = struct.unpack_from("<4sBBHQQII", raw, off)
        if magic != b"NHBL":
            break
        if bt == BT_SECONDARY:
            out.append((off + 32, plen))
        off += (32 + plen + 7) & ~7
    return out


def test_corrupt_secondary_block_is_detected(tmp_path):
    path = tmp_path / "i.nhydb"
    db = _small_db(path, n=12)
    db.create_index(1, 1, "ix", "x")
    db.build_indexes(commit=True)
    expected = db.find_u(1, {"ix": 2})
    db.close()
    raw = path.read_bytes()
    (start, plen), = [b for b in _secondary_blocks(raw) if b[1] > 0][:1]
    for pos in range(start, start + plen):
        bad = bytearray(raw)
        bad[pos] ^= 0x10
        path.write_bytes(bytes(bad))
        # either the open scan (block header/tag) or the first index access
        # (payload CRC) reports the damage; never a silent result
        with pytest.raises(nhydb.NHyDBError):
            db = nhydb.NHyDB(str(path))
            try:
                db.find_u(1, {"ix": 2})
            finally:
                db.close(commit=False)
    path.write_bytes(raw)
    db = nhydb.NHyDB(str(path))
    assert db.find_u(1, {"ix": 2}) == expected
    db.close()


# ---------------------------------------------------------------------------
# F022, F056 (query part)
# ---------------------------------------------------------------------------

def test_decode_big_posting_huge_count_is_an_error():
    with pytest.raises(nhydb.CorruptionError):
        native.decode_big_posting(b"\x01\x01", 2**63)
    assert native.decode_big_posting(b"\x01\x01", 2) == [1, 2]


def test_bm25_parameters_and_search_limit(tmp_path):
    db = _small_db(tmp_path / "f.nhydb", n=10)
    db.create_fulltext_index(1, 1, "ft", "note")
    db.build_indexes(commit=True)
    eng = QueryEngine(db)
    for k1 in (float("nan"), float("inf"), 0.0):
        with pytest.raises(nhydb.IndexConfigError):
            eng.search(Q.field("note").fulltext("fox"), realm=1, k1=k1)
    with pytest.raises(nhydb.IndexConfigError):
        eng.search(Q.field("note").fulltext("fox"), realm=1, b=float("nan"))
    assert eng.search(Q.field("note").fulltext("fox"), realm=1, limit=2**70)["returned"] == 10
    db.close()


def test_record_n_must_be_integral(tmp_path):
    db = _small_db(tmp_path / "n.nhydb", n=3)
    anchor = db.record_anchor(1, 2)["N"]
    assert db.sql("SELECT id FROM main WHERE @record_n = ?", [anchor])["rows"] == [{"id": 2}]
    assert db.sql("SELECT id FROM main WHERE @record_n = ?", [float(anchor)])["rows"] == [{"id": 2}]
    with pytest.raises(ValueError):
        db.sql("SELECT id FROM main WHERE @record_n = 3.7")
    db.close()
