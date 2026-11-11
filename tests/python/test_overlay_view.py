"""0.28.2/scan stage 1: metadata fast paths on overwritten data.

A realm/lane with overlapping DENSE segments or DELETE entries carries the
persisted mark ``overlay_keys``; until 0.28.2/scan every metadata fast path
(record universe, COUNT, NOT, field scan in u space, ORDER-BY index window)
then fell back to the cell cursor. The winner view (``db/winners.rs``) now
serves them exactly. Oracle: the same database opened with
``[query] overlay_view = false`` – the general path as before.

* Random histories (overwrites of every length, deletes, rewrites after a
  delete, rollbacks that hand block ids out again, commits, compaction,
  big u beyond 2**64, bounded layouts, two shapes): every query result and
  every error is identical (``fastpath``/``plan`` markers stripped).
* The view is actually used (``stats()["overlay_view"]["built"]`` grows on
  an unindexed field predicate) – otherwise the comparison would be general
  vs general.
* Stage 2: SPARSE cells (values, tombstones, fully tombstoned records,
  pure sparse records, DELETE + sparse cell, extensions) – reference is the
  general path (cell cursor), not ``get_fields``.
* A view above ``[query] overlay_view_max_bytes`` is not built (general
  path, same results).
"""
import random

import pytest

import nhydb
from nhydb.query import Q, QueryEngine

import test_perf_query_paths as pq

_open = pq.open_cfg
GENERAL = pq.GENERAL


def _views(db):
    return db.stats()["overlay_view"]


EXPRS = pq.EXPRS + [
    Q.field("x") == 1,
    Q.field("x").between(-1, 1),
    Q.field("s") == "a",
    Q.field("f").exists(),
    Q.field("s").contains("B"),
    (Q.field("x") == 1) | (Q.field("s") == "B"),
    ~((Q.field("x") == 0) & (Q.field("s") == "c")),
    (Q.field("x") >= 2) & ~(Q.field("s") == "a"),
]

CASES = [c for c in pq._engine_cases() if c.get("return_mode") in ("count", "n", "record") or "aggregate" in c][:40] + [
    {"return_mode": "fields", "select": ["id", "x"], "order_by": ["x", "-id"], "limit": 4},
    {"return_mode": "fields", "select": ["id"], "order_by": ["-x"], "limit": 3, "offset": 1},
    {"return_mode": "n", "limit": 5, "offset": 3},
]

SQL = pq.SQL + [
    "SELECT COUNT(*) FROM main WHERE x = 1",
    "SELECT id FROM main WHERE x BETWEEN -1 AND 1 ORDER BY id",
    "SELECT id FROM main WHERE NOT s = 'a' ORDER BY id DESC LIMIT 4",
    "SELECT id, x FROM main ORDER BY x, id LIMIT 6",
]


def _row(u, rng):
    return [u % 1000, rng.randint(-3, 3), ("a", "B", "c", "")[rng.randrange(4)], (1.5, -0.0, 2.0)[rng.randrange(3)]]


def _base(db, **layout):
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", pq.SHAPE)
    db.create_classic_layout(1, 1, "main", shape_id=1, **layout)


