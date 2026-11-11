"""Permanent equivalence harness of the pre-release review of 2026-11-09.

Condensed, deterministic form of the review harnesses (differential test,
exhaustive sequences, fuzzing). Every check compares the
engine with a plain Python cell model and returns its deviations as a list
(it does not assert), so that ``test_harness_detects_a_wrong_model`` can show
that a deliberately wrong model is caught.

1. Index against scan and model (R9C-01): database A with VALUE/TOKEN/TRIGRAM/
   FULLTEXT indexes, database B without indexes, same operations. States:
   freshly built, stale with overlay (also uncommitted), automatic
   maintenance, a merge rebuild at every commit, reopen, rollback,
   ``compact_file`` with and without ``rebuild_indexes``. Queries: SQL rows and
   ``COUNT(*)`` (also with exactly one TOKEN/FULLTEXT term), QueryEngine
   counts, ``index_candidates`` and ``find_u``.
2. u at and beyond 2^64 in one run with merge rebuilds (R9A-01).
3. Every SQL output path returns the same rows in the same order and the same
   error: ``sql``, ``sql_iter``, ``sql_numpy``, ``sql_to_file``,
   ``submit_sql``, QueryEngine (SQL and Q) – DENSE, pure SPARSE and mixed
   records, WHERE + ORDER BY with sort runs (R9C-02).
4. F1 differential test over random histories of all write operations and
   state changes, with and without the winner view (R9A-02, R9A-04, R9E-02).
5. Exhaustive: every sequence of length 3 of the single-cell operations.
6. Minimal cases of engine deviations this harness found (fixed, kept as regression tests).

Model rules (documented delete and write semantics), including the points
the reviewers first had wrong in their own models:

* the newest write of a cell wins; ``put`` replaces the DENSE record of its
  shape (DENSE cells missing from the new shape are gone), ``put_cell``
  changes one cell; cells outside the shape follow their own writes;
* a stored ``None`` is a live cell: ``IS NULL`` does not match it, ``= x`` never
  matches it, a VALUE index has the key ``None`` for it;
* ``delete_area`` removes the DENSE record only when every live cell of the u
  lies in the area (DELETE block); ``delete_record`` = area over all v;
* ``get``/``range``/``select`` see DENSE records only (a pure SPARSE u has no
  record there); SQL, QueryEngine and ``read_numpy._present`` see a record
  as soon as one of its cells lives;
* ``get_cell`` of a tombstoned cell without a live record returns ``None``,
  with a live record ``present: False`` (R9E-10) – compared as "lives";
* ``put_extensions`` needs a live DENSE record; ``merge=False`` drops the
  other extension cells (v > base reserve);
* ``batch()["deleted"]`` = sum of the single-call results;
* ``SUM`` over nothing is 0; ORDER BY is stable (ties keep u order), ``None``
  sorts last ascending and first descending.
"""
import itertools
import re
import random

import numpy as np
import pytest

import nhydb
from nhydb.query import Q, QueryEngine

R = 1
SHAPES = {1: (1, 2, 3), 2: (1, 2)}
RESERVE = 20                      # BASE = v1..v20, extensions from v21
V_RES, V_EXT, V_UNNAMED = 15, 30, 40
VS = (1, 2, 3, V_RES, V_EXT, V_UNNAMED)
TYPES = {1: "varuint", 2: "varint", 3: "utf8", V_RES: "varint", V_EXT: "varuint", V_UNNAMED: "utf8"}
NAMES = {1: "id", 2: "x", 3: "y", V_EXT: "ext"}
COLS = ("id", "x", "y", "ext")
COLV = {"id": 1, "x": 2, "y": 3, "ext": V_EXT}
IX, IY, IEXT, TY, GY, FY = 1, 2, 3, 4, 5, 6       # index ids

WORDS = ["alpha", "beta", "gamma", "Delta", "pi", "x1"]
NOVEL = ["zz1", "zz2"]            # words a first index build has not seen
BIG = [2**64 - 1, 2**64, 2**64 + 1, 10**30]

CFG = {
    "default": "",
    "general": "[query]\noverlay_view = false\n",
    "manual": "[index]\nmaintenance = \"manual\"\n",
    "auto": "[index]\nmaintenance = \"auto\"\nrebuild_ratio = 0.05\nrebuild_min_changes = 4\n"
            "bulk_commit_ratio = 0.5\ncrisis_ratio = 0.3\n",
    # a merge rebuild at every commit (health() shows merge_rebuilds rising)
    "merge": "[index]\nrebuild_min_changes = 1\nrebuild_ratio = 0.0001\nbulk_commit_ratio = 1.0\n",
}
SMALL_SORT = "[results]\nsort_memory_bytes = 65536\n"


@pytest.fixture(autouse=True)
def _no_env_config(monkeypatch):
    # the config file next to each database applies
    monkeypatch.delenv("NHYDB_CONFIG", raising=False)


def _path(tmp_path, name, cfg):
    d = tmp_path / name
    d.mkdir()
    (d / "nhydb_config.toml").write_text(cfg)
    return str(d / "x.nhydb")


def make_db(path, indexes=True, text=True):
    db = nhydb.NHyDB.create(path)
    db.create_realm(R, "r")
    db.create_shape(1, R, "A", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"},
                                 {"v": 3, "name": "y", "type": "utf8"}])
    db.create_shape(2, R, "B", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}])
    db.create_classic_layout(1, R, "main", shape_id=1, base_reserve_to=RESERVE)
    db.define_extension(R, V_EXT, "varuint", name="ext")
    if indexes:
        db.create_index(IX, R, "ix", "x")
        db.create_index(IY, R, "iy", "y")
        db.create_index(IEXT, R, "iext", "ext")
        if text:
            db.create_token_index(TY, R, "ty", "y")
            db.create_trigram_index(GY, R, "gy", "y")
            db.create_fulltext_index(FY, R, "fy", "y")
    db.commit()
    return db


def _call(fn, *a, **k):
    try:
        return ("ok", fn(*a, **k))
    except Exception as e:  # compared as (class, text)
        return ("err", type(e).__name__, str(e))


def _maint(db):
    """Sum of rebuilds/merge rebuilds of the handle (health)."""
    checks = {c["name"]: c for c in db.health()["checks"]}
    lanes = checks["indexes"]["details"]["maintenance"]["lanes"]
    return sum(x["rebuilds"] for x in lanes), sum(x["merge_rebuilds"] for x in lanes)


# ----------------------------------------------------------------------
# model
# ----------------------------------------------------------------------

