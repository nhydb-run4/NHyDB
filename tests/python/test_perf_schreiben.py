"""Performance 0.28 (write path): regression and invariant tests.

P002 compaction with bounded memory (raw BASE copy, tombstones computed on
demand, chunked sparse writes): the logical state after compaction equals
the state before, for in-place and export, with several shapes, both lanes,
extensions, overlays, tombstones, deleted records, gaps and MATH realms.
"""

import numpy as np
import pytest

import nhydb
import nhydb.math as nm
from nhydb.query import Q


def _dump(db):
    cells = sorted(
        ((c["realm"], c["u"], c["v"], c["lane"], repr(c["value"])) for c in db.iter_materialized_cells()),
    )
    return cells


def _rich_db(path):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s1", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.create_shape(2, 1, "s2", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"},
                                 {"v": 3, "name": "c", "type": "f64"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=10)
    db.define_extension(1, 20, "varint", name="e")
    db.define_extension(1, 21, "utf8", name="t")
    # NL: two shapes, a gap, overlapping rewrites (newest block wins)
    db.put_dense_batch(1, 1, [[i, f"s{i}"] for i in range(1, 41)])
    db.put_dense_batch(1, 41, [[i, f"x{i}", i / 2] for i in range(41, 61)], shape_id=2)
    db.put_dense_batch(1, 30, [[-i, f"r{i}"] for i in range(30, 46)])  # partly overwrites both
    db.put_dense_batch(1, 1000, [[i, "far"] for i in range(5)])
    # extensions on dense records, one overridden, one deleted, one None
    db.put(1, 5, [5, "e5"], extensions={20: 55, 21: "t5"})
    db.put(1, 6, [6, "e6"], extensions={20: 66})
    db.put(1, 7, [7, "e7"], extensions={21: "t7"})
    db.put_cell(1, 5, 20, 555)  # sparse overrides dense EXT
    db.delete_cell(1, 6, 20)  # tombstone hides dense EXT -> must survive
    db.delete_cell(1, 7, 2)  # tombstone hides a BASE field -> must survive
    db.delete_cell(1, 8, 99)  # tombstone without dense value -> dropped
    db.delete_cell(1, 500, 3)  # tombstone without record -> dropped
    db.put_cell(1, 9, 1, 999)  # overlay of a BASE field
    db.put_cell(1, 700, 3, "free", type="utf8")
    db.put_cell(1, 701, 3, 1.5, type="f64")
    db.put_cell(1, 701, 3, 2.5, type="f64")  # newest sparse state wins
    db.delete(1, 12)  # deleted dense record
    db.delete(1, 1002)
    # NR lane
    db.put_dense_batch(1, 3, [[100 + i, f"nr{i}"] for i in range(10)], lane=1)
    db.delete_cell(1, 4, 1, lane=1)
    db.put_cell(1, 3, 2, "nr-over", lane=1)
    # MATH realm (exclusive physical cells)
    m = nm.MathMap.compile("mm", ("x", "k"), u="x // 10 + 1", V="k*10**100 + x % 10 + 1", side="x % 2")
    db.create_math_layout(nm.MathMapLayoutSpec("mm", m, 16), layout_id=7, payload_type="varint")
    db.put_math_batch(7, [((x, x % 3), x * 3) for x in range(40)])
    db.commit()
    return db


@pytest.mark.parametrize("export", [False, True])
def test_p002_compaction_keeps_logical_state(tmp_path, export):
    p = tmp_path / "rich.nhydb"
    db = _rich_db(p)
    before = _dump(db)
    recs = {(u, ln): db.get(1, u, lane=ln) for u in range(1, 1010) for ln in (0, 1)}
    math_before = [db.get_math(7, (x, x % 3)) for x in range(40)]
    db.close()
    out = tmp_path / "exp.nhydb" if export else None
    res = nhydb.NHyDB.compact_file(p, output=out, make_backup=False, batch_records=7)
    target = out or p
    assert res["records_written"] == sum(1 for r in recs.values() if r is not None)
    assert res["batches_written"] > 8  # batch_records=7 forces many blocks
    with nhydb.NHyDB(target, readonly=True) as db2:
        assert db2.verify(deep=True)["ok"]
        assert _dump(db2) == before
        for (u, ln), r in recs.items():
            assert db2.get(1, u, lane=ln) == r, (u, ln)
        assert [db2.get_math(7, (x, x % 3)) for x in range(40)] == math_before
        # the tombstones that hide dense values survive, the others are dropped
        assert db2.get_cell(1, 6, 20, dyn_address=False) is None
        assert db2.get(1, 7)["b"] is None and db2.get(1, 5)["e"] == 555