def _history(db, rng, steps, base=0, max_u=400, lane="NL"):
    """Writes and deletes that leave overlapping segments and DELETE
    entries behind; some uncommitted steps are rolled back."""
    db.put_dense_batch(1, base + 1, [_row(u, rng) for u in range(1, max_u // 2)], shape_id=1, lane=lane)
    db.commit()
    for _ in range(steps):
        k = rng.random()
        if k < 0.45:
            n = rng.choice([1, 1, 1, 2, 3, 7, 20, 60])
            u0 = rng.randint(1, max_u)
            db.put_dense_batch(1, base + u0, [_row(u, rng) for u in range(u0, u0 + n)], shape_id=1, lane=lane)
        elif k < 0.7:
            db.delete_record(1, base + rng.randint(1, max_u), lane=lane)
        elif k < 0.85:
            db.commit()
        else:
            # uncommitted overwrite + delete discarded; ids are handed out again
            db.put_dense_batch(1, base + rng.randint(1, max_u), [_row(u, rng) for u in range(3)], shape_id=1, lane=lane)
            db.delete_record(1, base + rng.randint(1, max_u), lane=lane)
            db.rollback()
    db.commit()


PROBE = dict(expr=Q.field("x") == 1, return_mode="count")


def _compare(path, lanes=(0,), exprs=EXPRS, cases=CASES, sql=SQL, view_used=True, probe=PROBE):
    fast, gen = _open(path), _open(path, GENERAL)
    try:
        qf, qg = QueryEngine(fast), QueryEngine(gen)
        for lane in lanes:
            for expr in exprs:
                for kw in cases:
                    a = pq._call(lambda: qf.execute(expr, realm=1, lane=lane, **kw))
                    b = pq._call(lambda: qg.execute(expr, realm=1, lane=lane, **kw))
                    assert a == b, (lane, expr, kw, a, b)
        for s in sql:
            a, b = pq._call(lambda: fast.sql(s)), pq._call(lambda: gen.sql(s))
            assert a == b, (s, a, b)
        # the view was really used (else the comparison is general vs general)
        kw = dict(probe)
        expr = kw.pop("expr")
        b0 = _views(fast)["built"]
        qf.execute(expr, realm=1, **kw)
        used = _views(fast)["built"] > b0
        qg.execute(expr, realm=1, **kw)
        assert _views(gen) == {"enabled": False, "max_bytes": _views(gen)["max_bytes"], "built": 0, "declined": 0}
        if view_used:
            assert used
        return used
    finally:
        fast.close()
        gen.close()


@pytest.mark.parametrize("seed", range(12))
def test_random_overwrites_and_deletes_equal_general_path(tmp_path, seed):
    rng = random.Random(seed)
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db)
    _history(db, rng, 40)
    db.close()
    _compare(p)


@pytest.mark.parametrize("seed", range(4))
def test_both_lanes_and_big_u(tmp_path, seed):
    rng = random.Random(100 + seed)
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db)
    _history(db, rng, 25)
    _history(db, rng, 25, lane="NR")
    _history(db, rng, 25, base=2**70)          # u beyond 2**64 in the same lane
    db.close()
    _compare(p, lanes=(0, 1))


@pytest.mark.parametrize("seed", range(4))
def test_bounded_layout_clips_the_view(tmp_path, seed):
    rng = random.Random(200 + seed)
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db, u_start=50, u_end=250)
    _history(db, rng, 40)
    db.close()
    _compare(p)


def test_two_shapes_with_overwrites(tmp_path):
    """Overwrites with an older, narrower shape: the field-label check runs
    over every shape ever stored; NOT and COUNT still use the view."""
    rng = random.Random(7)
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "old", pq.SHAPE[:2])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 3] for u in range(1, 200)], shape_id=1)
    db.create_shape(2, 1, "new", pq.SHAPE)
    for _ in range(20):
        u0 = rng.randint(1, 250)
        db.put_dense_batch(1, u0, [_row(u, rng) for u in range(u0, u0 + rng.choice([1, 4, 30]))], shape_id=2)
        db.delete_record(1, rng.randint(1, 250))
    db.commit()
    db.close()
    _compare(p, sql=[s for s in SQL if "s =" not in s and " s" not in s and "s," not in s], view_used=False)


def test_delete_then_rewrite_and_compaction(tmp_path):
    """Delete order: a rewrite after a DELETE revives u, a DELETE after the
    rewrite hides it again. After compaction the mark is gone and the
    paths without overlays answer the same."""
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db)
    rng = random.Random(3)
    db.put_dense_batch(1, 1, [_row(u, rng) for u in range(1, 101)], shape_id=1)
    db.commit()
    for u in (5, 6, 7, 50):
        db.delete_record(1, u)
    db.put_dense_batch(1, 6, [_row(6, rng)], shape_id=1)          # revives 6
    db.put_dense_batch(1, 49, [_row(u, rng) for u in range(49, 52)], shape_id=1)   # revives 50
    db.delete_record(1, 51)                                                # hides 51 again
    db.commit()
    db.close()
    _compare(p)
    fast = _open(p)
    n = QueryEngine(fast).execute(Q.all(), realm=1, return_mode="count")["count"]
    fast.close()
    assert n == 100 - 3                                             # 5, 7, 51 dead
    nhydb.NHyDB.compact_file(str(p), make_backup=False)
    after = _open(p)
    assert after.stats()["has_overlays"] is False
    assert QueryEngine(after).execute(Q.all(), realm=1, return_mode="count")["count"] == n
    assert _views(after)["built"] == 0                              # no mark, no view
    after.close()


