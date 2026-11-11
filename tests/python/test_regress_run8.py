"""Regression pool of RUN8 (0.28.2): AP1 and AP2 of the resource concept.

Every test names its finding (N1-N3, A12, U10 ... from the resource
investigation of 2026-10-29 and the concept of 2026-10-30).
"""
import pytest

import nhydb

BIG = "x" * 4000


@pytest.fixture(autouse=True)
def _own_config(monkeypatch):
    """A NHYDB_CONFIG of the environment (dir3 QA runs) must not change the
    block layout these tests count on."""
    monkeypatch.delenv("NHYDB_CONFIG", raising=False)


def _db(p):
    """Database with realm 1 (CLASSIC, two fields, one extension) and a
    sparse realm 2, committed and larger than the 64 KiB minimum of
    ``db_max_size_bytes``."""
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=5)
    db.define_extension(1, 9, "utf8", name="e")
    db.define_extension(1, 10, "utf8", name="f")
    db.create_realm(2, "s")
    db.put_points(2, [(1, 1, "p" * 70_000, "utf8")], commit=True)
    return db


def _limit_after(db, p, extra):
    """Caps the file at exactly ``extra`` bytes beyond its committed length.
    Setting the limit appends a DB_CONFIG block itself, whose size depends on
    the value, so the limit is adjusted until the remaining room is exact."""
    db.commit()
    cfg = 256  # first guess, at least one DB_CONFIG block
    for _ in range(10):
        before = p.stat().st_size
        db.set_max_db_size(before + cfg + extra, commit=True)
        size = p.stat().st_size
        if before + cfg + extra - size == extra:
            return size
        cfg = size - before
    raise AssertionError("size limit did not settle")


# ----------------------------------------------------------------------
# N2: a multi-block write that fails leaves nothing for the next commit
# ----------------------------------------------------------------------

def test_n2_put_points_later_window_fails(tmp_path):
    p = tmp_path / "db.nhydb"
    db = _db(p)
    db.put_points(2, [(500, 1, "early", "utf8")])  # uncommitted, must survive
    _limit_after(db, p, 7000)
    db.put_points(2, [(500, 1, "early", "utf8")])  # a small block that still fits
    cells = [(u, 1, BIG, "utf8") for u in (10, 20, 30, 40)]
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.put_points(2, cells, u_window=10)  # one block per u: the first fits, a later one not
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert [r.get_value(2, u, 1) for u in (10, 20, 30, 40)] == [None] * 4
        assert r.get_value(2, 500, 1) == "early"
        assert r.verify(deep=True)["ok"]


def test_n2_put_dense_batch_extension_block_fails(tmp_path):
    p = tmp_path / "db.nhydb"
    db = _db(p)
    _limit_after(db, p, 3000)
    rows = [[i, "t"] for i in range(10)]
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.put_dense_batch(1, 1, rows, extensions_by_u={u: {9: BIG} for u in range(1, 11)})
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert r.get(1, 1) is None  # before RUN8 the DATA block was committed without its extensions
        assert r.verify(deep=True)["ok"]


def test_n2_put_extensions_replace_is_one_unit(tmp_path):
    p = tmp_path / "db.nhydb"
    db = _db(p)
    db.put(1, 1, {"a": 1, "t": "t"}, extensions={9: "keep"}, commit=True)
    _limit_after(db, p, 1500)
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        # merge=False first writes a tombstone for v9 (fits), then the new cell (too large)
        db.put_extensions(1, 1, {10: BIG * 2}, merge=False)
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert r.get(1, 1)["e"] == "keep"
        assert r.verify(deep=True)["ok"]


def test_n2_failed_index_tree_leaves_no_pages(tmp_path):
    p = tmp_path / "db.nhydb"
    db = _db(p)
    db.put_dense_batch(1, 1, [[i, f"text {i} " * 20] for i in range(3000)], commit=True)
    db.create_index(1, 1, "it", 2, kind="FULLTEXT")
    db.commit()
    # room for some pages of the tree, not for all of it
    before = _limit_after(db, p, 8_000)
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.build_indexes()
    db.commit()
    db.close()
    # before RUN8 the pages written up to the error were taken over by the commit
    assert p.stat().st_size == before
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert r.verify(deep=True)["ok"]


def test_n2_write_batch_still_atomic(tmp_path):
    p = tmp_path / "db.nhydb"
    db = _db(p)
    _limit_after(db, p, 6000)
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        with db.batch() as b:
            for u in (10, 20, 30):
                b.put_cell(2, u, 1, BIG, type="utf8")
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert [r.get_value(2, u, 1) for u in (10, 20, 30)] == [None] * 3


