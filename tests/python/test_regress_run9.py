"""Regression pool of RUN9 (0.28.2): the RAM corrections V1-V6 of the plan
of 2026-10-31 (``2026-10-31_umsetzungsplan_run9_run10``).

Every test names its finding. Results must equal the ones before the
correction; the tests marked "red on RUN8" show the memory property that
RUN8 did not have.
"""
import pytest

import nhydb


@pytest.fixture(autouse=True)
def _own_config(monkeypatch):
    """A NHYDB_CONFIG of the environment (dir3 QA runs) must not change the
    block layout these tests count on."""
    monkeypatch.delenv("NHYDB_CONFIG", raising=False)


def _wide(p, n=300):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    names = ["alpha", "bravo", "charlie", "x"]
    db.create_shape(1, 1, "s", [{"v": i, "name": k, "type": "utf8" if k == "charlie" else "varint"} for i, k in enumerate(names, 1)])
    db.create_classic_layout(1, 1, "w", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u * 2, "t%d" % u, u % 3] for u in range(1, n + 1)], shape_id=1, commit=True)
    return db


# ---------------------------------------------------------------- V2


def test_v2_sql_rows_share_their_key_objects(tmp_path):
    """V2: the rows of one ``sql()`` result share the Python string objects
    of their column names instead of one new string per row and column
    (red on RUN8: every row had its own key objects)."""
    db = _wide(tmp_path / "w.nhydb")
    rows = db.sql("SELECT alpha, bravo, charlie FROM w")["rows"]
    keys0 = [k for k in rows[0]]
    for r in rows[1:]:
        for a, b in zip(keys0, r):
            assert a == b and a is b
    db.close()


def test_v2_sql_result_equals_query_api_and_iter(tmp_path):
    """V2: the consuming conversion returns exactly the values, key order
    and types of before: ``sql()`` rows equal ``sql_iter`` and the record
    reads, including None and long keys that are not shared."""
    db = _wide(tmp_path / "w.nhydb")
    long = "k" * 100
    db.define_extension(1, 50, "utf8", name=long)
    db.put_extensions(1, 7, {50: "ext"}, commit=True)
    res = db.sql("SELECT * FROM w")
    it = list(db.sql_iter("SELECT * FROM w"))
    assert res["rows"] == it
    assert [list(r) for r in res["rows"]] == [list(r) for r in it]
    for r in res["rows"]:
        u = r["u"] if "u" in r else None
        if u is not None:
            assert r["alpha"] == u
    assert any(r.get(long) == "ext" for r in res["rows"])
    assert set(res) == set(db.sql("SELECT * FROM w LIMIT 1"))
    db.close()



# ---------------------------------------------------------------- V3


@pytest.mark.parametrize("n", [4095, 4096, 4097, 9000])
def test_v3_trigram_verification_in_chunks_equals_scan(tmp_path, n):
    """V3: the TRIGRAM verification reads the candidate texts in chunks of
    4096; across chunk boundaries the result equals the scan without index,
    including overwritten texts (sparse overlay), sparse-only records,
    deleted fields and ICONTAINS."""
    def make(p, with_index):
        db = nhydb.NHyDB.create(str(p))
        db.create_realm(1, "r")
        db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"}])
        db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=8)
        db.put_dense_batch(1, 1, [[u, ("Lazy fox %d" % u) if u % 3 else ("busy bee %d" % u)] for u in range(1, n + 1)], shape_id=1)
        db.put_points(1, [(5, 2, "now busy", "utf8"), (6, 2, "LAZY again", "utf8"), (n + 5, 2, "lazy sparse", "utf8")])
        db.delete_cell(1, 7, 2)
        if with_index:
            db.create_trigram_index(3, 1, "tri_body", 2)
        db.commit()
        if with_index:
            db.build_indexes(commit=True)
        return db
    a = make(tmp_path / "i.nhydb", True)
    b = make(tmp_path / "s.nhydb", False)
    for f in ("CONTAINS", "ICONTAINS"):
        q = "SELECT id FROM c WHERE %s(body, 'azy')" % f
        ra, rb = a.sql(q), b.sql(q)
        assert ra["rows"] == rb["rows"], f
        assert len(ra["rows"]) > (n // 3 if f == "CONTAINS" else n // 2)
    plan = a.sql("EXPLAIN SELECT id FROM c WHERE CONTAINS(body, 'azy')")
    assert "TRIGRAM" in str(plan)
    a.close(); b.close()


# ---------------------------------------------------------------- V6


def _not_db(p, overlay):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "varint"}, {"v": 2, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=8)
    for s in range(1, 9001, 3000):
        db.put_dense_batch(1, s, [[u % 7, "t%d" % (u % 11)] for u in range(s, s + 3000)], shape_id=1)
    if overlay:
        # sparse cells, a deleted field and a sparse-only record: the
        # universe then comes from the record cursor, not the segments
        db.put_points(1, [(5, 1, 3, "varint"), (9100, 1, 2, "varint"), (12, 2, "x", "utf8")])
        db.delete_cell(1, 20, 1)
    db.commit()
    return db