class Model:
    """(lane, u) → {v: (value, origin)}; origin "d" = DENSE record, "c" =
    written alone. ``dense[(lane, u)]`` = shape of the live DENSE record.
    ``mutation`` breaks one rule on purpose (sensitivity check)."""

    def __init__(self, mutation=None):
        self.cells = {}
        self.dense = {}
        self.mut = mutation

    def copy(self):
        m = Model(self.mut)
        m.cells = {k: dict(c) for k, c in self.cells.items()}
        m.dense = dict(self.dense)
        return m

    def _tidy(self, k):
        if not self.cells.get(k):
            self.cells.pop(k, None)

    def put(self, lane, u, shape, vals):
        c = self.cells.setdefault((lane, u), {})
        vs = SHAPES[shape]
        if self.mut != "keep_old_shape":
            for v in [v for v, (_, o) in c.items() if o == "d" and v not in vs]:
                del c[v]
        for v, x in zip(vs, vals):
            if self.mut == "sparse_on_top" and v in c and c[v][1] == "c":
                continue
            c[v] = (x, "d")
        self.dense[(lane, u)] = shape

    def put_cell(self, lane, u, v, x):
        self.cells.setdefault((lane, u), {})[v] = (x, "c")

    def delete_cell(self, lane, u, v):
        k = (lane, u)
        if self.mut == "ignore_tombstone":
            return v in self.cells.get(k, {})
        hit = self.cells.get(k, {}).pop(v, None) is not None
        self._tidy(k)
        return hit

    def delete_area(self, lane, u0, u1, v0, v1):
        n = 0
        for k in [k for k in self.cells if k[0] == lane and u0 <= k[1] <= u1]:
            c = self.cells[k]
            hit = [v for v in c if v0 <= v <= v1]
            if hit and len(hit) == len(c):
                self.dense.pop(k, None)
            for v in hit:
                del c[v]
            n += len(hit)
            self._tidy(k)
        return n

    def delete_record(self, lane, u):
        n = self.delete_area(lane, u, u, 1, 10**9)
        if n:
            self.dense.pop((lane, u), None)
        return n

    def put_extensions(self, lane, u, items, merge):
        k = (lane, u)
        if k not in self.dense:
            return False
        c = self.cells.setdefault(k, {})
        if not merge:
            for v in [v for v in c if v > RESERVE and v not in items]:
                del c[v]
        for v, x in items.items():
            c[v] = (x, "c")
        self._tidy(k)
        return True

    def live(self, lane):
        return {(u, v): x for (ln, u), c in self.cells.items() if ln == lane for v, (x, _) in c.items()}

    def live_origin(self, lane, origin):
        return {(u, v): x for (ln, u), c in self.cells.items() if ln == lane for v, (x, o) in c.items() if o == origin}

    def records(self, lane):
        """[(u, {v: value})] of the records with a live cell, u ascending."""
        return [(u, {v: x for v, (x, _) in c.items()})
                for (ln, u), c in sorted(self.cells.items(), key=lambda kv: kv[0][1]) if ln == lane and c]


def row_of(c):
    return {n: c.get(COLV[n]) for n in COLS}


# ---- predicates with Python semantics (SQL, Q and index must agree) ----
MISSING = object()


def p_eq(x, lit):
    if x is MISSING or x is None:
        return False
    if isinstance(x, (bytes, str)) != isinstance(lit, (bytes, str)):
        return False
    return x == lit


def p_cmp(x, op, lit):
    if x is MISSING or x is None:
        return False
    try:
        return {"<": x < lit, "<=": x <= lit, ">": x > lit, ">=": x >= lit}[op]
    except TypeError:            # incompatible ordering comparison → False
        return False


def p_between(x, lo, hi):
    return p_cmp(x, ">=", lo) and p_cmp(x, "<=", hi)


def _toks(x):
    return [t.casefold() for t in re.findall(r"\w+", "" if x is MISSING or x is None else str(x))]


def p_contains(x, lit, ci=False):
    if x is MISSING:
        return False
    a = "" if x is None else str(x)
    return lit.casefold() in a.casefold() if ci else lit in a


def p_token(x, t):
    return x is not MISSING and t.casefold() in set(_toks(x))


def p_fulltext(x, q, mode="all"):
    if x is MISSING:
        return False
    qt, dt = _toks(q), set(_toks(x))
    return bool(qt) and (all if mode == "all" else any)(t in dt for t in qt)


def p_phrase(x, q):
    if x is MISSING:
        return False
    qt, dt = _toks(q), _toks(x)
    return bool(qt) and any(dt[i:i + len(qt)] == qt for i in range(len(dt) - len(qt) + 1))


def X(c):
    return c.get(2, MISSING)


def Y(c):
    return c.get(3, MISSING)


def E(c):
    return c.get(V_EXT, MISSING)


# (WHERE, parameters, model predicate)
QUERIES = [
    ("x = ?", [0], lambda c: p_eq(X(c), 0)),
    ("x = ?", [1], lambda c: p_eq(X(c), 1)),               # 1 == 1.0
    ("x = ?", ["1"], lambda c: p_eq(X(c), "1")),
    ("x = ?", [2**70], lambda c: p_eq(X(c), 2**70)),
    ("x BETWEEN ? AND ?", [-1, 1], lambda c: p_between(X(c), -1, 1)),
    ("x < ?", [0], lambda c: p_cmp(X(c), "<", 0)),
    ("x >= ?", [1], lambda c: p_cmp(X(c), ">=", 1)),
    ("x = ? OR x = ?", [-2, 2], lambda c: p_eq(X(c), -2) or p_eq(X(c), 2)),   # IN list (no SQL IN)
    ("y = ?", ["alpha"], lambda c: p_eq(Y(c), "alpha")),
    ("y > ?", ["beta"], lambda c: p_cmp(Y(c), ">", "beta")),
    ("y CONTAINS ?", ["amm"], lambda c: p_contains(Y(c), "amm")),
    ("y ICONTAINS ?", ["DEL"], lambda c: p_contains(Y(c), "DEL", True)),
    ("TOKEN(y, ?)", ["beta"], lambda c: p_token(Y(c), "beta")),
    ("TOKEN(y, ?)", ["zz1"], lambda c: p_token(Y(c), "zz1")),
    ("FULLTEXT(y, ?)", ["alpha"], lambda c: p_fulltext(Y(c), "alpha")),
    ("FULLTEXT(y, ?)", ["delta"], lambda c: p_fulltext(Y(c), "delta")),
    ("FULLTEXT(y, ?)", ["zz2"], lambda c: p_fulltext(Y(c), "zz2")),
    ("FULLTEXT(y, ?)", ["alpha beta"], lambda c: p_fulltext(Y(c), "alpha beta")),
    ("FULLTEXT_ANY(y, ?)", ["pi gamma"], lambda c: p_fulltext(Y(c), "pi gamma", "any")),
    ("PHRASE(y, ?)", ["alpha beta"], lambda c: p_phrase(Y(c), "alpha beta")),
    ("ext = ?", [1], lambda c: p_eq(E(c), 1)),
    ("ext BETWEEN ? AND ?", [1, 2], lambda c: p_between(E(c), 1, 2)),
    ("y IS NULL", [], lambda c: 3 not in c),
    ("x IS NOT NULL", [], lambda c: 2 in c),
    ("NOT (x = ?) AND TOKEN(y, ?)", [0, "alpha"], lambda c: not p_eq(X(c), 0) and p_token(Y(c), "alpha")),
    ("TOKEN(y, ?) OR ext = ?", ["gamma", 2], lambda c: p_token(Y(c), "gamma") or p_eq(E(c), 2)),
]

# (index id, op, value, value2, model predicate) for index_candidates
IDX_PROBES = [
    (IX, "eq", 0, None, lambda c: p_eq(X(c), 0)),
    (IX, "eq", 1, None, lambda c: p_eq(X(c), 1)),
    (IX, "eq", "1", None, lambda c: p_eq(X(c), "1")),
    (IX, "eq", 2**70, None, lambda c: p_eq(X(c), 2**70)),
    (IX, "eq", None, None, lambda c: 2 in c and c[2] is None),     # VALUE key None = stored None
    (IX, "between", -1, 1, lambda c: p_between(X(c), -1, 1)),
    (IY, "eq", "alpha", None, lambda c: p_eq(Y(c), "alpha")),
    (IY, "between", "alpha", "beta", lambda c: p_between(Y(c), "alpha", "beta")),
    (IEXT, "eq", 1, None, lambda c: p_eq(E(c), 1)),
    (IEXT, "between", 0, 1, lambda c: p_between(E(c), 0, 1)),
    (TY, "token", "beta", None, lambda c: p_token(Y(c), "beta")),
    (TY, "token", "zz1", None, lambda c: p_token(Y(c), "zz1")),
    (GY, "contains", "amm", None, lambda c: p_contains(Y(c), "amm")),
    (GY, "contains", "lph", None, lambda c: p_contains(Y(c), "lph")),
    (GY, "contains", "zz2", None, lambda c: p_contains(Y(c), "zz2")),
    (FY, "fulltext", "alpha", None, lambda c: p_fulltext(Y(c), "alpha")),
    (FY, "fulltext", "alpha beta", None, lambda c: p_fulltext(Y(c), "alpha beta")),
    (FY, "fulltext", "zz1", None, lambda c: p_fulltext(Y(c), "zz1")),
]

