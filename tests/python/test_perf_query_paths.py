"""Equivalence tests for the 0.28 query fast paths (P004, P006, P007, P021,
P022): every new shortcut must return exactly what the general path returns.

* P007 (ALL from segment metadata, COUNT/ORDER BY/GROUP BY/NOT): every
  scenario is built three times; the second and third copy additionally
  write and delete one record far outside the data. That record is
  invisible, but the DELETE marks the (realm, lane) as overlaid. Since
  0.28.2/scan the second copy answers through the winner view
  (``db/winners.rs``); the third is opened with ``[query] overlay_view =
  false``, which forces the general scan path. Results (without
  ``fastpath``/``plan``) and errors of all three must be identical.
* P006 (STRUCT_RANGE over more than 4096 u): compared with the union of
  direct ranges of at most 4096 u (the reference path that visits every u).
* P004 (range/select skip gaps): compared with per-u ``get``/``get_record``.
"""
import math
import os
from pathlib import Path

import pytest

import nhydb
from nhydb.query import Q, QueryEngine

FAR = 10**30  # u of the invisible write+delete that marks the lane as overlaid

_ENV0 = os.environ.get("NHYDB_CONFIG")


def open_cfg(path, extra=None, readonly=True):
    """Opens ``path``; ``extra``: TOML lines for this handle. A configuration
    given by NHYDB_CONFIG (dir3 runs of the QA) takes precedence over a file
    next to the database, so its contents are kept and the lines appended in
    a combined file that NHYDB_CONFIG points to while the handle opens."""
    if not extra:
        return nhydb.NHyDB(str(path), readonly=readonly)
    d = Path(path).parent / ("_cfg_%d" % abs(hash(extra)))
    d.mkdir(exist_ok=True)
    f = d / "nhydb_config.toml"
    base = Path(_ENV0).read_text() + "\n" if _ENV0 else ""
    f.write_text(base + extra + "\n")
    os.environ["NHYDB_CONFIG"] = str(f)
    try:
        return nhydb.NHyDB(str(path), readonly=readonly)
    finally:
        if _ENV0 is None:
            os.environ.pop("NHYDB_CONFIG", None)
        else:
            os.environ["NHYDB_CONFIG"] = _ENV0


GENERAL = "[query]\noverlay_view = false"


def _strip(res):
    if isinstance(res, dict):
        return {k: _strip(v) for k, v in res.items() if k not in ("fastpath", "plan")}
    if isinstance(res, list):
        return [_strip(x) for x in res]
    if isinstance(res, float) and math.isnan(res):
        return "NaN"
    return res


def _call(f):
    try:
        return ("ok", _strip(f()))
    except Exception as e:  # compared by class and message
        return ("err", type(e).__name__, str(e))


SHAPE = [
    {"v": 1, "name": "id", "type": "varuint"},
    {"v": 2, "name": "x", "type": "i64"},
    {"v": 3, "name": "s", "type": "utf8"},
    {"v": 4, "name": "f", "type": "f64"},
]


def _row(u):
    return [u, (u * 7) % 11 - 3, ("a", "B", "c", "")[u % 4], (1.5, float("nan"), -0.0, 2.0)[u % 4]]


def _base(db, **layout):
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", SHAPE)
    db.create_classic_layout(1, 1, "main", shape_id=1, **layout)


def sc_plain(db):
    _base(db)
    db.put_dense_batch(1, 1, [_row(u) for u in range(1, 61)], shape_id=1)
    db.put_dense_batch(1, 200, [_row(u) for u in range(200, 230)], shape_id=1)
    for u in (500, 501, 777):
        db.put(1, u, _row(u), shape_id=1)
    db.put_dense_batch(1, 10**20, [_row(u) for u in range(5)], shape_id=1)   # big u
    db.commit()


def sc_lanes(db):
    sc_plain(db)
    db.put_dense_batch(1, 3, [_row(u) for u in range(3, 40)], shape_id=1, lane="NR")
    db.commit()


