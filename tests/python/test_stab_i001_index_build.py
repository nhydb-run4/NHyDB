"""I001: index build memory. Posting lists start as u32 and widen to u64
and exact BigInt values on demand; the result must equal a scan for u below
2^32, between 2^32 and 2^64 and above 2^64, in every combination."""
import pytest

import nhydb


@pytest.mark.parametrize("us", [
    [1, 2, 3, 70],
    [5, 2**32 - 1, 2**32, 2**40 + 7],
    [3, 2**64 - 1, 2**64, 2**70 + 1],
    [9, 2**70 + 5],
], ids=["u32", "u32_to_u64", "u32_u64_big", "u32_to_big"])
def test_index_lists_widen_exactly(tmp_path, us):
    db = nhydb.NHyDB.create(str(tmp_path / "w.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "k", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    for u in us:
        db.put_dense_batch(1, u, [[u % 2]], shape_id=1)
    db.commit()
    db.create_index(1, 1, "k", "k")
    res = db.build_indexes(commit=True)
    assert res["passes"] == 1
    for val in (0, 1):
        want = sorted(u for u in us if u % 2 == val)
        assert sorted(db.find_u(1, {"k": val})) == want
    db.close()
    with nhydb.NHyDB(tmp_path / "w.nhydb", readonly=True) as ro:
        assert ro.verify(deep=True)["ok"]
        assert sorted(ro.find_u(1, {"k": 1})) == sorted(u for u in us if u % 2)