# QueryEngine count (POSTING_COUNT fastpath for one term) and refs
QE_PROBES = [
    (lambda: Q.field("y").fulltext("alpha"), lambda c: p_fulltext(Y(c), "alpha")),
    (lambda: Q.field("y").fulltext("zz2"), lambda c: p_fulltext(Y(c), "zz2")),
    (lambda: Q.field("y").token("beta"), lambda c: p_token(Y(c), "beta")),
    (lambda: Q.field("y").token("zz1"), lambda c: p_token(Y(c), "zz1")),
    (lambda: Q.field("x") == 1, lambda c: p_eq(X(c), 1)),
    (lambda: ~(Q.field("x") == 0), lambda c: not p_eq(X(c), 0)),
]


def _us_of_refs(refs):
    return [nhydb.n_to_dynadron_address(int(r[1:]))["u"] for r in refs]


# ----------------------------------------------------------------------
# checks: each returns a list of deviations
# ----------------------------------------------------------------------

class Diffs(list):
    def eq(self, label, got, want):
        if repr(got) != repr(want):
            self.append(f"{label}: got={got!r:.300} want={want!r:.300}")


def diff_queries(A, B, m, where, stale_ok):
    """Index (A) against scan (B) against the model."""
    out = Diffs()
    for lane in (0, 1):
        recs = m.records(lane)
        for w, p, pred in (QUERIES if lane == 0 else QUERIES[:3] + QUERIES[12:16]):
            want = [row_of(c) for _, c in recs if pred(c)]
            q = f"SELECT id, x, y, ext FROM main WHERE {w}"
            qc = f"SELECT COUNT(*) AS n FROM main WHERE {w}"
            for name, db in (("A", A), ("B", B)):
                out.eq(f"{where} lane={lane} sql {name} {w} {p}", _call(lambda: db.sql(q, p, lane=lane)["rows"]), ("ok", want))
                out.eq(f"{where} lane={lane} COUNT {name} {w} {p}", _call(lambda: db.sql(qc, p, lane=lane)["rows"]),
                       ("ok", [{"n": len(want)}]))
        for name, db in (("A", A), ("B", B)):
            qe = QueryEngine(db)
            for mk, pred in QE_PROBES:
                want = [u for u, c in recs if pred(c)]
                out.eq(f"{where} lane={lane} engine count {name} {mk()!r}",
                       _call(lambda: qe.execute(mk(), realm=R, lane=lane, return_mode="count")["count"]), ("ok", len(want)))
                out.eq(f"{where} lane={lane} engine n {name} {mk()!r}",
                       _call(lambda: sorted(_us_of_refs(qe.execute(mk(), realm=R, lane=lane, return_mode="n")["refs"]))), ("ok", want))
        # direct index APIs (A)
        for iid, op, val, val2, pred in IDX_PROBES:
            got = _call(lambda: list(A.index_candidates(iid, op, val, val2, lane=lane)))
            if stale_ok[lane] and got[0] == "err" and got[1] == "StaleIndexError":
                continue
            out.eq(f"{where} lane={lane} index_candidates {iid} {op} {val!r} {val2!r}", got,
                   ("ok", [u for u, c in recs if pred(c)]))
        for k in (0, 1):
            got = _call(lambda: list(A.find_u(R, {"ix": k}, lane=lane)))
            if stale_ok[lane] and got[0] == "err" and got[1] == "StaleIndexError":
                continue
            out.eq(f"{where} lane={lane} find_u ix={k}", got, ("ok", [u for u, c in recs if p_eq(X(c), k)]))
    return out


DOMAIN_F1 = list(range(1, 11)) + list(range(2**64 - 3, 2**64 + 3))
WINDOWS_F1 = ((1, 10), (2**64 - 3, 6))


def diff_cells(db, m, where, stale_ok, domain=DOMAIN_F1, windows=WINDOWS_F1, full=True):
    """Every read path of one handle against the model (F1)."""
    out = Diffs()
    for lane in (0, 1):
        live = m.live(lane)
        recs = m.records(lane)
        ctx = f"{where} lane={lane}"
        # single cells: get_cell, get_value, get_n_cell (R9A-04)
        for u in domain:
            for v in VS:
                want = (u, v) in live
                c = db.get_cell(R, u, v, lane=lane, dyn_address=False)
                out.eq(f"{ctx} get_cell u={u} v={v}", (bool(c and c["present"]), c["value"] if want and c else None),
                       (want, live.get((u, v))))
                out.eq(f"{ctx} get_value u={u} v={v}", db.get_value(R, u, v, lane=lane), live.get((u, v)))
                if full:
                    g = db.get_n_cell(db.cell_address(R, u, v, lane=lane)["N"])
                    out.eq(f"{ctx} get_n_cell u={u} v={v}", (g["present"], g["value"] if want else None), (want, live.get((u, v))))
        # cell cursor and its relatives
        cells = list(db.iter_materialized_cells(R, lane=lane))
        out.eq(f"{ctx} iter_materialized_cells", sorted((c["u"], c["v"], repr(c["value"])) for c in cells),
               sorted((u, v, repr(x)) for (u, v), x in live.items()))
        if full:
            sp = sorted((p["u"], p["v"], repr(p["value"])) for p in db.iter_materialized_sparse_points(R, lane=lane))
            out.eq(f"{ctx} iter_materialized_sparse_points", sp, sorted((u, v, repr(x)) for (u, v), x in m.live_origin(lane, "c").items()))
            rn = {c["u"]: c["record_N"] for c in cells}
            out.eq(f"{ctx} iter_materialized_record_ns", sorted(db.iter_materialized_record_ns(R, lane=lane)), sorted(rn.values()))
        # record oriented: get / range / select (DENSE records)
        dense = sorted(u for (ln, u) in m.dense if ln == lane)
        for u in domain:
            rec = db.get(R, u, lane=lane)
            want = (lane, u) in m.dense
            out.eq(f"{ctx} get exists u={u}", rec is not None, want)
            if rec is not None and want:
                out.eq(f"{ctx} get _shape u={u}", rec["_shape"], m.dense[(lane, u)])
                for v, name in NAMES.items():
                    out.eq(f"{ctx} get {name} u={u}", rec.get(name), live.get((u, v)))
        hi = max(domain) + 10
        rg = list(db.range(R, 1, hi, lane=lane))
        out.eq(f"{ctx} range u", [r["_u"] for r in rg], [u for u in dense if u <= hi])
        sel = list(db.select(R, 1, hi, ["x", "y", "ext"], lane=lane))
        out.eq(f"{ctx} select", [(r["_u"], r["x"], r["y"], r["ext"]) for r in sel],
               [(u, live.get((u, 2)), live.get((u, 3)), live.get((u, V_EXT))) for u in dense if u <= hi])
        # read_numpy with masks: _present = record has a live cell (R9E-02), _cells per field (E3)
        for start, cnt in windows:
            nd = db.read_numpy(R, start, cnt, list(COLS), lane=lane, masks=True, object_fallback=True)
            for i in range(cnt):
                u = start + i
                out.eq(f"{ctx} read_numpy _present u={u}", bool(nd["_present"][i]), bool(m.cells.get((lane, u))))
                for name in COLS:
                    v = COLV[name]
                    out.eq(f"{ctx} read_numpy _cells {name} u={u}", bool(nd["_cells"][name][i]), (u, v) in live)
                    x = live.get((u, v))
                    if x is not None:
                        got = nd[name][i]
                        got = got.item() if hasattr(got, "item") else got
                        if got != x:
                            out.append(f"{ctx} read_numpy {name} u={u}: got={got!r} want={x!r}")
        # SQL
        allrows = [row_of(c) for _, c in recs]
        out.eq(f"{ctx} sql rows", db.sql("SELECT id, x, y, ext FROM main", lane=lane)["rows"], allrows)
        out.eq(f"{ctx} sql COUNT(*)", db.sql("SELECT COUNT(*) AS n FROM main", lane=lane)["rows"], [{"n": len(recs)}])
        npc = db.sql_numpy("SELECT id, x, y, ext FROM main", lane=lane)
        got = list(zip(*[npc[n].tolist() for n in COLS])) if npc and len(npc["id"]) else []
        if got != [tuple(r[n] for n in COLS) for r in allrows]:
            out.append(f"{ctx} sql_numpy: got={got!r:.300} want={allrows!r:.300}")
        if full:
            for w, p, pred in (QUERIES[0], QUERIES[12], QUERIES[20], QUERIES[22], QUERIES[24]):
                out.eq(f"{ctx} sql COUNT WHERE {w}", db.sql(f"SELECT COUNT(*) AS n FROM main WHERE {w}", p, lane=lane)["rows"],
                       [{"n": sum(1 for _, c in recs if pred(c))}])
        # index
        for idx, v, keys in (("ix", 2, (-1, 0, 1)), ("iext", V_EXT, (0, 1))):
            for k in keys:
                want = [u for u, c in recs if p_eq(c.get(v, MISSING), k)]
                got = _call(lambda: list(db.find_u(R, {idx: k}, lane=lane)))
                if not (stale_ok[lane] and got[0] == "err" and got[1] == "StaleIndexError"):
                    out.eq(f"{ctx} find_u {idx}={k}", got, ("ok", want))
        q = QueryEngine(db)
        out.eq(f"{ctx} engine count", q.execute(Q.all(), realm=R, lane=lane, return_mode="count")["count"], len(recs))
    return out


