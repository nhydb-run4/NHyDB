"""NumPy extensions: put_math_numpy and query results as columns."""
import numpy as np
import pytest

import nhydb
from nhydb.math import MathMap, MathMapLayoutSpec
from nhydb.query import Q, QueryEngine
from conftest import blocks


def _math_db(path, payload_type="varint"):
    db = nhydb.NHyDB.create(str(path))
    m = MathMap.compile("g", ("x", "k"), u="x // 100 + 1", V="k*10**100 + x % 100 + 1", side="x % 2")
    db.create_math_layout(MathMapLayoutSpec("g", m, 64), layout_id=1, payload_type=payload_type)
    return db


@pytest.mark.parametrize("form", ["2d", "columns", "lists"])
def test_put_math_numpy_equals_batch(tmp_path, form):
    n = 5000
    x = np.arange(n, dtype=np.int64)
    k = (x % 5).astype(np.int32)
    pay = (x * 7 - 100).astype(np.int64)
    a = _math_db(tmp_path / "a.nhydb")
    ra = a.put_math_batch(1, [((int(i), int(i % 5)), int(i * 7 - 100)) for i in range(n)], commit=True)
    b = _math_db(tmp_path / "b.nhydb")
    inputs = {"2d": np.stack([x, k], axis=1), "columns": [x, k], "lists": [x.tolist(), k.tolist()]}[form]
    rb = b.put_math_numpy(1, inputs, pay, commit=True)
    assert ra == rb
    for i in range(0, n, 97):
        assert b.get_math(1, (i, i % 5)) == a.get_math(1, (i, i % 5)) == i * 7 - 100
    assert a.describe_math(1)["realm_extents"] == b.describe_math(1)["realm_extents"]
    a.close(), b.close()
    assert [x[:1] + x[2:] for x in blocks(tmp_path / "a.nhydb")] == [x[:1] + x[2:] for x in blocks(tmp_path / "b.nhydb")]


def test_put_math_numpy_payload_types_and_errors(tmp_path):
    db = _math_db(tmp_path / "p.nhydb", payload_type="f64")
    db.put_math_numpy(1, np.array([1, 2, 3]).reshape(-1, 1).repeat(2, axis=1), np.array([0.5, 1.5, 2.5]))
    assert db.get_math(1, (2, 2)) == 1.5
    with pytest.raises(ValueError):
        db.put_math_numpy(1, [np.arange(3), np.arange(2)], np.zeros(3))
    with pytest.raises(nhydb.MathCellConflictError):
        db.put_math_numpy(1, np.array([[1, 1], [9, 1]]), np.array([9.0, 9.0]))
    assert db.get_math(1, (9, 1)) is None  # batch fully rolled back
    with pytest.raises(nhydb.MathDomainError):
        db.put_math_numpy(1, np.array([[-500, 0]]), np.array([1.0]))
    t = _math_db(tmp_path / "t.nhydb", payload_type="utf8")
    t.put_math_numpy(1, np.array([[1, 0], [2, 0]]), np.array(["a", "bé"], dtype=object))
    assert t.get_math(1, (2, 0)) == "bé"
    db.close(), t.close()


def test_query_results_as_numpy(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "q.nhydb"))
    db.create_realm(1, "main")
    db.create_shape(1, 1, "row", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "f64"},
                                  {"v": 3, "name": "name", "type": "utf8"}, {"v": 4, "name": "ok", "type": "bool"}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u / 4, f"n{u}", u % 2 == 0] for u in range(1, 21)], shape_id=1)
    db.create_index(1, 1, "idx_id", "id")
    db.build_indexes(commit=True)
    out = db.sql_numpy("SELECT id, x, name, ok FROM records WHERE id BETWEEN ? AND ? ORDER BY id DESC", [3, 7])
    assert list(out) == ["id", "x", "name", "ok"]
    assert out["id"].dtype == np.int64 and out["id"].tolist() == [7, 6, 5, 4, 3]
    assert out["x"].dtype == np.float64 and out["x"].tolist() == [1.75, 1.5, 1.25, 1.0, 0.75]
    assert out["name"].dtype == object and out["ok"].dtype == np.bool_
    agg = db.sql_numpy("SELECT ok, COUNT(*) AS n, AVG(x) AS a FROM records GROUP BY ok ORDER BY n")
    assert agg["n"].tolist() == [10, 10] and agg["a"].dtype == np.float64
    q = QueryEngine(db)
    r = q.execute_numpy(Q.field("id") >= 18, realm=1, select=["id", "name"], order_by=["-id"])
    assert list(r) == ["record_N", "id", "name"] and r["id"].tolist() == [20, 19, 18]
    s = q.execute_sql_numpy("SELECT id FROM records WHERE id = :i", {"i": 4})
    assert s["id"].tolist() == [4]
    empty = db.sql_numpy("SELECT id FROM records WHERE id > 1000")
    assert empty["id"].dtype == np.int64 and len(empty["id"]) == 0
    db.close()
