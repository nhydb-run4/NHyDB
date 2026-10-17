"""Address mathematics: mandatory example and excluded numbers GZ/N0."""

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