# ----------------------------------------------------------------------
# operations and runner
# ----------------------------------------------------------------------

def _text(rng, novel=True):
    words = [rng.choice(WORDS) for _ in range(rng.randint(1, 3))]
    if novel and rng.random() < 0.2:
        words.insert(rng.randint(0, len(words)), rng.choice(NOVEL))
    return " ".join(words)


def _dense_row(shape, rng):
    row = [rng.randrange(3), rng.randint(-2, 2), _text(rng)]
    return row[:len(SHAPES[shape])]


X_CELLS = [("varint", -1), ("varint", 0), ("varint", 1), ("varint", 2), ("f64", 1.0), ("utf8", "1"),
           ("varint", 2**70), ("varint", None)]


def _cell(v, rng):
    """(type, value) of a single cell; ``None`` 15 %."""
    if v == 2:
        return rng.choice(X_CELLS)
    if rng.random() < 0.15:
        return TYPES[v], None
    return TYPES[v], {1: rng.randrange(3), 3: _text(rng), V_RES: rng.randint(-1, 1), V_EXT: rng.randrange(3),
                      V_UNNAMED: rng.choice("pq")}[v]


def _area(rng, starts):
    """(u_from, u_to, v_from, v_to), also partial v ranges and huge u ranges."""
    u0 = rng.choice(starts)
    v0 = rng.choice([1, 2, 3, 10, 15, 16, 21, 30, 31, 40])
    return u0, u0 + rng.choice([0, 0, 1, 3, 2**64]), v0, v0 + rng.choice([0, 1, 13, 40, 900])


def gen_op(rng, starts, p_lane1, maxrun):
    k = rng.random()
    lane = 1 if rng.random() < p_lane1 else 0
    u = rng.choice(starts)
    if k < 0.18:
        shape = rng.choice([1, 1, 2])
        how = rng.choice(["dense_batch", "dense_batch", "put", "numpy"])
        n = 1 if how == "put" else rng.randint(1, maxrun)
        return ("put", how, lane, u, shape, [_dense_row(shape, rng) for _ in range(n)])
    if k < 0.38:
        v = rng.choice(VS + (2, 3))
        t, x = _cell(v, rng)
        return ("put_cell", lane, u, v, x, t)
    if k < 0.43:
        cells = {}
        for _ in range(rng.randint(1, 5)):
            v = rng.choice(VS)
            cells[(rng.choice(starts), v)] = _cell(v, rng)
        return ("put_points", lane, sorted((uu, v, x, t) for (uu, v), (t, x) in cells.items()))
    if k < 0.46:
        v = rng.choice([2, 3, V_EXT])
        us = sorted(set(rng.choice(starts[:8]) for _ in range(rng.randint(1, 4))))
        vals = [{2: rng.randint(-2, 2), 3: _text(rng), V_EXT: rng.randrange(3)}[v] for _ in us]
        return ("put_cells_numpy", lane, us, v, vals)
    if k < 0.58:
        return ("delete_cell", lane, u, rng.choice(VS))
    if k < 0.63:
        return ("delete_record", lane, u)
    if k < 0.70:
        return ("delete_area", lane) + _area(rng, starts)
    if k < 0.80:
        sub = []
        for _ in range(rng.randint(1, 7)):
            kk, ln, uu = rng.random(), (1 if rng.random() < p_lane1 else 0), rng.choice(starts)
            if kk < 0.45:
                v = rng.choice(VS)
                t, x = _cell(v, rng)
                sub.append(("put_cell", ln, uu, v, x, t))
            elif kk < 0.7:
                sub.append(("delete_cell", ln, uu, rng.choice(VS)))
            elif kk < 0.85:
                sub.append(("delete_record", ln, uu))
            else:
                sub.append(("delete_area", ln) + _area(rng, starts))
        if rng.random() < 0.3 and sub[0][0] == "delete_cell":
            # R9A-02: delete_cell → put_cell of the same cell in one batch
            sub.insert(1, ("put_cell", sub[0][1], sub[0][2], sub[0][3]) + _cell(sub[0][3], rng)[::-1])
        return ("batch", sub, rng.random() < 0.1)
    items = {V_EXT: rng.randrange(3)} if rng.random() < 0.7 else {}
    return ("put_extensions", lane, u, items, rng.random() < 0.5)


# state actions after each burst (every state is reached in every history)
CYCLE = ["none", "commit", "rollback", "commit", "reopen", "none", "compact_rebuild", "commit",
         "compact_norebuild", "none", "commit", "build"]


def gen_history(rng, rounds, starts, p_lane1, maxrun, burst=(2, 6)):
    ops = []
    for i in range(rounds):
        ops += [gen_op(rng, starts, p_lane1, maxrun) for _ in range(rng.randint(*burst))]
        ops.append(("action", CYCLE[i % len(CYCLE)]))
    return ops


def _put_db(db, how, lane, u, shape, rows):
    if how == "dense_batch":
        db.put_dense_batch(R, u, rows, shape_id=shape, lane=lane)
    elif how == "put":
        db.put(R, u, dict(zip([NAMES[v] for v in SHAPES[shape]], rows[0])), shape_id=shape, lane=lane)
    else:
        cols = {}
        for i, v in enumerate(SHAPES[shape]):
            cols[NAMES[v]] = np.array([r[i] for r in rows], dtype={1: np.uint64, 2: np.int64, 3: object}[v])
        db.put_numpy_batch(R, u, cols, shape_id=shape, lane=lane)


