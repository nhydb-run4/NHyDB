"""Memory-efficient iterators (range/select/find/iter_materialized_sparse_points).

They behave like Python generators: lazy (errors only on the first ``next``),
block-wise reading, deterministic order.
"""
import itertools

import pytest

import nhydb


def _db(path):
    db = nhydb.NHyDB.create(path)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "u32"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.set_realm_default_shape(1, 1)
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.define_extension(1, 10, "u16", name="x")
    return db


def test_range_is_lazy_and_skips_gaps(tmp_path):
    db = _db(tmp_path / "a.nhydb")
    db.put_dense_batch(1, 1, [(i, f"s{i}") for i in range(1, 1001)], shape_id=1)
    db.put_dense_batch(1, 10**15, [(7, "far")], shape_id=1)
    it = db.range(1, 1, 10**15)
    assert isinstance(it, nhydb._native.ResultIterator)
    first = list(itertools.islice(it, 3))
    assert [r["a"] for r in first] == [1, 2, 3]
    rest = list(it)
    assert len(rest) == 998 and rest[-1]["_u"] == 10**15  # the gap of 10^15 is skipped
    assert [r["b"] for r in db.select(1, 999, 1001, ["b"])] == ["s999", "s1000"]


def test_find_raises_lazily_like_generator(tmp_path):
    db = _db(tmp_path / "b.nhydb")
    db.put_dense_batch(1, 1, [(i % 3, f"s{i}") for i in range(1, 50)], shape_id=1)
    db.create_index(1, 1, "by_a", "a")
    it = db.find(1, {"by_a": 1})  # index not built yet: no error on creation
    with pytest.raises(nhydb.StaleIndexError):
        next(it)
    db.build_indexes()
    assert [r["_u"] for r in db.find(1, {"by_a": 1})] == list(range(1, 50, 3))


def test_writes_between_steps(tmp_path):
    db = _db(tmp_path / "c.nhydb")
    db.put_dense_batch(1, 1, [(i, "x") for i in range(1, 601)], shape_id=1)
    it = db.range(1, 1, 10_000)
    assert next(it)["_u"] == 1
    db.put(1, 5000, [5, "neu"])  # writing between two steps is allowed
    us = [r["_u"] for r in it]
    assert us[-1] == 5000 and len(us) == 600