def test_p002_compaction_chunked_sparse_and_ext(tmp_path):
    """More sparse cells and dense EXT values than one write chunk
    (50 000): every cell arrives, tombstones over dense values stay."""
    p = tmp_path / "many.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=5)
    db.define_extension(1, 9, "varint", name="e")
    n = 60_000
    db.put_dense_batch(1, 1, [[i] for i in range(n)], extensions_by_u={u: {9: u} for u in range(1, n + 1)})
    us = np.arange(1, 55_001, dtype=np.int64)
    db.put_cells_numpy(1, us, np.full(us.size, 7, dtype=np.int64), us * 2, type="varint")
    for u in range(1, 2001):
        db.delete_cell(1, u, 1 if u % 2 else 9)
    db.commit()
    before = _dump(db)
    db.close()
    res = nhydb.NHyDB.compact_file(p, make_backup=False)
    assert res["records_written"] == n
    with nhydb.NHyDB(p, readonly=True) as db2:
        assert db2.verify(deep=True)["ok"]
        assert _dump(db2) == before
        assert db2.get(1, 1)["a"] is None and "e" not in db2.get(1, 2) and db2.get(1, 3)["e"] == 3


# ----------------------------------------------------------------------
# P008: put_math into already mapped realms skips the realm planner
# ----------------------------------------------------------------------

def _math_db(path, realms):
    db = nhydb.NHyDB.create(str(path))
    m = nm.MathMap.compile("bench", ("x", "k"), u="x // 1000 + 1", V="k*10**100 + x % 1000 + 1", side="x % 2")
    db.create_math_layout(nm.MathMapLayoutSpec("bench", m, 64), layout_id=1, payload_type="varint")
    db.put_math_batch(1, [((0, k), 1) for k in range(realms)], commit=True)
    return db


def test_p008_put_math_fast_path_result_and_state(tmp_path):
    db = _math_db(tmp_path / "m.nhydb", 5)
    state = db.describe_math(1)
    r = db.put_math(1, (1001, 3), 42)
    phys = state["realm_extents"][0]["physical_start"] + 3
    assert r == {"layout_id": 1, "physical_realm": phys, "u": 2, "v": 2, "lane": 1, "lane_name": "NR"}
    assert db.get_math(1, (1001, 3)) == 42
    assert db.describe_math(1)["realm_extents"] == state["realm_extents"]
    # a new logical realm still goes through the planner
    r2 = db.put_math(1, (5, 9), 7)
    assert db.describe_math(1)["mapped_realms"] == 6 and db.get_math(1, (5, 9)) == 7
    assert r2["physical_realm"] != phys
    with pytest.raises(nhydb.MathCellConflictError):
        db.put_math(1, (1001, 3), 1)
    db.commit()
    db.close()
    with nhydb.NHyDB(tmp_path / "m.nhydb") as db2:
        assert db2.verify(deep=True)["ok"]
        assert db2.get_math(1, (1001, 3)) == 42 and db2.get_math(1, (5, 9)) == 7


