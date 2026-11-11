"""Differential test: queries with a fresh index return exactly what the scan
returns (T005; findings F009, F010, F025, F026, F045, F046).

The scan (general evaluator) is the reference semantics. Every scenario is
built twice with identical data: once with fresh VALUE/TOKEN/TRIGRAM/FULLTEXT
indexes, once without any index. Each predicate is executed in every return
mode on both databases; results (without ``fastpath``/``plan``) and errors
must be identical.
"""
import math

import pytest

import nhydb
from nhydb.query import Q

NAN = float("nan")

SHAPE = [
    {"v": 1, "name": "id", "type": "varuint"},
    {"v": 2, "name": "x", "type": "i64"},
    {"v": 3, "name": "st", "type": "u8", "enum": {"open": 0, "done": 1}},
    {"v": 4, "name": "f", "type": "f64"},
    {"v": 5, "name": "note", "type": "utf8"},
]

NOTES = ["quick brown fox", "lazy dog", "Quick quick FOX jumps", "straße ÄÖü", "", "fox"]


def _rows(start, n, x_of=lambda u: u % 5):
    f_vals = [1.0, NAN, 2.5, -0.0, 0.0, 3.0, math.inf]
    return [[u, x_of(u), ("open", "done")[u % 2], f_vals[u % len(f_vals)], NOTES[u % len(NOTES)]]
            for u in range(start, start + n)]


def _schema(db, realm=1, layout=True):
    db.create_realm(realm, "r%d" % realm)
    db.create_shape(realm, realm, "s", SHAPE)
    if layout:
        db.create_classic_layout(realm, realm, "main", shape_id=realm)
    db.define_extension(realm, 10, "utf8", name="ext")
    db.define_extension(realm, 11, "f64", name="ef")


def _indexes(db, realm=1, base=1):
    db.create_index(base + 0, realm, "ix", "x")
    db.create_index(base + 1, realm, "ist", "st")
    db.create_index(base + 2, realm, "if", "f")
    db.create_index(base + 3, realm, "iext", "ext")
    db.create_index(base + 4, realm, "ief", "ef")
    db.create_index(base + 5, realm, "iv12", 12)
    db.create_token_index(base + 6, realm, "itok", "note")
    db.create_trigram_index(base + 7, realm, "itri", "note")
    db.create_fulltext_index(base + 8, realm, "ift", "note")


# ---------------------------------------------------------------------------
# Data matrix
# ---------------------------------------------------------------------------

def sc_values(db):
    """None, NaN, -0.0/0.0/inf, enum, extension None/NaN, type mix at v12."""
    _schema(db)
    db.put_dense_batch(1, 1, _rows(1, 14), shape_id=1)
    db.put_extensions(1, 2, {"ext": "hello", "ef": 1.0})
    db.put_extensions(1, 4, {"ext": "World", "ef": NAN})
    db.put_extensions(1, 6, {"ext": "hello", "ef": 2})
    db.put_cell(1, 3, 10, None)
    db.put_cell(1, 5, 11, None)
    db.put_cell(1, 1, 12, 5, type="i64")
    db.put_cell(1, 2, 12, "a", type="utf8")
    db.put_cell(1, 3, 12, None, type="utf8")
    db.put_cell(1, 4, 12, True, type="bool")
    db.put_cell(1, 5, 12, 2.5, type="f64")
    db.put_cell(1, 6, 12, NAN, type="f64")
    db.put_cell(1, 7, 12, b"ab", type="bytes")
    db.commit()


def sc_overlay_delete(db):
    """Sparse overlays/tombstones on BASE fields, delete_area/delete_record,
    extensions surviving a delete of the BASE fields (F026, T-02), re-put
    after delete, overlapping dense rewrites, pure sparse records."""
    _schema(db)
    db.put_dense_batch(1, 1, _rows(1, 12), shape_id=1)
    db.put_extensions(1, 2, {"ext": "a"})
    db.put_extensions(1, 3, {"ext": "b"})
    db.commit()
    db.put_cell(1, 4, 2, 99)          # overlay on BASE x
    db.delete_cell(1, 5, 2)           # tombstone on BASE x
    db.delete_area(1, 2, 2, 1, 5)     # BASE only: extension 'a' stays visible (F026, T-02)
    db.delete_record(1, 3)
    db.put(1, 3, [3, 4, "done", 1.5, "fox again"], shape_id=1)
    db.delete_record(1, 6)
    db.put_cell(1, 6, 2, 77)          # sparse BASE value after delete_record
    db.put_dense_batch(1, 8, [[u, 3, "open", 9.0, "lazy fox"] for u in range(8, 11)], shape_id=1)
    db.put_cell(1, 40, 2, 4)          # pure sparse record
    db.put_cell(1, 41, 10, "a")       # pure sparse extension
    db.commit()


