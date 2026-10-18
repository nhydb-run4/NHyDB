"""MATH_MAP: a sparse 2-D grid addressed by formulas.

Values of a game board (x, y) are placed by the mapping u = x, V = y; very
large y values spill into further logical realms automatically. The example
plans the mapping with dry_run, writes and reads cells, and looks them up by
input tuples through the QueryEngine.

Run: python math_map_grid.py                (needs: pip install nhydb)
"""
import os
import tempfile

import nhydb
from nhydb.math import MathMap, MathMapLayoutSpec, dry_run
from nhydb.query import Q, QueryEngine


def main():
    board = MathMap.compile("board", ("x", "y"), u="x", V="y", side="(x + y) % 2")
    moves = [((1, 1), "rook"), ((5, 1), "king"), ((4, 8), "queen"), ((3, 10**100 + 2), "far away")]

    plan = dry_run(board, [inputs for inputs, _ in moves])
    print(f"dry run: {plan.valid} valid, {plan.collisions} collisions, logical realms {plan.logical_realms}")

    path = os.path.join(tempfile.mkdtemp(), "board.nhydb")
    with nhydb.create(path) as db:
        db.create_math_layout(MathMapLayoutSpec("board", board, 16), layout_id=1, payload_type="utf8")
        report = db.put_math_batch(1, moves, commit=True)
        print("written:", report["cells"], "cells in", report["mapped_realms"], "realms")

    with nhydb.open(path) as db:
        print("(4, 8):", db.get_math(1, (4, 8)))
        print("(2, 2):", db.get_math(1, (2, 2)))
        cell = db.get_math_cell(1, (3, 10**100 + 2))
        print("far cell lives in physical realm", cell["_realm"], "lane", cell["_lane_name"])
        r = QueryEngine(db).execute(Q.math("math_layout_inputs", layout_id=1, inputs=[(1, 1), (5, 1)]),
                                    return_mode="cell")
        print("lookup:", [row["value"] for row in r["rows"]])
        assert [row["value"] for row in r["rows"]] == ["rook", "king"]
        assert db.describe_math(1)["frozen"] and db.verify()["ok"]


if __name__ == "__main__":
    main()