# ----------------------------------------------------------------------
# N1: ORDER BY ... LIMIT in sql() keeps only the window (general path)
# ----------------------------------------------------------------------

def _general_db(p, n, values, cfg=None, typ="varint"):
    """Realm 1 with fields a (any type) and b; one sparse cell forces the
    general (non-CLASSIC) query path."""
    if cfg:
        (p.parent / "nhydb_config.toml").write_text(cfg)
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": typ}, {"v": 2, "name": "b", "type": "varint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[values(u), u % 7] for u in range(1, n + 1)], shape_id=1)
    db.put_cell(1, 3, 2, 99, commit=True)
    return db


def test_n1_sql_order_limit_within_small_result_limit(tmp_path):
    p = tmp_path / "a" / "b.nhydb"
    p.parent.mkdir()
    n = 50_000
    db = _general_db(p, n, lambda u: (u * 7919) % n, cfg="[query]\nmax_result_bytes = 2000000\n")
    q = "SELECT a FROM c ORDER BY a LIMIT 5"
    rows = db.sql(q)["rows"]  # before RUN8: QueryLimitError after 7317 rows
    assert [r["a"] for r in rows] == sorted((u * 7919) % n for u in range(1, n + 1))[:5]
    assert rows == list(db.sql_iter(q))
    with pytest.raises(nhydb.QueryLimitError):
        db.sql("SELECT a FROM c")  # the collected result itself is still bounded
    db.close()


_KINDS = {
    "int": ("varint", lambda u: (u * 37) % 50 - 20),
    "float": ("f64", lambda u: [1.0, 2.5, -3.0, 1e30, 0.0, -0.0, 7.25, 2.5][u % 8]),
    "str": ("utf8", lambda u: f"k{(u * 13) % 40:02d}"),
    "bool": ("bool", lambda u: bool(u % 3)),
    "nan": ("f64", lambda u: float("nan") if u % 17 == 0 else float(u % 9)),
}


@pytest.mark.parametrize("kind", sorted(_KINDS))
def test_n1_topk_equals_full_sort(tmp_path, kind):
    from nhydb.query import Q, QueryEngine
    p = tmp_path / "t.nhydb"
    typ, values = _KINDS[kind]
    db = _general_db(p, 3000, values, typ=typ)
    q = QueryEngine(db)
    for ob in (["a"], ["-a"], ["a", "-b"], ["-b", "a"]):
        for mode in ({"return_mode": "fields", "select": ["a", "b", "record_N"]}, {"return_mode": "record"}):
            for limit, offset in ((5, 0), (7, 3), (0, 0), (1, 2990)):
                def run(**kw):
                    try:
                        return q.execute(Q.all(), realm=1, order_by=ob, **mode, **kw)
                    except Exception as e:  # the same error with and without LIMIT
                        return type(e).__name__, str(e)
                win = run(limit=limit, offset=offset)
                full = run()
                if isinstance(full, tuple):
                    assert win == full, (kind, ob, mode, limit, offset)
                    continue
                assert "fastpath" not in win
                assert repr(win["rows"]) == repr(full["rows"][offset:offset + limit]), (kind, ob, mode, limit, offset)
    db.close()


def test_n2_put_objects_batch_runs_are_one_unit(tmp_path):
    p = tmp_path / "db.nhydb"
    db = _db(p)
    _limit_after(db, p, 9000)  # room for the first run (two rows), not for all three
    objs = [(u, {"a": u, "t": BIG}) for u in (1, 2, 10, 11, 20, 21)]  # three contiguous runs
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.put_objects_batch(1, objs)
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert [r.get(1, u) for u in (1, 2, 10, 11, 20, 21)] == [None] * 6
        assert r.verify(deep=True)["ok"]


def test_n2_put_objects_numpy_runs_are_one_unit(tmp_path):
    np = pytest.importorskip("numpy")
    p = tmp_path / "db.nhydb"
    db = _db(p)
    _limit_after(db, p, 9000)
    ids = np.array([1, 2, 10, 11, 20, 21], dtype=np.uint64)
    cols = {"a": ids.astype(np.int64), "t": np.array([BIG] * 6, dtype=object)}
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.put_objects_numpy(1, ids, cols)
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        assert [r.get(1, int(u)) for u in ids] == [None] * 6


def test_n2_delete_record_is_one_unit(tmp_path):
    p = tmp_path / "db.nhydb"
    db = _db(p)
    db.put(1, 1, {"a": 1, "t": "t"}, extensions={9: "keep"}, commit=True)
    _limit_after(db, p, 64)  # room for the DELETE block (40 bytes), not for the tombstone block
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.delete_record(1, 1)
    db.commit()
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as r:
        rec = r.get(1, 1)
        assert rec is not None and rec["a"] == 1 and rec["e"] == "keep"