class Runner:
    """Same operations on A (with indexes, ``cfg``) and optionally B (no
    index, manual maintenance), the model alongside. Deviations of return
    values and of the checks are collected in ``diffs``."""

    def __init__(self, tmp_path, cfg, *, ref, mode, mutation=None, base=(), text=True):
        self.cfg = cfg
        self.mode = mode                   # "queries" (diff_queries) or "cells" (diff_cells)
        self.paths = [_path(tmp_path, "A", CFG[cfg])] + ([_path(tmp_path, "B", CFG["manual"])] if ref else [])
        self.dbs = [make_db(self.paths[0], text=text)] + ([make_db(self.paths[1], indexes=False)] if ref else [])
        self.m = Model(mutation)
        self.diffs = Diffs()
        self.states = set()
        self.maint = [0, 0]
        self.stale_ok = [False, False]
        self.dirty = [False, False]
        self.check_kw = {}
        for lane, u, rows in base:
            for db in self.dbs:
                db.put_dense_batch(R, u, rows, shape_id=1, lane=lane)
            for i, row in enumerate(rows):
                self.m.put(lane, u + i, 1, row)
        for db in self.dbs:
            db.commit()
        self.dbs[0].build_indexes(commit=True)
        self.committed = self.m.copy()
        # a lane without data has no index base: its direct queries may be stale
        self.stale_ok = [self._lane_empty(0), self._lane_empty(1)]
        self.check("fresh")

    @property
    def A(self):
        return self.dbs[0]

    def close(self):
        for db in self.dbs:
            if not db.closed:
                db.close()

    def check(self, where):
        self.states.add(where.split(" ")[0])
        if self.mode == "queries":
            self.diffs += diff_queries(self.A, self.dbs[1], self.m, where, self.stale_ok)
        else:
            self.diffs += diff_cells(self.A, self.m, where, self.stale_ok, **self.check_kw)

    def _reopen(self, compact=None):
        for db in self.dbs:
            db.commit()
        self.committed = self.m.copy()
        rb, mr = _maint(self.A)
        self.maint[0] += rb
        self.maint[1] += mr
        self.close()
        if compact is not None:
            for i, p in enumerate(self.paths):
                nhydb.NHyDB.compact_file(p, make_backup=False, rebuild_indexes=compact or i > 0)
        self.dbs = [nhydb.NHyDB(p) for p in self.paths]

    def _lane_empty(self, lane):
        return not any(ln == lane for ln, _ in self.m.cells)

    def action(self, a):
        if a == "commit":
            for db in self.dbs:
                db.commit()
            self.committed = self.m.copy()
            if self.cfg != "manual":
                # automatic maintenance builds an index without a base at
                # the next commit of the lane (E4); an empty lane stays without
                for lane in (0, 1):
                    if self.dirty[lane]:
                        self.stale_ok[lane] = self._lane_empty(lane)
            self.dirty = [False, False]
        elif a == "rollback":
            for db in self.dbs:
                db.rollback()
            self.m = self.committed.copy()
            self.dirty = [False, False]
        elif a == "reopen":
            self._reopen()
        elif a in ("compact_rebuild", "compact_norebuild"):
            self._reopen(compact=a == "compact_rebuild")
            for lane in (0, 1):
                # without rebuild the index has no built part: StaleIndexError
                # with manual maintenance (automatic maintenance answers);
                # an emptied lane has no base (R9A-03)
                self.stale_ok[lane] = (a == "compact_norebuild" and self.cfg == "manual") or self._lane_empty(lane)
        elif a == "build":
            for db in self.dbs[1:]:
                db.commit()
            self.A.build_indexes(commit=True)
            self.committed = self.m.copy()
            self.stale_ok = [self._lane_empty(0), self._lane_empty(1)]
        if a in ("rollback", "reopen", "compact_rebuild", "compact_norebuild", "build"):
            self.dirty = [False, False]
        self.check(a if a != "none" else "overlay_uncommitted")

    def apply(self, op):
        t = op[0]
        if t == "action":
            return self.action(op[1])
        if t == "check":
            return self.check(op[1] if len(op) > 1 else "check")
        m = self.m
        lanes = set()
        if t == "put":
            _, how, lane, u, shape, rows = op
            for db in self.dbs:
                _put_db(db, how, lane, u, shape, rows)
            for i, row in enumerate(rows):
                m.put(lane, u + i, shape, row)
            lanes.add(lane)
        elif t == "put_cell":
            _, lane, u, v, x, typ = op
            for db in self.dbs:
                db.put_cell(R, u, v, x, type=typ, lane=lane)
            m.put_cell(lane, u, v, x)
            lanes.add(lane)
        elif t == "put_points":
            _, lane, cells = op
            for db in self.dbs:
                db.put_points(R, list(cells), lane=lane)
            for u, v, x, _ in cells:
                m.put_cell(lane, u, v, x)
            lanes.add(lane)
        elif t == "put_cells_numpy":
            _, lane, us, v, vals = op
            for db in self.dbs:
                db.put_cells_numpy(R, np.array(us, dtype=np.uint64), v,
                                   np.array(vals, dtype={2: np.int64, 3: object, V_EXT: np.uint64}[v]), type=TYPES[v], lane=lane)
            for u, x in zip(us, vals):
                m.put_cell(lane, u, v, x)
            lanes.add(lane)
        elif t in ("delete_cell", "delete_record", "delete_area"):
            lane = op[1]
            want = getattr(m, t)(*op[1:])
            for i, db in enumerate(self.dbs):
                if t == "delete_cell":
                    got = db.delete_cell(R, op[2], op[3], lane=lane)
                elif t == "delete_record":
                    got = db.delete_record(1, op[2], lane=lane)
                else:
                    got = db.delete_area(R, *op[2:], lane=lane)
                self.diffs.eq(f"{t} return db{i} {op}", got, want)
            lanes.add(lane)
        elif t == "batch":
            _, sub, fail = op
            trial = m.copy()
            deleted = 0
            for s in sub:
                if s[0] == "put_cell":
                    trial.put_cell(s[1], s[2], s[3], s[4])
                else:
                    deleted += int(getattr(trial, s[0])(*s[1:]))
            for i, db in enumerate(self.dbs):
                try:
                    with db.batch() as b:
                        for s in sub:
                            if s[0] == "put_cell":
                                b.put_cell(R, s[2], s[3], s[4], type=s[5], lane=s[1])
                            elif s[0] == "delete_cell":
                                b.delete_cell(R, s[2], s[3], lane=s[1])
                            elif s[0] == "delete_record":
                                b.delete_record(1, s[2], lane=s[1])
                            else:
                                b.delete_area(R, *s[2:], lane=s[1])
                        if fail:
                            raise KeyError("planned")
                        st = b.flush()
                    self.diffs.eq(f"batch deleted db{i} {op}", st["deleted"], deleted)
                except KeyError as e:
                    if str(e) != "'planned'":
                        raise
            if not fail:
                self.m = trial
                lanes |= {s[1] for s in sub}
        elif t == "put_extensions":
            _, lane, u, items, merge = op
            ok = m.copy().put_extensions(lane, u, items, merge)
            for i, db in enumerate(self.dbs):
                try:
                    db.put_extensions(R, u, {NAMES[v]: x for v, x in items.items()}, merge=merge, lane=lane)
                    got = True
                except (KeyError, nhydb.NHyDBError, ValueError):
                    got = False
                self.diffs.eq(f"put_extensions accepted db{i} {op}", got, ok)
            if ok:
                m.put_extensions(lane, u, items, merge)
                lanes.add(lane)
        else:
            raise AssertionError(op)
        for lane in lanes:
            self.dirty[lane] = True

    def run(self, ops, rng=None, p_check=0.0):
        for op in ops:
            self.apply(op)
            if rng is not None and op[0] != "action" and rng.random() < p_check:
                self.check("overlay_uncommitted")
        self.check("end")
        self._reopen()
        self.check("end_reopened")
        rb, mr = _maint(self.A)
        self.maint[0] += rb
        self.maint[1] += mr
        self.close()


def _report(r, n=8):
    return f"{len(r.diffs)} deviations, first:\n" + "\n".join(r.diffs[:n])


ALL_STATES = {"fresh", "overlay_uncommitted", "commit", "rollback", "reopen", "compact_rebuild",
              "compact_norebuild", "build", "end", "end_reopened"}