def sc_bounded(db):
    """Data outside the layout range is not part of the record universe."""
    _base(db, u_start=10, u_end=70)
    db.put_dense_batch(1, 1, [_row(u) for u in range(1, 101)], shape_id=1)
    db.commit()


def sc_extension(db):
    """An extension definition keeps the CLASSIC row builder off (strict
    shape condition); COUNT and the universe still use the metadata."""
    sc_plain(db)
    db.define_extension(1, 10, "utf8", name="ext")
    db.put_extensions(1, 5, {"ext": "hello"})
    db.commit()


def sc_shapes(db):
    """Two shapes in one realm: segments with an older shape."""
    db.create_realm(1, "r")
    db.create_shape(1, 1, "old", SHAPE[:2])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 3] for u in range(1, 20)], shape_id=1)
    db.create_shape(2, 1, "new", SHAPE)
    db.put_dense_batch(1, 20, [_row(u) for u in range(20, 30)], shape_id=2)
    db.commit()


def sc_empty(db):
    _base(db)
    db.commit()


SCENARIOS = {
    "plain": (sc_plain, (0,)),
    "lanes": (sc_lanes, (0, 1)),
    "bounded": (sc_bounded, (0,)),
    "extension": (sc_extension, (0,)),
    "shapes": (sc_shapes, (0,)),
    "empty": (sc_empty, (0,)),
}


def _force_general(db):
    for lane in ("NL", "NR"):
        try:
            db.put(1, FAR, [1, 1, "z", 1.0], shape_id=1, lane=lane)
        except nhydb.SchemaError:
            db.put(1, FAR, [1, 1], shape_id=1, lane=lane)
        db.delete_record(1, FAR, lane=lane)
    db.commit()


@pytest.fixture(scope="module")
def pairs(tmp_path_factory):
    root = tmp_path_factory.mktemp("perf_paths")
    out = {}
    for name, (setup, _lanes) in SCENARIOS.items():
        trio = []
        for kind in range(3):           # 0 fast, 1 general (view off), 2 winner view
            path = root / ("%s_%d.nhydb" % (name, kind))
            db = nhydb.NHyDB.create(str(path))
            setup(db)
            if kind:
                _force_general(db)
            if kind == 1:
                db.close()
                db = open_cfg(path, GENERAL, readonly=False)
            trio.append(db)
        out[name] = trio
    yield out
    for pair in out.values():
        for db in pair:
            db.close()


SELECTS = [["id"], ["x", "s", "u", "record_N", "missing"], ["record_N"], ["f", "id"], ["result", "id"]]
ORDERS = [["-id"], ["x", "-id"], ["s"], ["-s", "f"], ["record_N"], ["-u"], ["missing"], ["f"], ["result"]]
AGG = {"n": {"op": "count", "field": "*"}, "sx": {"op": "sum", "field": "x"}, "hi": {"op": "max", "field": "id"},
       "lo": {"op": "min", "field": "s"}, "av": {"op": "avg", "field": "x"}}


def _engine_cases():
    cases = [{"return_mode": m} for m in ("count", "n", "record", "cell")]
    cases += [{"return_mode": "n", "limit": 3, "offset": 2}, {"return_mode": "record", "limit": 2}]
    for sel in SELECTS:
        cases.append({"return_mode": "fields", "select": sel})
        cases.append({"return_mode": "fields", "select": sel, "limit": 3, "offset": 1})
        for ob in ORDERS:
            cases.append({"return_mode": "fields", "select": sel, "order_by": ob, "limit": 4})
            cases.append({"return_mode": "fields", "select": sel, "order_by": ob, "offset": 2})
    for ob in ORDERS[:4]:
        cases.append({"return_mode": "record", "order_by": ob, "limit": 3})
    for gb in ([], ["s"], ["x"], ["f"], ["missing"]):
        cases.append({"aggregate": AGG, "group_by": gb})
        cases.append({"aggregate": AGG, "group_by": gb, "order_by": ["-n"], "limit": 3})
    cases.append({"aggregate": {"bad": {"op": "sum", "field": "s"}}, "group_by": ["x"]})   # plan error
    cases.append({"aggregate": {"n": {"op": "count", "field": "*"}}, "group_by": ["u"]})   # structural name
    cases.append({"return_mode": "fields", "select": ["id"], "order_by": [{"bad": 1}]})     # invalid ORDER BY
    return cases


