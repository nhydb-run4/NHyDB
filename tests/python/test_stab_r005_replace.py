"""R005: atomic file replacement (layout plan, compaction, restore) on every
platform – in particular on Windows, where the replacement must work while a
read-only handle still has the file open and with paths above MAX_PATH."""
import nhydb


def _make(p, n=50):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, n + 1)], shape_id=1, commit=True)
    db.put_cell(1, 3, 1, 999, commit=True)  # overlay -> compaction has work
    db.close()


def test_compact_in_place_while_readonly_reader_is_open(tmp_path):
    p = tmp_path / "c.nhydb"
    _make(p)
    reader = nhydb.NHyDB(p, readonly=True)
    try:
        res = nhydb.NHyDB.compact_file(p)
        assert res["replaced_original"]
        # the open reader keeps its view
        assert reader.get(1, 3)["a"] == 999 and reader.get(1, 50)["a"] == 50
    finally:
        reader.close()
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get(1, 3)["a"] == 999 and db.verify(deep=True)["ok"]
    # restore while a reader is open
    reader = nhydb.NHyDB(p, readonly=True)
    try:
        assert nhydb.NHyDB.restore_backup(res["backup"], p)["ok"]
    finally:
        reader.close()
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 3)["a"] == 999 and db.verify(deep=True)["ok"]


def test_create_schema_compact_restore_with_long_path(tmp_path):
    d = tmp_path
    for i in range(5):
        d = d / f"{i}_{'x' * 50}"
    d.mkdir(parents=True)
    p = d / "long_path_database.nhydb"
    assert len(str(p)) > 260
    _make(p)
    with nhydb.NHyDB(p) as db:  # schema commit replaces the layout plan
        db.create_realm(2, "r2")
        db.commit()
    res = nhydb.NHyDB.compact_file(p)
    assert res["replaced_original"]
    assert nhydb.NHyDB.restore_backup(res["backup"], p)["ok"]
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get(1, 3)["a"] == 999 and db.verify(deep=True)["ok"]
