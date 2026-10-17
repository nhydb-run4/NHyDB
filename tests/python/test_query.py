"""Differenztests der QueryEngine gegen Python-v0.27 (zufällige Ausdrucksbäume).

Dasselbe Szenario (Datenbank + deterministisch erzeugte Abfragen) läuft in
getrennten Prozessen gegen die Referenz und gegen die Rust-Engine; alle
Ergebnisse, Pläne (inkl. Kostenschätzung) und Fehler müssen gleich sein.
"""
import pytest

from ref_v027 import diff

SETUP = r'''
import random
from nhydb_v027 import NHyDB
from nhydb_query_v027 import Q, QueryEngine, DOMAIN_RECORD

db = NHyDB.create("q.nhydb")
db.create_realm(1, "data", base_reserve_to=16)
db.create_shape(1, 1, "row", [
    {"v": 1, "name": "id", "type": "varuint"},
    {"v": 2, "name": "temperature", "type": "varint"},
    {"v": 3, "name": "status", "type": "utf8"},
    {"v": 4, "name": "note", "type": "utf8"},
    {"v": 5, "name": "category", "type": "varuint"},
    {"v": 6, "name": "amount", "type": "varuint"},
    {"v": 7, "name": "ratio", "type": "f64"},
])
db.create_classic_layout(1, 1, "records", shape_id=1, base_reserve_to=16)
db.define_extension(1, 100, "utf8", name="tag")
WORDS = ["quick", "brown", "fox", "lazy", "dog", "abc", "abcd", "Red", "red", "ÄÖü", "straße", "x_1"]
rr = random.Random(7)
rows = []
for u in range(1, 61):
    note = " ".join(rr.choice(WORDS) for _ in range(rr.randint(1, 6)))
    rows.append([u, -5 + (u * 7) % 23, ["active", "idle", "gone"][u % 3], note, u % 7, u * 3, (u % 9) / 4])
ext = {u: {"tag": rr.choice(["alpha beta", "beta", "gamma delta", "Alpha"])} for u in range(1, 61, 4)}
db.put_dense_batch(1, 1, rows, shape_id=1, extensions_by_u=ext)
db.create_index(1, 1, "idx_temp", "temperature")
db.create_index(2, 1, "idx_status", "status")
db.create_index(3, 1, "idx_cat", "category")
db.create_token_index(4, 1, "idx_note_tok", "note")
db.create_trigram_index(5, 1, "idx_note_tri", "note")
db.create_trigram_index(6, 1, "idx_note_tri_ci", "note", casefold=True)
db.create_fulltext_index(7, 1, "idx_note_ft", "note")
db.create_token_index(8, 1, "idx_tag_tok", "tag")
db.delete(1, 13)
db.put(1, 61, [61, 3, "late", "late fox", 2, 7, 0.5])
# Sparse-Überschreibungen von BASE-Feldern und reine Sparse-Records: S7/S8-Test
# (Python v0.27 indiziert/selektiert dort veraltete Werte).
db.put_points(1, [(70, 100, "sparse-only tag", "utf8")])
db.build_indexes(commit=True)
db.create_index(9, 1, "idx_amount_stale", "amount")
db.create_realm(2, "mixed")
db.create_column_layout(2, 2, "col", fields=[{"u": 20, "name": "a", "type": "varint"}, {"u": 21, "name": "b", "type": "utf8"}])
db.create_packed_layout(3, 2, "pk", u=30, span=4, fields=[{"offset": 0, "name": "a", "type": "varint"}, {"offset": 1, "name": "b", "type": "utf8"}])
objs = [(i, {"a": i * 3 - 7, "b": f"o{i}"}) for i in range(1, 12)]
db.put_objects_batch(2, objs)
db.put_objects_batch(3, objs)
db.put_objects_batch(2, objs[:3], lane="NR")
db.put_points(2, [(50, 1, 5, "varint"), (50, 2, "free", "utf8")])
db.create_math_map_layout("sq", ("x",), u="x", V="x*x", side="NL", layout_id=20, payload_type="varuint")
db.put_math_batch(20, [((2,), 200), ((5,), 500)])
db.commit()
q = QueryEngine(db)

def evens(engine, ctx, params):
    for u in range(2, int(params.get("stop", 20)) + 1, 2):
        yield engine.db.record_anchor(1, u, lane=ctx.lane)["N_ref"]

def cells_of(engine, ctx, params):
    yield {"realm": 1, "u": 3, "v": 2}
    yield (1, 4, 3)
    yield (1, 5, 1, "NL")
    yield 1

q.register_math_provider("evens", evens, domain=DOMAIN_RECORD, estimator=lambda e, c, p: int(p.get("stop", 20)) // 2)
q.register_math_provider("cells_of", cells_of, description="fixed cells")
ANCHORS = [db.record_anchor(1, u)["N_ref"] for u in (1, 2, 7, 13, 30)]
'''

