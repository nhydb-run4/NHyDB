"""NumPy batch API: same results as the single-item/list API, strict validation."""
import numpy as np
import pytest

import nhydb
from conftest import data_region

KQ = [
    {"v": 1, "name": "N", "type": "varuint"},
    {"v": 2, "name": "tp", "type": "varuint"},
    {"v": 3, "name": "lage", "type": "u8", "enum": {"N0": 0, "NL": 1, "Gz": 2, "NR": 3}},
    {"v": 4, "name": "station", "type": "varint"},
    {"v": 5, "name": "x", "type": "f64"},
    {"v": 6, "name": "y", "type": "f32"},
    {"v": 7, "name": "ok", "type": "bool"},
    {"v": 8, "name": "s16", "type": "i16"},
    {"v": 9, "name": "label", "type": "utf8"},
]


def _db(path):
    db = nhydb.NHyDB.create(path)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "kq", KQ)
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=100)
    db.create_column_layout(2, 1, "col", fields=[{"u": 10_000 + i, "name": f["name"], "type": f["type"]} for i, f in enumerate(KQ[:2])])
    db.create_packed_layout(3, 1, "pk", u=20_000, span=8, fields=[{"offset": i, "name": f["name"], "type": f["type"]} for i, f in enumerate(KQ[:2])])
    return db


def _cols(n):
    i = np.arange(1, n + 1, dtype=np.uint64)
    return {
        "N": i,
        "tp": i * 3,
        "lage": (i % 4).astype(np.uint8),
        "station": (i.astype(np.int64) - 500),
        "x": i / 7,
        "y": (i / 3).astype(np.float32),
        "ok": (i % 2 == 0),
        "s16": (i % 1000).astype(np.int16) - 500,
        "label": np.array([f"l{k}" for k in range(1, n + 1)], dtype=object),
    }


def _rows(cols, n):
    names = [f["name"] for f in KQ]
    out = []
    for k in range(n):
        row = []
        for nm in names:
            v = cols[nm][k]
            row.append(v.item() if hasattr(v, "item") else v)
        out.append(row)
    return out


def test_numpy_batch_bytes_equal_list_batch(tmp_path):
    n = 5000
    cols = _cols(n)
    a = _db(tmp_path / "a.nhydb")
    a.put_numpy_batch(1, 1, cols, commit=True)
    a.close()
    b = _db(tmp_path / "b.nhydb")
    b.put_dense_batch(1, 1, _rows(cols, n), commit=True)
    b.close()
    assert data_region(tmp_path / "a.nhydb") == data_region(tmp_path / "b.nhydb")


def test_numpy_roundtrip_read(tmp_path):
    n = 2000
    cols = _cols(n)
    db = _db(tmp_path / "a.nhydb")
    db.put_numpy_batch(1, 1, cols)
    db.put_points(1, [(5, 4, 12345)])          # overlay on a BASE field
    db.delete_record(1, 7)                            # deleted record
    out = db.read_numpy(1, 1, n + 2)
    assert out["_present"].dtype == bool and out["_present"].sum() == n - 1
    assert not out["_present"][6] and not out["_present"][n] and not out["_present"][n + 1]
    assert out["N"].dtype == np.uint64 and out["N"][9] == 10
    assert out["station"][4] == 12345
    assert out["lage"][1] == "Gz"               # u=2 → code 2 → label (object array)
    assert out["x"].dtype == np.float64 and out["x"][0] == pytest.approx(1 / 7)
    assert out["y"].dtype == np.float32
    assert out["ok"].dtype == bool and out["ok"][1]
    assert out["label"][99] == "l100"
    sub = db.read_numpy(1, 10, 3, ["N", "label"])
    assert list(sub["N"]) == [10, 11, 12] and set(sub) == {"N", "label", "_present"}


def test_dtype_variants(tmp_path):
    db = _db(tmp_path / "a.nhydb")
    n = 10
    base = _cols(n)
    variants = [
        np.arange(1, n + 1, dtype=">u8"),            # foreign endianness
        np.arange(1, 2 * n + 1, dtype=np.int32)[::2],  # non-contiguous
        list(range(1, n + 1)),                        # Python list
        np.arange(1, n + 1, dtype=np.uint16),
    ]
    for k, v in enumerate(variants):
        c = dict(base, N=v)
        db.put_numpy_batch(1, 1 + k * 100, c)
        got = db.read_numpy(1, 1 + k * 100, n, ["N"])["N"]
        assert list(got) == list(np.asarray(v).astype(np.int64)), k


