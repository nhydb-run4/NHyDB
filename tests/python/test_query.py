"""QueryEngine: index-backed counts, aggregation, ordering, BM25 search and
structural predicates."""
from nhydb.query import Q, QueryEngine


def _indexed(db):
    db.create_index(1, 1, "idx_status", "status")
    db.create_fulltext_index(4, 1, "ft_body", 3)
    db.build_indexes(commit=True)
    return QueryEngine(db)


def test_count_uses_fresh_value_index(sample_db):
    r = _indexed(sample_db).execute(Q.field("status") == "active", realm=1, return_mode="count", explain=True)
    assert r["count"] == 33
    assert r["plan"]["access"] == "VALUE_INDEX" and r["plan"]["index_name"] == "idx_status"


def test_aggregate_and_group_by(sample_db):
    q = _indexed(sample_db)
    r = q.execute(Q.field("status") == "active", realm=1, aggregate={"n": ("count", "*"), "s": ("sum", "amount")})
    assert r["rows"] == [{"n": 33, "s": 10 * sum(range(3, 101, 3))}]
    r = q.execute(Q.all(), realm=1, aggregate={"n": ("count", "*")}, group_by=["status"])
    assert sorted((x["status"], x["n"]) for x in r["rows"]) == [("active", 33), ("idle", 67)]


def test_fields_order_and_limit(sample_db):
    q = QueryEngine(sample_db)
    r = q.execute(Q.field("amount") >= 950, realm=1, return_mode="fields", select=["id", "amount"],
                  order_by=[("amount", "desc")], limit=3)
    assert r["count"] == 6
    assert [(x["id"], x["amount"]) for x in r["rows"]] == [(100, 1000), (99, 990), (98, 980)]


def test_bm25_search_ranking(sample_db):
    r = _indexed(sample_db).search(Q.field("body").fulltext("quick"), realm=1, select=["id"], limit=3)
    assert r["count"] == 50
    # "quick" occurs twice in the shorter documents u % 4 == 2; ties are ordered by u
    assert [x["id"] for x in r["rows"]] == [2, 6, 10]
    assert r["rows"][0]["relevance"] > 0


def test_struct_range_over_huge_u(tmp_path):
    """A structural range up to 10**30 only visits materialized u."""
    import nhydb
    from nhydb.query import Q, QueryEngine
    db = nhydb.NHyDB.create(str(tmp_path / "h.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 51)], shape_id=1)
    db.put_points(1, [(10**25, 1, 7, "varuint")])
    q = QueryEngine(db)
    r = q.execute(Q.u.between(1, 10**30), realm=1, return_mode="count", explain=True)
    assert r["count"] == 51 and r["plan"]["access"] == "STRUCT_RANGE"
    r = q.execute(Q.u.between(40, 10**30), realm=1, return_mode="fields", select=["id", "u"])
    assert [x["u"] for x in r["rows"]] == list(range(40, 51)) + [10**25]
    db.close()

def test_sparse_overrides_consistent_across_access_paths(tmp_path):
    """Sparse overrides of BASE fields and sparse-only records must be visible
    in select/select_us and in the index build, so that the index, CLASSIC_U and
    scan paths all return the logical (overridden) values. The CLASSIC_U path
    must also keep sparse-only records: count, rows and aggregates agree."""
    import nhydb
    from nhydb.query import Q, QueryEngine
    db = nhydb.NHyDB.create(str(tmp_path / "sparse.nhydb"))
    db.create_realm(1, "r", base_reserve_to=8)
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "status", "type": "utf8"},
                                {"v": 3, "name": "amount", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=8)
    db.put_dense_batch(1, 1, [[u, "a" if u % 2 else "b", u] for u in range(1, 21)], shape_id=1)
    db.put_points(1, [(4, 2, "a", "utf8"), (6, 3, 1000, "varuint"), (30, 1, 30, "varuint"), (30, 2, "b", "utf8")])
    db.delete_cell(1, 8, 3)
    db.create_index(1, 1, "idx_status", "status")
    db.create_index(2, 1, "idx_amount", "amount")
    db.build_indexes(commit=True)
    assert list(db.index_candidates(1, "eq", "b", None)) == [2, 6, 8, 10, 12, 14, 16, 18, 20, 30]
    assert 4 in db.index_candidates(1, "eq", "a", None)
    assert list(db.index_candidates(2, "eq", 1000, None)) == [6] and list(db.index_candidates(2, "eq", 8, None)) == []
    q = QueryEngine(db)
    scan = q.execute(Q.field("status").contains("b"), realm=1, return_mode="n")
    idx = q.execute(Q.field("status") == "b", realm=1, return_mode="n")
    assert scan["refs"] == idx["refs"]
    agg = {"n": ("count", "*"), "s": ("sum", "amount"), "c": ("count", "amount")}
    fast = q.execute(Q.field("status") == "b", realm=1, aggregate=agg)
    slow = q.execute((Q.field("status") == "b") & Q.all(), realm=1, aggregate=agg)
    assert fast.get("fastpath") == "CLASSIC_U" and "fastpath" not in slow
    assert fast["rows"] == slow["rows"] == [{"n": 10, "s": sum(range(2, 21, 2)) - 4 - 6 + 1000 - 8, "c": 8}]
    f1 = q.execute(Q.field("status") == "b", realm=1, return_mode="fields", select=["id", "amount"])
    f2 = q.execute((Q.field("status") == "b") & Q.all(), realm=1, return_mode="fields", select=["id", "amount"])
    assert f1["rows"] == f2["rows"] and {r["id"]: r["amount"] for r in f1["rows"]}[6] == 1000
    assert f1["count"] == len(f1["rows"]) == 10 and f1["rows"][-1]["id"] == 30
    db.close()