GEN = r'''
rnd = random.Random(SEED)
VALS = [None, 0, 1, 3, 5, 7, 10, -2, 2.5, True, "active", "idle", "fox", "abc", "", "x"]

def leaf():
    k = rnd.randrange(14)
    f = rnd.choice(["id", "temperature", "status", "note", "category", "amount", "ratio", "tag", "missing"])
    if k == 0:
        return getattr(Q.field(f), rnd.choice(["eq", "ne", "lt", "le", "gt", "ge"]))(rnd.choice(VALS))
    if k == 1:
        a, b = sorted([rnd.randint(-5, 20), rnd.randint(-5, 20)])
        return Q.field(f).between(a, b)
    if k == 2:
        s = rnd.choice(["abc", "ab", "fox", "Fox", "RED", "x_", "straße", "zz", "ÄÖ"])
        return getattr(Q.field(rnd.choice(["note", "tag", "status"])), rnd.choice(["contains", "icontains"]))(s)
    if k == 3:
        w = [rnd.choice(["fox", "red", "Dog", "beta", "alpha", "zzz"]) for _ in range(rnd.randint(1, 3))]
        g = Q.field(rnd.choice(["note", "tag"]))
        return rnd.choice([lambda: g.token(w[0]), lambda: g.tokens_all(*w), lambda: g.tokens_any(*w)])()
    if k == 4:
        t = " ".join(rnd.choice(["quick", "fox", "brown", "lazy", "zzz", "RED"]) for _ in range(rnd.randint(1, 3)))
        g = Q.field(rnd.choice(["note", "tag"]))
        return rnd.choice([lambda: g.fulltext(t), lambda: g.fulltext(t, mode="any"), lambda: g.phrase(t)])()
    if k == 5:
        return Q.field(f).exists()
    if k == 6:
        a = rnd.randint(-2, 70)
        return rnd.choice([lambda: Q.u == a, lambda: Q.u.between(a, a + rnd.randint(-1, 9)), lambda: Q.v == rnd.randint(1, 8),
                           lambda: Q.realm == rnd.choice([1, 2]), lambda: Q.lane == 0, lambda: Q.struct("layout_id") == 1,
                           lambda: Q.struct("object") >= 30, lambda: Q.struct("zzz") == 1])()
    if k == 7:
        return rnd.choice([lambda: Q.record_N == rnd.choice(ANCHORS), lambda: Q.N == rnd.choice(ANCHORS),
                           lambda: Q.n(*rnd.sample(ANCHORS, 2), domain=rnd.choice(["record", "cell"]))])()
    if k == 8:
        st = rnd.randint(1, 3)
        return Q.math("formula", parameter="k", start=1, stop=rnd.randint(5, 40), step=st,
                      u="k", V=rnd.choice(["1", "2", "k % 7 + 1"]), side="NL")
    if k == 9:
        return Q.math("n_sequence", start=rnd.randint(1, 50), stop=rnd.randint(50, 400), step=rnd.randint(1, 5))
    if k == 10:
        return Q.math(rnd.choice(["evens", "cells_of", "nope"]), stop=rnd.randint(4, 40))
    if k == 11:
        return Q.all()
    if k == 12:
        return Q.field("status") == rnd.choice(["active", "idle", "gone", "late"])
    return Q.field("category") == rnd.randint(0, 7)

def gen(d):
    if d == 0 or rnd.random() < 0.35:
        return leaf()
    op = rnd.random()
    if op < 0.45:
        return gen(d - 1) & gen(d - 1)
    if op < 0.8:
        return gen(d - 1) | gen(d - 1)
    return ~gen(d - 1)

AGG = {"rows": {"op": "count", "field": "*"}, "s": {"op": "sum", "field": "amount"},
       "lo": {"op": "min", "field": "temperature"}, "hi": ("max", "status"), "av": {"op": "avg", "field": "ratio"},
       "ct": {"op": "count", "field": "tag"}}

for i in range(N_EXPR):
    e = gen(DEPTH)
    realm = rnd.choice([1, 1, 1, 2, None])
    attempt(f"{i}:count", q.execute, e, realm=realm, return_mode="count", explain=True)
    attempt(f"{i}:n", q.execute, e, realm=realm, return_mode="n", limit=rnd.choice([None, 3]), offset=rnd.choice([0, 1]))
    attempt(f"{i}:fields", q.execute, e, realm=realm, return_mode="fields", select=["id", "status", "amount", "tag", "u", "record_N"],
            order_by=rnd.choice([None, ["-amount"], ["status", "-id"], [("tag", "desc")], ["ratio"]]), limit=rnd.choice([None, 4]))
    if i % 3 == 0:
        attempt(f"{i}:record", q.execute, e, realm=realm, return_mode="record", limit=3, order_by=rnd.choice([None, ["-u"]]))
        attempt(f"{i}:cell", q.execute, e, realm=realm, return_mode="cell", limit=6, offset=1, explain=True)
    if i % 2 == 0:
        attempt(f"{i}:agg", q.execute, e, realm=realm, aggregate=AGG, group_by=rnd.choice([[], ["status"], ["category", "tag"]]),
                order_by=rnd.choice([None, ["-rows"], ["status"], ["-s", "lo"]]), limit=rnd.choice([None, 2]))
    if i % 4 == 0:
        attempt(f"{i}:explain", q.explain, e, realm=realm, return_mode=rnd.choice(["record", "cell", "count", "weird"]))
'''


