"""Differenztests MATH_MAP und Realmplaner gegen Python-v0.27."""
import importlib
import random

import pytest

import nhydb
import nhydb.math as rm
import nhydb.mathlayout as rl
from conftest import blocks
from ref_v027 import diff


@pytest.fixture(scope="module")
def pm(ref):
    return importlib.import_module("nhydb_math_v027")


@pytest.fixture(scope="module")
def pl(ref):
    return importlib.import_module("nhydb_layout_v027")


def _outcome(fn, *a, **k):
    try:
        r = fn(*a, **k)
    except Exception as e:  # noqa: BLE001
        return ("exc", type(e).__name__, str(e))
    return ("ok", r)


def _target_tuple(t):
    return (t.inputs, t.u, t.V_global, t.logical_realm, t.v_local, t.side)


EXPRESSIONS = [
    "x+1", "x*y+1", "x**2 - y", "(x+y)//3 + 1", "x % 7 + 1", "-x + 10**6", "abs(x - y) + 1",
    "min(x, y) + max(x, 3)", "x << 3", "(x ^ y) | 1", "x & 255", "~x + 2**40", "1 if x > y else 2",
    "x if x >= 1 and y else y + 1", "not x", "x == y", "x // 0", "x ** 300", "2 ** (x * 1000)",
    "x % y", "pow(x, 2) + 1", "x.real", "x[0]", "lambda: 1", "x +", "'a'", "1.5", "True + x",
    "x < y < 10", "(x, y)", "y >> 2", "x ** -1", "__import__('os')", "x if y else",
]


@pytest.mark.parametrize("src", EXPRESSIONS)
def test_expression_compile_and_evaluate(pm, src):
    a = _outcome(pm.MathExpression.compile, src, ("x", "y"))
    b = _outcome(rm.MathExpression.compile, src, ("x", "y"))
    assert a[0] == b[0] and repr(a[1]) == repr(b[1]), (src, a, b)
    if a[0] == "exc":
        if a[2].endswith("(<unknown>, line 1)"):
            # Text des CPython-Parsers (F5: abweichende interne Fehlertexte zulässig)
            assert a[1] == b[1] == "MathSyntaxError"
        else:
            assert a == b
        return
    pe, re_ = a[1], b[1]
    assert pe.to_dict() == re_.to_dict()
    rnd = random.Random(src)
    # Python-v0.27 hat keine Rechengrenze (S6): unbeschränkte Fälle nur in Rust prüfen
    xs = [0, 1, -1, 5, -3] if "1000" in src else [0, 1, -1, 5, 2**70, -3]
    for _ in range(40):
        x, y = rnd.choice(xs), rnd.choice([0, 2, 7, -9, 3**50])
        assert _outcome(pe.evaluate, x=x, y=y) == _outcome(re_.evaluate, x=x, y=y), (src, x, y)


def test_intermediate_guard_rust_only():
    e = rm.MathExpression.compile("2 ** (x * 1000)", ("x",))
    with pytest.raises(nhydb.MathDomainError):
        e.evaluate(x=2**70)
    old = rm.get_math_max_intermediate_bits()
    try:
        rm.set_math_max_intermediate_bits(64)
        with pytest.raises(nhydb.MathDomainError):
            e.evaluate(x=1)
    finally:
        rm.set_math_max_intermediate_bits(old)
    assert e.evaluate(x=1) == 2**1000


MAPS = [
    ("m1", ("x",), "x+1", "x+1", "NL"),
    ("m2", ("x", "y"), "x+1", "y*10**100 + x + 1", "NR"),
    ("m3", ("a", "b"), "a*b+1", "a+b", "a % 2"),
    ("m4", ("n",), "n", "n*n", 1),
    ("bad", ("x",), "x", "x - 10", "NL"),
    ("bad2", ("x",), "x+1", "x+1", "x+5"),
]