def test_order_by_index_window_with_overwrites(tmp_path):
    """R4-A3 with a fresh VALUE index on overwritten data: the window reads
    the index; the universe for its completeness check comes from the view."""
    rng = random.Random(11)
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db)
    db.create_index(1, 1, "i_x", "x")
    _history(db, rng, 40)
    db.build_indexes()
    db.commit()
    db.close()
    # x is indexed: the probe is the window itself (its completeness check
    # needs the universe of the view)
    _compare(p, probe=dict(expr=Q.all(), return_mode="fields", select=["id"], order_by=["x"], limit=3))


def _sparse_history(db, rng, steps, max_u=400, ext=False):
    """SPARSE cells on top of overwritten DENSE data: values on shape fields
    (and extensions), tombstones up to fully tombstoned records, pure sparse
    records, DELETEs of u with sparse cells, rollbacks."""
    vs = [1, 2, 3, 4] + ([10] if ext else [])
    for _ in range(steps):
        k = rng.random()
        u = rng.randint(1, max_u + 100)            # beyond max_u: pure sparse
        if k < 0.3:
            v = rng.choice(vs)
            val = {1: u % 1000, 2: rng.randint(-3, 3), 3: rng.choice(["a", "B", "c"]), 4: 1.5, 10: "e%d" % rng.randrange(3)}[v]
            if v == 10:
                try:
                    db.put_extensions(1, u, {"ext": val})
                except KeyError:                   # needs a DENSE record
                    db.put_cell(1, u, 10, val)
            else:
                db.put_cell(1, u, v, val)
        elif k < 0.5:
            db.delete_cell(1, u, rng.choice(vs))
        elif k < 0.6:
            for v in vs:                           # every cell tombstoned
                db.delete_cell(1, u, v)
        elif k < 0.65:
            db.delete_record(1, u)                        # the record and all its cells
        elif k < 0.7:
            # T-03: partial delete in v (and over several u): what lies outside
            # the v range survives, as tombstones without a DELETE block
            lo = rng.choice(vs)
            db.delete_area(1, u, u + rng.choice([0, 2, 9]), lo, rng.choice([x for x in vs if x >= lo]))
        elif k < 0.8:
            n = rng.choice([1, 3, 20])
            db.put_dense_batch(1, u, [_row(x, rng) for x in range(u, u + n)], shape_id=1)
        elif k < 0.92:
            db.commit()
        else:
            db.put_cell(1, rng.randint(1, max_u), 2, 99)
            db.delete_cell(1, rng.randint(1, max_u), 3)
            db.rollback()
    db.commit()


SPARSE_EXPRS = EXPRS + [Q.field("x").exists(), Q.field("x") == 99, ~Q.field("s").exists(), Q.field("id") >= 0]


@pytest.mark.parametrize("seed", range(10))
def test_sparse_cells_equal_cell_cursor(tmp_path, seed):
    """Stage 2: the general path (cell cursor, ``overlay_view = false``) is
    the reference – not ``get_fields``, which knows no pure sparse records."""
    rng = random.Random(300 + seed)
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db)
    _history(db, rng, 25)
    _sparse_history(db, rng, 60)
    db.close()
    _compare(p, exprs=SPARSE_EXPRS)