def sc_lanes(db):
    _schema(db)
    db.put_dense_batch(1, 1, _rows(1, 6), shape_id=1)
    db.put_dense_batch(1, 1, _rows(1, 6, x_of=lambda u: u % 3 + 10), shape_id=1, lane="NR")
    db.put_extensions(1, 2, {"ext": "hello"}, lane="NR")
    db.put_cell(1, 9, 2, 11, lane="NR")
    db.commit()


def sc_mixed_layouts(db):
    """CLASSIC + COLUMN in one realm; a second realm with two CLASSIC layouts
    whose shapes put different fields at v=2 (F010)."""
    db.create_realm(1, "r1")
    db.create_shape(1, 1, "s", SHAPE)
    db.create_layout(1, 1, "L", "CLASSIC", shape_id=1, u_start=1, u_end=99)
    db.create_layout(2, 1, "C", "COLUMN", u_start=100, u_end=200,
                     params={"fields": [{"u": 100, "name": "x", "type": "i64"}, {"u": 101, "name": "y", "type": "i64"}]})
    db.define_extension(1, 10, "utf8", name="ext")
    db.define_extension(1, 11, "f64", name="ef")
    db.put_dense_batch(1, 1, _rows(1, 6), shape_id=1)
    db.put_object(2, 2, {"x": 7, "y": 2})     # COLUMN cells (100, 2) = 7, (101, 2) = 2
    db.put_object(2, 3, {"x": 2, "y": 1})
    db.put_cell(1, 250, 2, 2, type="i64")     # u without layout
    db.commit()