@pytest.mark.parametrize("name,params,u,V,side", MAPS)
def test_mathmap_targets_identical(pm, name, params, u, V, side):
    p = pm.MathMap.compile(name, params, u=u, V=V, side=side)
    r = rm.MathMap.compile(name, params, u=u, V=V, side=side)
    assert p.fingerprint == r.fingerprint
    assert p.canonical_dict() == r.canonical_dict()
    assert p.to_dict() == r.to_dict()
    assert rm.MathMap.from_dict(p.to_dict()).fingerprint == p.fingerprint
    rnd = random.Random(name)
    for _ in range(200):
        args = tuple(rnd.choice([1, 2, 3, 0, -1, 10**99, 10**100, 10**101 + 7, rnd.randrange(1, 10**120)]) for _ in params)
        a = _outcome(p.target, *args)
        b = _outcome(r.target, *args)
        if a[0] == "ok":
            assert b[0] == "ok" and _target_tuple(a[1]) == _target_tuple(b[1]), args
        else:
            assert a == b, args
    spec_p = pm.MathMapLayoutSpec(name, p, 17, "d")
    spec_r = rm.MathMapLayoutSpec(name, r, 17, "d")
    assert spec_p.fingerprint == spec_r.fingerprint
    assert spec_p.to_dict() == spec_r.to_dict()


def test_dry_run_identical(pm):
    rows = [(x, y) for x in range(-2, 30) for y in (1, 2, 10**100)]
    p = pm.MathMap.compile("d", ("x", "y"), u="x % 5 + 1", V="y*x + 1", side="x % 2")
    r = rm.MathMap.compile("d", ("x", "y"), u="x % 5 + 1", V="y*x + 1", side="x % 2")
    a = pm.dry_run(p, rows, realm_auto_limit=8)
    b = rm.dry_run(r, rows, realm_auto_limit=8)
    for f in ("total", "valid", "invalid", "unique_cells", "collisions", "collision_examples", "logical_realms",
              "logical_realm_extents", "realm_count", "realm_budget_exceeded", "min_u", "max_u", "min_V", "max_V",
              "nl_count", "nr_count", "invalid_examples"):
        assert getattr(a, f) == getattr(b, f), f


def test_split_join_identical(pm):
    for v in [1, 10**100, 10**100 + 1, 5 * 10**100, 0, -1, True, "3"]:
        assert _outcome(pm.split_global_v, v) == _outcome(rm.split_global_v, v), v
    for r, v in [(1, 1), (2, 10**100), (0, 1), (1, 0), (3, 10**100 + 1)]:
        assert _outcome(pm.join_global_v, r, v) == _outcome(rm.join_global_v, r, v), (r, v)
    for s in ["NL", "nr", 0, 1, 2, "x", True, None]:
        assert _outcome(pm.normalize_side, s) == _outcome(rm.normalize_side, s), s


def _planner(mod, needed, limit=8):
    snap = mod.CoreLayoutSnapshot(5, (mod.OwnedRealmExtent(1, 3, "CORE", 1), mod.OwnedRealmExtent(6, 6, "CORE", 1)))
    m = mod.MathRealmMap(1, "fp", (), limit)
    plan = mod.plan_math_realms(snap, m, needed)
    out = (plan.allowed, plan.reason, plan.missing_realms, plan.missing_logical_extents,
           [e.to_dict() for e in plan.proposed_extents], [e.to_dict() for e in plan.resulting_extents],
           plan.new_count, plan.resulting_count)
    if plan.allowed:
        mod.validate_plan(snap, m, plan)
        s2, m2 = mod.commit_plan_simulated(snap, m, plan)
        out += (s2.to_dict(), m2.to_dict(), m2.state_fingerprint)
        mod.validate_map_against_snapshot(s2, m2)
    return out


@pytest.mark.parametrize("needed", [(1,), (1, 2, 3), (1, 5, 6, 7), tuple(range(1, 10)), (100, 101, 300)])
def test_planner_identical(pl, needed):
    assert _planner(pl, needed) == _planner(rl, needed)


def test_planner_errors_identical(pl):
    for args in [(0, 1, "CORE", 1), (2, 1, "CORE", 1), (1, 2, "X", 1), (1, 2, "MATH", 0)]:
        a = _outcome(pl.OwnedRealmExtent, *args)
        b = _outcome(rl.OwnedRealmExtent, *args)
        assert a[0] == b[0] and (a[0] == "ok" or a == b), args
    for args in [(1, 2, 5, 6), (1, 2, 5, 7), (0, 1, 1, 2), (3, 2, 1, 0)]:
        a = _outcome(pl.MathRealmExtent, *args)
        b = _outcome(rl.MathRealmExtent, *args)
        assert a[0] == b[0] and (a[0] == "ok" or a == b), args
    snap_p = pl.CoreLayoutSnapshot(1, ())
    snap_r = rl.CoreLayoutSnapshot(1, ())
    a = _outcome(lambda: pl.plan_math_realms(snap_p, pl.MathRealmMap(1, "f", (), 2), (1, 2, 3)).require_allowed())
    b = _outcome(lambda: rl.plan_math_realms(snap_r, rl.MathRealmMap(1, "f", (), 2), (1, 2, 3)).require_allowed())
    assert a == b


