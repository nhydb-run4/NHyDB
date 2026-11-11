"""Regression pool of round 4 (0.28.2): large result sets ("monster queries").

* A1: AND/OR/NOT trees of indexed FIELD predicates run in compact u space.
  Results, row shapes, ordering, aggregates and errors equal the general
  path (forced with ``& Q.all()``), also for huge realms and u (> 2**64).
"""
import random

import numpy as np
import pytest

import nhydb
from nhydb.query import Q, QueryEngine

STATUS = ["a", "b", "c", "d"]


def _rows(n, seed):
    r = random.Random(seed)
    return [[k, STATUS[r.randrange(4)], r.randrange(50), r.randrange(7)] for k in range(1, n + 1)]


def _db(p, realm, u0, n=400, seed=5):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(realm, "r")
    db.create_shape(1, realm, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "status", "type": "utf8"},
                                    {"v": 3, "name": "amount", "type": "varuint"}, {"v": 4, "name": "cat", "type": "varuint"}])
    db.create_classic_layout(1, realm, "c", shape_id=1)
    rows = _rows(n, seed)
    # two segments with a gap in u (the NOT universe has two ranges)
    db.put_dense_batch(realm, u0, rows[: n // 2], shape_id=1)
    db.put_dense_batch(realm, u0 + n, rows[n // 2:], shape_id=1)
    db.create_index(1, realm, "i_status", "status")
    db.create_index(2, realm, "i_amount", "amount")
    db.create_index(3, realm, "i_cat", "cat")
    db.build_indexes(commit=True)
    return db


def _leaf(r):
    k = r.randrange(5)
    if k == 3:                                           # no index: scan in u space
        lo = r.randrange(1, 400)
        return Q.field("id").between(lo, lo + r.randrange(150))
    if k == 4:                                           # VALUE index cannot answer contains: scan
        return Q.field("status").contains(r.choice(["a", "b", "q"]))
    if k == 0:
        return Q.field("status") == r.choice(STATUS + ["zz"])
    if k == 1:
        lo = r.randrange(50)
        return Q.field("amount").between(lo, lo + r.randrange(20))
    return Q.field("cat") == r.randrange(8)


def _tree(r, depth=0):
    if depth >= 3 or r.random() < 0.3:
        return _leaf(r)
    k = r.randrange(3)
    if k == 0:
        return ~_tree(r, depth + 1)
    a, b = _tree(r, depth + 1), _tree(r, depth + 1)
    return (a & b) if k == 1 else (a | b)


MODES = [
    dict(return_mode="count"),
    dict(return_mode="n"),
    dict(return_mode="n", limit=5, offset=3),
    dict(return_mode="fields", select=["id", "status", "u", "record_N"]),
    dict(return_mode="fields", select=["id", "amount"], order_by=[("amount", "desc"), ("id", "asc")], limit=7, offset=2),
    dict(return_mode="fields", select=["id"], order_by=[("record_N", "desc")], limit=4),
    dict(return_mode="record", limit=3),
    dict(aggregate={"n": ("count", "*"), "s": ("sum", "amount"), "m": ("max", "amount")}, group_by=["status"]),
]


@pytest.mark.parametrize("realm,u0", [(1, 1), (2**70 + 3, 2**70 + 1)])
def test_a1_bool_trees_in_u_space_equal_general_path(tmp_path, realm, u0):
    db = _db(tmp_path / "t.nhydb", realm, u0)
    q = QueryEngine(db)
    r = random.Random(11)
    checked = 0
    for _ in range(60):
        e = _tree(r)
        for m in MODES:
            fast = q.execute(e, realm=realm, **m)
            slow = q.execute(e & Q.all(), realm=realm, **m)
            assert "fastpath" not in slow
            if fast.get("fastpath") in ("CLASSIC_U", "POSTING_COUNT"):  # pre-existing FIELD/AND fast paths
                fast = {k: v for k, v in fast.items() if k != "fastpath"}
            assert fast == slow, (e, m)
            checked += 1
    assert checked == 60 * len(MODES)
    db.close()


def test_a1_not_and_or_cover_expected_sets(tmp_path):
    db = _db(tmp_path / "t.nhydb", 1, 1, n=200)
    q = QueryEngine(db)
    rows = {u: row for u, row in zip(list(range(1, 101)) + list(range(201, 301)), _rows(200, 5))}
    got = q.execute(~(Q.field("status") == "a") | (Q.field("cat") == 3), realm=1, return_mode="fields", select=["u"])
    want = sorted(u for u, row in rows.items() if row[1] != "a" or row[3] == 3)
    assert [x["u"] for x in got["rows"]] == want and got["count"] == len(want)
    db.close()


# --------------------------------------------------------------------------
# A2: ranked FULLTEXT search streamed in u order with a top-k window

def _ft_db(p, n=140_000, seed=9):
    """n documents (more than two scoring chunks of 65 536); every 5th
    document is a copy of document 1 (equal scores, ties broken by u)."""
    r = random.Random(seed)
    vocab = [f"w{k}" for k in range(400)]
    docs = []
    for u in range(1, n + 1):
        docs.append(docs[0] if u % 5 == 0 and docs else " ".join(r.choice(vocab) for _ in range(r.randint(1, 12))))
    for u in range(7, n, 4999):  # a rare term next to frequent ones
        docs[u - 1] += " seltenx " + r.choice(vocab[:3])
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"},
                                {"v": 3, "name": "grp", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_fulltext_index(5, 1, "ft", 2)
    db.create_index(6, 1, "i_grp", "grp")
    db.put_numpy_batch(1, 1, {"id": np.arange(1, n + 1, dtype=np.uint64), "body": np.array(docs, dtype=object),
                              "grp": np.arange(1, n + 1, dtype=np.uint64) % 3}, shape_id=1)
    db.build_indexes(commit=True)
    return db, docs


@pytest.fixture(scope="module")
def ftdb(tmp_path_factory):
    db, docs = _ft_db(tmp_path_factory.mktemp("r4ft") / "ft.nhydb")
    yield db, docs
    db.close()


@pytest.mark.parametrize("expr", [
    lambda: Q.field("body").fulltext("w1"),
    lambda: Q.field("body").fulltext("w1 w2", mode="any"),
    lambda: Q.field("body").fulltext("w3") & (Q.field("grp") == 1),            # indexed AND filter
    lambda: Q.field("body").fulltext("w4") & (Q.field("id").between(5, 90_000)),  # filter via scan
    lambda: Q.field("body").fulltext("nothere") & (Q.field("grp") == 1),
])
def test_a2_ranked_windows_equal_slices_of_the_full_ranking(ftdb, expr):
    db, _docs = ftdb
    q = QueryEngine(db)
    full = q.search(expr(), realm=1, select=["id"], limit=None)
    rows = full["rows"]
    assert full["count"] == len(rows)
    for lim, off in [(0, 0), (1, 0), (10, 0), (10, 7), (1000, 3), (len(rows) + 5, 0), (3, len(rows) + 1)]:
        part = q.search(expr(), realm=1, select=["id"], limit=lim, offset=off)
        assert part["rows"] == rows[off:off + lim], (lim, off)
        assert part["count"] == full["count"]
    # ties (copies of document 1) come in ascending u
    rel = [r["relevance"] for r in rows]
    assert rel == sorted(rel, reverse=True)
    for a, b in zip(rows, rows[1:]):
        if a["relevance"] == b["relevance"]:
            assert a["id"] < b["id"]


def test_a2_top10_matches_full_search(ftdb):
    db, _docs = ftdb
    full = QueryEngine(db).search(Q.field("body").fulltext("w7"), realm=1, select=["id"], limit=None)["rows"]
    got = QueryEngine(db).search(Q.field("body").fulltext("w7"), realm=1, select=["id"], limit=10)["rows"]
    assert got == full[:10]


# --------------------------------------------------------------------------
# A2: ORDER BY … LIMIT with a top-k window (CLASSIC fields path)

def test_a2_order_limit_windows_equal_full_sort(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "o.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "s", "type": "utf8"},
                                {"v": 3, "name": "x", "type": "f64"}, {"v": 4, "name": "k", "type": "varint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    r = random.Random(3)
    n = 9000
    db.put_numpy_batch(1, 1, {"id": np.arange(1, n + 1, dtype=np.uint64),
                              "s": np.array([r.choice(["a", "b", "ab", "", "Z"]) for _ in range(n)], dtype=object),
                              "x": np.array([r.choice([0.5, -1.0, 2.25]) for _ in range(n)]),
                              "k": np.array([r.randrange(-5, 5) for _ in range(n)], dtype=np.int64)}, shape_id=1)
    db.create_index(1, 1, "i_k", "k")
    db.build_indexes(commit=True)
    q = QueryEngine(db)
    where = [Q.field("k").between(-3, 4), Q.all(), (Q.field("k") == 1) | (Q.field("k") == -2)]
    orders = [[("k", "desc")], [("s", "asc"), ("k", "desc")], [("x", "desc")], [("missing", "asc"), ("k", "asc")],
              [("s", "desc"), ("x", "asc"), ("id", "desc")], [("record_N", "desc")]]
    for w in where:
        for ob in orders:
            full = q.execute(w, realm=1, return_mode="fields", select=["id", "s", "x", "k"], order_by=ob)["rows"]
            for lim, off in [(1, 0), (10, 0), (25, 13), (0, 0), (len(full) - 1, 0), (5, len(full))]:
                part = q.execute(w, realm=1, return_mode="fields", select=["id", "s", "x", "k"], order_by=ob, limit=lim, offset=off)
                assert part["rows"] == full[off:off + lim], (w, ob, lim, off)
                assert part["count"] == len(full)
    db.close()


# --------------------------------------------------------------------------
# A3: ORDER BY … LIMIT and aggregates from a covering VALUE index

def _cover_db(p, gap_shape=False):
    """3000 records: k (varint, 40 values), s (utf8), amount; index on k and
    s. With `gap_shape` 200 records use a shape without k and s (the
    indexes then do not cover every record)."""
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": "varint"},
                                {"v": 3, "name": "s", "type": "utf8"}, {"v": 4, "name": "amount", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    r = random.Random(17)
    n = 3000
    rows = [[u, r.randrange(-20, 20), r.choice(["x", "y", "xy", "Ä", ""]), r.randrange(100)] for u in range(1, n + 1)]
    db.put_dense_batch(1, 1, rows, shape_id=1)
    if gap_shape:
        db.create_shape(2, 1, "t", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 4, "name": "amount", "type": "varuint"}])
        db.put_dense_batch(1, n + 1, [[u, 1] for u in range(n + 1, n + 201)], shape_id=2)
    db.create_index(1, 1, "i_k", "k")
    db.create_index(2, 1, "i_s", "s")
    db.create_index(3, 1, "i_a", "amount")
    db.build_indexes(commit=True)
    return db


AGGS = [
    ({"n": ("count", "*"), "c": ("count", "k"), "s": ("sum", "k"), "a": ("avg", "k"), "lo": ("min", "k"), "hi": ("max", "k")}, []),
    ({"n": ("count", "*"), "lo": ("min", "k"), "s": ("sum", "k")}, ["k"]),
    ({"n": ("count", "*"), "lo": ("min", "s"), "hi": ("max", "s")}, []),
    ({"n": ("count", "s")}, ["s"]),
    ({"s": ("sum", "s")}, []),                         # error (not numeric) as in the general path
    ({"n": ("count", "*"), "a": ("avg", "amount")}, ["k"]),  # two fields: regular path
]


@pytest.mark.parametrize("gap", [False, True])
def test_a3_index_order_and_aggregates_equal_general_path(tmp_path, gap):
    db = _cover_db(tmp_path / "c.nhydb", gap)
    q = QueryEngine(db)
    wheres = [Q.all(), Q.field("amount").between(10, 60), (Q.field("amount") == 3) | ~(Q.field("amount").between(5, 99)),
              Q.field("amount") == 1000]

    def run(fn):
        try:
            return fn()
        except Exception as e:  # noqa: BLE001 – compare error class and text
            return (type(e).__name__, str(e))

    for w in wheres:
        for agg, grp in AGGS:
            a = run(lambda: q.execute(w, realm=1, aggregate=agg, group_by=grp))
            b = run(lambda: q.execute(w & Q.all(), realm=1, aggregate=agg, group_by=grp))
            if isinstance(a, dict):
                a.pop("fastpath", None)
            assert a == b, (w, agg, grp)
        for ob in ([("k", "desc")], [("k", "asc")], [("s", "desc")], [("s", "asc")]):
            for lim, off in [(1, 0), (10, 0), (40, 25), (0, 3), (500, 0)]:
                kw = dict(realm=1, return_mode="fields", select=["id", "k", "s"], order_by=ob, limit=lim, offset=off)
                a = q.execute(w, **kw)
                b = q.execute(w & Q.all(), **kw)
                a.pop("fastpath", None)
                assert a == b, (w, ob, lim, off)
    db.close()


@pytest.mark.parametrize("realm,u0", [(1, 1), (2**70 + 3, 2**70 + 1)])
def test_a5_all_windows_from_ranges_equal_general_path(tmp_path, realm, u0):
    """ALL keeps the metadata universe as ranges; LIMIT windows across the
    gap between two segments equal the general path."""
    db = _db(tmp_path / "w.nhydb", realm, u0, n=300)
    q = QueryEngine(db)
    for mode in ("n", "fields", "record"):
        for lim, off in [(None, 0), (5, 0), (10, 145), (3, 299), (7, 300), (0, 0), (1000, 1)]:
            kw = dict(realm=realm, return_mode=mode, limit=lim, offset=off)
            if mode == "fields":
                kw["select"] = ["id", "u"]
            assert q.execute(Q.all(), **kw) == q.execute(Q.all() & Q.all(), **kw), (mode, lim, off)
    db.close()


@pytest.mark.parametrize("text,opt", [("w1 seltenx", {}), ("seltenx w2", {}), ("seltenx w0", {"phrase": True}),
                                      ("w2 seltenx", {"mode": "any"})])
def test_a4_conjunction_with_rare_term_equals_full_postings(ftdb, tmp_path, text, opt):
    """Large postings decoded only for the documents of the rarest term give
    the same candidates and scores as fully decoded (cached) postings."""
    db, _docs = ftdb
    p = db.path

    def expr():
        return Q.field("body").phrase(text) if opt.get("phrase") else Q.field("body").fulltext(text, **{k: v for k, v in opt.items() if k != "phrase"})

    with nhydb.NHyDB(p, readonly=True) as fresh, nhydb.NHyDB(p, readonly=True) as primed:
        for t in text.split():  # full postings in the cache of `primed`
            QueryEngine(primed).search(Q.field("body").fulltext(t), realm=1, limit=1)
        for h in (fresh, primed):
            assert QueryEngine(h).execute(expr(), realm=1, return_mode="n")["refs"]
        a = QueryEngine(fresh).search(expr(), realm=1, select=["id"], limit=None)
        b = QueryEngine(primed).search(expr(), realm=1, select=["id"], limit=None)
        assert a == b and a["count"] > 0
        assert QueryEngine(fresh).execute(expr(), realm=1, return_mode="n") == QueryEngine(primed).execute(expr(), realm=1, return_mode="n")


@pytest.mark.parametrize("u0", [1, 2**33, 2**70 + 1])  # posting widths 4, 8 and big-u
def test_a4_and_with_small_set_probes_large_postings(tmp_path, u0):
    db = nhydb.NHyDB.create(str(tmp_path / "p.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "big", "type": "utf8"}, {"v": 2, "name": "rare", "type": "varuint"},
                                {"v": 3, "name": "n", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    r = random.Random(23)
    n = 20000
    rows = [[("b" if r.random() < 0.9 else "c"), (r.randrange(5) if r.random() < 0.01 else 99), k % 500] for k in range(n)]
    db.put_dense_batch(1, u0, rows, shape_id=1)
    for i, f in enumerate(["big", "rare", "n"], 1):
        db.create_index(i, 1, f"i_{f}", f)
    db.build_indexes(commit=True)
    q = QueryEngine(db)
    exprs = [(Q.field("rare") == 2) & (Q.field("big") == "b"), (Q.field("big") == "b") & (Q.field("rare").between(0, 3)),
             (Q.field("rare") == 1) & (Q.field("n").between(10, 400)) & (Q.field("big") == "b"),
             (Q.field("rare") == 4) & (Q.field("big") == "zz"), ((Q.field("rare") == 0) | (Q.field("rare") == 3)) & (Q.field("big") == "c")]
    for e in exprs:
        for m in (dict(return_mode="n"), dict(return_mode="count"), dict(return_mode="fields", select=["big", "rare", "u"])):
            a = q.execute(e, realm=1, **m)
            b = q.execute(e & Q.all(), realm=1, **m)
            a.pop("fastpath", None)
            assert a == b, (e, m)
    db.close()


# --------------------------------------------------------------------------
# A6: optional bound of the estimated result rows, hints in explain

def test_a6_max_result_rows_is_opt_in_and_rejects_clearly(tmp_path, monkeypatch):
    monkeypatch.delenv("NHYDB_CONFIG", raising=False)
    db = _db(tmp_path / "t.nhydb", 1, 1, n=400)
    db.create_fulltext_index(9, 1, "ft", 2)
    db.build_indexes(commit=True)
    q = QueryEngine(db)
    big = Q.field("amount").between(0, 49)          # all 400 records
    # default: no bound, no hint for small estimates
    assert q.execute(big, realm=1, return_mode="n")["count"] == 400
    assert "hints" not in q.explain(big, realm=1)
    db.close()
    (tmp_path / "q.toml").write_text("[query]\nmax_result_rows = 50\n")
    monkeypatch.setenv("NHYDB_CONFIG", str(tmp_path / "q.toml"))
    db = nhydb.NHyDB(str(tmp_path / "t.nhydb"))
    q = QueryEngine(db)
    with pytest.raises(nhydb.QueryLimitError, match=r"about 400 result rows estimated, more than \[query\] max_result_rows = 50"):
        q.execute(big, realm=1, return_mode="fields", select=["id"])
    with pytest.raises(nhydb.QueryLimitError):
        db.sql("SELECT id FROM c WHERE amount BETWEEN 0 AND 49")
    assert issubclass(nhydb.QueryLimitError, nhydb.QueryError)
    # LIMIT below the bound, COUNT and aggregates run
    assert len(q.execute(big, realm=1, return_mode="fields", select=["id"], limit=50)["rows"]) == 50
    assert q.execute(big, realm=1, return_mode="count")["count"] == 400
    assert q.execute(big, realm=1, aggregate={"n": ("count", "*")})["rows"] == [{"n": 400}]
    assert db.sql("SELECT id FROM c WHERE amount BETWEEN 0 AND 49 LIMIT 5")["returned"] == 5
    assert any("max_result_rows = 50" in h for h in q.explain(big, realm=1)["hints"])
    # ranked search: bounded by its candidates
    with pytest.raises(nhydb.QueryLimitError):
        q.search(Q.field("status").fulltext("a"), realm=1, limit=None)
    assert len(q.search(Q.field("status").fulltext("a"), realm=1, limit=10)["rows"]) == 10
    db.close()


# --------------------------------------------------------------------------
# B: streaming rows (sql_iter), sql_numpy from streamed rows

SQLS = [
    "SELECT id, status FROM c",
    "SELECT * FROM c WHERE amount BETWEEN 3 AND 40",
    "SELECT id, amount AS a, id AS again FROM c WHERE status = 'b' OR cat = 2 LIMIT 37 OFFSET 5",
    "SELECT id FROM c WHERE NOT status = 'a' ORDER BY amount DESC, id LIMIT 15",
    "SELECT id, cat FROM c ORDER BY cat",
    "SELECT id FROM c LIMIT 7 OFFSET 140",            # window across the gap between two segments
    "SELECT id, status FROM c WHERE id BETWEEN 30 AND 260",   # scan (general path)
    "SELECT COUNT(*) FROM c WHERE status = 'b'",
    "SELECT status, COUNT(*) AS n, SUM(amount) AS s FROM c GROUP BY status",
    "SELECT id FROM c WHERE status = 'zz'",
]


def _np_rule(vals):
    """dtype rule of sql_numpy (vals_to_out)."""
    if vals and all(isinstance(v, bool) for v in vals):
        return "bool"
    if all(type(v) is int and -2**63 <= v < 2**63 for v in vals):
        return "int64"
    if all(type(v) is float or (type(v) is int and abs(v) <= 2**53) for v in vals):
        return "float64"
    return "object"


@pytest.mark.parametrize("realm,u0", [(1, 1), (2**70 + 3, 2**70 + 1)])
def test_b_sql_iter_and_sql_numpy_equal_sql(tmp_path, realm, u0):
    db = _db(tmp_path / "b.nhydb", realm, u0, n=300)
    for q in SQLS:
        want = db.sql(q)
        rows = want["rows"]
        assert list(db.sql_iter(q)) == rows, q
        arrs = db.sql_numpy(q)
        assert list(arrs) == want["columns"], q
        for c in want["columns"]:
            vals = [r.get(c) for r in rows]
            assert arrs[c].dtype.name == _np_rule(vals), (q, c)
            got = arrs[c].tolist()
            assert got == vals or [None if v is None else v for v in got] == vals, (q, c)
    db.close()


def test_b_sql_iter_is_lazy_and_streams(tmp_path):
    db = _db(tmp_path / "l.nhydb", 1, 1, n=3000)
    it = db.sql_iter("SELECT id FROM nowhere")           # error only on the first step
    with pytest.raises(nhydb.SQLSyntaxError, match="'nowhere' not found"):
        next(it)
    it = db.sql_iter("SELECT id, status FROM c WHERE amount BETWEEN 0 AND 49")
    first = next(it)
    assert first["id"] == 1
    # a write between two steps is allowed; the iterator continues
    db.put_dense_batch(1, 10_000, [[1, "a", 1, 1]], shape_id=1)
    db.commit()
    rest = list(it)
    assert len(rest) == 2999
    db.close()


def test_b_sql_iter_ignores_max_result_rows_and_sql_numpy_honours_it(tmp_path, monkeypatch):
    monkeypatch.delenv("NHYDB_CONFIG", raising=False)
    db = _db(tmp_path / "m.nhydb", 1, 1, n=400)
    db.close()
    (tmp_path / "q.toml").write_text("[query]\nmax_result_rows = 50\n")
    monkeypatch.setenv("NHYDB_CONFIG", str(tmp_path / "q.toml"))
    db = nhydb.NHyDB(str(tmp_path / "m.nhydb"))
    q = "SELECT id FROM c WHERE amount BETWEEN 0 AND 49"
    assert len(list(db.sql_iter(q))) == 400               # streaming: no bound
    with pytest.raises(nhydb.QueryLimitError):
        db.sql_numpy(q)
    with pytest.raises(nhydb.QueryLimitError):
        db.sql(q)
    db.close()


def test_b_scan_falls_back_when_a_shape_names_the_field_differently(tmp_path):
    db = _db(tmp_path / "s.nhydb", 1, 1, n=200)
    db.create_shape(2, 1, "t", [{"v": 1, "name": "ident", "type": "varuint"}, {"v": 2, "name": "status", "type": "utf8"},
                                {"v": 3, "name": "amount", "type": "varuint"}, {"v": 4, "name": "cat", "type": "varuint"}])
    db.put_dense_batch(1, 1000, [[k, "a", 1, 1] for k in range(1, 51)], shape_id=2)
    db.commit()
    q = QueryEngine(db)
    for e in [Q.field("id").between(1, 1000), Q.field("ident").between(1, 30), (Q.field("id") == 5) | (Q.field("ident") == 5)]:
        for m in (dict(return_mode="n"), dict(return_mode="fields", select=["id", "ident", "u"])):
            a = q.execute(e, realm=1, **m)
            b = q.execute(e & Q.all(), realm=1, **m)
            a.pop("fastpath", None)
            assert a == b, (e, m)
    db.close()


# --------------------------------------------------------------------------
# C: result files, background jobs, external sort

def _cfg(tmp_path, monkeypatch, extra=""):
    """Test configuration via NHYDB_CONFIG (takes precedence over a file next
    to the database, also in the QA runs with a global NHYDB_CONFIG)."""
    rd = tmp_path / "results"
    cfg = tmp_path / "r4_config.toml"
    cfg.write_text(f'[results]\ntemp_dir = "{rd.as_posix()}"\n{extra}')
    monkeypatch.setenv("NHYDB_CONFIG", str(cfg))
    return rd


def _files(rd):
    return sorted(p.name for p in rd.iterdir()) if rd.exists() else []


def test_c_result_file_equals_sql_and_its_lifecycle(tmp_path, monkeypatch):
    rd = _cfg(tmp_path, monkeypatch)
    db = _db(tmp_path / "c.nhydb", 1, 1, n=3000)
    assert db.result_dir == rd
    for q in SQLS:
        want = db.sql(q)
        with db.sql_to_file(q) as rf:
            assert rf.managed and not rf.pinned and rf.path.parent == rd
            assert rf.columns == want["columns"] and rf.rows == len(want["rows"]) == len(rf)
            assert list(rf) == want["rows"] == rf.fetchall(), q
            got, ref = rf.to_numpy(), db.sql_numpy(q)
            assert list(got) == list(ref) and all(got[c].tolist() == ref[c].tolist() and got[c].dtype == ref[c].dtype for c in got)
        assert _files(rd) == []                     # unpinned managed file deleted on close
    rf = db.sql_to_file(SQLS[1])
    rf.pin()
    p = rf.path
    assert rf.pinned and p.name.endswith(".pinned.nhyc")
    rf.close()
    assert p.exists()                               # pinned: stays
    again = nhydb.ResultFile(p)
    assert not again.managed and list(again) == db.sql(SQLS[1])["rows"]
    again.delete()
    assert not p.exists()
    # unpin makes it temporary again
    rf = db.sql_to_file(SQLS[0])
    rf.pin()
    rf.unpin()
    assert not rf.pinned
    rf.close()
    assert _files(rd) == []
    db.close()


def test_c_explicit_path_compression_and_corruption(tmp_path, monkeypatch):
    _cfg(tmp_path, monkeypatch)
    db = _db(tmp_path / "e.nhydb", 1, 1, n=3000)
    out = tmp_path / "mine.nhyc"
    for compress in (False, True):
        rf = db.sql_to_file("SELECT * FROM c", path=out, compress=compress)
        assert not rf.managed and rf.rows == 3000
        rf.close()
        assert out.exists()                         # caller-owned: never deleted
        assert list(nhydb.ResultFile(out)) == db.sql("SELECT * FROM c")["rows"]
        with pytest.raises(FileExistsError, match="already exists"):
            db.sql_to_file("SELECT id FROM c", path=out)
        assert not (tmp_path / "mine.nhyc.part").exists()
        raw = bytearray(out.read_bytes())
        raw[len(raw) // 2] ^= 0x10
        bad = tmp_path / "bad.nhyc"
        bad.write_bytes(bytes(raw))
        with pytest.raises(nhydb.CorruptionError):
            list(nhydb.ResultFile(bad))
        bad.unlink()
        out.unlink()
    db.close()


def test_c_budget_and_cleanup(tmp_path, monkeypatch):
    rd = _cfg(tmp_path, monkeypatch, "budget_bytes = 40000\n")
    db = _db(tmp_path / "b.nhydb", 1, 1, n=3000)
    with pytest.raises(nhydb.DatabaseSizeLimitError, match="budget_bytes = 40000"):
        db.sql_to_file("SELECT * FROM c")
    assert _files(rd) == []                         # the part file is removed
    small = db.sql_to_file("SELECT id FROM c LIMIT 10")
    # simulated crash: a part file and an unpinned result nobody holds
    (rd / "nhydb-999999-1-1.nhyc.part").write_bytes(b"x" * 100)
    (rd / "nhydb-999999-2-1.nhyc").write_bytes(b"y" * 100)
    pinned = rd / "nhydb-999999-3-1.pinned.nhyc"
    pinned.write_bytes(b"z")
    assert nhydb.cleanup_results(rd) == 2
    assert sorted(_files(rd)) == sorted([small.path.name, pinned.name])   # open file kept, pinned kept
    small.close()
    assert _files(rd) == [pinned.name]
    db.close()


def test_c_background_job_reads_a_snapshot(tmp_path, monkeypatch):
    _cfg(tmp_path, monkeypatch)
    db = _db(tmp_path / "j.nhydb", 1, 1, n=3000)
    db.put_dense_batch(1, 20_000, [[1, "a", 1, 1]], shape_id=1)     # uncommitted: not in the snapshot
    job = db.submit_sql("SELECT id, status FROM c ORDER BY id DESC")
    db.put_dense_batch(1, 30_000, [[2, "b", 2, 2]], shape_id=1)     # writes stay possible
    db.commit()
    rf = job.wait(timeout=60)
    assert job.done() and job.rows_written == rf.rows == 3000
    ids = [r["id"] for r in rf]
    assert ids == sorted(ids, reverse=True) and 1 in ids and len(ids) == 3000
    assert job.wait() is rf
    rf.close()
    with pytest.raises(nhydb.SQLSyntaxError):
        db.submit_sql("SELECT x FROM nowhere").wait(timeout=60)
    db.close()


def test_c_external_sort_equals_in_memory_order(tmp_path, monkeypatch):
    rd = _cfg(tmp_path, monkeypatch, "sort_memory_bytes = 65536\n")
    db = _db(tmp_path / "s.nhydb", 1, 1, n=20_000)
    for q in ["SELECT id, amount, status FROM c ORDER BY amount DESC, status",
              "SELECT id, status FROM c WHERE NOT status = 'a' ORDER BY status, amount DESC",
              "SELECT id FROM c ORDER BY cat"]:
        want = db.sql(q)["rows"]
        assert list(db.sql_iter(q)) == want, q
        with db.sql_to_file(q) as rf:
            assert list(rf) == want
        assert [f for f in _files(rd) if "sortrun" in f] == []      # runs removed after the merge
    # RUN5 (RD4-01): runs are anonymous temporary files – no name in the
    # result directory even while an iterator merges them
    it = db.sql_iter("SELECT id FROM c ORDER BY amount")
    next(it)
    assert [f for f in _files(rd) if "sortrun" in f] == []
    del it
    import gc
    gc.collect()
    assert [f for f in _files(rd) if "sortrun" in f] == []
    db.close()


def test_c_cancelled_job_leaves_no_files(tmp_path, monkeypatch):
    rd = _cfg(tmp_path, monkeypatch)
    db = _db(tmp_path / "x.nhydb", 1, 1, n=20_000)
    job = db.submit_sql("SELECT * FROM c")
    job.cancel()
    try:
        rf = job.wait(timeout=60)
    except nhydb.QueryError as e:                  # cancelled before the end
        assert "cancelled" in str(e)
        assert _files(rd) == []
    else:                                          # finished before the cancel was seen
        assert rf.rows == 20_000
        rf.close()
    db.close()