EXPRS = [Q.all(), ~(Q.field("x") == 1), ~Q.all(), Q.all() & (Q.field("s") == "a"), ~(Q.u.between(1, 5000))]

SQL = [
    "SELECT COUNT(*) FROM main",
    "SELECT COUNT(*) AS n FROM main",
    "SELECT id FROM main ORDER BY id DESC LIMIT 5",
    "SELECT id, s FROM main ORDER BY s, id DESC LIMIT 7 OFFSET 2",
    "SELECT * FROM main LIMIT 3",
    "SELECT x, COUNT(*) FROM main GROUP BY x",
    "SELECT s, COUNT(*), SUM(x), MAX(id) FROM main GROUP BY s ORDER BY s",
    "SELECT COUNT(*) FROM main WHERE NOT x = 1",
]


@pytest.mark.parametrize("scenario", list(SCENARIOS))
def test_all_fast_path_equals_general_path(pairs, scenario):
    fast, general, view = pairs[scenario]
    assert general.stats()["overlay_view"]["enabled"] is False
    for lane in SCENARIOS[scenario][1]:
        qf, qg, qv = QueryEngine(fast), QueryEngine(general), QueryEngine(view)
        for expr in EXPRS:
            for kw in _engine_cases():
                a = _call(lambda: qf.execute(expr, realm=1, lane=lane, **kw))
                b = _call(lambda: qg.execute(expr, realm=1, lane=lane, **kw))
                c = _call(lambda: qv.execute(expr, realm=1, lane=lane, **kw))
                assert a == b, (scenario, lane, expr, kw, a, b)
                assert c == b, ("view", scenario, lane, expr, kw, c, b)
        if lane == 0:
            for sql in SQL:
                a, b, c = _call(lambda: fast.sql(sql)), _call(lambda: general.sql(sql)), _call(lambda: view.sql(sql))
                assert a == b, (scenario, sql, a, b)
                assert c == b, ("view", scenario, sql, c, b)


def test_count_matches_records(pairs):
    fast, _, _ = pairs["plain"]
    n = sum(1 for _ in fast.range(1, 1, 10**21))
    assert fast.sql("SELECT COUNT(*) FROM main")["rows"] == [{"COUNT(*)": n}] or \
        list(fast.sql("SELECT COUNT(*) FROM main")["rows"][0].values()) == [n]
    assert QueryEngine(fast).execute(Q.all(), realm=1, return_mode="count")["count"] == n
    bounded, _, _ = pairs["bounded"]
    assert QueryEngine(bounded).execute(Q.all(), realm=1, return_mode="count")["count"] == 61


def test_order_by_mixed_types_raises_like_general(tmp_path):
    """Keys of incompatible types keep the reference comparator (plan error)."""
    out = []
    for general in (False, True):
        db = nhydb.NHyDB.create(str(tmp_path / ("m%d.nhydb" % general)))
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}])
        db.create_classic_layout(1, 1, "main", shape_id=1)
        db.put_dense_batch(1, 1, [[u] for u in range(1, 9)], shape_id=1)
        db.define_extension(1, 5, "utf8", name="e")
        db.put_cell(1, 3, 5, "text")
        db.put_cell(1, 4, 5, 7, type="i64")
        db.commit()
        if general:
            db.put(1, FAR, [1], shape_id=1)
            db.delete_record(1, FAR)
            db.commit()
        q = QueryEngine(db)
        out.append(_call(lambda: q.execute(Q.all(), realm=1, return_mode="fields", select=["id", "e"], order_by=["e"], limit=2)))
        db.close()
    assert out[0] == out[1]


