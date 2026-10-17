"""Address mathematics, partly checked against an external reference implementation as oracle (not included)."""
import random

import pytest

import nhydb


def test_mandatory_example_r23():
    n = nhydb.dynadron_n(23, 14, 744, "NL")
    assert n == (22 * 10**100 + 758) ** 2 - 14
    assert len(str(n)) == 203
    a = nhydb.n_to_dynadron_address(f"N{n}")
    assert (a["realm"], a["u"], a["v_local"], a["lane"]) == (23, 14, 744, 0)
    nr = nhydb.dynadron_n(23, 14, 744, "NR")
    assert nhydb.n_to_dynadron_address(nr)["lane"] == 1


@pytest.mark.parametrize("n", [552, 576, 600, 4, 16])
def test_gz_n0_rejected(n):
    with pytest.raises(nhydb.AddressError):
        nhydb.n_to_dynadron_address(n)


def test_random_against_reference(ref):
    rnd = random.Random(14)
    T = ref.REALM_SPAN
    for _ in range(3000):
        realm = rnd.choice([1, 2, 7, 500, rnd.randrange(1, 10**30)])
        u = rnd.choice([1, rnd.randrange(1, 1000), rnd.randrange(1, 2**64), rnd.randrange(1, 10**40)])
        v = rnd.choice([1, T, rnd.randrange(1, 10**6), rnd.randrange(1, T + 1)])
        lane = rnd.choice(["NL", "NR", 0, 1])
        assert nhydb.cell_address(realm, u, v, lane) == ref.cell_address(realm, u, v, lane)
        assert nhydb.dynadron_address(realm, u, v) == ref.dynadron_address(realm, u, v)
        n = ref.dynadron_n(realm, u, v, lane)
        assert nhydb.n_to_dynadron_address(n) == ref.n_to_dynadron_address(n)
    for n in range(1, 50_000):
        try:
            exp = ref.n_to_dynadron_address(n)
        except ref.AddressError:
            with pytest.raises(nhydb.AddressError):
                nhydb.n_to_dynadron_address(n)
            continue
        assert nhydb.n_to_dynadron_address(n) == exp


def test_helpers_against_reference(ref):
    T = ref.REALM_SPAN
    assert nhydb.REALM_SPAN == T
    for r in (1, 2, 99, 10**20):
        assert nhydb.realm_v_bounds(r) == ref.realm_v_bounds(r)
    for V in (1, T, T + 1, 5 * T + 7):
        assert nhydb.split_global_v(V) == ref.split_global_v(V)
    assert nhydb.global_v(2, 1) == T + 1
    for x in (0, 1, 127, 128, 300, 2**64, 10**100):
        assert nhydb.enc_vu(x) == ref.enc_vu(x)
        assert nhydb.dec_vu(ref.enc_vu(x)) == ref.dec_vu(ref.enc_vu(x))
    for x in (0, -1, 1, -(10**50), 10**50):
        assert nhydb.enc_vi(x) == ref.enc_vi(x)
        assert nhydb.dec_vi(ref.enc_vi(x)) == ref.dec_vi(ref.enc_vi(x))
    assert nhydb.parse_n_reference("N589") == 589
    for bad in ("Nx", "", "N"):
        with pytest.raises(nhydb.AddressError):
            nhydb.parse_n_reference(bad)


@pytest.mark.parametrize("args, exc", [
    ((0, 1, 1), ValueError), ((1, 0, 1), ValueError), ((1, 1, 0), "RealmLimitError"),
    ((1, 1, 10**100 + 1), "RealmLimitError"), ((-3, 1, 1), ValueError),
])
def test_invalid_coordinates(args, exc, ref):
    exc_n = getattr(nhydb, exc) if isinstance(exc, str) else exc
    exc_r = getattr(ref, exc) if isinstance(exc, str) else exc
    with pytest.raises(exc_n) as e1:
        nhydb.dynadron_address(*args)
    with pytest.raises(exc_r) as e2:
        ref.dynadron_address(*args)
    assert str(e1.value) == str(e2.value)


def test_lane_normalization(ref):
    for x in (None, 0, 1, "0", "1", "NL", "nr", " L ", "R", "default"):
        assert nhydb.normalize_lane(x) == ref.normalize_lane(x)
    for bad in (2, -1, "X"):
        with pytest.raises(ValueError):
            nhydb.normalize_lane(bad)
