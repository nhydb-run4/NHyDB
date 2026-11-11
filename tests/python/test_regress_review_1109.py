"""Regression pool of the pre-release review of 2026-11-09 and its
corrections.

Every test names its finding. The expectations are concrete values computed
from the data the test writes; none of them was taken from a run of the
corrected build.
"""
import json
import os
import subprocess
import sys
import textwrap
import threading
import time

import numpy as np
import pytest

import nhydb
import nhydb.math as nm
from nhydb.query import Q, QueryEngine


@pytest.fixture(autouse=True)
def _own_config(monkeypatch):
    """A NHYDB_CONFIG of the environment (dir3 QA runs) must not change the
    configuration these tests count on (rebuild thresholds, limiter)."""
    monkeypatch.delenv("NHYDB_CONFIG", raising=False)


def _is_panic(e):
    """pyo3_runtime.PanicException derives from BaseException only."""
    return type(e).__name__ == "PanicException"


def _child_env():
    env = dict(os.environ)
    env.pop("NHYDB_CONFIG", None)
    return env


def _run_child(code, cwd, timeout=120):
    """Runs ``code`` in a fresh interpreter (the RAM limiter is process wide);
    returns the JSON of its last stdout line."""
    r = subprocess.run([sys.executable, "-c", textwrap.dedent(code)], cwd=str(cwd), env=_child_env(),
                       capture_output=True, text=True, timeout=timeout)
    assert "panicked" not in r.stderr, r.stderr[-2000:]
    assert r.returncode == 0, (r.returncode, r.stdout[-2000:], r.stderr[-2000:])
    return json.loads(r.stdout.strip().splitlines()[-1])


class _Holder:
    """Another thread that keeps the handle busy: ``build_indexes`` holds the
    handle (mutably borrowed, GIL released) for its whole run."""

    def __init__(self, db):
        self.db = db
        self.stop = threading.Event()
        self.busy = threading.Event()
        self.errors = []
        self.t = threading.Thread(target=self._run, daemon=True)

    def _run(self):
        try:
            while not self.stop.is_set():
                self.busy.set()
                self.db.build_indexes(commit=False)
        except BaseException as e:  # noqa: BLE001 - reported by the test
            self.errors.append(e)
            self.busy.set()

    def __enter__(self):
        self.t.start()
        assert self.busy.wait(30)
        time.sleep(0.02)
        return self

    def __exit__(self, *exc):
        self.stop.set()
        self.t.join(120)
        assert not self.t.is_alive()
        assert not self.errors, self.errors
        return False


