"""Secondary indexes: VALUE, TOKEN, TRIGRAM and FULLTEXT, freshness."""
import pytest

import nhydb


def _build(db):
    db.create_index(1, 1, "idx_status", "status")
    db.create_index(5, 1, "idx_amount", "amount")
    db.create_token_index(2, 1, "tok_body", 3)
    db.create_trigram_index(3, 1, "tri_body", 3)
    db.create_fulltext_index(4, 1, "ft_body", 3)
    return db.build_indexes(commit=True)


def test_build_reports_all_indexes(sample_db):
    rep = _build(sample_db)
    kinds = {i["name"]: i["kind"] for i in rep["indexes"]}
    assert kinds == {"idx_status": "VALUE", "idx_amount": "VALUE", "tok_body": "TOKEN",
                     "tri_body": "TRIGRAM", "ft_body": "FULLTEXT"}
    assert rep["records_scanned"] == 100


def test_value_index_equality_and_range(sample_db):
    _build(sample_db)
    active = [u for u in range(1, 101) if u % 3 == 0]
    assert list(sample_db.index_candidates(1, "eq", "active", None)) == active
    assert list(sample_db.find_u(1, {"idx_status": "active"})) == active
    assert list(sample_db.find_u(1, {"idx_amount": (100, 130)})) == [10, 11, 12, 13]


def test_text_indexes(sample_db):
    _build(sample_db)
    with_fox = [u for u in range(1, 101) if u % 4 in (0, 2)]
    lazy = [u for u in range(1, 101) if u % 4 == 1]
    assert list(sample_db.index_candidates(2, "token", "fox", None)) == with_fox
    assert list(sample_db.index_candidates(3, "contains", "azy", None)) == lazy
    assert list(sample_db.index_candidates(4, "fulltext", "quick fox", None)) == with_fox


def test_stale_index_is_detected_and_rebuilt(sample_db):
    _build(sample_db)
    sample_db.put_dense_batch(1, 101, [[101, "active", "new", 5]], shape_id=1)
    sample_db.commit()
    # AP9: a stale index answers through its overlay
    assert list(sample_db.index_candidates(1, "eq", "active", None))[-1] == 101
    assert list(sample_db.index_candidates(2, "token", "new", None)) == [101]
    assert list(sample_db.index_candidates(3, "contains", "new", None)) == [101]
    assert list(sample_db.index_candidates(4, "fulltext", "new", None)) == [101]
    v = sample_db.verify()  # F054: a stale index is a warning, not a structural error
    assert v["ok"] and any("stale" in w for w in v["warnings"])
    sample_db.build_indexes(commit=True)
    assert list(sample_db.index_candidates(1, "eq", "active", None))[-1] == 101
    assert sample_db.verify()["ok"]