@pytest.mark.parametrize("overlay", [False, True])
def test_v6_not_equals_universe_minus_child(tmp_path, overlay):
    """V6: NOT no longer materializes the universe (record and cell domain,
    segment ranges, record cursor and cell cursor); its result equals the
    complement of the child computed from ``Q.all()``."""
    from nhydb.query import Q, QueryEngine
    F = Q.field
    db = _not_db(tmp_path / "n.nhydb", overlay)
    q = QueryEngine(db)

    def refs(expr, mode):
        r = q.execute(expr, realm=1, return_mode=mode)
        return r["refs"] if mode == "n" else [row["N"] for row in r["rows"]]

    for mode in ("n", "cell"):
        every = set(refs(Q.all(), mode))
        assert len(every) >= 9000
        some = sorted(every)[:3]
        direct = Q.n(1, 3, 5, domain="record") if mode == "n" else Q.n(*some, domain="cell")
        for e in (F("x") == 2, (F("x") == 2) | (F("t") == "t3"), F("x").between(1, 5), Q.all(), direct):
            child = set(refs(e, mode))
            got = refs(~e, mode)
            assert len(got) == len(set(got)), (mode, e)
            assert set(got) == every - child, (mode, e)
            assert set(refs(~~e, mode)) == child & every, (mode, e)
            mixed = refs((F("x") == 3) & ~e, mode)
            assert set(mixed) == set(refs(F("x") == 3, mode)) - child, (mode, e)
    db.close()


# ---------------------------------------------------------------- V1 (write) and V5 (part A)


def _cfg(tmp_path, max_block):
    p = tmp_path / ("cfg_%d.toml" % max_block)
    p.write_text("[write]\nmax_block_bytes = %d\n" % max_block)
    return str(p)


def _data_blocks(path):
    from test_stab_format_golden import describe_file
    return [b for b in describe_file(path)["blocks"] if b["type"] == "DATA_DENSE"]


def _kv_db(path, cfg):
    db = nhydb.NHyDB.create(str(path), config=cfg)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.define_extension(1, 9, "utf8", name="e")
    return db


ROWS = [[u, "row %d " % u + "x" * (u % 50)] for u in range(1, 20_001)]


