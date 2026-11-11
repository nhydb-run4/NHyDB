"""IT1: iterators and ``rollback()`` on the same database object.

A rollback discards blocks whose file offsets a new write can reuse. An
iterator started before it used to keep reading with the old offsets:
``iter_materialized_sparse_points`` returned values under the wrong ``u`` (or
raised a false ``CorruptionError``), and every iterator still returned the
elements it had buffered from the discarded state. Now the iterator ends with
``NHyDBError``; writes and commits between two steps stay allowed.
"""
import itertools

import pytest

import nhydb

N = 3000  # several batches of 256


def _db(path):
    db = nhydb.NHyDB.create(path)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "u32"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.set_realm_default_shape(1, 1)
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.define_extension(1, 10, "u16", name="x")
    return db


def _filled(path):
    db = _db(path)
    db.put_dense_batch(1, 1, [(u % 2, f"b{u}") for u in range(1, N + 1)], shape_id=1)
    db.put_points(1, [(u, 10, 1) for u in range(1, N + 1)])
    db.create_index(1, 1, "by_a", "a")
    db.build_indexes()
    db.commit()
    return db


ITERATORS = {
    "range": lambda db: db.range(1, 1, 10**9),
    "select": lambda db: db.select(1, 1, 10**9, ["a", "b", "x"]),
    "find": lambda db: db.find(1, {"by_a": 1}),
    "select_us": lambda db: db.select_us(1, range(1, N + 1), ["a", "b"]),
    "materialized_cells": lambda db: db.iter_materialized_cells(1),
    "materialized_record_ns": lambda db: db.iter_materialized_record_ns(1),
    "sparse_points": lambda db: db.iter_materialized_sparse_points(1),
    "sql_iter": lambda db: db.sql_iter("SELECT a, b, x FROM c"),
}


def _invalidated():
    return pytest.raises(nhydb.NHyDBError, match="iterator invalidated")


def test_sparse_points_never_misattribute_values_after_rollback(tmp_path):
    db = _db(tmp_path / "s.nhydb")
    db.put_points(1, [(u, 10, 1) for u in range(1, N + 1)], commit=True)
    db.put_points(1, [(u, 10, 2) for u in range(1, N + 1)])            # uncommitted block
    it = db.iter_materialized_sparse_points(1)
    assert next(it)["value"] == 2                                       # buffer: 256 uncommitted values
    db.rollback()
    db.put_points(1, [(u, 10, 10000 + u) for u in range(2, N + 2)])    # reuses the discarded offsets
    with _invalidated():
        next(it)                       # neither a buffered value 2 nor a misattributed 10000+u
    assert list(it) == []
    fresh = {p["u"]: p["value"] for p in db.iter_materialized_sparse_points(1)}
    assert fresh[1] == 1 and all(fresh[u] == 10000 + u for u in range(2, N + 2))
    assert db.verify()["ok"]


@pytest.mark.parametrize("kind", sorted(ITERATORS))
def test_rollback_ends_every_iterator_including_its_buffer(tmp_path, kind):
    db = _filled(tmp_path / f"{kind}.nhydb")
    it = ITERATORS[kind](db)
    next(it)
    db.put(1, N + 10, [1, "neu"])
    db.rollback()
    with _invalidated():
        next(it)
    assert list(it) == []                       # stays finished
    assert len(list(itertools.islice(ITERATORS[kind](db), 10))) == 10   # a new query works


@pytest.mark.parametrize("kind", sorted(ITERATORS))
def test_writes_commits_and_empty_rollback_keep_iterators_valid(tmp_path, kind):
    db = _filled(tmp_path / f"{kind}.nhydb")
    expected = len(list(ITERATORS[kind](db)))
    it = ITERATORS[kind](db)
    got = [next(it)]
    db.rollback()                                # nothing pending: discards nothing
    got += list(itertools.islice(it, 300))       # crosses a batch boundary
    db.put(1, 1, [1, "geändert"])                # write between two steps (allowed, as before)
    db.commit()
    got += list(it)
    assert len(got) >= expected - 1              # live view: the overwrite may drop one find hit


def test_rollback_before_first_step_and_after_exhaustion(tmp_path):
    db = _filled(tmp_path / "lazy.nhydb")
    before = db.range(1, 1, 10**9)               # created, not started
    done = db.iter_materialized_sparse_points(1)
    assert len(list(done)) == N                  # fully consumed
    db.put(1, N + 10, [1, "x"])
    db.rollback()
    assert len(list(before)) == N                # starts after the rollback: valid
    assert list(done) == []                      # already finished: plain StopIteration


def test_readonly_reader_is_not_affected_by_writer_rollback(tmp_path):
    p = tmp_path / "ro.nhydb"
    _filled(p).close()
    w = nhydb.NHyDB(p)
    r = nhydb.NHyDB(p, readonly=True)
    it = r.iter_materialized_sparse_points(1)
    next(it)
    w.put_points(1, [(u, 10, 7) for u in range(1, N + 1)])
    w.rollback()
    w.put_points(1, [(u, 10, 9) for u in range(2, N + 2)])
    rest = list(it)
    assert len(rest) == N - 1 and {p["value"] for p in rest} == {1}
    r.close()
    w.close()