@pytest.mark.parametrize("seed,depth", [(1, 3), (2, 3), (3, 3), (4, 3), (5, 1), (6, 1), (7, 0)])
def test_random_expressions_identical(tmp_path, seed, depth):
    code = SETUP + f"\nSEED = {seed}\nN_EXPR = 45\nDEPTH = {depth}\n" + GEN
    py, rs = diff.compare(code, tmp_path)
    assert len(py) == len(rs)
    for a, b in zip(py, rs):
        assert a == b, (a, b)


SPECIFIC = r'''
emit("cells", list(db.iter_materialized_cells(None)))
emit("cells_r2_nr", list(db.iter_materialized_cells(2, lane="NR")))
emit("record_ns", list(db.iter_materialized_record_ns()))
emit("rna", [db.record_n_for_address(r, u, v) for r, u, v in [(1, 5, 3), (1, 13, 1), (2, 20, 4), (2, 30, 6), (2, 50, 1), (9, 1, 1)]])
emit("rnn", [db.record_n_from_n(x) for x in ANCHORS])
attempt("rnn-gz", db.record_n_from_n, 4)
emit("bridge", db.query(Q.field("status") == "idle", realm=1, return_mode="count"))
emit("bridge-explain", db.explain_query(Q.field("note").contains("ab"), realm=1))
for t in ["quick fox", "fox", "lazy dog", "zzz", "Quick brown"]:
    attempt(f"search:{t}", q.search, Q.field("note").fulltext(t), realm=1, select=["id", "note"], limit=5, explain=True)
    attempt(f"search-any:{t}", q.search, Q.field("note").fulltext(t, mode="any") & (Q.field("category") >= 3), realm=1, select=["id"])
    attempt(f"search-ph:{t}", q.search, Q.field("note").phrase(t) & (Q.field("status") == "idle"), realm=1, select=["id"], limit=None)
    attempt(f"ranked:{t}", q.execute, Q.field("note").fulltext(t) & Q.u.between(5, 40), realm=1, return_mode="ranked", select=["id"], offset=1)
attempt("search-norealm", q.search, Q.field("note").fulltext("fox"))
attempt("search-r2", q.search, Q.field("b").fulltext("o1"), realm=2)
attempt("search-two", q.search, Q.field("note").fulltext("fox") & Q.field("note").phrase("fox"), realm=1)
attempt("search-noidx", q.search, Q.field("status").fulltext("idle"), realm=1)
attempt("search-k1", q.search, Q.field("note").fulltext("fox"), realm=1, k1=0)
attempt("spec1", q.execute_spec, {"realm": 1, "return": "count", "where": {"and": [{"field": "status", "op": "eq", "value": "active"}, {"struct": "u", "op": "between", "lo": 1, "hi": 30}]}})
attempt("spec2", q.execute_spec, {"realm": 1, "aggregate": {"rows": {"op": "count", "field": "*"}}, "group_by": ["status"], "order_by": ["-rows"], "limit": 2})
attempt("spec3", q.execute_spec, {"realm": 1, "return": "fields", "select": ["id"], "where": {"or": [{"n": ANCHORS[:2], "domain": "record"}, {"not": {"all": True}}]}})
attempt("spec4", q.execute_spec, {"realm": 1, "where": {"math": "formula", "params": {"start": 1, "stop": 9, "u": "k", "v": "2"}}, "return": "cell"})
attempt("spec-bad", q.execute_spec, {"where": {"zzz": 1}})
attempt("spec-empty-and", q.execute_spec, {"where": {"and": []}})
attempt("spec-notobj", q.execute_spec, {"where": [1]})
attempt("spec-ranked", q.execute_spec, {"realm": 1, "return": "ranked", "select": ["id"], "limit": 2, "where": {"field": "note", "op": "fulltext", "value": "quick fox", "options": {"mode": "all", "casefold": True, "tokenizer": "simple", "positions": True}}, "explain": True})
attempt("bad-mode", q.execute, Q.all(), realm=1, return_mode="xyz")
attempt("agg-empty", q.execute, Q.all(), realm=1, return_mode="aggregate")
attempt("agg-bad", q.execute, Q.all(), realm=1, aggregate={"x": {"op": "median", "field": "id"}})
attempt("agg-star", q.execute, Q.all(), realm=1, aggregate={"x": {"op": "sum", "field": "*"}})
attempt("agg-nonnum", q.execute, Q.all(), realm=1, aggregate={"x": {"op": "sum", "field": "status"}})
attempt("agg-tuple", q.execute, Q.field("status") == "idle", realm=1, aggregate={"count": "*", "max": "note"}, group_by=["category"], order_by=[{"field": "count", "direction": "desc"}, "category"])
attempt("agg-r2", q.execute, Q.all(), realm=2, aggregate={"n": ("count", "*"), "s": ("sum", "a"), "m": ("max", "b")}, group_by=["layout_kind"])
attempt("agg-mixed-minmax", q.execute, Q.all(), realm=None, aggregate={"lo": ("min", "u")}, group_by=["realm"])
attempt("order-bad", q.execute, Q.all(), realm=1, return_mode="fields", select=["id"], order_by=[123])
attempt("order-mixed", q.execute, Q.all(), realm=None, return_mode="cell", order_by=["value"])
attempt("fields-empty", q.execute, Q.all(), realm=1, return_mode="fields")
attempt("domain-cell-to-record", q.execute, Q.math("cells_of"), realm=None, return_mode="count")
attempt("domain-bad", q.execute, Q.n(4), realm=1, return_mode="record")
attempt("n-bad", q.execute, Q.n("Nabc"), realm=1, return_mode="n")
attempt("n-neg", q.execute, Q.n(-4), realm=1, return_mode="n")
attempt("formula-zero", q.execute, Q.math("formula", start=1, stop=3, step=0, u="k", V="1"), realm=1)
attempt("formula-nov", q.execute, Q.math("formula", start=1, stop=3, u="k"), realm=1)
attempt("formula-v-norealm", q.execute, Q.math("formula", start=1, stop=3, u="k", v="1"), return_mode="cell")
attempt("formula-desc", q.execute, Q.math("formula", start=9, stop=1, step=-2, u="k", v="k"), realm=1, return_mode="n")
attempt("nseq-strict", q.execute, Q.math("n_sequence", start=1, stop=20, strict=True), return_mode="cell")
attempt("nseq-nostop", q.execute, Q.math("n_sequence", start=1), return_mode="cell", explain=True)
attempt("mli", q.execute, Q.math("math_layout_inputs", layout_id=20, inputs=[[2], 5, (9,)]), return_mode="cell")
attempt("mli-missing", q.execute, Q.math("math_layout_inputs", layout_id=99, inputs=[[2]]), return_mode="cell")
attempt("mli-noinputs", q.execute, Q.math("math_layout_inputs", layout_id=20), return_mode="cell", explain=True)
attempt("math-as-record", q.execute, Q.math("math_layout_inputs", layout_id=20, inputs=[[2]]), return_mode="count")
attempt("provider-bad", q.register_math_provider, " ", evens)
attempt("provider-dom", q.register_math_provider, "x", evens, domain="weird")
attempt("ensure", q.execute, 5)
attempt("q-n-dom", Q.n, 1, domain="foo")
attempt("ft-mode", Q.field("x").fulltext, "a", mode="maybe")
attempt("cell-struct", q.execute, (Q.v == 2) & (Q.u.between(3, 9)), realm=1, return_mode="cell", explain=True)
attempt("cell-field", q.execute, Q.field("a") > 0, realm=2, return_mode="cell")
attempt("rec-field-r2", q.execute, Q.field("b").contains("o1"), realm=2, return_mode="fields", select=["a", "b", "object", "layout_name", "u", "v"])
attempt("rec-struct-r2", q.execute, Q.struct("layout_kind") == "PACKED", realm=2, return_mode="record", limit=2)
attempt("not-all", q.execute, ~Q.all(), realm=1, return_mode="count", explain=True)
attempt("lane-nr", q.execute, Q.all(), realm=2, lane="NR", return_mode="n")
attempt("u-wide", q.execute, Q.u.between(3, 5000), realm=1, return_mode="count", explain=True)
attempt("stale", q.explain, Q.field("amount") == 9, realm=1)
attempt("mixed-and-bad-op", q.execute, Q.field("id").eq(1) & Q.struct("u").between("a", "b"), realm=1, return_mode="count")
'''