def test_p008_failed_put_math_in_mapped_realm_rolls_back(tmp_path):
    p = tmp_path / "m.nhydb"
    db = _math_db(p, 3)
    db.put_math(1, (10, 1), 10, commit=True)
    state = db.describe_math(1)
    end = db.stats()["file_bytes"] if "file_bytes" in db.stats() else None
    nblock = db.next_block_id
    # the payload fails while the cells are encoded (after the rollback point)
    with pytest.raises((TypeError, ValueError, nhydb.NHyDBError)):
        db.put_math(1, (11, 1), "not an int")
    assert db.describe_math(1) == state and db.next_block_id == nblock
    assert db.get_math(1, (11, 1)) is None and db.get_math(1, (10, 1)) == 10
    if end is not None:
        assert db.stats()["file_bytes"] == end
    # still writable, also into a new realm
    db.put_math(1, (11, 1), 11)
    db.put_math(1, (12, 7), 12, commit=True)
    db.close()
    with nhydb.NHyDB(p) as db2:
        assert db2.verify(deep=True)["ok"]
        assert [db2.get_math(1, x) for x in ((10, 1), (11, 1), (12, 7))] == [10, 11, 12]


# ----------------------------------------------------------------------
# P023: durability="full" – barrier before, F_FULLFSYNC after the superblock
# ----------------------------------------------------------------------

def test_p023_full_commit_roundtrip(tmp_path):
    p = tmp_path / "full.nhydb"
    db = nhydb.NHyDB.create(str(p), durability="full")
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.commit()  # schema commit (plan publication + barrier + superblock)
    gens = []
    for i in range(5):
        db.put_dense_batch(1, 1 + i * 10, [[i * 10 + k] for k in range(10)])
        gens.append(db.commit())
    assert gens == sorted(set(gens))
    assert db.commit() == gens[-1]  # unchanged: writes nothing
    db.close()
    tail = nhydb.NHyDB.inspect_tail(p)
    assert tail["clean"] and all(s["valid"] for s in tail["superblocks"])
    with nhydb.NHyDB(p, durability="full") as db2:
        assert db2.verify(deep=True)["ok"] and db2.get(1, 50)["a"] == 49


# ----------------------------------------------------------------------
# P009: query/sql/health/stats release the GIL (thread model R003 unchanged)
# ----------------------------------------------------------------------

