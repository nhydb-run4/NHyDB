"""Phase 2 integration: the verified block cache (P001/P003/P005) and the
sparse index refresh (P019) never serve data of blocks that rollback()
(P024), batch() (P011) or compaction (P002) removed or rewrote."""
import nhydb
import pytest


def _db(tmp_path, cache=None):
    db = nhydb.NHyDB.create(str(tmp_path / "t.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"},
                                {"v": 2, "name": "b", "type": "utf8"}])
    db.commit()
    return db


def test_rollback_then_rewrite_same_offsets_is_not_served_from_cache(tmp_path):
    db = _db(tmp_path)
    db.put_dense_batch(1, 1, [{"a": i, "b": "c%d" % i} for i in range(100)], shape_id=1, commit=True)
    for _ in range(3):
        db.put_dense_batch(1, 101, [{"a": -i, "b": "old"} for i in range(50)], shape_id=1)
        db.put_cell(1, 120, 9, 7, type="varint")
        # read the uncommitted blocks (they get verified and cached)
        assert db.get(1, 101)["b"] == "old"
        assert db.get_value(1, 120, 9) == 7
        db.rollback()
        assert db.get(1, 101) is None
        assert db.get_value(1, 120, 9) is None
        # new blocks land on the same file offsets
        db.put_dense_batch(1, 101, [{"a": i * 3, "b": "new"} for i in range(50)], shape_id=1)
        db.put_cell(1, 120, 9, 11, type="varint")
        assert db.get(1, 101) == {**db.get(1, 101), "a": 0, "b": "new"}
        assert db.get(1, 150)["a"] == 49 * 3
        assert db.get_value(1, 120, 9) == 11
        db.rollback()
    assert db.verify(deep=True)["ok"]
    db.close()


def test_batch_and_compaction_keep_cache_coherent(tmp_path):
    db = _db(tmp_path)
    db.put_dense_batch(1, 1, [{"a": i, "b": "x"} for i in range(200)], shape_id=1, commit=True)
    assert db.get_value(1, 5, 9) is None
    with db.batch() as b:
        for u in range(1, 200, 7):
            b.put_cell(1, u, 9, u * 10, type="varint")
    assert db.get_value(1, 8, 9) == 80
    db.commit()
    path = db.path
    db.close()
    nhydb.NHyDB.compact_file(path)
    db2 = nhydb.NHyDB(path)
    assert [db2.get_value(1, u, 9) for u in (1, 8, 15)] == [10, 80, 150]
    assert db2.get(1, 199)["a"] == 198
    assert sum(1 for _ in db2.range(1, 1, 400)) == 200
    db2.close()