def sc_routing(db):
    db.create_realm(1, "r1")
    db.create_shape(1, 1, "s", SHAPE)
    db.create_shape(2, 1, "t", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "y", "type": "i64"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.create_layout(2, 1, "special", "CLASSIC", shape_id=2, u_start=100, u_end=199, priority=5)
    db.define_extension(1, 10, "utf8", name="ext")
    db.define_extension(1, 11, "f64", name="ef")
    db.put_dense_batch(1, 1, _rows(1, 6), shape_id=1)
    db.put_dense_batch(1, 100, [[100, 2], [101, 3]], shape_id=2)
    db.commit()


def sc_big_u(db):
    """Sparse u beyond 2^32 and 2^64 next to small dense records (F009)."""
    _schema(db)
    db.put_dense_batch(1, 1, _rows(1, 4), shape_id=1)
    db.put_cell(1, 2**33 + 7, 2, 2)
    db.put_cell(1, 2**64 + 5, 2, 2)
    db.put_cell(1, 2**70 + 1, 10, "hello")
    db.put_cell(1, 2**33 + 9, 5, "quick fox")
    db.commit()


def sc_plain_classic(db):
    """0.28 fast paths (P007, P014): one plain CLASSIC layout without
    extensions or overlays, several segments, u beyond 2^32; ALL/NOT/COUNT
    and ORDER BY use the segment metadata, multi-key ranges the u64 union."""
    db.create_realm(1, "r1")
    db.create_shape(1, 1, "s", SHAPE)
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put_dense_batch(1, 1, _rows(1, 30), shape_id=1)
    db.put_dense_batch(1, 100, _rows(100, 20, x_of=lambda u: u % 7), shape_id=1)
    db.put(1, 2**33 + 1, _rows(3, 1)[0], shape_id=1)
    db.commit()


def _indexes_plain(db, realm=1, base=1):
    db.create_index(base + 0, realm, "ix", "x")
    db.create_index(base + 1, realm, "ist", "st")
    db.create_index(base + 2, realm, "if", "f")
    db.create_token_index(base + 6, realm, "itok", "note")
    db.create_trigram_index(base + 7, realm, "itri", "note")
    db.create_fulltext_index(base + 8, realm, "ift", "note")


SCENARIOS = {
    "plain_classic": (sc_plain_classic, (0,), _indexes_plain),
    "values": (sc_values, (0,)),
    "overlay_delete": (sc_overlay_delete, (0,)),
    "lanes": (sc_lanes, (0, 1)),
    "mixed_layouts": (sc_mixed_layouts, (0,)),
    "routing": (sc_routing, (0,)),
    "big_u": (sc_big_u, (0,)),
}


# ---------------------------------------------------------------------------
# Predicate matrix
# ---------------------------------------------------------------------------

def predicates():
    F = Q.field
    return [
        F("x") == 1, F("x") == True, F("x") == 1.0, F("x") == 2, F("x") == 99, F("x") == 77, F("x") == None,  # noqa: E711,E712
        F("x").between(0, 2), F("x").between(1.5, 3), F("x").between(0.5, "z"), F("x").between(None, 3),
        F("x").between(NAN, 3), F("x").between("a", "z"), F("x").between(3, 1),
        F("st") == "done", F("st") == 1,
        F("f") == NAN, F("f") == 0.0, F("f") == -0.0, F("f") == math.inf, F("f").between(0, 3), F("f").between(-1, math.inf),
        F("ext") == "hello", F("ext") == "a", F("ext") == None, F("ext").between("a", "i"),  # noqa: E711
        F("ef") == 1, F("ef") == NAN, F("ef") == None, F("ef").between(0, 2),  # noqa: E711
        F("v12") == 5, F("v12") == 1, F("v12") == "a", F("v12") == None, F("v12") == b"ab",  # noqa: E711
        F("v12").between(0, 10), F("v12").between("a", "b"), F("v12").between(b"a", b"b"),
        F("note").contains("fox"), F("note").icontains("QUICK"), F("note").contains("ß"),
        F("note").token("fox"), F("note").tokens_all("quick", "fox"), F("note").tokens_any("lazy", "jumps"),
        F("note").fulltext("quick fox"), F("note").fulltext("lazy zzz", mode="any"), F("note").phrase("brown fox"),
        (F("x") == 2) & (F("st") == "done"), (F("x").between(0, 3)) & (F("ext") == "hello"),
        (F("x") == 2) | (F("ext") == "a"), ~(F("x") == 2),
        # 0.28 fast paths: universe from segment metadata, STRUCT_RANGE > 4096,
        # multi-key posting unions
        Q.all(), ~Q.all(), Q.u.between(1, 5000), Q.u.between(2, 2**40), F("x").between(-10, 10),
        F("x").between(1, 6) & F("x").between(3, 9),
    ]


AGG = {"rows": {"op": "count", "field": "*"}, "s": {"op": "sum", "field": "x"}, "hi": {"op": "max", "field": "id"}}
SELECT = ["id", "x", "ext", "u", "record_N", "missing"]


def _strip(res):
    if isinstance(res, dict):
        return {k: _strip(v) for k, v in res.items() if k not in ("fastpath", "plan")}
    if isinstance(res, list):
        return [_strip(x) for x in res]
    if isinstance(res, float) and math.isnan(res):
        return "NaN"
    return res


def _run(db, expr, lane, **kw):
    try:
        return ("ok", _strip(db.query(expr, realm=1, lane=lane, **kw)))
    except Exception as e:  # compared by class and message
        return ("err", type(e).__name__, str(e))


def _run_engine(db, expr, lane, **kw):
    try:
        return ("ok", _strip(nhydb.query.QueryEngine(db).execute(expr, realm=1, lane=lane, **kw)))
    except Exception as e:
        return ("err", type(e).__name__, str(e))


MODES = [
    ("count", {"return_mode": "count"}),
    ("n", {"return_mode": "n"}),
    ("record", {"return_mode": "record"}),
    ("cell", {"return_mode": "cell"}),
]
ENGINE_MODES = [
    ("fields", {"return_mode": "fields", "select": SELECT}),
    ("fields_ordered", {"return_mode": "fields", "select": SELECT, "order_by": ["-id"], "limit": 4}),
    ("fields_limit", {"return_mode": "fields", "select": ["x", "u"], "limit": 2, "offset": 1}),
    ("record_ordered", {"return_mode": "record", "order_by": ["-u"], "limit": 3}),
    ("aggregate", {"aggregate": AGG, "group_by": ["st"]}),
    ("aggregate_ext", {"aggregate": AGG, "group_by": ["ext", "f"]}),
    ("fields_ordered_x", {"return_mode": "fields", "select": ["x", "note", "u"], "order_by": ["x", "-note"], "limit": 5, "offset": 1}),
]


@pytest.fixture(scope="module")
def dbs(tmp_path_factory):
    out = {}
    root = tmp_path_factory.mktemp("index_vs_scan")
    for name, spec in SCENARIOS.items():
        setup, make_indexes = spec[0], (spec[2] if len(spec) > 2 else _indexes)
        pair = []
        for indexed in (True, False):
            db = nhydb.NHyDB.create(str(root / ("%s_%d.nhydb" % (name, indexed))))
            setup(db)
            if indexed:
                make_indexes(db)
                db.build_indexes(commit=True)
            pair.append(db)
        out[name] = pair
    yield out
    for pair in out.values():
        for db in pair:
            db.close()


@pytest.mark.parametrize("scenario", list(SCENARIOS))
def test_index_equals_scan(dbs, scenario):
    idx_db, scan_db = dbs[scenario]
    lanes = SCENARIOS[scenario][1]
    for lane in lanes:
        for expr in predicates():
            for label, kw in MODES:
                a, b = _run(idx_db, expr, lane, **kw), _run(scan_db, expr, lane, **kw)
                assert a == b, (scenario, lane, expr, label, a, b)
            for label, kw in ENGINE_MODES:
                a, b = _run_engine(idx_db, expr, lane, **kw), _run_engine(scan_db, expr, lane, **kw)
                assert a == b, (scenario, lane, expr, label, a, b)


def test_index_paths_are_used(dbs):
    """The comparison above is meaningful only if the index paths run."""
    idx_db, _ = dbs["values"]
    assert idx_db.query(Q.field("x") == 2, realm=1, return_mode="count")["fastpath"] == "POSTING_COUNT"
    assert idx_db.query(Q.field("x") == 2, realm=1, return_mode="n")["fastpath"] == "CLASSIC_U"
    plan = idx_db.query(Q.field("x") == 2, realm=1, return_mode="cell", explain=True)["plan"]
    assert plan["access"] == "VALUE_INDEX"
    # mixed realm: the index is not used for a field whose v means something else (F010)
    mixed, _ = dbs["mixed_layouts"]
    plan = mixed.query(Q.field("x") == 2, realm=1, return_mode="n", explain=True)["plan"]
    assert plan["access"] == "MATERIALIZED_SCAN"


def test_big_u_postings_and_find_u(dbs):
    """F009: postings keep sparse u beyond 2^32 and 2^64 exactly."""
    idx_db, scan_db = dbs["big_u"]
    assert idx_db.find_u(1, {"ix": 2}) == [2, 2**33 + 7, 2**64 + 5]
    assert idx_db.find_u(1, {"iext": "hello"}) == [2**70 + 1]
    a = idx_db.query(Q.field("x") == 2, realm=1, return_mode="n")
    b = scan_db.query(Q.field("x") == 2, realm=1, return_mode="n")
    assert a["refs"] == b["refs"] and len(a["refs"]) == 3
    assert idx_db.verify(deep=True)["ok"]


def test_nan_and_incomparable_bounds_via_find_u(dbs):
    """F025/F045 on the direct index API: NaN never equals, incomparable
    bounds give an empty result instead of TypeError."""
    idx_db, _ = dbs["values"]
    assert idx_db.find_u(1, {"if": NAN}) == []
    assert idx_db.find_u(1, {"ief": NAN}) == []
    assert idx_db.find_u(1, {"ix": (None, 3)}) == []
    assert idx_db.find_u(1, {"ix": ("a", "z")}) == []
    assert idx_db.find_u(1, {"iv12": (0, 10)}) == [1, 4, 5]   # 5, True, 2.5
    assert idx_db.find_u(1, {"iv12": ("a", "b")}) == [2]
    assert idx_db.find_u(1, {"iv12": (b"a", b"b")}) == [7]
    f = idx_db.find_u(1, {"if": (0, 3)})
    assert f == [u for u in range(1, 15) if _rows(u, 1)[0][3] == _rows(u, 1)[0][3] and 0 <= _rows(u, 1)[0][3] <= 3]


def test_nan_groups_are_separate(dbs):
    """F045: like Python, every NaN forms its own group (NaN != NaN)."""
    for db in dbs["values"]:
        r = nhydb.query.QueryEngine(db).execute(Q.all(), realm=1, aggregate={"n": {"op": "count", "field": "*"}}, group_by=["f"])
        nan_groups = [row for row in r["rows"] if isinstance(row["f"], float) and math.isnan(row["f"])]
        assert len(nan_groups) == 2 and all(row["n"] == 1 for row in nan_groups)