# ---------------------------------------------------------------------------
# P006: STRUCT_RANGE over more than 4096 u
# ---------------------------------------------------------------------------

def _struct_db(path):
    db = nhydb.NHyDB.create(path)
    _base(db)
    db.put_dense_batch(1, 1, [_row(u) for u in range(1, 3001)], shape_id=1)
    db.put_dense_batch(1, 2500, [_row(u) for u in range(2500, 2600)], shape_id=1)   # overlapping rewrite
    db.put_dense_batch(1, 9000, [_row(u) for u in range(9000, 9100)], shape_id=1, lane="NR")
    db.commit()
    db.delete_record(1, 17)
    db.delete_record(1, 18)
    for u in (4500, 6100, 6101):
        db.put_cell(1, u, 2, 5)                    # pure sparse records
    db.put_cell(1, 7000, 2, 5)
    db.delete_cell(1, 7000, 2)                     # tombstoned pure sparse cell
    db.delete_cell(1, 20, 1)                       # tombstone on a dense record
    db.put_cell(1, 9050, 3, "s", lane="NR")
    db.commit()
    return db


def _direct(q, lo, hi, lane, mode):
    refs = []
    a = lo
    while a <= hi:
        b = min(hi, a + 4095)
        refs += q.execute(Q.u.between(a, b), realm=1, lane=lane, return_mode=mode)["refs"]
        a = b + 1
    return sorted(set(refs), key=lambda r: int(r[1:]))


@pytest.mark.parametrize("lo,hi", [(1, 4097), (1, 12000), (2000, 9050), (4000, 8200), (5000, 5000 + 4096)])
def test_struct_range_large_equals_direct(tmp_path, lo, hi):
    db = _struct_db(str(tmp_path / "s.nhydb"))
    q = QueryEngine(db)
    for lane in (0, 1):
        for mode in ("n",):
            got = q.execute(Q.u.between(lo, hi), realm=1, lane=lane, return_mode=mode)
            assert got["count"] == len(got["refs"])
            assert got["refs"] == _direct(q, lo, hi, lane, mode), (lo, hi, lane)
        cells = q.execute(Q.u.between(lo, hi), realm=1, lane=lane, return_mode="count")["count"]
        direct_cells = 0
        a = lo
        while a <= hi:
            b = min(hi, a + 4095)
            direct_cells += q.execute(Q.u.between(a, b), realm=1, lane=lane, return_mode="count")["count"]
            a = b + 1
        assert cells == direct_cells
    db.close()


def test_struct_range_huge_span_visits_only_stored_u(tmp_path):
    db = _struct_db(str(tmp_path / "s.nhydb"))
    q = QueryEngine(db)
    r = q.execute(Q.u.between(1, 10**40), realm=1, return_mode="n")
    assert r["refs"] == _direct(q, 1, 12000, 0, "n")
    db.close()


# ---------------------------------------------------------------------------
# P004: range/select over many segments and gaps
# ---------------------------------------------------------------------------

def test_range_select_equal_per_u_reads(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "r.nhydb"))
    _base(db)
    written = set()
    for u in range(1, 400, 3):                      # one segment per record
        db.put(1, u, _row(u), shape_id=1)
        written.add(u)
    db.put_dense_batch(1, 100, [_row(u) for u in range(100, 160)], shape_id=1)   # overlaps singles
    written.update(range(100, 160))
    for u in (2**40, 2**40 + 1, 2**70):
        db.put(1, u, _row(5), shape_id=1)
        written.add(u)
    db.commit()
    db.delete_record(1, 4)
    db.delete_record(1, 2**40)
    db.put_cell(1, 7, 2, 99)                        # overlay on a dense record
    db.put_cell(1, 8, 2, 1)                         # pure sparse u (not part of range)
    db.commit()
    for lo, hi in [(1, 2**71), (50, 130), (2**39, 2**41), (2, 2), (5, 6)]:
        exp_get = [db.get(1, u) for u in sorted(written) if lo <= u <= hi and db.get(1, u) is not None]
        assert _strip(list(db.range(1, lo, hi))) == _strip(exp_get), (lo, hi)
        sel = list(db.select(1, lo, hi, ["x", "id"]))
        assert [r["_u"] for r in sel] == [g["_u"] for g in exp_get]
        assert _strip([(r["x"], r["id"]) for r in sel]) == _strip([(g["x"], g["id"]) for g in exp_get])
    db.close()