@pytest.mark.parametrize("form", ["tuples", "dicts", "numpy"])
def test_v1_large_dense_call_is_split_into_blocks(tmp_path, form):
    """V1 write side: a dense call larger than ``[write] max_block_bytes``
    becomes several DATA blocks of at most that size with consecutive u
    ranges; every record, the extension of the call and ``verify(deep)``
    equal a write as one block (``max_block_bytes = 0``)."""
    limit = 64 << 10
    out = {}
    for mb in (0, limit):
        p = tmp_path / ("w%d_%s.nhydb" % (mb, form))
        db = _kv_db(p, _cfg(tmp_path, mb))
        if form == "tuples":
            r = db.put_dense_batch(1, 1, ROWS, shape_id=1, extensions_by_u={7: {9: "ext"}}, commit=True)
        elif form == "dicts":
            r = db.put_dense_batch(1, 1, [{"id": a, "t": b} for a, b in ROWS], shape_id=1, extensions_by_u={7: {9: "ext"}}, commit=True)
        else:
            import numpy as np
            r = db.put_numpy_batch(1, 1, {"id": np.array([a for a, _ in ROWS], dtype=np.uint64), "t": np.array([b for _, b in ROWS], dtype=object)}, shape_id=1, commit=True)
            db.put_extensions(1, 7, {9: "ext"}, commit=True)
        assert db.verify(deep=True)["ok"]
        recs = [db.get(1, u) for u in (1, 2, 7, 4096, 9999, 20_000)]
        rows = db.sql("SELECT id, t, e FROM c")["rows"]
        db.close()
        out[mb] = (r, recs, rows, _data_blocks(p))
    r0, recs0, rows0, blocks0 = out[0]
    r1, recs1, rows1, blocks1 = out[limit]
    assert recs0 == recs1 and rows0 == rows1 and len(rows1) == 20_000
    assert len(blocks0) == 1 and len(blocks1) > 5
    assert all(b["len"] <= limit for b in blocks1)
    if form != "numpy":
        # the result keeps the keys of the reference: first block, all bytes
        assert set(r1) == set(r0) and "blocks" not in r1
        assert r1["records"] == 20_000 and r1["bytes"] == sum(b["len"] for b in blocks1)


def test_v1_split_write_is_atomic(tmp_path):
    """V1/V5: a bad row in a late block of a split call leaves nothing
    behind (all blocks of the call are one unit, RUN8 write scope)."""
    p = tmp_path / "a.nhydb"
    db = _kv_db(p, _cfg(tmp_path, 64 << 10))
    db.put_dense_batch(1, 1, ROWS[:10], shape_id=1, commit=True)
    size = p.stat().st_size
    bad = [list(r) for r in ROWS]
    bad[15_000] = [1, 2, 3]
    with pytest.raises(nhydb.SchemaError):
        db.put_dense_batch(1, 100, bad, shape_id=1, commit=True)
    assert p.stat().st_size == size
    assert db.get(1, 100) is None and db.get(1, 10) is not None
    db.commit()
    assert db.verify(deep=True)["ok"] and len(_data_blocks(p)) == 1
    db.close()


def test_v1_record_larger_than_the_block_limit_gets_its_own_block(tmp_path):
    """V1: a single record above the limit is not split; it gets a block of
    its own, the records around it are packed as usual."""
    p = tmp_path / "b.nhydb"
    db = _kv_db(p, _cfg(tmp_path, 64 << 10))
    rows = [[1, "a"], [2, "y" * 200_000], [3, "b"]]
    db.put_dense_batch(1, 1, rows, shape_id=1, commit=True)
    assert [b["len"] > 64 << 10 for b in _data_blocks(p)] == [False, True, False]
    assert [db.get(1, u)["t"] for u in (1, 2, 3)] == ["a", "y" * 200_000, "b"]
    assert db.verify(deep=True)["ok"]
    db.close()


def test_v1_compaction_splits_an_old_oversized_block(tmp_path):
    """V1: compaction rewrites the records of a block written as one (limit
    0) into blocks within the limit; content and deep verify unchanged."""
    p = tmp_path / "c.nhydb"
    db = _kv_db(p, _cfg(tmp_path, 0))
    db.put_dense_batch(1, 1, ROWS, shape_id=1, commit=True)
    before = db.sql("SELECT id, t FROM c")["rows"]
    db.close()
    assert len(_data_blocks(p)) == 1 and _data_blocks(p)[0]["len"] > 64 << 10
    (tmp_path / "nhydb_config.toml").write_text("[write]\nmax_block_bytes = 65536\n")
    nhydb.NHyDB.compact_file(str(p), make_backup=False)
    blocks = _data_blocks(p)
    assert len(blocks) > 5 and all(b["len"] <= 64 << 10 for b in blocks)
    db = nhydb.NHyDB(str(p), readonly=True)
    assert db.sql("SELECT id, t FROM c")["rows"] == before and db.verify(deep=True)["ok"]
    db.close()
