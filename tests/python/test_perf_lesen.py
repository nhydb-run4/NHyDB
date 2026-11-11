"""Performance phase 0.28, read path (P001/P003/P005/P019): the "verified"
registry with partial block reads, the configurable LRU block cache and the
lazily updated sparse v index must not change any result.

Every block is CRC-checked completely before its first use; afterwards only
the needed bytes are read. The tests compare the paths against each other
(cache budget 0 = partial reads only, small budget = eviction, default).
"""
import numpy as np
import pytest

import nhydb


def _cfg(tmp_path, cache_bytes):
    p = tmp_path / f"cache_{cache_bytes}.toml"
    p.write_text(f"[cache]\nblock_cache_bytes = {cache_bytes}\n")
    return str(p)


def _build(p, n, batch):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"},
                                {"v": 2, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    for u0 in range(1, n + 1, batch):
        m = min(batch, n - u0 + 1)
        db.put_dense_batch(1, u0, [[u * 3, f"text {u}" * (1 + u % 5)] for u in range(u0, u0 + m)], shape_id=1)
    db.put_cells_numpy(1, 7, np.arange(1000, 1000 + 5000, dtype=np.int64),
                       np.arange(5000, dtype=np.float64), type="f64")
    db.put_cell(1, 9, 3, 1.5, type="f64")
    db.delete_record(1, 11)
    db.commit()
    db.close()


def _snapshot(db, n):
    out = [db.get(1, u) for u in range(1, n + 2)]
    out.append(list(db.range(1, 1, n)))
    cols = db.read_numpy(1, 1, n)
    out.append({k: np.asarray(v).tolist() for k, v in cols.items()})
    out.append([db.get_value(1, 7, v) for v in (999, 1000, 1001, 3500, 5999, 6000)])
    out.append([db.get_cell(1, 9, 3), db.get_value(1, 12, 1), db.get_value(1, 12, 2)])
    return out


@pytest.mark.parametrize("batch", [50, 4000])
def test_cache_budgets_give_identical_results(tmp_path, batch):
    p = tmp_path / "c.nhydb"
    n = 4000
    _build(p, n, batch)
    ref = nhydb.NHyDB(str(p), readonly=True)
    want = _snapshot(ref, n)
    ref.close()
    for budget in (0, 4096, 64 << 10, 256 << 20):
        db = nhydb.NHyDB(str(p), readonly=True, config=_cfg(tmp_path, budget))
        assert _snapshot(db, n) == want, budget
        assert _snapshot(db, n) == want, budget  # second pass: verified blocks
        st = db.stats()["block_cache"]
        assert st["budget_bytes"] == budget
        assert st["cached_bytes"] <= budget
        db.close()


def test_big_block_reads_partially(tmp_path):
    """P001: a block above the largest cacheable item (here: tiny budget) is
    read completely once, then only partially."""
    p = tmp_path / "b.nhydb"
    _build(p, 3000, 3000)
    db = nhydb.NHyDB(str(p), readonly=True, config=_cfg(tmp_path, 8192))
    assert db.get(1, 1)["a"] == 3
    loads = db.stats()["block_cache"]["loads"]
    for u in range(2, 3001, 13):
        assert db.get(1, u)["a"] == 3 * u
    st = db.stats()["block_cache"]
    assert st["loads"] == loads
    assert st["partial_reads"] >= len(range(2, 3001, 13))
    db.close()


def test_corruption_still_detected_on_first_use(tmp_path):
    p = tmp_path / "x.nhydb"
    _build(p, 3000, 3000)
    db = nhydb.NHyDB(str(p), readonly=True)
    seg_end = db.stats()["file_bytes"]
    db.close()
    raw = bytearray(p.read_bytes())
    # flip a byte inside the text values of the big DATA block (not its prefix)
    i = raw.find(b"text 1500")
    assert 0 < i < seg_end
    raw[i] ^= 0x20
    p.write_bytes(bytes(raw))
    for budget in (0, 8192, 64 << 20):
        db = nhydb.NHyDB(str(p), readonly=True, config=_cfg(tmp_path, budget))
        for _ in range(2):
            with pytest.raises(nhydb.CorruptionError):
                db.get(1, 2)
        with pytest.raises(nhydb.CorruptionError):
            db.read_numpy(1, 1, 10)
        db.close()


def test_sparse_reads_after_writes_without_commit(tmp_path):
    """P019: cached v indexes follow later writes (overrides, tombstones,
    other u groups) without clearing all caches."""
    p = tmp_path / "s.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    for u in range(1, 50):
        db.put_cell(1, u, 1, float(u), type="f64")
    assert [db.get_value(1, u, 1) for u in range(1, 50)] == [float(u) for u in range(1, 50)]
    for u in range(1, 50, 3):
        db.put_cell(1, u, 1, -float(u), type="f64")
        db.put_cell(1, u, 2, 2.0, type="f64")
    db.delete_cell(1, 2, 1)
    got = [(db.get_value(1, u, 1), db.get_value(1, u, 2)) for u in range(1, 50)]
    want = [(None if u == 2 else (-float(u) if u % 3 == 1 else float(u)), 2.0 if u % 3 == 1 else None)
            for u in range(1, 50)]
    assert got == want
    db.commit()
    db.close()
    db = nhydb.NHyDB(str(p), readonly=True)
    assert [(db.get_value(1, u, 1), db.get_value(1, u, 2)) for u in range(1, 50)] == want
    db.close()


def _cells(db, us, vs):
    out = []
    for u in us:
        for v in vs:
            c = db.get_cell(1, u, v, dyn_address=False)
            out.append((u, v, db.get_value(1, u, v), None if c is None else (c["field"], c["_shape"], c["value"])))
    return out


def test_selective_cell_decoder_equals_full_record(tmp_path):
    """P016: get_value/get_cell decode only the requested BASE field when the
    realm/lane has no sparse overlays; the result equals the full record
    (old and new shape, current-shape fields missing in old records,
    extensions, deletes, all field types)."""
    p = tmp_path / "c.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    f1 = [{"v": 1, "name": "a", "type": "u32"}, {"v": 2, "name": "s", "type": "utf8"},
          {"v": 3, "name": "f", "type": "f64"}, {"v": 4, "name": "b", "type": "bytes"},
          {"v": 5, "name": "n", "type": "varint"}]
    db.create_shape(1, 1, "s1", f1)
    db.create_shape(2, 1, "s2", f1 + [{"v": 6, "name": "late", "type": "varuint"}])
    db.set_realm_default_shape(1, 1)
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [(u, f"t{u}" * (u % 4), u / 3, bytes([u % 256]) * (u % 5), -u * 10**12)
                              for u in range(1, 301)], shape_id=1)
    db.set_realm_default_shape(1, 2)
    db.put_dense_batch(1, 301, [(u, "", 0.5, b"", u, u * 7) for u in range(301, 351)], shape_id=2)
    db.delete_record(1, 17)
    db.commit()
    us = [1, 2, 3, 16, 17, 18, 150, 300, 301, 333, 350, 351]
    vs = [1, 2, 3, 4, 5, 6, 7, "s", "late"]
    got = _cells(db, us, vs)
    for u, v, val, cell in got:
        rec = db.get(1, u)
        if rec is None:
            assert val is None and cell is None
            continue
        if isinstance(v, int) and v <= 6:
            name = (f1 + [{"name": "late"}])[v - 1]["name"]
            assert val == rec.get(name), (u, v)
    # the same database with a sparse overlay realm/lane uses the general path
    db.define_extension(1, 50, "utf8", name="note")
    db.put_extensions(1, 2, {"note": "n2"})
    db.put_cell(1, 3, 1, 999, type="u32")
    assert db.get_value(1, 3, 1) == 999 and db.get_value(1, 2, 50) == "n2"
    db.close()
    db = nhydb.NHyDB(str(p), readonly=True)
    after = _cells(db, us, vs)
    db.close()
    changed = {(u, v) for (u, v, a, _), (_, _, b, _) in zip(got, after) if a != b}
    assert changed == {(3, 1)}