# ---------------------------------------------------------------------------
# P021 / P022
# ---------------------------------------------------------------------------

def test_routing_impact_counts_unique_sparse_groups(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "p.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", SHAPE[:1])
    db.create_classic_layout(1, 1, "main", shape_id=1, priority=5)
    db.put_points(1, [(u, 1, u, "varuint") for u in range(1, 301)], commit=True)
    db.put_points(1, [(u, 2, u, "varuint") for u in range(100, 201)], commit=True)   # same u, second block
    db.put_points(1, [(u, 1, None, "varuint") for u in range(250, 260)], commit=True)
    db.delete_cell(1, 280, 1)
    db.create_classic_layout(2, 1, "shadow", shape_id=1, u_start=1, u_end=10**9, priority=0)
    db.commit()
    imp = db.plan_layout_evolution(2, priority=9)["occupied_impact"]
    assert imp["sparse_u_groups_at_risk"] == 299        # u=280 has no live cell
    assert imp["sparse_cells_at_risk"] == 300 + 101 - 1
    assert imp["sample_u"] == list(range(1, 13))
    db.close()


def test_deep_alternating_nesting_equals_set_semantics(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "n.nhydb"))
    _base(db)
    db.put_dense_batch(1, 1, [_row(u) for u in range(1, 301)], shape_id=1)
    db.commit()
    db.create_index(1, 1, "ix", "x")
    db.build_indexes(commit=True)
    xs = {u: _row(u)[1] for u in range(1, 301)}
    e, s = Q.field("x") == 0, {u for u in xs if xs[u] == 0}
    for k in range(150):
        val = k % 11 - 3
        hit = {u for u in xs if xs[u] == val} if k % 3 else {u for u in xs if xs[u] >= val}
        leaf = (Q.field("x") == val) if k % 3 else (Q.field("x") >= val)
        if k % 2:
            e, s = e & leaf, s & hit
        else:
            e, s = e | leaf, s | hit
    q = QueryEngine(db)
    r = q.execute(e, realm=1, return_mode="n", explain=True)
    assert r["count"] == len(s)
    assert r["plan"]["node"] in ("AND", "OR")
    db.close()


def test_lazy_plan_is_still_reported_with_explain(tmp_path):
    """The plan is computed lazily on the index fast paths; with explain=True
    it is the same plan as `explain()` returns, in every fast path."""
    db = nhydb.NHyDB.create(str(tmp_path / "e.nhydb"))
    _base(db)
    db.put_dense_batch(1, 1, [_row(u) for u in range(1, 101)], shape_id=1)
    db.commit()
    db.create_index(1, 1, "ix", "x")
    db.create_index(2, 1, "is", "s")
    db.build_indexes(commit=True)
    q = QueryEngine(db)
    cases = [
        (Q.field("x") == 2, "count", "POSTING_COUNT"),
        (Q.field("x") == 2, "n", "CLASSIC_U"),
        ((Q.field("x") == 2) & (Q.field("s") == "a"), "count", "CLASSIC_U"),
        (Q.all(), "count", None),
        (Q.all(), "n", None),
        (Q.field("f") == 1.5, "count", None),
    ]
    for expr, mode, fp in cases:
        r = q.execute(expr, realm=1, return_mode=mode, explain=True)
        assert r.get("fastpath") == fp, (expr, mode, r.get("fastpath"))
        assert _strip(r["plan"]) == _strip(q.explain(expr, realm=1, return_mode=mode)["plan"])
        assert "plan" not in q.execute(expr, realm=1, return_mode=mode)
    db.close()
