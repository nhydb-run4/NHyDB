"""MATH_MAP: expressions, mapping targets, MATH layouts, persistence and
resource limits."""
import pytest

import nhydb
import nhydb.math as rm

BIG = 10**100  # REALM_SPAN


def test_expression_evaluate():
    assert rm.MathExpression.compile("2**3 + gcd(x, 6)", ("x",)).evaluate(x=4) == 10
    with pytest.raises(nhydb.MathSyntaxError):
        rm.MathExpression.compile("x if x else 1", ("x",))


def test_mathmap_target():
    m = rm.MathMap.compile("grid", ("x", "y"), u="x+1", V="y", side="x % 2")
    t = m.target(3, 5)
    assert (t.u, t.V_global, t.logical_realm, t.v_local, t.side) == (4, 5, 1, 5, 1)
    t = m.target(3, BIG + 1)
    assert (t.logical_realm, t.v_local) == (2, 1)


def test_math_layout_write_read_reopen(tmp_path):
    p = str(tmp_path / "m.nhydb")
    m = rm.MathMap.compile("grid", ("x", "y"), u="x+1", V="y", side="x % 2")
    db = nhydb.NHyDB.create(p)
    db.create_math_layout(rm.MathMapLayoutSpec("grid", m, 4096), layout_id=7, payload_type="utf8")
    rep = db.put_math_batch(7, [((x, y), f"{x},{y}") for x in range(1, 5) for y in (1, BIG + 1)], commit=True)
    assert rep["cells"] == 8 and rep["new_realms"] == 2
    assert db.get_math(7, (3, BIG + 1)) == f"3,{BIG + 1}"
    assert db.get_math(7, (9, 9)) is None
    cell = db.get_math_cell(7, (3, BIG + 1))
    assert (cell["_u"], cell["_lane_name"], cell["_logical_realm"]) == (4, "NR", 2)
    db.close()
    db = nhydb.NHyDB(p)
    assert db.get_math(7, (4, 1)) == "4,1"
    assert db.verify()["ok"]
    db.close()


def test_intermediate_result_guard():
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