SCENARIO = r'''
from nhydb_v027 import NHyDB, MathMap, MathMapLayoutSpec
M = 10**100
def _name(r):  # Python: RealmDef, Rust: dict
    return r["name"] if isinstance(r, dict) else r.name
db = NHyDB.create("m.nhydb")
db.create_realm(1, "core")
m = MathMap.compile("grid", ("x", "y"), u="x+1", V="y", side="x % 2")
st = db.create_math_layout(MathMapLayoutSpec("grid", m, 16, "demo"), layout_id=7, payload_type="utf8")
emit("state", (st.layout_id, st.frozen, st.payload_type))
attempt("dup_name", db.create_math_layout, MathMapLayoutSpec("grid", m, 16), layout_id=8)
attempt("dup_id", db.create_math_layout, MathMapLayoutSpec("grid2", m, 16), layout_id=7)
m2 = MathMap.compile("grid", ("x", "y"), u="x+2", V="y", side="NR")
emit("replaced", db.replace_math_spec(7, MathMapLayoutSpec("grid", m2, 16)).frozen)
m3 = MathMap.compile("grid", ("x", "y"), u="x+1", V="y", side="x % 2")
db.replace_math_spec(7, MathMapLayoutSpec("grid", m3, 16, "demo"))
emit("put", db.put_math_batch(7, [((x, y), f"v{x}-{y}") for x in range(1, 6) for y in (1, M + 1, 3 * M + 5)]))
emit("put1", db.put_math(7, (9, 2), "single"))
attempt("conflict", db.put_math, 7, (9, 2), "again")
attempt("dup_batch", db.put_math_batch, 7, [((1, 1), "a"), ((1, 1), "b")])
emit("overwrite", db.put_math(7, (9, 2), "again", overwrite=True))
attempt("frozen", db.replace_math_spec, 7, MathMapLayoutSpec("grid", m2, 16))
phys = db.describe_math(7)["realm_extents"][0]["physical_start"]
attempt("core_in_math", lambda: db.create_realm(phys, "x") and _name(db.realms[phys]))
attempt("shape_in_math", db.create_shape, 1, phys, "s", [{"v": 1, "name": "a", "type": "u8"}])
attempt("points_in_math", db.put_points, phys, [(1, 1, 5, "varuint")])
attempt("ext_in_math", db.define_extension, phys, 3, "utf8")
attempt("index_in_math", db.create_index, 5, phys, "i", 3)
attempt("budget", db.put_math_batch, 7, [((1, k * M + 1), 1) for k in range(10, 40)])
emit("get", [db.get_math(7, (x, y)) for x in range(1, 6) for y in (1, M + 1, 3 * M + 5, 7 * M)])
c = db.get_math_cell(7, (2, M + 1))
emit("cell", {k: c[k] for k in sorted(c) if k.startswith("_") or k == "value"})
emit("missing", db.get_math_cell(7, (2, 50 * M)))
d = db.describe_math(7)
emit("describe", {k: d[k] for k in sorted(d) if k not in ("created_block_id", "first_write_block_id")})
attempt("nolayout", db.get_math, 99, (1, 1))
attempt("bad_in", db.put_math, 7, (0, 1), "x")
attempt("bad_arity", db.put_math, 7, (1,), "x")
emit("health", db.health()["ok"])
db.commit()
db.close()
db = NHyDB("m.nhydb")
emit("reopen", [db.get_math(7, (x, y)) for x in range(1, 6) for y in (1, M + 1, 3 * M + 5)])
emit("reopen_describe", db.describe_math(7)["realm_extents"])
emit("realms", sorted(db.realms))
db.close()
'''


