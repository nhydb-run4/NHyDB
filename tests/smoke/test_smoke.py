"""Smoke tests for an installed nhydb package (used by GitHub Actions).

A few short checks of the basic functions: import, create, write, read,
SQL, index, persistence. The full test suites run before every release.
"""
from importlib.metadata import version
from pathlib import Path

import pytest

import nhydb

ROWS = [[1, "active", "quick brown fox", 10],
        [2, "idle", "lazy dog", 20],
        [3, "active", "quick fox jumps", 30],
        [4, "idle", "nothing here", 40]]


def _create(path):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "main")
    db.create_shape(1, 1, "row", [
        {"v": 1, "name": "id", "type": "varuint"},
        {"v": 2, "name": "status", "type": "utf8"},
        {"v": 3, "name": "body", "type": "utf8"},
        {"v": 4, "name": "amount", "type": "varuint"},
    ])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, ROWS, shape_id=1)
    db.commit()
    return db


@pytest.fixture
def db(tmp_path):
    d = _create(tmp_path / "smoke.nhydb")
    yield d
    d.close()


def test_import_and_version():
    assert version("nhydb").count(".") == 2
    assert nhydb._native is not None


def test_create_writes_the_database_files(tmp_path):
    p = tmp_path / "files.nhydb"
    _create(p).close()
    for suffix in ("", ".layout.enc", ".layout.key"):
        assert Path(str(p) + suffix).is_file()


def test_batch_write_and_read(db):
    r = db.get(1, 3)
    assert r["status"] == "active" and r["body"] == "quick fox jumps" and r["amount"] == 30
    assert db.get(1, 99) is None


def test_single_record_and_cell(db):
    db.put_record(1, 5, {"id": 5, "status": "idle", "body": "five", "amount": 50}, commit=True)
    assert db.get_record(1, 5)["body"] == "five"
    db.put_cell(1, 5, 744, "note", type="utf8", commit=True)
    assert db.get_value(1, 5, 744) == "note"


def test_delete_record(db):
    assert db.delete_record(1, 2, commit=True) > 0
    assert db.get(1, 2) is None


def test_sql_filter_and_order(db):
    r = db.sql("SELECT id FROM records WHERE status = 'active' ORDER BY id")
    assert [x["id"] for x in r["rows"]] == [1, 3]


def test_sql_parameters_and_group_by(db):
    r = db.sql("SELECT id FROM records WHERE amount BETWEEN ? AND ? ORDER BY id", [15, 35])
    assert [x["id"] for x in r["rows"]] == [2, 3]
    r = db.sql("SELECT status, COUNT(*) AS n FROM records GROUP BY status ORDER BY status")
    assert r["rows"] == [{"status": "active", "n": 2}, {"status": "idle", "n": 2}]


def test_sql_syntax_error(db):
    with pytest.raises(nhydb.SQLSyntaxError):
        db.sql("SELEC id FROM records")


def test_value_index(db):
    db.create_index(1, 1, "idx_status", "status")
    db.build_indexes(commit=True)
    assert list(db.find_u(1, {"idx_status": "active"})) == [1, 3]


def test_index_follows_new_commits(db):
    db.create_index(1, 1, "idx_status", "status")
    db.build_indexes(commit=True)
    db.put_dense_batch(1, 6, [[6, "active", "new", 60]], shape_id=1)
    db.commit()
    assert list(db.find_u(1, {"idx_status": "active"})) == [1, 3, 6]


def test_fulltext_index(db):
    db.create_fulltext_index(4, 1, "ft_body", 3)
    db.build_indexes(commit=True)
    r = db.sql("SELECT id FROM records WHERE FULLTEXT(body, :q) ORDER BY id", {"q": "fox"})
    assert [x["id"] for x in r["rows"]] == [1, 3]


def test_persistence_after_reopen(tmp_path):
    p = tmp_path / "persist.nhydb"
    _create(p).close()
    with nhydb.open(str(p)) as db:
        assert db.get(1, 4)["body"] == "nothing here"
        assert db.verify(deep=True)["ok"]


def test_uncommitted_write_is_discarded(tmp_path):
    p = tmp_path / "crash.nhydb"
    db = _create(p)
    db.put_dense_batch(1, 7, [[7, "idle", "lost", 70]], shape_id=1)
    db.close(commit=False)  # like a crash before the commit
    with nhydb.open(str(p)) as db:
        assert db.get(1, 7) is None
        assert db.get(1, 1) is not None


def test_readonly_rejects_writes(tmp_path):
    p = tmp_path / "ro.nhydb"
    _create(p).close()
    with nhydb.NHyDB(str(p), readonly=True) as db:
        assert db.get(1, 1)["id"] == 1
        with pytest.raises(nhydb.NHyDBError):
            db.put_record(1, 8, {"id": 8}, commit=True)


def test_compaction_keeps_data(tmp_path):
    p = tmp_path / "compact.nhydb"
    db = _create(p)
    db.put_dense_batch(1, 1, [[1, "idle", "overwritten", 11]], shape_id=1)
    db.close()
    assert nhydb.NHyDB.compact_file(str(p), make_backup=False)
    with nhydb.open(str(p)) as db:
        assert db.get(1, 1)["body"] == "overwritten"
        assert db.verify(deep=True)["ok"]


def test_numpy_output(db):
    np = pytest.importorskip("numpy")
    cols = db.sql_numpy("SELECT id, amount FROM records WHERE id <= 2")
    assert np.array_equal(cols["id"], [1, 2]) and np.array_equal(cols["amount"], [10, 20])
