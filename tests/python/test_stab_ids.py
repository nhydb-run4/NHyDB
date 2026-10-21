"""Stabilisation: administrative IDs (F004) and occupied IDs (F008)."""
import pytest

import nhydb

MAX_ID = 2**63 - 1
FIELDS = [{"v": 1, "name": "a", "type": "varuint"}]


def _db(tmp_path, name="ids.nhydb"):
    db = nhydb.NHyDB.create(str(tmp_path / name))
    db.create_realm(1, "main")
    return db


def test_max_ids_roundtrip(tmp_path):
    p = tmp_path / "ids.nhydb"
    db = _db(tmp_path)
    s = db.create_shape(MAX_ID, 1, "row", FIELDS)
    assert s["shape_id"] == MAX_ID
    db.create_classic_layout(MAX_ID, 1, "rec", shape_id=MAX_ID)
    db.put_dense_batch(1, 1, [[7]], shape_id=MAX_ID)
    db.create_index(MAX_ID, 1, "ia", 1)
    db.create_realm(2, "other", default_shape_id=None)
    st = db.create_math_map_layout("m", ("x", "y"), u="x", V="y", layout_id=MAX_ID - 1)
    assert st.layout_id == MAX_ID - 1
    db.commit()
    db.close()
    db = nhydb.NHyDB(str(p))
    assert db.get(1, 1)["a"] == 7
    assert db.get(1, 1)["_shape"] == MAX_ID
    assert db.describe_math(MAX_ID - 1)["layout_id"] == MAX_ID - 1
    assert db.verify(deep=True)["ok"]
    db.close()


@pytest.mark.parametrize("bad", [2**63, 2**64 - 1, 2**64])
def test_ids_above_2_63_rejected(tmp_path, bad):
    p = tmp_path / "ids.nhydb"
    db = _db(tmp_path)
    db.create_shape(1, 1, "row", FIELDS)
    with pytest.raises(ValueError):
        db.create_shape(bad, 1, "row2", FIELDS)
    with pytest.raises(ValueError):
        db.create_classic_layout(bad, 1, "rec", shape_id=1)
    with pytest.raises(ValueError):
        db.create_index(bad, 1, "ia", 1)
    with pytest.raises(ValueError):
        db.create_realm(3, "r3", default_shape_id=bad)
    if bad < 2**64:
        with pytest.raises(ValueError):
            db.create_math_map_layout("m", ("x", "y"), u="x", V="y", layout_id=bad)
    db.commit()
    db.close()
    db = nhydb.NHyDB(str(p))  # DB stays openable
    assert db.verify()["ok"]
    db.close()


def test_create_shape_occupied_id_rejected(tmp_path):
    p = tmp_path / "ids.nhydb"
    db = _db(tmp_path)
    db.create_shape(1, 1, "row", FIELDS)
    db.create_classic_layout(1, 1, "rec", shape_id=1)
    db.put_dense_batch(1, 1, [[5], [6]], shape_id=1, commit=True)
    with pytest.raises(nhydb.SchemaError):
        db.create_shape(1, 1, "neu", [{"v": 1, "name": "x", "type": "utf8"}, {"v": 2, "name": "y", "type": "u8"}])
    db.commit()
    db.close()
    db = nhydb.NHyDB(str(p))
    assert db.get(1, 2)["a"] == 6
    assert db.verify(deep=True)["ok"]
    db.close()


def test_create_realm_occupied_id_rejected(tmp_path):
    db = _db(tmp_path)
    with pytest.raises(nhydb.SchemaError):
        db.create_realm(1, "again")
    assert db.realms[1]["name"] == "main"
    # internal realm updates (default shape) keep working
    db.create_shape(1, 1, "row", FIELDS)
    db.set_realm_default_shape(1, 1)
    db.create_classic_layout(1, 1, "rec", shape_id=1)
    db.close()


def test_create_index_occupied_id_rejected(tmp_path):
    db = _db(tmp_path)
    db.create_shape(1, 1, "row", FIELDS)
    db.create_classic_layout(1, 1, "rec", shape_id=1)
    db.create_index(5, 1, "ia", 1)
    with pytest.raises(nhydb.SchemaError):
        db.create_index(5, 1, "ib", 1)
    db.close()


def test_create_layout_occupied_id_rejected(tmp_path):
    db = _db(tmp_path)
    db.create_shape(1, 1, "row", FIELDS)
    db.create_classic_layout(1, 1, "rec", shape_id=1)
    with pytest.raises(nhydb.SchemaError):
        db.create_classic_layout(1, 1, "rec2", shape_id=1)
    st = db.create_math_map_layout("m", ("x", "y"), u="x", V="y", layout_id=9)
    with pytest.raises(nhydb.SchemaError):
        db.create_math_map_layout("m2", ("x", "y"), u="x", V="y", layout_id=9)
    with pytest.raises(nhydb.SchemaError):
        db.create_classic_layout(st.layout_id, 1, "rec3", shape_id=1)
    db.close()