def _busy_db(p, n=150_000):
    """A database whose index build takes long enough for thread conflicts."""
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"},
                                {"v": 3, "name": "k", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_index(5, 1, "ix_k", 3)
    db.put_numpy_batch(1, 1, {"id": np.arange(1, n + 1, dtype=np.uint64),
                              "body": np.array(["b%07d" % u + "y" * 20 for u in range(n)], dtype=object),
                              "k": np.arange(1, n + 1, dtype=np.uint64) % 1000}, shape_id=1)
    db.commit()
    db.build_indexes(commit=True)
    return db


def _conflict(call, attempts=3000):
    """Calls ``call`` until it is refused because the other thread holds the
    handle. Returns the exception (None: no conflict seen). A PanicException
    fails the test at once."""
    for _ in range(attempts):
        try:
            call()
        except BaseException as e:  # noqa: BLE001 - PanicException is no Exception
            assert not _is_panic(e), f"PanicException instead of RuntimeError: {e}"
            if isinstance(e, RuntimeError) and "borrow" in str(e).lower():
                return e
            raise
        time.sleep(0.0005)
    return None


# ---------------------------------------------------------------- R9C-01


def _stale_text_db(p):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s1", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 4, "name": "s", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_token_index(7, 1, "ts", "s")
    db.create_fulltext_index(9, 1, "fs", "s")
    db.put_dense_batch(1, 1, [[u, "beta" if u % 2 else "alpha"] for u in range(1, 21)], shape_id=1)
    db.build_indexes(commit=True)
    db.put_dense_batch(1, 5, [[u, "gamma"] for u in range(5, 10)], shape_id=1)   # u 5..9 rewritten
    db.delete_record(1, 11)
    db.commit()
    return db


BETA = [1, 3, 13, 15, 17, 19]          # odd u of 1..20 without 5, 7, 9 (gamma) and 11 (deleted)


def test_r9c01_count_token_fulltext_on_stale_index(tmp_path):
    """R9C-01: COUNT(*) and the count return mode with exactly one TOKEN or
    FULLTEXT term on an index that is stale (default maintenance=auto, the
    changes stay below the rebuild threshold) count the rows of the same
    condition, not the old key tree; the estimate is not exact while the
    index is stale and exact again after build_indexes."""
    db = _stale_text_db(tmp_path / "x.nhydb")
    for iid, op in ((7, "token"), (9, "fulltext")):
        est = db.index_candidate_estimate(iid, op, "beta")
        assert est["exact"] is False, (op, est)
    assert db.posting_count(7, "beta") == len(BETA)
    for w in ("FULLTEXT(s, 'beta')", "TOKEN(s, 'beta')"):
        ids = [r["id"] for r in db.sql(f"SELECT id FROM c WHERE {w} ORDER BY id")["rows"]]
        assert ids == BETA, w
        assert db.sql(f"SELECT COUNT(*) AS n FROM c WHERE {w}")["rows"][0]["n"] == len(BETA), w
    qe = QueryEngine(db)
    for e in (Q.field("s").fulltext("beta"), Q.field("s").token("beta")):
        assert qe.execute(e, realm=1, return_mode="count")["count"] == len(BETA), e
        assert db.query(e, realm=1, return_mode="count")["count"] == len(BETA), e
    db.build_indexes(commit=True)
    for iid, op in ((7, "token"), (9, "fulltext")):
        est = db.index_candidate_estimate(iid, op, "beta")
        assert est["exact"] is True and est["estimated_rows"] == len(BETA), (op, est)
    for w in ("FULLTEXT(s, 'beta')", "TOKEN(s, 'beta')"):
        assert db.sql(f"SELECT COUNT(*) AS n FROM c WHERE {w}")["rows"][0]["n"] == len(BETA), w
    for e in (Q.field("s").fulltext("beta"), Q.field("s").token("beta")):
        assert qe.execute(e, realm=1, return_mode="count")["count"] == len(BETA), e
    db.close()


# ---------------------------------------------------------------- R9A-01


MERGE = "[index]\nrebuild_min_changes = 1\nrebuild_ratio = 0.0001\nbulk_commit_ratio = 1.0\n"


def _bigu_schema(db, n):
    db.create_realm(1, "r")
    db.create_shape(1, 1, "A", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.define_extension(1, 30, "varuint", name="ext")
    db.create_index(1, 1, "ix", "x")
    db.put_dense_batch(1, 1, [[u, u % 3] for u in range(1, n + 1)], shape_id=1)
    db.commit()
    db.build_indexes(commit=True)


def _merges(db):
    lanes = [c for c in db.health()["checks"] if c["name"] == "indexes"][0]["details"]["maintenance"]["lanes"]
    return sum(x["merge_rebuilds"] for x in lanes if x["realm"] == 1 and x["lane"] == 0)


def _bigu_check(db, want):
    assert db.find_u(1, {"ix": 0}) == want
    assert db.sql("SELECT COUNT(*) AS n FROM main WHERE x = 0")["rows"][0]["n"] == len(want)
    assert [r["id"] for r in db.sql("SELECT id FROM main WHERE x = 0 ORDER BY id")["rows"]] == want


@pytest.mark.parametrize("v", [30, 9])
@pytest.mark.parametrize("big_u", [2**64, 2**64 + 1, 10**30])
def test_r9a01_merge_rebuild_with_big_u_keeps_entries(tmp_path, big_u, v):
    """R9A-01: a commit that touches a u >= 2^64 of the lane without an
    indexed value (an extension cell v=30 or a sparse cell v=9) triggers the
    merge rebuild (thresholds of 1 change); it keeps every posting – find_u
    and COUNT(*) equal the expected set after the commit and after reopening,
    and the merge left the index fresh."""
    cfg = tmp_path / "merge.toml"
    cfg.write_text(MERGE)
    p = str(tmp_path / "a.nhydb")
    n = 3000
    db = nhydb.NHyDB.create(p, config=str(cfg))
    _bigu_schema(db, n)
    want = [u for u in range(1, n + 1) if u % 3 == 0]
    db.put_cell(1, big_u, v, 1, type="varuint")
    db.commit()
    assert _merges(db) == 1
    assert db.index_candidate_estimate(1, "eq", 0)["exact"] is True     # merged, fresh again
    _bigu_check(db, want)
    assert db.get_value(1, big_u, v) == 1
    assert db.verify(deep=True)["ok"]
    db.close()
    db = nhydb.NHyDB(p, config=str(cfg))
    _bigu_check(db, want)
    db.close()


def test_r9a01_merge_rebuild_default_config_bulk_then_small_commit(tmp_path):
    """R9A-01 with the default configuration: a bulk commit of extension
    cells (one at u = 2^64), then a small commit that triggers the merge
    rebuild; no posting is lost, also after reopening."""
    p = str(tmp_path / "a.nhydb")
    n = 2000
    db = nhydb.NHyDB.create(p)
    _bigu_schema(db, n)
    want = [u for u in range(1, n + 1) if u % 3 == 0]
    db.put_points(1, [(u, 30, 1, "varuint") for u in range(1, 1100)] + [(2**64, 30, 1, "varuint")])
    db.commit()
    assert _merges(db) == 1                   # crisis bound: merged at this commit
    _bigu_check(db, want)
    db.put_cell(1, 7, 30, 2, type="varuint")
    db.commit()
    _bigu_check(db, want)
    assert len(db.sql("SELECT id FROM main WHERE x = 0 AND id > 1500")["rows"]) == len([u for u in want if u > 1500])
    db.close()
    db = nhydb.NHyDB(p)
    _bigu_check(db, want)
    db.close()


# ---------------------------------------------------------------- R9C-02


def _sparse_order_db(p, dense=True):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "varuint"}, {"v": 2, "name": "y", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    if dense:
        db.put_dense_batch(1, 1, [[u, u % 3] for u in range(1, 6)])     # DENSE u 1..5
        db.put_cell(1, 3, 1, 20, type="varuint")                         # mixed: u 3 x overwritten
    db.put_cell(1, 50, 1, 7, type="varuint")                             # u 50: sparse cells only
    db.put_cell(1, 50, 2, 1, type="varuint")
    db.put_cell(1, 60, 1, 9, type="varuint")                             # u 60: one sparse cell (x)
    db.put_cell(1, 70, 1, 11, type="varuint")
    db.put_cell(1, 70, 2, 2, type="varuint")
    db.commit()
    return db


def _streams(db, q):
    """Rows (x, y) of the four streaming paths."""
    out = {"sql_iter": [(r["x"], r["y"]) for r in db.sql_iter(q)]}
    cols = db.sql_numpy(q)
    out["sql_numpy"] = [int(x) for x in cols["x"]]
    f = db.sql_to_file(q)
    try:
        out["sql_to_file"] = [(r["x"], r["y"]) for r in f]
    finally:
        f.close()
    jf = db.submit_sql(q).wait()
    try:
        out["submit_sql"] = [(r["x"], r["y"]) for r in jf]
    finally:
        jf.close()
    return out


ORDERS = ["ORDER BY x", "ORDER BY x ASC", "ORDER BY x DESC"]
LIMITS = ["", " LIMIT 3", " LIMIT 3 OFFSET 2", " LIMIT 100 OFFSET 1"]


@pytest.mark.parametrize("dense", [True, False], ids=["mixed", "sparse_only"])
def test_r9c02_streaming_sql_where_order_by_includes_sparse_records(tmp_path, dense):
    """R9C-02: sql_iter, sql_numpy, sql_to_file and submit_sql with WHERE +
    ORDER BY return the rows of sql() in the same order, including records
    made of sparse cells only (u 50, 60, 70) and a DENSE record whose key was
    overwritten by a sparse cell (u 3); ASC/DESC, with and without
    LIMIT/OFFSET."""
    db = _sparse_order_db(tmp_path / "a.nhydb", dense)
    xs = sorted(([1, 2, 20, 4, 5] if dense else []) + [7, 9, 11])
    full = db.sql("SELECT x, y FROM c WHERE x >= 0 ORDER BY x DESC")["rows"]
    assert [r["x"] for r in full] == xs[::-1]
    assert {r["x"]: r["y"] for r in full}[9] is None
    for order in ORDERS:
        for lim in LIMITS:
            for where in ("x >= 0", "x > 1", "y IS NULL OR x >= 7"):
                q = f"SELECT x, y FROM c WHERE {where} {order}{lim}"
                want = [(r["x"], r["y"]) for r in db.sql(q)["rows"]]
                got = _streams(db, q)
                for path, rows in got.items():
                    exp = [x for x, _ in want] if path == "sql_numpy" else want
                    assert rows == exp, (path, q)
    # explicit values of one case, independent of sql()
    q = "SELECT x, y FROM c WHERE x >= 7 ORDER BY x ASC"
    assert _streams(db, q)["sql_iter"] == ([(7, 1), (9, None), (11, 2), (20, 0)] if dense else [(7, 1), (9, None), (11, 2)])
    db.close()


# ---------------------------------------------------------------- R9A-04


def test_r9a04_n_paths_report_cell_existence(tmp_path):
    """R9A-04: get_n_cell, get_n(mode="cell"/"auto"), get_address(mode="cell")
    and describe_n report present/occupied False for a BASE cell of a live
    record deleted by delete_cell and for a field the stored shape lacks
    (stored under another realm default shape), True for live cells
    including a stored None – as get_cell does."""
    db = nhydb.NHyDB.create(str(tmp_path / "a.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "A", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}])
    db.create_shape(2, 1, "B", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"},
                                {"v": 3, "name": "y", "type": "utf8"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put(1, 7, [7, 1])                                          # stored with shape A: no y
    db.set_realm_default_shape(1, 2)                              # current shape B has y
    db.put(1, 5, [5, 1, "a"])
    assert db.delete_cell(1, 5, 3) is True                       # BASE y of a live record deleted
    db.put(1, 8, [8, 4, "b"])
    db.put_cell(1, 8, 2, None)                                    # stored None: a live cell
    db.commit()
    cases = [((5, 3), False, None), ((7, 3), False, None), ((8, 2), True, None), ((5, 2), True, 1), ((7, 2), True, 1)]
    for (u, v), live, value in cases:
        gc = db.get_cell(1, u, v, dyn_address=False)
        assert gc["present"] is live, (u, v, gc)
        n = db.cell_address(1, u, v)["N"]
        gn = db.get_n_cell(n)
        assert (gn["present"], gn["value"]) == (live, value), (u, v, gn)
        g = db.get_n(n, mode="cell")
        assert (g["present"], g["value"]) == (live, value), (u, v, g)
        ga = db.get_n(n, mode="auto")
        assert ga["effective_mode"] == "cell" and ga["present"] is live, (u, v, ga)
        ad = db.get_address(1, u, v, mode="cell")
        assert (ad["present"], ad["value"]) == (live, value), (u, v, ad)
        assert db.describe_n(n)["occupied"] is live, (u, v)
    db.close()


# ---------------------------------------------------------------- R9D-02 / R9B-01 (E2)


def _small_db(p):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "A", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put(1, 1, [1, 1])
    db.commit()
    return db


def _assert_consumed(b):
    assert len(b) == 0
    for call in (lambda: b.put_cell(1, 3, 2, 1), lambda: b.delete_cell(1, 1, 2), lambda: b.delete_record(1, 1),
                 lambda: b.delete_area(1, 1, 2, 1, 2), lambda: b.flush()):
        with pytest.raises(RuntimeError, match="consumed"):
            call()
    assert len(b) == 0


def test_r9d02_failed_flush_write_error_consumes_batch(tmp_path):
    """R9D-02/R9B-01, E2 (a): a flush that fails with DatabaseSizeLimitError
    consumes the batch – every further call raises RuntimeError, len() is 0,
    the batch cells are missing; an earlier uncommitted put_cell of the handle
    is kept and becomes persistent with commit()."""
    p = str(tmp_path / "a.nhydb")
    db = _small_db(p)
    db.set_max_db_size(db.stats()["current_end"] + 4096, commit=True)
    db.put_cell(1, 5, 2, 9)                                       # uncommitted, before the batch
    b = db.batch()
    for u in range(10, 2010):
        b.put_cell(1, u, 2, u % 3)
    assert len(b) == 2000
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        b.flush()
    _assert_consumed(b)
    assert db.get_value(1, 5, 2) == 9
    assert [db.get_cell(1, u, 2) for u in (10, 11, 2009)] == [None, None, None]
    db.commit()
    db.close()
    db = nhydb.NHyDB(p)
    assert db.get_value(1, 5, 2) == 9
    assert db.get_value(1, 1, 2) == 1
    assert db.get_cell(1, 10, 2) is None
    assert db.verify()["ok"]
    db.close()


def test_r9d02_failed_flush_in_with_block_commits_nothing(tmp_path):
    """E2 (a) in a ``with batch(commit=True)`` block: the failed flush is
    caught inside the block; leaving the block writes and commits nothing –
    the earlier uncommitted write stays uncommitted (lost on reopen)."""
    p = str(tmp_path / "a.nhydb")
    db = _small_db(p)
    db.set_max_db_size(db.stats()["current_end"] + 4096, commit=True)
    db.put_cell(1, 5, 2, 9)
    committed = db.stats()["committed_end"]
    with db.batch(commit=True) as b:
        for u in range(10, 2010):
            b.put_cell(1, u, 2, 1)
        with pytest.raises(nhydb.DatabaseSizeLimitError):
            b.flush()
        with pytest.raises(RuntimeError, match="consumed"):
            b.put_cell(1, 3, 2, 1)
    st = db.stats()
    assert st["committed_end"] == committed
    assert st["current_end"] > committed                         # the put_cell before the batch
    assert db.get_value(1, 5, 2) == 9 and db.get_cell(1, 10, 2) is None
    _assert_consumed(b)
    db.close(commit=False)
    db = nhydb.NHyDB(p)
    assert db.get_cell(1, 5, 2) is None and db.get_cell(1, 10, 2) is None
    assert db.get_value(1, 1, 2) == 1
    db.close()


def test_r9d02_successful_flush_keeps_batch_active(tmp_path):
    """E2: a successful flush() inside the block leaves the batch active;
    further calls are collected and written at the end of the block."""
    db = _small_db(tmp_path / "a.nhydb")
    with db.batch(commit=True) as b:
        b.put_cell(1, 3, 2, 30)
        b.delete_cell(1, 1, 2)
        st = b.flush()
        assert (st["cells"], st["tombstones"], st["deleted"]) == (1, 1, 1)
        assert len(b) == 0
        b.put_cell(1, 4, 2, 40)
        b.put_cell(1, 4, 2, 41)
        assert len(b) == 2
    assert db.get_value(1, 3, 2) == 30 and db.get_value(1, 4, 2) == 41
    assert db.get_cell(1, 1, 2)["present"] is False
    with pytest.raises(RuntimeError, match="already been written"):
        b.put_cell(1, 5, 2, 1)
    db.close()


def test_r9d02_flush_while_other_thread_holds_handle(tmp_path):
    """R9D-02, E2 (b): flush() while another thread holds the handle raises
    RuntimeError (no PanicException) and consumes the batch: its cells are
    never written."""
    db = _busy_db(tmp_path / "t.nhydb")
    before = db.get_value(1, 5, 3)
    seen = None
    batches = []
    for _ in range(200):                      # collected before the other thread starts
        b = db.batch()
        b.put_cell(1, 5, 3, 4242)
        b.delete_cell(1, 6, 3)
        assert len(b) == 2
        batches.append(b)
    with _Holder(db):
        for b in batches:
            seen = _conflict(b.flush, attempts=1)
            if seen is not None:
                break
            time.sleep(0.001)                 # the flush got through: try the next one
        if seen is None:
            pytest.skip("no conflicting flush observed in 200 attempts")
        _assert_consumed(b)
    assert isinstance(seen, RuntimeError)
    db.rollback()                              # flushes that got through are not committed
    assert db.get_value(1, 5, 3) == before
    assert db.get_cell(1, 6, 3)["present"] is True
    db.rollback()
    db.close()


# ---------------------------------------------------------------- R9D-01


class _Plugin:
    pass


def test_r9d01_concurrent_use_raises_runtime_error(tmp_path):
    """R9D-01: while another thread writes on the same handle, iterator
    next(), QueryEngine.execute/compile_sql, WriteBatch.put_cell/delete_area,
    ``with db:`` (its enter), set_dyn_address_output and
    register_layout_plugin raise RuntimeError – never PanicException."""
    db = _busy_db(tmp_path / "t.nhydb")
    it = db.sql_iter("SELECT id FROM c")
    qe = QueryEngine(db)
    b = db.batch()
    names = iter(range(10**9))
    calls = {
        "iter_next": lambda: next(it),
        "QueryEngine.execute": lambda: qe.execute(Q.field("k") == 7, realm=1, return_mode="count"),
        "QueryEngine.compile_sql": lambda: qe.compile_sql("SELECT id FROM c WHERE k = 7"),
        "WriteBatch.put_cell": lambda: b.put_cell(1, 5, 3, 1),
        "WriteBatch.delete_area": lambda: b.delete_area(1, 1, 2, 3, 3),
        "with_db_enter": lambda: db.__enter__(),
        "set_dyn_address_output": lambda: db.set_dyn_address_output("off"),
        "register_layout_plugin": lambda: db.register_layout_plugin("p%d" % next(names), _Plugin()),
    }
    missing = []
    with _Holder(db):
        for name, call in calls.items():
            e = _conflict(call)
            if e is None:
                missing.append(name)
            else:
                assert isinstance(e, RuntimeError), (name, e)
    if missing:
        pytest.skip(f"no conflict observed for {missing}")
    b.discard()
    db.rollback()
    db.close()


# ---------------------------------------------------------------- R9A-02


def _counter_db(p):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "A", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.put(1, 5, [5, 0])                      # DENSE cell (5, 2)
    db.put_cell(1, 6, 2, 3, type="varint")    # SPARSE cell (6, 2)
    db.put_cell(1, 8, 2, None)                # stored None: a live cell
    db.commit()
    return db


R9A02_CASES = {
    "delete_put_dense": ([("d", 5, 2), ("p", 5, 2, 9)], 1),
    "delete_put_sparse": ([("d", 6, 2), ("p", 6, 2, 9)], 1),
    "delete_put_none": ([("d", 8, 2), ("p", 8, 2, 9)], 1),
    "delete_put_delete": ([("d", 5, 2), ("p", 5, 2, 9), ("d", 5, 2)], 2),
    "put_delete_new": ([("p", 7, 2, 9), ("d", 7, 2)], 1),
    "put_delete_live": ([("p", 6, 2, 9), ("d", 6, 2)], 1),
    "delete_not_live": ([("d", 7, 2)], 0),
    "delete_twice": ([("d", 5, 2), ("d", 5, 2)], 1),
    "delete_put_put_delete_put": ([("d", 5, 2), ("p", 5, 2, 1), ("p", 5, 2, 2), ("d", 5, 2), ("p", 5, 2, 3)], 2),
}


@pytest.mark.parametrize("case", sorted(R9A02_CASES))
def test_r9a02_batch_deleted_counter_equals_single_calls(tmp_path, case):
    """R9A-02: the ``deleted`` counter of a batch flush equals the sum of the
    return values of the same single calls on a second database; the final
    state is equal too."""
    ops, expected = R9A02_CASES[case]
    a = _counter_db(tmp_path / "single.nhydb")
    single = 0
    for op in ops:
        if op[0] == "d":
            single += int(a.delete_cell(1, op[1], op[2]))
        else:
            a.put_cell(1, op[1], op[2], op[3], type="varint")
    assert single == expected
    bdb = _counter_db(tmp_path / "batch.nhydb")
    with bdb.batch() as b:
        for op in ops:
            if op[0] == "d":
                b.delete_cell(1, op[1], op[2])
            else:
                b.put_cell(1, op[1], op[2], op[3], type="varint")
        assert len(b) == len(ops)
        st = b.flush()
    assert st["deleted"] == expected, st
    for u in (5, 6, 7, 8):
        ca, cb = a.get_cell(1, u, 2), bdb.get_cell(1, u, 2)
        assert (ca and (ca["present"], ca["value"])) == (cb and (cb["present"], cb["value"])), u
    a.close()
    bdb.close()


# ---------------------------------------------------------------- R9D-04


def test_r9d04_limiter_log_interval_too_large_is_value_error(tmp_path):
    """R9D-04: ``[limiter_log] interval_min`` beyond one year (10**17,
    2**63-1, 527041) raises ValueError on create and open (no
    PanicException); afterwards a database opens normally without this
    configuration and close() works. In a subprocess: the limiter is process
    wide."""
    code = f"""
    import json, nhydb
    out = {{}}
    base = {str(tmp_path)!r}
    db = nhydb.NHyDB.create(base + "/ok.nhydb")
    db.create_realm(1, "r"); db.commit(); db.close()
    for val in (10**17, 2**63 - 1, 366 * 24 * 60 + 1):
        cfg = base + "/iv_%d.toml" % val
        open(cfg, "w").write("[limiter_log]\\nenabled = true\\ninterval_min = %d\\n" % val)
        for what, fn in (("create", lambda: nhydb.NHyDB.create(base + "/c_%d.nhydb" % val, config=cfg)),
                         ("open", lambda: nhydb.NHyDB(base + "/ok.nhydb", config=cfg))):
            try:
                h = fn()
                h.close()
                out[f"{{what}}_{{val}}"] = "opened"
            except BaseException as e:
                out[f"{{what}}_{{val}}"] = [type(e).__name__, isinstance(e, ValueError), str(e)]
    db = nhydb.NHyDB(base + "/ok.nhydb")
    db.put_cell(1, 1, 1, 5, type="varint", commit=True)
    out["value"] = db.get_value(1, 1, 1)
    db.close()
    db = nhydb.NHyDB.create(base + "/after.nhydb")
    db.create_realm(1, "r"); db.commit(); db.close()
    out["closed"] = db.closed
    print(json.dumps(out))
    """
    out = _run_child(code, tmp_path)
    for val in (10**17, 2**63 - 1, 527041):
        for what in ("create", "open"):
            r = out[f"{what}_{val}"]
            assert r[0] == "ValueError" and r[1] is True, (what, val, r)
            assert "interval_min" in r[2] and "between 1 and 527040" in r[2], r
    assert out["value"] == 5 and out["closed"] is True


# ---------------------------------------------------------------- R9D-05 (E5)


def test_r9d05_delete_area_and_batch_delete_record_cancellable(tmp_path):
    """R9D-05, E5: delete_area over many whole records and a batch flush with
    many delete_record are registered with the RAM limiter and cancellable by
    cancel_job: they end with ResourceLimitError (cancelled_by_app), the data
    and the file size are unchanged, verify() is ok. In a subprocess (the
    limiter is process wide)."""
    code = f"""
    import json, os, threading, time
    import numpy as np, nhydb
    p = {str(tmp_path / "d.nhydb")!r}
    N = 300_000
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{{"v": 1, "name": "id", "type": "varuint"}}, {{"v": 2, "name": "k", "type": "varuint"}}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_numpy_batch(1, 1, {{"id": np.arange(1, N + 1, dtype=np.uint64), "k": np.arange(1, N + 1, dtype=np.uint64) % 10}}, shape_id=1)
    db.commit()

    def snap():
        return (db.sql("SELECT COUNT(*) AS n FROM c")["rows"][0]["n"], db.sql("SELECT SUM(k) AS s FROM c")["rows"][0]["s"],
                db.get_value(1, 1, 2), db.get_value(1, N, 2), os.path.getsize(p))

    def area():
        db.delete_area(1, 1, N, 1, 2)

    def batch():
        with db.batch() as b:
            for u in range(1, N + 1):
                b.delete_record(1, u)

    base = snap()
    out = {{"base": list(base)}}
    for name, fn in (("delete_area", area), ("batch_delete_record", batch)):
        res = {{"cancelled": 0, "missed": 0, "seen_job": 0, "other": [], "changed": []}}
        for rep in range(6):
            box = {{}}
            def run():
                try:
                    box["r"] = fn()
                except BaseException as e:
                    box["e"] = e
            th = threading.Thread(target=run)
            th.start()
            hit = False
            while th.is_alive() and not hit:
                for j in nhydb.resource_status()["jobs"]:
                    hit = True
                    res["seen_job"] += 1
                    nhydb.cancel_job(j["id"])
                    break
            th.join(120)
            e = box.get("e")
            if e is None:
                res["missed"] += 1
                db.rollback()
            elif isinstance(e, nhydb.ResourceLimitError) and getattr(e, "reason", None) == "cancelled_by_app":
                res["cancelled"] += 1
                now = snap()
                if now != base:
                    res["changed"].append(list(now))
            else:
                res["other"].append([type(e).__name__, str(e)[:200]])
            if snap() != base:
                res["changed"].append(["after", list(snap())])
            if res["cancelled"] >= 2:
                break
        st = nhydb.resource_status()
        res["jobs_after"] = len(st["jobs"])
        out[name] = res
    out["verify"] = bool(db.verify(deep=True)["ok"])
    db.close()
    print(json.dumps(out))
    """
    out = _run_child(code, tmp_path, timeout=300)
    assert out["base"][0] == 300_000
    for name in ("delete_area", "batch_delete_record"):
        res = out[name]
        assert res["other"] == [] and res["changed"] == [], (name, res)
        assert res["seen_job"] > 0, (name, "never registered with the limiter", res)
        assert res["cancelled"] >= 1, (name, res)
        assert res["jobs_after"] == 0, (name, res)
    assert out["verify"] is True


# ---------------------------------------------------------------- R9E-01 (E1)


E1_FIELDS = [("s", "utf8", "x"), ("b", "bool", True), ("k", "varint", 1), ("f", "f64", 1.5), ("by", "bytes", b"x")]


def _e1_db(p):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "a", [{"v": i, "name": n, "type": t} for i, (n, t, _) in enumerate(E1_FIELDS, 1)])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.commit()
    return db


@pytest.mark.parametrize("field", [f[0] for f in E1_FIELDS])
def test_r9e01_none_in_dense_field_is_value_error(tmp_path, field):
    """R9E-01, E1: None in a DENSE field (utf8, bool, varint, f64, bytes)
    raises ValueError on put, put_dense_batch, put_numpy_batch (object
    column) and put_record; nothing is written, also not the valid rows of
    the same call. put_cell(..., None) stays allowed (a live null cell)."""
    db = _e1_db(tmp_path / "a.nhydb")
    good = {n: x for n, _, x in E1_FIELDS}
    bad = dict(good, **{field: None})
    end = db.stats()["current_end"]
    calls = {
        "put": lambda: db.put(1, 1, bad),
        "put_list": lambda: db.put(1, 1, [bad[n] for n, _, _ in E1_FIELDS]),
        "put_dense_batch": lambda: db.put_dense_batch(1, 1, [good, bad]),
        "put_numpy_batch": lambda: db.put_numpy_batch(1, 1, {n: np.array([good[n], bad[n]], dtype=object) for n, _, _ in E1_FIELDS}),
        "put_record": lambda: db.put_record(1, 1, bad),
    }
    for name, call in calls.items():
        with pytest.raises(ValueError, match="cannot store None"):
            call()
        assert db.get(1, 1) is None and db.get(1, 2) is None, name
        assert db.stats()["current_end"] == end, name
    v = [n for n, _, _ in E1_FIELDS].index(field) + 1
    db.put(1, 1, good)
    db.put_cell(1, 1, v, None)
    db.commit()
    c = db.get_cell(1, 1, v)
    assert (c["present"], c["value"]) == (True, None)
    assert db.sql(f"SELECT {field} FROM c")["rows"] == [{field: None}]
    db.close()


# ---------------------------------------------------------------- R9E-02 (E3)


def test_r9e02_read_numpy_cell_masks(tmp_path):
    """R9E-02, E3: read_numpy(masks=True) returns "_cells" {field: bool
    array}: True for real 0/False values and a stored None (value = fill
    value), False for a deleted cell; a record without a live cell has
    _present False; a sparse-only record has _present True and its sparse
    values. Without masks there is no "_cells" key."""
    db = nhydb.NHyDB.create(str(tmp_path / "a.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "k", "type": "varint"},
                                {"v": 3, "name": "b", "type": "bool"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[1, 0, False], [2, 5, True], [3, 6, True], [4, 7, True]])     # u 1..4
    db.put(1, 8, [8, 8, True])
    db.delete_cell(1, 2, 2)                           # u2: k deleted
    db.put_cell(1, 3, 2, None)                        # u3: k stored None
    for v in (1, 2, 3):                               # u4: every cell deleted
        db.delete_cell(1, 4, v)
    db.put_cell(1, 6, 2, -3, type="varint")           # u6: sparse only
    db.put_cell(1, 6, 3, True, type="bool")
    db.delete_record(1, 8)                            # u8: record deleted
    db.commit()
    r = db.read_numpy(1, 1, 8, ["id", "k", "b"], masks=True)
    assert r["_present"].dtype == bool
    assert r["_present"].tolist() == [True, True, True, False, False, True, False, False]
    assert r["k"].tolist() == [0, 0, 0, 0, 0, -3, 0, 0]
    assert r["b"].tolist() == [False, True, True, False, False, True, False, False]
    cells = r["_cells"]
    assert set(cells) == {"id", "k", "b"}
    assert all(m.dtype == bool for m in cells.values())
    assert cells["id"].tolist() == [True, True, True, False, False, False, False, False]
    assert cells["k"].tolist() == [True, False, True, False, False, True, False, False]
    assert cells["b"].tolist() == [True, True, True, False, False, True, False, False]
    plain = db.read_numpy(1, 1, 8, ["id", "k", "b"])
    assert "_cells" not in plain
    assert plain["_present"].tolist() == r["_present"].tolist()
    assert plain["k"].tolist() == r["k"].tolist()
    # SQL agrees about which records exist
    assert db.sql("SELECT id, k FROM c")["rows"] == [{"id": 1, "k": 0}, {"id": 2, "k": None}, {"id": 3, "k": None},
                                                     {"id": None, "k": -3}]
    db.close()


# ---------------------------------------------------------------- R9E-05


def test_r9e05_len_counts_calls(tmp_path):
    """R9E-05: len(WriteBatch) counts the collected calls (four calls on the
    same cell are four), 0 after flush() and after discard()."""
    db = _small_db(tmp_path / "a.nhydb")
    b = db.batch()
    for i in range(3):
        b.put_cell(1, 5, 2, i)
    b.delete_cell(1, 5, 2)
    assert len(b) == 4
    b.delete_record(1, 1)
    b.delete_record(1, 1)
    b.delete_area(1, 1, 3, 1, 2)
    assert len(b) == 7
    b.flush()
    assert len(b) == 0
    b.put_cell(1, 6, 2, 1)
    b.put_cell(1, 6, 2, 2)
    assert len(b) == 2
    b.discard()
    assert len(b) == 0
    b.flush()
    db.rollback()
    db.close()


# ---------------------------------------------------------------- R9E-06


def test_r9e06_negative_ids_are_value_errors(tmp_path):
    """R9E-06: a negative id is an error, not id 0."""
    db = _small_db(tmp_path / "a.nhydb")
    shapes = len(db.sql("SELECT id FROM main")["rows"])
    with pytest.raises(ValueError, match="must be >= 0"):
        db.create_shape(-5, 1, "neg", [{"v": 1, "name": "q", "type": "varint"}])
    with pytest.raises(ValueError, match="must be >= 0"):
        db.delete_record(-1, 1)
    with pytest.raises(ValueError, match="must be >= 0"):
        db.create_index(-1, 1, "ix", "x")
    assert len(db.index_defs) == 0
    assert db.get(1, 1)["x"] == 1 and len(db.sql("SELECT id FROM main")["rows"]) == shapes
    db.create_shape(0, 1, "zero", [{"v": 1, "name": "q", "type": "varint"}])   # 0 itself stays valid
    db.close()


# ---------------------------------------------------------------- R9E-07


def test_r9e07_put_math_batch_malformed_rows(tmp_path):
    """R9E-07: malformed rows of put_math_batch raise TypeError naming the
    row, not IndexError; nothing is written."""
    db = nhydb.NHyDB.create(str(tmp_path / "m.nhydb"))
    mm = nm.MathMap.compile("mm", ("x",), u="x + 1", V="1", side="0")
    db.create_math_layout(nm.MathMapLayoutSpec("mm", mm, 4), layout_id=9, payload_type="varint")
    for rows, row in (("NR", 0), ([(1,)], 0), ([1, 2], 0), ([((1,), 5), (1, 2, 3)], 1), ([((1,), 5), "ab"], 1)):
        with pytest.raises(TypeError, match=f"row {row} must be an \\(inputs, payload\\) pair"):
            db.put_math_batch(9, rows)
    assert db.get_math(9, (1,)) is None
    assert db.put_math_batch(9, [((1,), 5), ((2,), 6)], commit=True)["cells"] == 2
    assert (db.get_math(9, (1,)), db.get_math(9, (2,))) == (5, 6)
    db.close()


# ---------------------------------------------------------------- E4 (R9A-03)


def _e4_schema(db):
    db.create_realm(1, "r")
    db.create_shape(1, 1, "A", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}])
    db.create_classic_layout(1, 1, "main", shape_id=1)
    db.create_index(1, 1, "ix", "x")


def test_e4_index_after_compacting_emptied_lane(tmp_path):
    """E4 (R9A-03): the last record of a lane is deleted, the file
    compacted and reopened; after put_cell + commit find_u answers (no
    StaleIndexError) with maintenance=auto."""
    p = str(tmp_path / "a.nhydb")
    db = nhydb.NHyDB.create(p)
    _e4_schema(db)
    db.put(1, 5, [5, 1])
    db.commit()
    db.build_indexes(commit=True)
    assert db.find_u(1, {"ix": 1}) == [5]
    db.delete_record(1, 5, commit=True)
    db.close()
    nhydb.NHyDB.compact_file(p, make_backup=False)
    db = nhydb.NHyDB(p)
    db.put_cell(1, 7, 2, 1, type="varint")
    db.commit()
    assert db.find_u(1, {"ix": 1}) == [7]
    for i in range(3):
        db.put_cell(1, 8 + i, 2, 1, type="varint")
        db.commit()
    assert db.find_u(1, {"ix": 1}) == [7, 8, 9, 10]
    assert db.sql("SELECT COUNT(*) AS n FROM main WHERE x = 1")["rows"][0]["n"] == 4
    db.close()
    db = nhydb.NHyDB(p)
    assert db.find_u(1, {"ix": 1}) == [7, 8, 9, 10]
    db.close()


def test_e4_index_built_on_empty_lane(tmp_path):
    """E4: an index built on an empty lane answers after the first record
    and its commit."""
    db = nhydb.NHyDB.create(str(tmp_path / "b.nhydb"))
    _e4_schema(db)
    db.commit()
    db.build_indexes(commit=True)
    db.put_cell(1, 7, 2, 1, type="varint")
    db.commit()
    assert db.find_u(1, {"ix": 1}) == [7]
    db.put(1, 9, [9, 1], commit=True)
    assert db.find_u(1, {"ix": 1}) == [7, 9]
    db.close()


# ---------------------------------------------------------------- E6 (R9B-04)


def _leftover_checks(db):
    return [c for c in db.health()["checks"] if c["name"] == "compaction_leftover"]


def test_e6_compaction_leftover_removed_by_writer(tmp_path):
    """E6 (R9B-04): a leftover ``<db>.compacting`` is removed when a writer
    holding the lock opens the database; without one health() has no
    compaction_leftover check."""
    p = str(tmp_path / "a.nhydb")
    db = _small_db(p)
    assert _leftover_checks(db) == []
    db.close()
    left = p + ".compacting"
    with open(left, "wb") as f:
        f.write(b"\0" * 12345)
    db = nhydb.NHyDB(p)
    assert not os.path.exists(left)
    assert _leftover_checks(db) == []
    assert db.cleanup_compaction() == 0
    assert db.get_value(1, 1, 2) == 1
    db.close()


def test_e6_compaction_leftover_readonly_reports_and_keeps(tmp_path):
    """E6: a readonly handle keeps the file; health() reports it as a
    warning that is not removable, cleanup_compaction() is refused; a writer
    then removes it with cleanup_compaction() semantics on open."""
    p = str(tmp_path / "a.nhydb")
    _small_db(p).close()
    left = p + ".compacting"
    with open(left, "wb") as f:
        f.write(b"x" * 4321)
    ro = nhydb.NHyDB(p, readonly=True)
    assert os.path.getsize(left) == 4321
    checks = _leftover_checks(ro)
    assert len(checks) == 1
    c = checks[0]
    assert c["status"] == "warning"
    assert c["details"]["removable"] is False
    assert c["details"]["bytes"] == 4321
    assert c["details"]["path"].endswith("a.nhydb.compacting")
    with pytest.raises(nhydb.NHyDBError, match="read-only"):
        ro.cleanup_compaction()
    assert os.path.getsize(left) == 4321
    ro.close()
    db = nhydb.NHyDB(p)
    assert not os.path.exists(left)
    db.close()


@pytest.mark.skipif(sys.platform == "win32", reason="symlinks need privileges on Windows")
def test_e6_compaction_leftover_symlink_not_removed(tmp_path):
    """E6: a symlink named ``<db>.compacting`` is never removed (only a
    regular file of exactly that name); its target stays intact and
    cleanup_compaction() refuses it."""
    p = str(tmp_path / "a.nhydb")
    _small_db(p).close()
    target = tmp_path / "precious.bin"
    target.write_bytes(b"keep me")
    left = p + ".compacting"
    os.symlink(str(target), left)
    db = nhydb.NHyDB(p)
    assert os.path.islink(left)
    assert target.read_bytes() == b"keep me"
    assert [c["status"] for c in _leftover_checks(db)] == ["warning"]
    with pytest.raises(nhydb.NHyDBError, match="not a regular file"):
        db.cleanup_compaction()
    assert os.path.islink(left) and target.read_bytes() == b"keep me"
    db.close()
    os.unlink(left)


# ---------------------------------------------------------------- R9C-H1


def test_r9ch1_trigram_candidates_short_pattern(tmp_path):
    """R9C-H1: index_candidates on a TRIGRAM index with a pattern shorter
    than its n-gram length raises ValueError (it cannot restrict it); sql()
    with the same CONTAINS returns the correct rows (it scans)."""
    db = nhydb.NHyDB.create(str(tmp_path / "x.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s1", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 4, "name": "s", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_trigram_index(8, 1, "gs", "s")
    db.put_dense_batch(1, 1, [[1, "alpha"], [2, "beta"], [3, "ω pi"], [4, "palace"]], shape_id=1)
    db.build_indexes(commit=True)
    for pat, ids in (("al", [1, 4]), ("ω", [3]), ("a", [1, 2, 4])):
        with pytest.raises(ValueError, match="shorter than its n-gram length"):
            db.index_candidates(8, "contains", pat)
        got = [r["id"] for r in db.sql("SELECT id FROM c WHERE CONTAINS(s, ?) ORDER BY id", [pat])["rows"]]
        assert got == ids, pat
    assert len(list(db.index_candidates(8, "contains", "alp"))) >= 1
    assert [r["id"] for r in db.sql("SELECT id FROM c WHERE CONTAINS(s, 'alp')")["rows"]] == [1]
    db.close()