@pytest.mark.parametrize("seed", range(3))
def test_sparse_with_extension_and_index(tmp_path, seed):
    rng = random.Random(400 + seed)
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db)
    db.define_extension(1, 10, "utf8", name="ext")
    db.create_index(1, 1, "i_x", "x")
    _history(db, rng, 25)
    _sparse_history(db, rng, 60, ext=True)
    db.build_indexes()
    db.commit()
    _sparse_history(db, rng, 15, ext=True)     # stale index answers through its overlay
    db.close()
    _compare(p, exprs=SPARSE_EXPRS + [Q.field("ext") == "e1", Q.field("ext").exists()],
             probe=dict(expr=Q.field("s") == "a", return_mode="count"))


def test_sparse_only_dense_free_u_and_all_tombed(tmp_path):
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db)
    rng = random.Random(1)
    db.put_dense_batch(1, 1, [_row(u, rng) for u in range(1, 51)], shape_id=1)
    db.put_dense_batch(1, 10, [_row(u, rng) for u in range(10, 13)], shape_id=1)
    for v in (1, 2, 3, 4):
        db.delete_cell(1, 20, v)                   # u20: gone
    db.put_cell(1, 70, 2, 1)                       # u70: pure
    db.delete_record(1, 30)
    db.put_cell(1, 30, 3, "a")                     # u30: DENSE deleted, sparse cell stays
    db.commit()
    db.close()
    _compare(p, exprs=SPARSE_EXPRS)
    h = _open(p)
    q = QueryEngine(h)
    assert q.execute(Q.all(), realm=1, return_mode="count")["count"] == 50 - 2 + 2
    assert q.execute(Q.field("s") == "a", realm=1, return_mode="n")["refs"]  # u30 among them
    h.close()


def test_view_above_budget_takes_general_path(tmp_path):
    rng = random.Random(9)
    p = tmp_path / "w.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _base(db)
    _history(db, rng, 40)
    db.close()
    small = _open(p, "[query]\noverlay_view_max_bytes = 1")
    gen = _open(p, GENERAL)
    try:
        for expr in EXPRS:
            a = pq._call(lambda: QueryEngine(small).execute(expr, realm=1, return_mode="n"))
            b = pq._call(lambda: QueryEngine(gen).execute(expr, realm=1, return_mode="n"))
            assert a == b
        QueryEngine(small).execute(Q.field("x") == 1, realm=1, return_mode="count")
        st = _views(small)
        assert st["built"] == 0 and st["declined"] > 0
    finally:
        small.close()
        gen.close()


def test_s6_cell_semantics_of_the_general_path(tmp_path):
    """S6 probe (analysis 2026-11-07, Teil III §10) as a fixed regression
    test: the record set of the general path with SPARSE cells and DELETE.
    Stage 2 must reproduce exactly this."""
    p = tmp_path / "s6.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "i64"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 3] for u in range(1, 11)], shape_id=1)
    db.delete_cell(1, 2, 1)
    db.delete_cell(1, 2, 2)                    # u2: every cell tombstoned
    db.delete_cell(1, 3, 2)                    # u3: one cell tombstoned
    db.put_cell(1, 20, 2, 1)                   # u20: pure sparse record
    db.delete_record(1, 4)                     # u4: DELETE
    db.delete_record(1, 5)
    db.put_dense_batch(1, 5, [[5, 1]], shape_id=1)   # u5: DELETE, then rewritten
    db.commit()
    for h in (db, ):
        q = QueryEngine(h)
        rows = q.execute(Q.all(), realm=1, return_mode="record")["rows"]
        assert [r["object"] for r in rows] == [1, 3, 5, 6, 7, 8, 9, 10, 20]
        assert q.execute(Q.all(), realm=1, return_mode="count")["count"] == 9
        assert q.execute(Q.field("x") == 1, realm=1, return_mode="n")["count"] == 5
        assert q.execute(~(Q.field("x") == 1), realm=1, return_mode="count")["count"] == 4
    # reference behaviour kept (find_u / get are not the query universe)
    assert db.find_u(1, {}) == [1, 2, 3, 5, 6, 7, 8, 9, 10]
    assert db.get(1, 20) is None
    db.close()