def test_math_db_scenario_identical(tmp_path):
    py, rs = diff.compare(SCENARIO, tmp_path)
    assert len(py) == len(rs)
    for a, b in zip(py, rs):
        assert a == b, (a, b)
    # physische Blockfolge identisch (SYSTEM-Blöcke nur Typ/ID)
    assert blocks(tmp_path / "rs" / "m.nhydb") == blocks(tmp_path / "py" / "m.nhydb")


def test_put_math_batch_rollback_on_size_limit(tmp_path):
    p = tmp_path / "r.nhydb"
    db = nhydb.NHyDB.create(str(p))
    m = rm.MathMap.compile("g", ("x", "y"), u="x", V="y", side="NL")
    db.create_math_layout(rm.MathMapLayoutSpec("g", m, 64), layout_id=3, payload_type="utf8")
    db.put_math_batch(3, [((x, 1), f"a{x}") for x in range(1, 50)], commit=True)
    before = (db.describe_math(3), sorted(db.realms), db.next_block_id)
    db.f.seek(0, 2)
    size = db.f.tell()
    db.set_max_db_size(size + 4096)
    db.commit()
    db.f.seek(0, 2)
    size = db.f.tell()
    big = [((x, 10**100 * k + 1), "z" * 3000) for k in (2, 3) for x in range(1, 4)]
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.put_math_batch(3, big)
    db.f.seek(0, 2)
    assert db.f.tell() == size
    assert db.describe_math(3)["realm_extents"] == before[0]["realm_extents"]
    assert sorted(db.realms) == before[1]
    assert db.get_math(3, (7, 1)) == "a7"
    assert db.get_math(3, (1, 2 * 10**100 + 1)) is None
    assert db.health()["ok"] is True
    db.put_math(3, (60, 1), "after", commit=True)
    db.close()
    db = nhydb.NHyDB(str(p))
    assert db.get_math(3, (60, 1)) == "after" and db.get_math(3, (49, 1)) == "a49"
    db.close()


def _rand_expr(rnd, d):
    if d == 0 or rnd.random() < 0.3:
        return rnd.choice(["x", "y", "x", "y", str(rnd.choice([0, 1, 2, 3, 7, 100, 2**31, 2**62, 9223372036854775807, 10**18]))])
    op = rnd.random()
    a, b = _rand_expr(rnd, d - 1), _rand_expr(rnd, d - 1)
    if op < 0.55:
        return f"({a} {rnd.choice(['+', '-', '*', '//', '%', '+', '-', '*'])} {b})"
    if op < 0.65:
        return f"({a} ** {rnd.choice(['0', '1', '2', '3', '5', '63', '64', '100'])})"
    if op < 0.75:
        return f"-({a})"
    if op < 0.9:
        f = rnd.choice(["abs", "min", "max", "gcd", "isqrt"])
        return f"{f}({a})" if f in ("abs", "isqrt") else f"{f}({a}, {b})"
    return f"max({a}, min({b}, {a} - {b}))"


@pytest.mark.parametrize("seed", range(4))
def test_fast_path_targets_identical(pm, seed):
    """Schnellpfad (i64 mit BigInt-Hochstufung, Konstantenfaltung) gegen Python."""
    rnd = random.Random(1000 + seed)
    edge = [0, 1, -1, 2, -2, 99, 2**31, -(2**31), 2**62, -(2**62), 9223372036854775807, -9223372036854775808,
            9223372036854775806, 3037000499, 3037000500, -3037000500, 10**17]
    for _ in range(120):
        e = _rand_expr(rnd, 3)
        src_u = f"abs({e}) + 1"
        src_v = rnd.choice([f"abs({e}) % 1000 + 1", f"(abs({e}) % 7) * 10**100 + 5", f"abs({e}) + 1"])
        side = rnd.choice(["NL", "x % 2", "x > y"])
        try:
            p = pm.MathMap.compile("t", ("x", "y"), u=src_u, V=src_v, side=side)
        except Exception:  # noqa: BLE001
            continue
        r = rm.MathMap.compile("t", ("x", "y"), u=src_u, V=src_v, side=side)
        for _ in range(12):
            args = (rnd.choice(edge), rnd.choice(edge))
            a = _outcome(p.target, *args)
            b = _outcome(r.target, *args)
            if a[0] == "ok":
                assert b[0] == "ok" and _target_tuple(a[1]) == _target_tuple(b[1]), (src_u, src_v, side, args)
            else:
                assert a == b, (src_u, src_v, side, args)
