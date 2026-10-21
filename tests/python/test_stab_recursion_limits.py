"""Nesting limits of the query layer (T009; findings F015, F016).

Deeply nested SQL/Q/spec expressions must raise a catchable exception
(``SQLSyntaxError`` / ``QueryPlanError``) instead of overflowing the native
stack; long flat AND/OR chains must keep working. Every case runs in a
subprocess so that a regression cannot take down the test runner.
"""
import subprocess
import sys
import textwrap

import pytest

PRELUDE = textwrap.dedent("""
    import os, sys
    import nhydb
    from nhydb.query import Q, QueryEngine, parse_sql, expr_from_spec
    db = nhydb.NHyDB.create(os.path.join(sys.argv[1], "r.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "i64"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put_dense_batch(1, 1, [[1, 1], [2, 1], [3, 2]], shape_id=1)
    db.commit()
    def outcome(f):
        try:
            r = f()
        except Exception as e:
            print("EXC", type(e).__name__, str(e)[:120])
        else:
            print("OK", r)
""")

CASES = {
    # F016: SQL parser
    "sql_parens_deep": ('outcome(lambda: db.sql("SELECT id FROM main WHERE " + "(" * 10000 + "x = 1" + ")" * 10000))',
                        "EXC SQLSyntaxError expression nested deeper than 256 levels"),
    "sql_parens_ok": ('outcome(lambda: db.sql("SELECT COUNT(*) FROM main WHERE " + "(" * 250 + "x = 1" + ")" * 250)["rows"])',
                      "OK [{'count_all': 2}]"),
    "sql_not_deep": ('outcome(lambda: db.sql("SELECT id FROM main WHERE " + "NOT " * 100000 + "x = 1"))',
                     "EXC SQLSyntaxError expression nested deeper than 256 levels"),
    "sql_not_ok": ('outcome(lambda: db.sql("SELECT COUNT(*) FROM main WHERE " + "NOT " * 255 + "x = 1")["rows"])',
                   "OK [{'count_all': 1}]"),
    "sql_and_chain": ('outcome(lambda: db.sql("SELECT COUNT(*) FROM main WHERE " + " AND ".join(["x = 1"] * 20000))["rows"])',
                      "OK [{'count_all': 2}]"),
    "sql_or_chain": ('outcome(lambda: db.sql("SELECT COUNT(*) FROM main WHERE " + " OR ".join(["x = %d" % (i % 5) for i in range(20000)]))["rows"])',
                     "OK [{'count_all': 3}]"),
    "parse_sql_chain": ('outcome(lambda: type(parse_sql("SELECT id FROM main WHERE " + " AND ".join(["x = 1"] * 20000)).where).__name__)',
                        "OK BoolExpr"),
    # F015: Q objects, specifications
    "q_and_chain": (textwrap.dedent("""
        e = Q.field("x") == 1
        for _ in range(30000):
            e = e & (Q.field("x") == 1)
        outcome(lambda: db.query(e, realm=1, return_mode="count")["count"])
    """), "OK 2"),
    "q_not_deep": (textwrap.dedent("""
        e = Q.field("x") == 1
        for _ in range(100000):
            e = ~e
        outcome(lambda: db.query(e, realm=1, return_mode="count"))
    """), "EXC QueryPlanError query expression nested deeper than 256 levels"),
    "q_not_ok": (textwrap.dedent("""
        e = Q.field("x") == 1
        for _ in range(255):
            e = ~e
        outcome(lambda: db.query(e, realm=1, return_mode="count")["count"])
    """), "OK 1"),
    "q_alternating_deep": (textwrap.dedent("""
        e = Q.field("x") == 1
        for i in range(400):
            e = (e & (Q.field("x") == 1)) if i % 2 else (e | (Q.field("x") == 2))
        outcome(lambda: db.query(e, realm=1, return_mode="count"))
    """), "EXC QueryPlanError query expression nested deeper than 256 levels"),
    "spec_not_deep": (textwrap.dedent("""
        s = {"field": "x", "value": 1}
        for _ in range(100000):
            s = {"not": s}
        outcome(lambda: QueryEngine(db).execute_spec({"where": s, "realm": 1, "return_mode": "count"}))
    """), "EXC QueryPlanError query expression nested deeper than 256 levels"),
    "spec_and_chain": ('outcome(lambda: QueryEngine(db).execute_spec({"where": {"and": [{"field": "x", "value": 1}] * 20000}, "realm": 1, "return_mode": "count"})["count"])',
                       "OK 2"),
    "expr_from_spec_deep": (textwrap.dedent("""
        s = {"field": "x", "value": 1}
        for _ in range(1000):
            s = {"not": s}
        outcome(lambda: expr_from_spec(s))
    """), "EXC QueryPlanError query expression nested deeper than 256 levels"),
}


@pytest.mark.parametrize("case", list(CASES))
def test_nesting_limits(tmp_path, case):
    code, expected = CASES[case]
    proc = subprocess.run([sys.executable, "-c", PRELUDE + code, str(tmp_path)],
                          capture_output=True, text=True, timeout=300)
    assert proc.returncode == 0, (case, proc.returncode, proc.stderr[-2000:])
    assert proc.stdout.strip().splitlines()[-1].startswith(expected), (case, proc.stdout[-500:])


def test_flat_chains_keep_python_shape():
    """n-ary chains inside the engine come back as the left-deep objects that
    the Python operators build (unchanged public expression shape)."""
    from nhydb.query import Q, BoolExpr, parse_sql, expr_from_spec
    a, b, c = Q.field("a") == 1, Q.field("b") == 2, Q.field("c") == 3
    assert parse_sql("SELECT a FROM t WHERE a = 1 AND b = 2 AND c = 3").where == (a & b) & c
    assert expr_from_spec({"or": [{"field": "a", "value": 1}, {"field": "b", "value": 2}, {"field": "c", "value": 3}]}) == (a | b) | c
    explicit = BoolExpr("and", (a, b, c))
    assert expr_from_spec(explicit) is explicit
