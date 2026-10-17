"""NHySQL: SELECT with filters, parameters, grouping, ordering, full-text
relevance, NumPy output, error handling and the query CLI."""
import pytest

import nhydb


def test_select_group_by_order(sample_db):
    r = sample_db.sql("SELECT status, COUNT(*) AS n FROM records GROUP BY status ORDER BY n DESC")
    assert r["language"] == "NHySQL-0.24" and r["columns"] == ["status", "n"]
    assert r["rows"] == [{"status": "idle", "n": 67}, {"status": "active", "n": 33}]


def test_positional_and_named_parameters(sample_db):
    r = sample_db.sql("SELECT id FROM records WHERE amount BETWEEN ? AND ? ORDER BY id", [100, 130])
    assert [x["id"] for x in r["rows"]] == [10, 11, 12, 13]
    r = sample_db.sql("SELECT id FROM records WHERE status = :s AND id < 10", {"s": "active"})
    assert [x["id"] for x in r["rows"]] == [3, 6, 9]


def test_fulltext_relevance(sample_db):
    sample_db.create_fulltext_index(4, 1, "ft_body", 3)
    sample_db.build_indexes(commit=True)
    r = sample_db.sql("SELECT id, relevance FROM records WHERE FULLTEXT(body, :q) ORDER BY relevance DESC LIMIT 3",
                      {"q": "quick"})
    assert [x["id"] for x in r["rows"]] == [2, 6, 10]


def test_sql_numpy(sample_db):
    np = pytest.importorskip("numpy")
    cols = sample_db.sql_numpy("SELECT id, amount FROM records WHERE id <= 3")
    assert np.array_equal(cols["id"], [1, 2, 3]) and np.array_equal(cols["amount"], [10, 20, 30])


@pytest.mark.parametrize("text", ["SELEC id FROM records", "INSERT INTO records VALUES (1)",
                                  "SELECT DISTINCT status FROM records"])
def test_invalid_or_unsupported_statements(sample_db, text):
    with pytest.raises(nhydb.SQLSyntaxError):
        sample_db.sql(text)


def test_query_cli_sql(tmp_path):
    """`python -m nhydb.query` with --sql and --params."""
    import json
    import subprocess
    import sys
    import nhydb
    p = tmp_path / "c.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "main")
    db.create_shape(1, 1, "row", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "cat", "type": "varuint"}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 2] for u in range(1, 11)], shape_id=1)
    db.close()
    r = subprocess.run([sys.executable, "-m", "nhydb.query", str(p), "--sql", "SELECT id FROM records WHERE cat = ? ORDER BY id DESC LIMIT 2",
                        "--params", "[1]", "--json"], capture_output=True, text=True, check=True)
    out = json.loads(r.stdout)
    assert [x["id"] for x in out["rows"]] == [9, 7] and out["language"] == "NHySQL-0.24"
    r = subprocess.run([sys.executable, "-m", "nhydb.query", str(p), "--realm", "1", "--eq", "cat=0", "--return", "count"],
                       capture_output=True, text=True, check=True)
    assert "Count  : 5" in r.stdout
