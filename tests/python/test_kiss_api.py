"""Einfache Einzel-API: ein Datensatz/eine Zelle pro Aufruf, optional mit Commit."""
import pytest

import nhydb


def _setup(path):
    db = nhydb.NHyDB.create(path)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "N", "type": "varuint"}, {"v": 2, "name": "tp", "type": "varint"}])
    db.create_classic_layout(1, 1, "classic", shape_id=1, base_reserve_to=10)
    db.create_column_layout(2, 1, "col", fields=[{"u": 1000, "name": "N", "type": "varuint"}, {"u": 1001, "name": "tp", "type": "varint"}])
    db.create_packed_layout(3, 1, "pk", u=2000, span=16, fields=[{"offset": 0, "name": "N", "type": "varuint"}, {"offset": 1, "name": "tp", "type": "varint"}])
    db.commit()
    return db


def test_put_record_commit_each_and_reopen(tmpdb):
    db = _setup(tmpdb)
    for i in range(1, 51):
        for lid in (1, 2, 3):
            db.put_record(lid, i, {"N": i, "tp": -i}, commit=True)
    db.close(commit=False)
    db = nhydb.NHyDB(tmpdb)
    assert db.recovered_tail_bytes == 0
    for lid in (1, 2, 3):
        for i in (1, 25, 50):
            r = db.get_record(lid, i)
            assert r["N"] == i and r["tp"] == -i
    assert db.verify(deep=True)["ok"]


def test_uncommitted_record_lost_after_crash(tmpdb):
    db = _setup(tmpdb)
    db.put_record(1, 1, {"N": 1, "tp": 1}, commit=True)
    db.put_record(1, 2, {"N": 2, "tp": 2}, commit=False)
    db.close(commit=False)  # wie Absturz: Daten im Tail, aber kein Superblock-Commit
    db = nhydb.NHyDB(tmpdb)
    assert db.recovered_tail_bytes > 0
    assert db.get_record(1, 1) is not None
    assert db.get_record(1, 2) is None


def test_put_cell_get_value_delete(tmpdb):
    db = _setup(tmpdb)
    db.put_cell(1, 14, 744, "x", type="utf8", commit=True)
    db.put_cell(1, 14, 744, "y", type="utf8", lane="NR", commit=True)
    assert db.get_value(1, 14, 744) == "x"
    assert db.get_value(1, 14, 744, lane="NR") == "y"
    assert db.get_value(1, 14, 743) is None
    db.put_cell(1, 14, 745, None, type="bytes")
    assert db.get_cell(1, 14, 745)["value"] is None  # Null ist belegt
    db.delete_cell(1, 14, 744, commit=True)
    assert db.get_value(1, 14, 744) is None
    assert db.get_value(1, 14, 744, lane="NR") == "y"
    with pytest.raises(nhydb.SchemaError):
        db.put_cell(1, 14, 746, 5)  # kein Typ ableitbar
    with pytest.raises(nhydb.RealmLimitError):
        db.put_cell(1, 14, 10**100 + 1, 5, type="u8")


def test_delete_record_all_geometries(tmpdb):
    db = _setup(tmpdb)
    db.define_extension(1, 50, "utf8", name="note")
    for lid in (1, 2, 3):
        db.put_record(lid, 7, {"N": 7, "tp": 7})
    db.put_extensions(1, 7, {"note": "n"})
    assert db.delete_record(1, 7, commit=True) == 2  # BASE-Record + Extension-Zelle
    assert db.get_record(1, 7) is None
    assert db.get_value(1, 7, 50) is None
    for lid in (2, 3):
        assert db.delete_record(lid, 7) == 2
        assert db.get_record(lid, 7) is None
    assert db.verify(deep=True)["ok"]


def test_durability_option(tmpdb):
    db = nhydb.NHyDB.create(tmpdb, durability="full")
    assert db.durability == "full"
    db.create_realm(1, "r")
    db.commit()
    db.durability = "fsync"
    db.close()
    with pytest.raises(ValueError):
        nhydb.NHyDB(tmpdb, durability="nope")


def test_context_manager_commits(tmpdb):
    with nhydb.create(tmpdb) as db:
        db.create_realm(5, "r")
    with nhydb.open(tmpdb) as db:
        assert db.realm_bounds(5)["realm"] == 5