# ----------------------------------------------------------------------
# 1. index against scan and model
# ----------------------------------------------------------------------

IX_STARTS = list(range(1, 131))
IX_BASE = [(0, 1, [[u % 50, (u * 7) % 5 - 2, " ".join(random.Random(u).sample(WORDS, 2))] for u in range(1, 121)]),
           (1, 1, [[u, u % 3, WORDS[u % len(WORDS)]] for u in range(1, 11)])]


@pytest.mark.parametrize("seed", range(1, 5))
@pytest.mark.parametrize("cfg", ["manual", "auto", "merge"])
def test_index_equals_scan_and_model_in_every_state(tmp_path, cfg, seed):
    rng = random.Random(f"ix-{cfg}-{seed}")
    r = Runner(tmp_path, cfg, ref=True, mode="queries", base=IX_BASE)
    r.run(gen_history(rng, 36, IX_STARTS, 0.1, 8), rng, p_check=0.05)
    assert not r.diffs, _report(r)
    assert ALL_STATES <= r.states, ALL_STATES - r.states
    rebuilds, merges = r.maint
    if cfg == "manual":
        assert (rebuilds, merges) == (0, 0)
    elif cfg == "merge":
        assert merges >= 10, r.maint            # the merge rebuild really ran
    else:
        assert rebuilds > 0, r.maint


def test_count_one_term_on_stale_index_equals_rows(tmp_path):
    """R9C-01: COUNT(*) with one TOKEN/FULLTEXT term on a stale index
    (default config: changes below the rebuild threshold → overlay)."""
    db = make_db(_path(tmp_path, "d", CFG["default"]))
    db.put_dense_batch(R, 1, [[u, u % 3, "beta" if u % 2 else "alpha"] for u in range(1, 21)], shape_id=1)
    db.build_indexes(commit=True)
    db.put_dense_batch(R, 5, [[u, 0, "gamma zz1"] for u in range(5, 10)], shape_id=1)
    db.delete_record(1, 11)
    for commit in (False, True):
        if commit:
            db.commit()
        for w, p in (("FULLTEXT(y, ?)", ["beta"]), ("TOKEN(y, ?)", ["beta"]), ("FULLTEXT(y, ?)", ["zz1"]), ("TOKEN(y, ?)", ["zz1"])):
            rows = db.sql(f"SELECT id FROM main WHERE {w}", p)["rows"]
            assert db.sql(f"SELECT COUNT(*) AS n FROM main WHERE {w}", p)["rows"] == [{"n": len(rows)}], (w, p, commit)
        assert len(db.sql("SELECT id FROM main WHERE TOKEN(y, ?)", ["beta"])["rows"]) == 6
        qe = QueryEngine(db)
        assert qe.execute(Q.field("y").fulltext("beta"), realm=R, return_mode="count")["count"] == 6
        assert qe.execute(Q.field("y").token("zz1"), realm=R, return_mode="count")["count"] == 5
        est = db.index_candidate_estimate(FY, "fulltext", "beta")
        assert not est["exact"] or est["estimated_rows"] == 6
    # R9C-H1: a TRIGRAM pattern shorter than the n-gram has no answer from the index
    with pytest.raises(ValueError):
        db.index_candidates(GY, "contains", "al")
    assert db.sql("SELECT id FROM main WHERE y CONTAINS ?", ["al"])["rows"] == [{"id": u} for u in range(2, 21, 2) if u not in range(5, 10)]
    db.close()


# ----------------------------------------------------------------------
# 2. u >= 2^64 with a merge rebuild at every commit (R9A-01)
# ----------------------------------------------------------------------

def test_big_u_with_merge_rebuild(tmp_path):
    base = [(0, 1, [[u, u % 3, WORDS[u % len(WORDS)]] for u in range(1, 41)]),
            (1, 1, [[u, u % 3, "alpha"] for u in range(1, 6)])]
    r = Runner(tmp_path, "merge", ref=True, mode="queries", base=base)
    b0, b1, b2, b3 = BIG
    steps = [
        [("put_cell", 0, b0, V_EXT, 1, "varuint")],
        [("put_cell", 0, b1, V_EXT, 1, "varuint")],                     # no indexed value: 8-byte fast path
        [("put_cell", 0, b2, V_EXT, 2, "varuint"), ("put_cell", 0, 7, 2, 0, "varint")],
        [("put_cell", 0, b3, V_EXT, 0, "varuint"), ("put_cell", 1, b1, V_EXT, 1, "varuint")],
        [("put", "put", 0, b2, 1, [[1, 0, "beta zz1"]])],                # indexed value at u > 2^64 (width 16)
        [("put_cell", 0, b3, 2, 0, "varint"), ("put_cell", 0, b0, 3, "alpha zz2", "utf8")],
        [("delete_cell", 0, 3, 2), ("put_cell", 0, 9, 3, "gamma", "utf8")],
        [("delete_record", 0, b2)],
        [("delete_cell", 0, b3, 2), ("delete_area", 0, b0, b3, V_EXT, V_EXT)],
        [("put_cells_numpy", 0, [2, 4, 6], 2, [0, 1, 2])],
    ]
    merges = []
    for ops in steps:
        for op in ops:
            r.apply(op)
        r.check("overlay_uncommitted")
        mr0 = _maint(r.A)[1]
        r.action("commit")
        merges.append(_maint(r.A)[1] - mr0)
        r.diffs += diff_cells(r.A, r.m, "big u", r.stale_ok, domain=[1, 2, 3, 7] + BIG,
                              windows=((2**64 - 2, 5), (10**30 - 1, 3)), full=False)
    r.action("reopen")
    r.action("compact_rebuild")
    r.run([])
    assert not r.diffs, _report(r)
    assert all(x >= 1 for x in merges), merges     # every commit merged


# ----------------------------------------------------------------------
# 3. every SQL output path returns the same
# ----------------------------------------------------------------------

def _np_rows(d):
    names = list(d)
    if not names:
        return []
    return [dict(zip(names, t)) for t in zip(*[d[n].tolist() for n in names])]


def _paths(db, q, p, qexpr):
    """Result of every output path as ("ok", rows) or ("err", class, text)."""
    out = {"sql": _call(lambda: db.sql(q, p)["rows"]),
           "sql_iter": _call(lambda: list(db.sql_iter(q, p))),
           "sql_numpy": _call(lambda: _np_rows(db.sql_numpy(q, p)))}

    def to_file():
        with db.sql_to_file(q, p) as rf:
            return list(rf)

    def submit():
        with db.submit_sql(q, p).wait(60) as rf:
            return rf.fetchall()
    out["sql_to_file"] = _call(to_file)
    out["submit_sql"] = _call(submit)
    out["engine_sql"] = _call(lambda: QueryEngine(db).execute_sql(q, p)["rows"])
    if qexpr is not None:
        expr, kw = qexpr
        res = _call(lambda: QueryEngine(db).execute(expr(), realm=R, return_mode="fields", select=list(COLS), **kw)["rows"])
        if res[0] == "ok":
            res = ("ok", [{k: v for k, v in row.items() if k != "record_N"} for row in res[1]])
        out["engine_q"] = res
    return out


SORT_WHERE = [
    (None, [], lambda c: True, lambda: Q.all()),
    ("x >= ?", [0], lambda c: p_cmp(X(c), ">=", 0), lambda: Q.field("x") >= 0),
    ("x = ?", [1], lambda c: p_eq(X(c), 1), lambda: Q.field("x") == 1),
    ("y CONTAINS ?", ["alp"], lambda c: p_contains(Y(c), "alp"), lambda: Q.field("y").contains("alp")),
    ("id IS NULL", [], lambda c: 1 not in c, lambda: ~Q.field("id").exists()),
    ("x >= ? OR y >= ?", [0, ""], lambda c: p_cmp(X(c), ">=", 0) or p_cmp(Y(c), ">=", ""),
     lambda: (Q.field("x") >= 0) | (Q.field("y") >= "")),
    ("NOT (x = ?)", [2], lambda c: not p_eq(X(c), 2), lambda: ~(Q.field("x") == 2)),
    ("TOKEN(y, ?)", ["beta"], lambda c: p_token(Y(c), "beta"), lambda: Q.field("y").token("beta")),
]
ORDERS = [None, ("x", False), ("x", True), ("y", False), ("y", True), ("id", False), ("id", True)]
LIMITS = [None, (7, None), (20, 5)]