def test_numpy_errors(tmp_path):
    db = _db(tmp_path / "a.nhydb")
    c = _cols(10)
    with pytest.raises(ValueError):
        db.put_numpy_batch(1, 1, dict(c, N=np.arange(5, dtype=np.uint64)))  # length mismatch
    with pytest.raises(ValueError):
        db.put_numpy_batch(1, 1, dict(c, N=np.arange(-1, 9, dtype=np.int64)))  # negative → varuint
    with pytest.raises(ValueError):
        db.put_numpy_batch(1, 1, dict(c, s16=np.full(10, 40000, dtype=np.int64)))  # i16 range
    with pytest.raises(nhydb.SchemaError):
        db.put_numpy_batch(1, 1, {k: v for k, v in c.items() if k != "tp"})
    with pytest.raises(ValueError):
        db.put_numpy_batch(1, 1, dict(c, lage=np.array(["XX"] * 10, dtype=object)))
    assert db.stats()["storage"]["physical_dense_records"] == 0  # nothing written


def test_put_objects_numpy_matches_objects_batch(tmp_path):
    n = 300
    ids = np.arange(1, n + 1, dtype=np.uint64)
    cols = {"N": ids * 2, "tp": ids + 7}
    objs = [(int(i), {"N": int(i) * 2, "tp": int(i) + 7}) for i in ids]
    for lid in (2, 3):
        a = _db(tmp_path / f"a{lid}.nhydb")
        a.put_objects_numpy(lid, ids, cols, commit=True)
        a.close()
        b = _db(tmp_path / f"b{lid}.nhydb")
        b.put_objects_batch(lid, objs, commit=True)
        b.close()
        assert data_region(tmp_path / f"a{lid}.nhydb") == data_region(tmp_path / f"b{lid}.nhydb")
    db = nhydb.NHyDB(tmp_path / "a3.nhydb")
    assert db.get_object(3, 77) == {"_layout": 3, "_layout_name": "pk", "_kind": "PACKED", "_object": 77,
                                     "_lane": 0, "_lane_name": "NL", "N": 154, "tp": 84}


def test_put_objects_numpy_classic_runs_and_ext(tmp_path):
    db = _db(tmp_path / "a.nhydb")
    db.define_extension(1, 200, "varuint", name="extra")
    ids = np.array([1, 2, 3, 10, 11], dtype=np.uint64)
    c = {k: (v[:5] if not isinstance(v, np.ndarray) else v[:5]) for k, v in _cols(5).items()}
    c["extra"] = np.array([0, 5, 0, 7, 0], dtype=object)
    c["extra"][[0, 2, 4]] = None
    r = db.put_objects_numpy(1, ids, c)
    assert r["objects"] == 5
    assert db.get(1, 11)["N"] == 5 and db.get(1, 10)["extra"] == 7 and "extra" not in db.get(1, 1)
    with pytest.raises(nhydb.SchemaError):
        db.put_objects_numpy(1, np.array([5, 4], dtype=np.uint64), {k: v[:2] for k, v in c.items()})


def test_put_cells_numpy(tmp_path):
    db = _db(tmp_path / "a.nhydb")
    n = 1000
    u = np.full(n, 77, dtype=np.uint64)
    v = np.arange(1, n + 1, dtype=np.uint64) * 1000
    vals = np.arange(n, dtype=np.int64) - 3
    db.put_cells_numpy(1, u, v, vals, type="varint", lane="NR", commit=True)
    assert db.get_value(1, 77, 5000, lane="NR") == 1
    assert db.get_value(1, 77, 5000) is None
    db.put_cells_numpy(1, 78, np.array([1, 2], dtype=np.uint64), np.array([1.5, 2.5]), type="f64")
    assert db.get_value(1, 78, 2) == 2.5
    with pytest.raises(ValueError):
        db.put_cells_numpy(1, u[:3], v[:2], vals[:3], type="u8")