def test_specific_cases_identical(tmp_path):
    py, rs = diff.compare(SETUP + SPECIFIC, tmp_path)
    assert len(py) == len(rs)
    for a, b in zip(py, rs):
        assert a == b, (a, b)


def test_struct_range_huge_rust_only(tmp_path):
    """Python-v0.27 iteriert hier über 10**30 u (hängt); Rust nur über materialisierte u."""
    import nhydb
    from nhydb.query import Q, QueryEngine
    db = nhydb.NHyDB.create(str(tmp_path / "h.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 51)], shape_id=1)
    db.put_points(1, [(10**25, 1, 7, "varuint")])
    q = QueryEngine(db)
    r = q.execute(Q.u.between(1, 10**30), realm=1, return_mode="count", explain=True)
    assert r["count"] == 51 and r["plan"]["access"] == "STRUCT_RANGE"
    r = q.execute(Q.u.between(40, 10**30), realm=1, return_mode="fields", select=["id", "u"])
    assert [x["u"] for x in r["rows"]] == list(range(40, 51)) + [10**25]
    db.close()


def test_s7_s8_s9_sparse_consistency(tmp_path):
    """S7/S8: Python-v0.27 ignoriert in select/select_us (S7) und im Indexaufbau (S8)
    Sparse-Überschreibungen von BASE-Feldern und reine Sparse-Records; Index-,
    CLASSIC_U- und Scan-Pfad liefern dort verschiedene Ergebnisse. Rust liefert in
    allen Pfaden die logischen (überschriebenen) Werte. S9: Der CLASSIC_U-Pfad
    verliert in Python reine Sparse-Records (select_us liest nur Dense-Records):
    count zählt sie, Zeilen/Aggregate nicht."""
    import nhydb
    from nhydb.query import Q, QueryEngine
    db = nhydb.NHyDB.create(str(tmp_path / "s7.nhydb"))
    db.create_realm(1, "r", base_reserve_to=8)
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "status", "type": "utf8"},
                                {"v": 3, "name": "amount", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=8)
    db.put_dense_batch(1, 1, [[u, "a" if u % 2 else "b", u] for u in range(1, 21)], shape_id=1)
    db.put_points(1, [(4, 2, "a", "utf8"), (6, 3, 1000, "varuint"), (30, 1, 30, "varuint"), (30, 2, "b", "utf8")])
    db.delete_point(1, 8, 3)
    db.create_index(1, 1, "idx_status", "status")
    db.create_index(2, 1, "idx_amount", "amount")
    db.build_indexes(commit=True)
    assert list(db.index_candidates(1, "eq", "b", None)) == [2, 6, 8, 10, 12, 14, 16, 18, 20, 30]
    assert 4 in db.index_candidates(1, "eq", "a", None)
    assert list(db.index_candidates(2, "eq", 1000, None)) == [6] and list(db.index_candidates(2, "eq", 8, None)) == []
    q = QueryEngine(db)
    scan = q.execute(Q.field("status").contains("b"), realm=1, return_mode="n")
    idx = q.execute(Q.field("status") == "b", realm=1, return_mode="n")
    assert scan["refs"] == idx["refs"]
    agg = {"n": ("count", "*"), "s": ("sum", "amount"), "c": ("count", "amount")}
    fast = q.execute(Q.field("status") == "b", realm=1, aggregate=agg)
    slow = q.execute((Q.field("status") == "b") & Q.all(), realm=1, aggregate=agg)
    assert fast.get("fastpath") == "CLASSIC_U" and "fastpath" not in slow
    assert fast["rows"] == slow["rows"] == [{"n": 10, "s": sum(range(2, 21, 2)) - 4 - 6 + 1000 - 8, "c": 8}]
    f1 = q.execute(Q.field("status") == "b", realm=1, return_mode="fields", select=["id", "amount"])
    f2 = q.execute((Q.field("status") == "b") & Q.all(), realm=1, return_mode="fields", select=["id", "amount"])
    assert f1["rows"] == f2["rows"] and {r["id"]: r["amount"] for r in f1["rows"]}[6] == 1000
    assert f1["count"] == len(f1["rows"]) == 10 and f1["rows"][-1]["id"] == 30
    db.close()