def _sort_db(path, flavor):
    """DENSE records (two shapes), pure SPARSE records (put_cell only) or
    both mixed with overrides, stored None and deletes; model alongside."""
    rng = random.Random(f"sort-{flavor}")
    db = make_db(path)
    m = Model()
    n = 1800
    if flavor in ("dense", "mixed"):
        top = n if flavor == "dense" else 1200
        rows = [[u % 40, rng.randint(-3, 3), _text(rng, False)] for u in range(1, top + 1)]
        db.put_dense_batch(R, 1, rows, shape_id=1)
        rows2 = [[u % 40, rng.randint(-3, 3)] for u in range(300, 400)]
        db.put_dense_batch(R, 300, rows2, shape_id=2)
        for u, row in enumerate(rows, 1):
            m.put(0, u, 1, row)
        for u, row in enumerate(rows2, 300):
            m.put(0, u, 2, row)
        db.build_indexes(commit=True)

    def cell(u, v, x, t):
        db.put_cell(R, u, v, x, type=t)
        m.put_cell(0, u, v, x)
    if flavor in ("sparse", "mixed"):
        for u in range(1 if flavor == "sparse" else 1201, n + 1):
            wrote = False
            for v, p in ((2, 0.8), (3, 0.7), (1, 0.3), (V_EXT, 0.2)):
                if rng.random() < p:
                    x = {1: u % 40, 2: rng.randint(-3, 3), 3: _text(rng, False), V_EXT: rng.randrange(3)}[v]
                    cell(u, v, None if rng.random() < 0.05 else x, TYPES[v])
                    wrote = True
            if not wrote:
                cell(u, 2, rng.randint(-3, 3), "varint")
    if flavor == "mixed":
        for u in rng.sample(range(1, 1200), 200):
            cell(u, 2, rng.randint(-3, 3), "varint")
        for u in rng.sample(range(1, 1200), 20):
            cell(u, 2, None, "varint")
        for u in rng.sample(range(1, 1200), 100):
            assert db.delete_cell(R, u, 3) == m.delete_cell(0, u, 3)
        for u in rng.sample(range(1, 1800), 30):
            assert db.delete_record(1, u) == m.delete_record(0, u)
    if flavor == "sparse":
        db.build_indexes()
    db.commit()
    return db, m


def _model_rows(m, where_pred, order, limit):
    rows = [row_of(c) for _, c in m.records(0) if where_pred(c)]
    if order is not None:
        col, desc = order
        # stable: ties keep u order; None last ascending, first descending
        try:
            rows = sorted(rows, key=lambda r: (r[col] is None, r[col] if r[col] is not None else 0), reverse=desc)
        except TypeError:
            return None
    if limit is not None:
        lim, off = limit
        rows = rows[(off or 0):(off or 0) + lim]
    return rows


@pytest.mark.parametrize("sortmem", ["small", "default"])
@pytest.mark.parametrize("flavor", ["dense", "sparse", "mixed"])
def test_every_sql_output_path_returns_the_same(tmp_path, flavor, sortmem):
    db, m = _sort_db(_path(tmp_path, "d", SMALL_SORT if sortmem == "small" else ""), flavor)
    bad = []
    n = 0
    for i, ((w, p, pred, qx), order) in enumerate(itertools.product(SORT_WHERE, ORDERS)):
        limit = LIMITS[i % len(LIMITS)]
        q = "SELECT id, x, y, ext FROM main" + (f" WHERE {w}" if w else "")
        kw = {}
        if order is not None:
            q += f" ORDER BY {order[0]}{' DESC' if order[1] else ''}"
            kw["order_by"] = [("-" if order[1] else "") + order[0]]
        if limit is not None:
            q += f" LIMIT {limit[0]}" + (f" OFFSET {limit[1]}" if limit[1] is not None else "")
            kw["limit"] = limit[0]
            if limit[1] is not None:
                kw["offset"] = limit[1]
        res = _paths(db, q, p, (qx, kw))
        n += len(res)
        want = _model_rows(m, pred, order, limit)
        if res["sql"] != ("ok", want):
            bad.append(f"sql vs model {q} {p}: {res['sql']!r:.300} want {want!r:.300}")
        for k, v in res.items():
            if v != res["sql"]:
                bad.append(f"{k} != sql: {q} {p}: {v!r:.300} vs {res['sql']!r:.300}")
    # errors: same class and text on every SQL path
    db.put_cell(R, 5, V_EXT, "zz", type="utf8")
    db.put_cell(R, 6, V_EXT, 1, type="varuint")
    db.commit()
    for q, cls in (("SELECT id, ext FROM main ORDER BY ext", "QueryPlanError"),
                   ("SELECT id, ext FROM main WHERE id >= 0 OR id IS NULL ORDER BY ext DESC LIMIT 5", "QueryPlanError"),
                   ("SELECT id FROM main WHERE x IN (1, 2)", "SQLSyntaxError")):
        res = _paths(db, q, [], None)
        n += len(res)
        if res["sql"][:2] != ("err", cls):
            bad.append(f"sql error {q}: {res['sql']!r:.200}")
        for k, v in res.items():
            if v != res["sql"]:
                bad.append(f"{k} error != sql: {q}: {v!r:.200} vs {res['sql']!r:.200}")
    qe = _call(lambda: QueryEngine(db).execute(Q.all(), realm=R, return_mode="fields", select=["id", "ext"], order_by=["ext"]))
    if qe[:2] != ("err", "QueryPlanError"):
        bad.append(f"engine order_by ext: {qe!r:.200}")
    db.close()
    assert not bad, f"{len(bad)} of {n} deviate:\n" + "\n".join(bad[:8])


def test_streaming_order_by_keeps_pure_sparse_records(tmp_path):
    """R9C-02 repro: WHERE + ORDER BY on the streaming paths."""
    db = make_db(_path(tmp_path, "d", SMALL_SORT))
    db.put_dense_batch(R, 1, [[u, 1, "a"] for u in range(1, 6)], shape_id=1)
    db.put_cell(R, 10, 3, "zz", type="utf8")
    db.put_cell(R, 11, 1, 11)
    db.commit()
    for q, want in (("SELECT id, y FROM main WHERE y = 'zz' ORDER BY id", [{"id": None, "y": "zz"}]),
                    ("SELECT id, y FROM main WHERE id = 11 ORDER BY y", [{"id": 11, "y": None}]),
                    ("SELECT id FROM main WHERE id >= 0 OR y >= '' ORDER BY id DESC", None)):
        res = _paths(db, q, [], None)
        assert all(v == res["sql"] for v in res.values()), (q, res)
        if want is not None:
            assert res["sql"] == ("ok", want)
        else:
            assert len(res["sql"][1]) == 7
    db.close()


# ----------------------------------------------------------------------
# 4. F1 differential test of all read paths
# ----------------------------------------------------------------------

F1_STARTS = list(range(1, 9)) + [2**64 - 1, 2**64, 2**64 + 1]
F1_BASE = [(0, 5, [[0, 0, "alpha"]]), (1, 5, [[0, 0, "alpha"]])]


