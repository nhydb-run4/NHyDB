"""Stabilisation T008: boundary matrix for very large coordinates and values
(F003/F014: no fixed decoder bit limit; F004: ID range)."""
import sys

import pytest

import nhydb

BITS = [16380, 16400, 10**5, 10**6]
SPAN = 10**100


@pytest.fixture(autouse=True)
def _big_str():
    # The digit limit exists from Python 3.9.14/3.10.7/3.11 on; older
    # interpreters have no limit, so the tests run unchanged there.
    if not hasattr(sys, "get_int_max_str_digits"):
        yield
        return
    old = sys.get_int_max_str_digits()
    sys.set_int_max_str_digits(0)
    try:
        yield
    finally:
        sys.set_int_max_str_digits(old)


def test_bigint_matrix_commit_reopen_verify(tmp_path):
    p = tmp_path / "big.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "main")
    bigR = {b: 2**b + 3 for b in BITS}
    for b, R in bigR.items():
        db.create_realm(R, f"r{b}")
    db.create_shape(1, bigR[16400], "row", [{"v": 1, "name": "a", "type": "varint"}])
    db.create_classic_layout(1, bigR[16400], "rec", shape_id=1)
    math = db.create_math_map_layout("pass", ("x", "y"), u="x", V="y", payload_type="utf8")
    for b in BITS:
        u = 2**b + 1
        db.put_cell(1, u, 1, b, type="varuint")                      # huge u (sparse)
        db.put_cell(1, 7, b, 2**b + 1, type="varuint")               # huge varuint value
        db.put_cell(1, 8, b, -(2**b) - 1, type="varint")             # huge varint value
        db.put_points(1, [(u + 1, 2, -(2**b), "varint")])            # put_points, huge u + value
        db.put_cell(bigR[b], u, 3, 5, type="u8")                     # huge R and u
        db.put_dense_batch(bigR[16400], u, [[b]], shape_id=1)        # dense block with huge R/u prefix
        db.put_math(math.layout_id, (u, b), f"u{b}")                 # MATH pass-through u
        db.put_math(math.layout_id, (3, SPAN * 2**b + 5), f"V{b}")   # pass-through V → huge logical realm
    db.commit()
    db.close()

    db = nhydb.NHyDB(str(p))
    for b in BITS:
        u = 2**b + 1
        assert db.get_value(1, u, 1) == b
        assert db.get_value(1, 7, b) == 2**b + 1
        assert db.get_value(1, 8, b) == -(2**b) - 1
        assert db.get_value(1, u + 1, 2) == -(2**b)
        assert db.get_value(bigR[b], u, 3) == 5
        assert db.get(bigR[16400], u)["a"] == b
        assert db.get_math(math.layout_id, (u, b)) == f"u{b}"
        assert db.get_math(math.layout_id, (3, SPAN * 2**b + 5)) == f"V{b}"
    ext = db.describe_math(math.layout_id)["realm_extents"]
    assert len(ext) >= 2  # automatic realms for the huge logical realms
    assert db.verify(deep=True)["ok"]
    db.close()


def test_address_roundtrip_huge():
    for b in (16400, 10**5):
        R, u, v = 2**b + 7, 2**b + 3, 10**99 + 11
        a = nhydb.cell_address(R, u, v, "NR")
        back = nhydb.n_to_dynadron_address(a["N"])
        assert (back["realm"], back["u"], back["v_local"], back["lane"]) == (R, u, v, 1)


@pytest.mark.parametrize("val", [2**63 - 1])
def test_id_boundary_ok(tmp_path, val):
    db = nhydb.NHyDB.create(str(tmp_path / "i.nhydb"))
    db.create_realm(1, "m")
    db.create_shape(val, 1, "s", [{"v": 1, "name": "a", "type": "u8"}])
    with pytest.raises(ValueError):
        db.create_shape(val + 1, 1, "s2", [{"v": 1, "name": "a", "type": "u8"}])
    db.close()