# ----------------------------------------------------------------------
# U4/U10: find_u, ResultFile.fetchall/to_numpy obey max_result_bytes;
# execute_numpy moves the values instead of copying all rows
# ----------------------------------------------------------------------

def _indexed_db(p, n, cfg=None):
    if cfg:
        (p.parent / "nhydb_config.toml").write_text(cfg)
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u % 3, f"t{u}"] for u in range(1, n + 1)], shape_id=1)
    db.create_index(1, 1, "ia", 1)
    db.build_indexes(commit=True)
    return db


def test_u10_find_u_obeys_max_result_bytes(tmp_path):
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    db = _indexed_db(p, 30_000, cfg="[query]\nmax_result_bytes = 100000\n")
    assert len(db.find_u(1, {"ia": 1}, u_from=1, u_to=3000)) == 1000  # small result: fine
    with pytest.raises(nhydb.QueryLimitError, match="find_u"):
        db.find_u(1, {"ia": 1})  # 10 000 u x 16 bytes
    with pytest.raises(nhydb.QueryLimitError, match="find_u"):
        db.find_u(1, {})  # every u of the realm
    with pytest.raises(nhydb.QueryLimitError):
        next(iter(db.find(1, {"ia": 1})))
    db.close()


def test_u4_result_file_fetchall_and_to_numpy_obey_limit(tmp_path):
    pytest.importorskip("numpy")
    p = tmp_path / "a" / "db.nhydb"
    p.parent.mkdir()
    db = _indexed_db(p, 30_000, cfg=f'[query]\nmax_result_bytes = 200000\n[results]\ntemp_dir = "{tmp_path.as_posix()}/res"\n')
    rf = db.sql_to_file("SELECT a, t FROM c")
    assert sum(1 for _ in rf) == 30_000  # iterating stays unbounded
    with pytest.raises(nhydb.QueryLimitError, match="fetchall"):
        rf.fetchall()
    with pytest.raises(nhydb.QueryLimitError):
        rf.to_numpy()
    small = db.sql_to_file("SELECT a, t FROM c WHERE a = 2 LIMIT 50")
    assert len(small.fetchall()) == 50 and len(small.to_numpy()["t"]) == 50
    job = db.submit_sql("SELECT a, t FROM c")
    with pytest.raises(nhydb.QueryLimitError):
        job.wait().fetchall()
    rf.close(); small.close(); job.wait().close()
    db.close()


def test_u10_execute_numpy_equals_sql_numpy(tmp_path):
    np = pytest.importorskip("numpy")
    from nhydb.query import Q, QueryEngine
    p = tmp_path / "db.nhydb"
    db = _indexed_db(p, 2000)
    q = QueryEngine(db)
    a = q.execute_numpy(Q.field("a") == 1, realm=1, select=["a", "t", "a"], order_by=["-t"], limit=40)
    b = db.sql_numpy("SELECT a, t FROM c WHERE a = 1 ORDER BY t DESC LIMIT 40")
    assert list(a) == ["record_N", "a", "t"]
    assert np.array_equal(a["a"], b["a"]) and list(a["t"]) == list(b["t"])
    s = q.execute_sql_numpy("SELECT a, t FROM c WHERE a = :x ORDER BY t LIMIT 5", {"x": 2})
    assert list(s["t"]) == list(db.sql_numpy("SELECT t FROM c WHERE a = 2 ORDER BY t LIMIT 5")["t"])
    db.close()


# ----------------------------------------------------------------------
# N3: python -m nhydb.query reads without taking the write lock
# ----------------------------------------------------------------------

def test_n3_query_cli_is_readonly(tmp_path):
    import json
    import subprocess
    import sys
    p = tmp_path / "db.nhydb"
    db = _indexed_db(p, 100)
    db.put_cell(1, 500, 1, 5, type="varint")  # uncommitted, must stay untouched
    before = p.stat().st_size
    r = subprocess.run([sys.executable, "-m", "nhydb.query", str(p), "--sql", "SELECT COUNT(*) AS n FROM c", "--json"],
                       capture_output=True, text=True, timeout=120)
    # before RUN8: LockedError, because the CLI opened the database for writing
    assert r.returncode == 0, r.stderr
    assert json.loads(r.stdout)["rows"] == [{"n": 100}]
    assert p.stat().st_size == before
    db.commit()
    assert db.get_value(1, 500, 1) == 5
    db.close()