def _kq_small(path, n=3000):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "kq")
    db.create_shape(1, 1, "KQ", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.create_classic_layout(1, 1, "KQ", shape_id=1)
    db.put_dense_batch(1, 1, [[i % 17, f"s{i % 5}"] for i in range(n)], commit=True)
    return db


def test_p009_parallel_queries_same_and_separate_handles(tmp_path):
    import threading
    p = tmp_path / "q.nhydb"
    db = _kq_small(p)
    q = "SELECT COUNT(*) AS c FROM KQ WHERE a > 8 AND b = 's2'"
    expected = db.sql(q)
    expected_query = db.query(Q.field("a") > 8, realm=1, return_mode="n")
    db.close()
    shared = nhydb.NHyDB(p, readonly=True)
    own = [nhydb.NHyDB(p, readonly=True) for _ in range(3)]
    bad = []

    def run(h):
        try:
            for _ in range(15):
                if h.sql(q) != expected or h.query(Q.field("a") > 8, realm=1, return_mode="n") != expected_query:
                    bad.append("result")
                h.stats()
                h.health()
        except Exception as e:  # pragma: no cover - regression
            bad.append(repr(e))

    ts = [threading.Thread(target=run, args=(h,)) for h in own + [shared, shared, shared]]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    assert not bad, bad[:3]
    for h in own + [shared]:
        h.close()


def test_p009_writer_during_released_query_gets_defined_error(tmp_path):
    """While a query runs without the GIL, a write call on the same object
    is rejected with RuntimeError (borrow) – never a corruption (R003)."""
    import threading
    p = tmp_path / "w.nhydb"
    db = _kq_small(p, 20_000)
    q = "SELECT COUNT(*) AS c FROM KQ WHERE a > 3"
    stop = threading.Event()
    errors, unexpected, written = [], [], []

    def reader():
        while not stop.is_set():
            try:
                db.sql(q)
                db.stats()
            except RuntimeError as e:
                errors.append(str(e))
            except Exception as e:  # pragma: no cover
                unexpected.append(repr(e))

    def writer():
        u = 100_000
        while not stop.is_set():
            try:
                db.put_dense_batch(1, u, [[1, "w"]] * 50)
                written.append(u)
                u += 50
            except RuntimeError as e:
                errors.append(str(e))
            except Exception as e:  # pragma: no cover
                unexpected.append(repr(e))

    ts = [threading.Thread(target=reader), threading.Thread(target=reader), threading.Thread(target=writer)]
    for t in ts:
        t.start()
    threading.Event().wait(1.0)
    stop.set()
    for t in ts:
        t.join()
    assert not unexpected, unexpected[:3]
    assert all("borrow" in e.lower() for e in errors), set(errors)
    db.commit()
    assert db.verify(deep=True)["ok"]
    for u in written:
        assert db.get(1, u)["b"] == "w"
    db.close()


def test_p009_python_provider_runs_while_gil_released(tmp_path):
    """Runtime providers (Python functions) re-acquire the GIL themselves."""
    from nhydb.query import MathPredicate
    db = _kq_small(tmp_path / "p.nhydb", 50)
    eng = nhydb.QueryEngine(db)
    seen = []

    def prov(engine, ctx, params):
        seen.append(params)
        for u in range(2, int(params["stop"]) + 1, 2):
            # calls back into the DB (shared borrow) while the query runs
            yield engine.db.record_anchor(1, u, lane=ctx.lane)["N_ref"]

    eng.register_math_provider("mine", prov, domain="record")
    res = eng.execute(MathPredicate("mine", {"stop": 6}), realm=1, return_mode="count")
    assert seen and res["count"] == 3
    db.close()


# ----------------------------------------------------------------------
# P017: read_numpy/sql_numpy hand the Rust vectors to NumPy without a copy
# ----------------------------------------------------------------------

def test_p017_numpy_output_owns_memory_and_is_writable(tmp_path):
    import gc
    db = nhydb.NHyDB.create(str(tmp_path / "n.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "f", "type": "f64"},
                                {"v": 3, "name": "b", "type": "bool"}, {"v": 4, "name": "x", "type": "f32"},
                                {"v": 5, "name": "u", "type": "u64"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    n = 1000
    db.put_numpy_batch(1, 1, {"a": np.arange(n), "f": np.arange(n) / 2, "b": np.arange(n) % 3 == 0,
                              "x": np.arange(n, dtype=np.float32), "u": np.arange(n, dtype=np.uint64) * 2**40},
                       commit=True)
    r = db.read_numpy(1, 1, n + 5)
    assert {k: v.dtype.str[1:] for k, v in r.items()} == {"a": "i8", "f": "f8", "b": "b1", "x": "f4", "u": "u8", "_present": "b1"}
    assert all(v.flags.writeable and v.shape == (n + 5,) for v in r.values())
    a, u = r["a"], r["u"]
    del r
    gc.collect()
    assert a[:n].tolist() == list(range(n)) and a[n:].tolist() == [0] * 5
    assert int(u[n - 1]) == (n - 1) * 2**40
    a[0] = 77  # writable, independent of the database
    assert db.read_numpy(1, 1, 1)["a"][0] == 0
    empty = db.read_numpy(1, 1, 0)
    assert all(v.shape == (0,) for v in empty.values())
    s = db.sql_numpy("SELECT a, f FROM c WHERE a < 3")
    assert s["a"].tolist() == [0, 1, 2] and s["f"].tolist() == [0.0, 0.5, 1.0]
    db.close()
    assert a[:3].tolist() == [77, 1, 2]  # the arrays outlive the handle


# ----------------------------------------------------------------------
# P018: read_columns decodes typed values directly – same result as get()
# ----------------------------------------------------------------------

TYPES_ALL = [("u8", "i8"), ("i8", "i8"), ("u16", "i8"), ("i16", "i8"), ("u32", "i8"), ("i32", "i8"),
             ("i64", "i8"), ("u64", "u8"), ("varuint", "u8"), ("varint", "i8"), ("f32", "f4"),
             ("f64", "f8"), ("bool", "b1")]


def _expected(rec, name, kind):
    v = None if rec is None else rec.get(name)
    if v is None:
        if kind == "O":
            return None
        return {"f4": "nan", "f8": "nan"}.get(kind, 0 if kind != "b1" else False)
    return v


def test_p018_read_numpy_matches_get_all_types(tmp_path):
    import random
    rnd = random.Random(18)
    db = nhydb.NHyDB.create(str(tmp_path / "t.nhydb"))
    db.create_realm(1, "r")
    fields = [{"v": i + 1, "name": f"c{i}", "type": t} for i, (t, _) in enumerate(TYPES_ALL)]
    fields.append({"v": 20, "name": "lage", "type": "u8", "enum": {"A": 0, "B": 1}})
    db.create_shape(1, 1, "s", fields)
    # an older shape with other types for the same v (generic path)
    db.create_shape(2, 1, "old", [{"v": 1, "name": "c0", "type": "varint"}, {"v": 2, "name": "c1", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=30)
    db.define_extension(1, 40, "varint", name="e")
    lim = {"u8": (0, 255), "i8": (-128, 127), "u16": (0, 65535), "i16": (-32768, 32767), "u32": (0, 2**32 - 1),
           "i32": (-2**31, 2**31 - 1), "i64": (-2**63, 2**63 - 1), "u64": (0, 2**64 - 1), "varuint": (0, 2**64 - 1),
           "varint": (-2**63, 2**63 - 1)}

    def val(t):
        if t in lim:
            lo, hi = lim[t]
            return rnd.choice([lo, hi, rnd.randint(lo, hi), 0])
        if t == "bool":
            return rnd.random() < 0.5
        if t == "f32":
            return rnd.choice([1.5, -2.25, 0.0, float("inf")])
        return rnd.choice([rnd.random() * 1e9, -0.5, float("inf")])

    n = 400
    rows = [[val(t) for t, _ in TYPES_ALL] + [rnd.choice(["A", "B"])] for _ in range(n)]
    db.put_dense_batch(1, 1, rows)
    db.put_dense_batch(1, 50, [[7, "x"], [-8, "y"]], shape_id=2)  # other shape
    db.put(1, 60, rows[0], extensions={40: 5})  # extension -> generic path
    db.put_cell(1, 70, 3, 99, type="u16")  # sparse overlay
    db.delete_cell(1, 71, 1)  # tombstone over BASE
    db.delete(1, 80)  # deleted record
    db.put_cell(1, 90, 9, 2**70, type="varuint")  # does not fit uint64
    db.commit()
    names = [f["name"] for f in fields] + ["e"]
    out = db.read_numpy(1, 1, n + 10, fields=names[:-1] + ["e"], object_fallback=True)
    for u in range(1, n + 11):
        rec = db.get(1, u)
        i = u - 1
        assert bool(out["_present"][i]) == (rec is not None), u
        for name in names:
            got = out[name][i]
            exp = _expected(rec, name, {"O": "O", "f": "f8", "b": "b1"}.get(out[name].dtype.kind, "i8"))
            if isinstance(exp, str) and exp == "nan":
                assert got != got, (u, name, got)
            elif isinstance(exp, float) and exp != exp:
                assert got != got
            elif exp is None:
                # object column: a column switched to object late keeps the
                # typed null value (0) of the rows before
                assert got is None or got == 0, (u, name, got)
            else:
                assert got == exp, (u, name, got, exp)
    # the value beyond 64 bit makes c8 an object column only with object_fallback
    assert out["c8"].dtype == object and out["c8"][89] == 2**70
    with pytest.raises(ValueError, match="does not fit"):
        db.read_numpy(1, 1, n, fields=["c8"])
    assert db.read_numpy(1, 1, 80, fields=["c8"])["c8"].dtype == np.uint64
    db.close()


# ----------------------------------------------------------------------
# P011: opt-in write batch
# ----------------------------------------------------------------------

def _two_realms(path):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "a")
    db.create_realm(2, "b")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "varint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=5)
    db.put_dense_batch(1, 1, [[i] for i in range(1, 21)])
    db.put_cell(2, 7, 7, "old", type="utf8")
    db.commit()
    return db


def _ops():
    return [
        ("put", 2, 1, 1, 1.5, "f64", 0), ("put", 2, 1, 2, 2.5, "f64", 1), ("put", 2, 1, 1, 9.5, "f64", 0),  # overwrite
        ("put", 1, 3, 1, 333, None, 0),  # overlay of a BASE field (type from the shape)
        ("del", 1, 4, 1, None, None, 0),  # delete of a BASE value
        ("del", 2, 99, 1, None, None, 0),  # delete of an empty cell -> nothing
        ("put", 2, 50, 3, "a", "utf8", 0), ("del", 2, 50, 3, None, None, 0),  # put then delete
        ("del", 2, 7, 7, None, None, 0), ("put", 2, 7, 7, "new", "utf8", 0),  # delete then put
        ("del", 2, 7, 8, None, None, 0),
    ] + [("put", 2, 100 + i // 7, 1 + i % 7, i, "varint", i % 2) for i in range(300)]


def _apply_single(db, ops):
    for op, r, u, v, val, t, lane in ops:
        if op == "put":
            db.put_cell(r, u, v, val, type=t, lane=lane)
        else:
            db.delete_cell(r, u, v, lane=lane)


def test_p011_batch_equals_single_calls(tmp_path):
    a = _two_realms(tmp_path / "a.nhydb")
    b = _two_realms(tmp_path / "b.nhydb")
    _apply_single(a, _ops())
    blocks_before = b.next_block_id
    with b.batch() as w:
        for op, r, u, v, val, t, lane in _ops():
            if op == "put":
                assert w.put_cell(r, u, v, val, type=t, lane=lane) is None
            else:
                w.delete_cell(r, u, v, lane=lane)
        assert len(w) > 0
        assert b.get_value(2, 1, 1) is None  # documented: no read-your-writes inside
    a.commit()
    b.commit()
    assert _dump(a) == _dump(b)
    assert b.next_block_id - blocks_before <= 6 < a.next_block_id - blocks_before
    assert b.get(1, 4)["x"] is None and b.get_value(2, 7, 7) == "new" and b.get_value(2, 50, 3) is None
    a.close()
    b.close()
    with nhydb.NHyDB(tmp_path / "b.nhydb", readonly=True) as r:
        assert r.verify(deep=True)["ok"]


def test_p011_batch_errors_and_lifecycle(tmp_path):
    p = tmp_path / "e.nhydb"
    db = _two_realms(p)
    m = nm.MathMap.compile("mm", ("x",), u="x + 1", V="1", side="0")
    db.create_math_layout(nm.MathMapLayoutSpec("mm", m, 4), layout_id=9, payload_type="varint")
    db.put_math(9, (1,), 5, commit=True)
    math_realm = db.put_math(9, (2,), 6)["physical_realm"]
    db.commit()
    size = p.stat().st_size
    w = db.batch()
    with pytest.raises(nhydb.SchemaError):
        w.put_cell(77, 1, 1, 1, type="varint")  # unknown realm, reported at the call
    with pytest.raises(nhydb.SchemaError, match="exclusively"):
        w.put_cell(math_realm, 1, 1, 1, type="varint")
    with pytest.raises((nhydb.SchemaError, ValueError, TypeError)):
        w.put_cell(2, 1, 1, 1)  # type cannot be derived
    with pytest.raises((ValueError, TypeError, nhydb.NHyDBError)):
        w.put_cell(2, 1, 1, "text", type="varint")
    with pytest.raises((ValueError, nhydb.AddressError, nhydb.NHyDBError)):
        w.put_cell(2, 0, 1, 1, type="varint")
    assert len(w) == 0
    # an exception inside the block writes nothing and is not swallowed
    with pytest.raises(KeyError):
        with db.batch() as w2:
            w2.put_cell(2, 1, 1, 1, type="varint")
            raise KeyError("boom")
    with pytest.raises(RuntimeError, match="already"):
        w2.put_cell(2, 1, 1, 1, type="varint")
    db.commit()
    assert db.get_value(2, 1, 1) is None and p.stat().st_size == size
    # commit=True commits
    with db.batch(commit=True) as w3:
        w3.put_cell(2, 5, 5, 55, type="varint")
    db.close(commit=False)
    with nhydb.NHyDB(p) as db2:
        assert db2.get_value(2, 5, 5) == 55
    with nhydb.NHyDB(p, readonly=True) as ro:
        with pytest.raises(nhydb.NHyDBError, match="read-only"):
            ro.batch().put_cell(2, 6, 6, 1, type="varint")


# ----------------------------------------------------------------------
# P024: rollback() (opt-in)
# ----------------------------------------------------------------------

def test_p024_rollback_restores_committed_state(tmp_path):
    import hashlib
    p = tmp_path / "r.nhydb"
    db = _two_realms(p)
    m = nm.MathMap.compile("mm", ("x", "k"), u="x + 1", V="k*10**100 + 1", side="0")
    db.create_math_layout(nm.MathMapLayoutSpec("mm", m, 8), layout_id=9, payload_type="varint")
    db.put_math(9, (1, 0), 5)
    db.commit()
    state = _dump(db)
    schema = (db.realms, db._export_external_schema(), db.describe_math(9), db.max_db_size_bytes, db.stats()["generation"])
    committed = db.committed_end
    digest = hashlib.sha256(p.read_bytes()[:committed]).hexdigest()
    assert db.rollback() == committed  # nothing pending: no-op
    # uncommitted: data, overlays, deletes, new realm/shape/layout, MATH realms, size limit
    db.put_dense_batch(1, 30, [[i] for i in range(50)])
    db.put_cell(2, 1, 1, 5, type="varint")
    db.delete(1, 3)
    db.create_realm(50, "neu")
    db.create_shape(5, 50, "s5", [{"v": 1, "name": "y", "type": "varint"}])
    db.put_math(9, (2, 3), 7)  # new MATH realm
    db.set_max_db_size(10**9)
    db.sync()  # make sure blocks reached the file
    assert db.rollback() == committed
    assert _dump(db) == state
    assert (db.realms, db._export_external_schema(), db.describe_math(9), db.max_db_size_bytes, db.stats()["generation"]) == schema
    assert db.get(1, 3)["x"] == 3 and db.get(1, 30) is None
    assert p.stat().st_size == committed
    assert hashlib.sha256(p.read_bytes()[:committed]).hexdigest() == digest
    with pytest.raises(nhydb.NHyDBError):
        db.create_shape(5, 50, "x", [])  # realm 50 is gone
    # the handle keeps working
    db.put_cell(2, 2, 2, 22, type="varint")
    db.put_math(9, (3, 1), 8)
    db.commit()
    db.close()
    with nhydb.NHyDB(p) as db2:
        assert db2.verify(deep=True)["ok"]
        assert db2.get_value(2, 2, 2) == 22 and db2.get_math(9, (3, 1)) == 8 and db2.get_math(9, (2, 3)) is None
    with nhydb.NHyDB(p, readonly=True) as ro:
        with pytest.raises(nhydb.NHyDBError, match="read-only"):
            ro.rollback()


def test_p024_rollback_without_lock_detects_concurrent_writer(tmp_path):
    p = tmp_path / "n.nhydb"
    w = _two_realms(p)
    r = nhydb.NHyDB(str(p), lock=False)
    w.put_cell(2, 1, 1, 1, type="varint", commit=True)
    r.put_cell(2, 9, 9, 9, type="varint")  # stays in the append buffer
    with pytest.raises(nhydb.DatabaseLockedError, match="concurrent writer"):
        r.rollback()
    with pytest.raises(nhydb.NHyDBError):
        r.close()
    w.put_cell(2, 3, 3, 3, type="varint", commit=True)
    w.close()
    with nhydb.NHyDB(p, readonly=True) as x:
        assert x.verify(deep=True)["ok"]
        assert x.get_value(2, 1, 1) == 1 and x.get_value(2, 3, 3) == 3