@pytest.mark.parametrize("seed", range(8))
@pytest.mark.parametrize("cfg", ["default", "general", "merge"])
def test_f1_random_history_equals_model(tmp_path, cfg, seed):
    rng = random.Random(f"f1-{cfg}-{seed}")
    r = Runner(tmp_path, cfg, ref=False, mode="cells", base=F1_BASE)
    r.run(gen_history(rng, 30, F1_STARTS, 0.25, 3, burst=(2, 5)), rng, p_check=0.2)
    assert not r.diffs, _report(r)
    assert ALL_STATES <= r.states, ALL_STATES - r.states


SENSITIVITY_OPS = [
    ("put", "dense_batch", 0, 3, 1, [[1, 1, "alpha"]]),
    ("delete_cell", 0, 3, 3),                                  # ignore_tombstone: model keeps y
    ("put_cell", 0, 4, 2, 5, "varint"),
    ("put", "put", 0, 4, 1, [[1, 1, "beta"]]),                 # sparse_on_top: model keeps x=5
    ("put", "dense_batch", 0, 6, 1, [[1, 1, "gamma"]]),
    ("put", "dense_batch", 0, 6, 2, [[1, 2]]),                 # keep_old_shape: model keeps y
    ("action", "none"),
    ("action", "commit"),
]


@pytest.mark.parametrize("mode", ["cells", "queries"])
@pytest.mark.parametrize("mutation", [None, "ignore_tombstone", "sparse_on_top", "keep_old_shape"])
def test_harness_detects_a_wrong_model(tmp_path, mutation, mode):
    """Counter check: a model that breaks one rule must be reported; the
    correct model (None) must not."""
    r = Runner(tmp_path, "default", ref=mode == "queries", mode=mode, mutation=mutation,
               base=[(0, 1, [[1, 0, "pi"], [1, 1, "pi"]])])
    r.run(SENSITIVITY_OPS)
    if mutation is None:
        assert not r.diffs, _report(r)
    else:
        assert r.diffs, f"harness ({mode}) did not notice the wrong model '{mutation}'"


# ----------------------------------------------------------------------
# 5. exhaustive: all sequences of length 3 on one cell group, lanes 0 and 1
# ----------------------------------------------------------------------

U = 3
ALPHA = [
    ("put", "dense_batch", U, 1, [[1, 1, "alpha"]]),
    ("put", "dense_batch", U, 2, [[2, 2]]),                     # shape change
    ("put", "dense_batch", U - 1, 1, [[0, -1, "beta"], [0, -2, "gamma"]]),   # segment over u=2..3
    ("put_cell", U, 2, 0, "varint"),
    ("put_cell", U, 2, None, "varint"),
    ("put_cell", U, 3, "beta", "utf8"),
    ("put_cell", U, V_EXT, 1, "varuint"),
    ("delete_cell", U, 2),
    ("delete_cell", U, 3),
    ("delete_record", U),
    ("delete_area", U, U, 2, 3),
]


def _lane_op(op, lane):
    if op[0] == "put":
        return op[:2] + (lane,) + op[2:]
    return (op[0], lane) + op[1:]


@pytest.mark.parametrize("first", range(len(ALPHA)))
def test_f1_exhaustive_length_3(tmp_path, first):
    """Every sequence (first, b, c) of ALPHA, applied to lane 0 and lane 1,
    checked after each step, after the commit (merge rebuild) and after
    compact_file + reopen."""
    bad = []
    n = 0
    for b, c in itertools.product(range(len(ALPHA)), repeat=2):
        d = tmp_path / f"s{b}_{c}"
        d.mkdir()
        r = Runner(d, "merge", ref=False, mode="cells", base=[(0, 5, [[0, 0, "pi"]]), (1, 5, [[0, 0, "pi"]])], text=False)
        r.check_kw = {"domain": [2, 3, 4], "windows": ((1, 5),)}
        for i in (first, b, c):
            for lane in (0, 1):
                r.apply(_lane_op(ALPHA[i], lane))
            r.check("overlay_uncommitted")
        r.action("commit")
        r.action("compact_rebuild")
        r.close()
        n += 1
        if r.diffs:
            bad.append(f"{(first, b, c)}: " + "; ".join(r.diffs[:3]))
    assert n == len(ALPHA) ** 2
    assert not bad, f"{len(bad)} of {n} sequences deviate:\n" + "\n".join(bad[:5])


# ----------------------------------------------------------------------
# deviations found by this harness (minimal cases)
# ----------------------------------------------------------------------

def test_fulltext_maintenance_drops_field_removed_by_shape_change(tmp_path):
    """Found by this harness (wide run, merge seed 208), fixed: the full
    build of the FULLTEXT index for lane 1 in the same maintenance commit
    rebuilt lane 0 too and indexed the old y of u=5, whose newer record of
    shape B lacks y (same cause as the next test)."""
    p = _path(tmp_path, "d", CFG["merge"])
    db = make_db(p)
    db.put_dense_batch(R, 1, [[u, 0, "alpha"] for u in range(1, 11)], shape_id=1)
    db.put_dense_batch(R, 1, [[1, 0, "alpha"]], shape_id=1, lane=1)
    db.commit()
    db.build_indexes(commit=True)
    db.delete_area(R, 1, 1, 2, 3, lane=1)            # lane 1: only the id cell of u=1 stays
    db.close()
    nhydb.NHyDB.compact_file(p, make_backup=False)
    db = nhydb.NHyDB(p)
    db.put_dense_batch(R, 5, [[5, 1]], shape_id=2)   # shape B: y of u=5 is gone
    db.put_cell(R, 9, 1, 9, type="varuint", lane=1)  # a write in lane 1 in the same commit
    want = [u for u in range(1, 11) if u != 5]
    assert [r["id"] for r in db.sql("SELECT id FROM main WHERE FULLTEXT(y, 'alpha')")["rows"]] == want   # overlay: right
    db.commit()                                      # lane 0: merge [.. 6 ..], lane 1: full [6]
    try:
        assert [r["id"] for r in db.sql("SELECT id FROM main WHERE y CONTAINS 'alpha'")["rows"]] == want
        assert list(db.index_candidates(FY, "fulltext", "alpha")) == want
        assert db.sql("SELECT COUNT(*) AS n FROM main WHERE FULLTEXT(y, 'alpha')")["rows"] == [{"n": 9}]
    finally:
        db.close()


@pytest.mark.parametrize("kind", ["value", "token"])
def test_build_drops_field_removed_by_shape_change(tmp_path, kind):
    """Found by this harness, fixed: with every index of the realm on y, the
    build skipped the newer record of shape B (no indexed field) without
    marking its u, so the older record of shape A indexed its y."""
    db = nhydb.NHyDB.create(_path(tmp_path, "d", CFG["manual"]))
    db.create_realm(R, "r")
    db.create_shape(1, R, "A", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"},
                                 {"v": 3, "name": "y", "type": "utf8"}])
    db.create_shape(2, R, "B", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}])
    db.create_classic_layout(1, R, "main", shape_id=1)
    if kind == "value":
        db.create_index(IY, R, "iy", "y")            # (with a second index on x the build is right)
    else:
        db.create_token_index(IY, R, "iy", "y")
    db.commit()
    db.put_dense_batch(R, 1, [[u, 0, "alpha"] for u in range(1, 4)], shape_id=1)
    db.commit()
    db.put_dense_batch(R, 2, [[2, 1]], shape_id=2)   # shape B: y of u=2 is gone
    db.commit()
    db.build_indexes(commit=True)
    try:
        assert db.get(R, 2)["y"] is None
        assert [r["id"] for r in db.sql("SELECT id FROM main WHERE y CONTAINS 'alph'")["rows"]] == [1, 3]
        assert list(db.index_candidates(IY, "eq" if kind == "value" else "token", "alpha")) == [1, 3]
        if kind == "value":
            assert [r["id"] for r in db.sql("SELECT id FROM main WHERE y = 'alpha'")["rows"]] == [1, 3]
    finally:
        db.close()
