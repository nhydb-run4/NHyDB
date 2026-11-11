"""AP9 S1: a stale index answers through base \\ mask ∪ overlay.

Property test: random sequences of writes (dense records of two shapes,
contiguous batches over existing records, sparse cells on BASE and
extension fields, pure sparse records, `delete`, `delete_record`), commits,
rollbacks and reopens. At every check the index answers of
* the writer (overlay incl. uncommitted writes, after a commit),
* a fresh read-only handle (its own overlay) and
* a copy of the file with a full `build_indexes`
must be equal for VALUE, TOKEN, TRIGRAM and FULLTEXT indexes (terms,
phrases, `mode="any"`, corpus totals and document lengths; in a pure
CLASSIC realm also the ranked SQL search with BM25 scores).
"""
import random
import shutil
from pathlib import Path

import pytest

import nhydb

N_U = 400
CATS = list(range(6))
WORDS = ["alpha", "beta", "gamma", "delta", "omega", "pi"]
# maintenance off: this test checks the overlay alone
MANUAL = "[index]\nmaintenance = \"manual\"\n"


NOVEL = [f"zz{k}" for k in range(8)]   # words the first build has not seen


def text(rng, novel=True):
    words = [rng.choice(WORDS) for _ in range(rng.randint(0, 4))]
    if novel and rng.random() < 0.2:
        words.insert(rng.randint(0, len(words)), rng.choice(NOVEL))
    return " ".join(words)


def setup(path: Path, column=True):
    COLUMN[0] = column
    path.parent.mkdir(parents=True, exist_ok=True)
    (path.parent / "nhydb_config.toml").write_text(MANUAL)
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    base = [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "cat", "type": "varuint"},
            {"v": 3, "name": "body", "type": "utf8"}, {"v": 4, "name": "score", "type": "varint"}]
    db.create_shape(1, 1, "s1", base)
    # shape 2 adds v=5; v=5 is no BASE field of the realm (default shape 1)
    db.create_shape(2, 1, "s2", base + [{"v": 5, "name": "extra", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    if column:
        # F010: u 380/381 belong to a COLUMN layout and are never indexed
        db.create_column_layout(2, 1, "col", fields=[{"u": 380, "name": "x", "type": "varuint"},
                                                    {"u": 381, "name": "y", "type": "varuint"}])
    db.define_extension(1, 10, "varuint", name="tag")
    rng = random.Random(1)
    db.put_dense_batch(1, 1, [[u, u % 6, text(rng, novel=False), u - 200] for u in range(1, 301)], shape_id=1)
    db.create_index(1, 1, "i_cat", "cat")
    db.create_index(2, 1, "i_score", "score")
    db.create_index(3, 1, "i_tag", 10)
    db.create_index(4, 1, "i_extra", 5)
    db.create_token_index(5, 1, "t_body", 3)
    db.create_trigram_index(6, 1, "g_body", 3)
    db.create_fulltext_index(7, 1, "f_body", 3)
    db.build_indexes(commit=True)
    return db


def probes():
    p = [(1, "eq", c, None) for c in CATS] + [(1, "between", 2, 4, )]
    p += [(2, "between", lo, lo + 40) for lo in (-250, -100, 0, 60)] + [(2, "eq", -150, None)]
    p += [(3, "eq", t, None) for t in range(4)] + [(3, "between", 1, 2)]
    p += [(4, "eq", e, None) for e in range(4)]
    p += [(5, "token", w, None) for w in WORDS + NOVEL[:3]] + [(5, "tokens_any", ["pi", "beta"], None), (5, "tokens_all", ["alpha", "beta"], None)]
    p += [(6, "contains", s, None) for s in ("alp", "ega", "mma", "elt", "zz1", "zz5")]
    return [(x[0], x[1], x[2], x[3] if len(x) > 3 else None) for x in p]


def answers(db):
    out = {}
    for iid, op, a, b in probes():
        out[(iid, op, repr(a), repr(b))] = list(db.index_candidates(iid, op, a, b))
    out["find_u"] = list(db.find_u(1, {"i_cat": 2, "i_score": (-300, 300)}))
    out["count"] = db.posting_count(1, 3)
    for q in ("alpha", "beta gamma", "pi omega delta", "zz2", "zz4 alpha"):
        out[("ft", q)] = list(db.index_candidates(7, "fulltext", q, None))
        out[("ft any", q)] = list(db.index_candidates(7, "fulltext", q, None, predicate_options={"mode": "any"}))
    out[("phrase", "alpha beta")] = list(db.index_candidates(7, "phrase", "alpha beta", None))
    c = db.fulltext_corpus(7)
    out["corpus"] = (c.total_docs, c.total_tokens, sorted(c.lengths.items()))
    out["term"] = [(p.u, tuple(p.positions)) for p in db.fulltext_term_posting(7, "gamma")]
    if not COLUMN[0]:
        r = db.sql("SELECT id, relevance FROM c WHERE FULLTEXT_ANY(body, 'alpha beta zz3') ORDER BY relevance DESC LIMIT 40")
        out["ranked"] = [(x["id"], round(x["relevance"], 12)) for x in r["rows"]]
    return out


COLUMN = [True]


def reference(path: Path, tmp: Path, k: int):
    """Copy of the committed file (same name: bound in R0,u1) with a full rebuild."""
    d = tmp / f"ref{k}"
    d.mkdir()
    for f in path.parent.iterdir():
        if f.name.startswith(path.name) and not f.name.endswith(".lock"):
            shutil.copy2(f, d / f.name)
    shutil.copy2(path.parent / "nhydb_config.toml", d / "nhydb_config.toml")
    ref = nhydb.NHyDB(str(d / path.name))
    ref.build_indexes(commit=True)
    res = answers(ref)
    ref.close()
    return res


def random_step(db, rng):
    u = rng.randint(1, N_U)
    r = rng.random()
    if r < 0.22:
        db.put(1, u, [u, rng.choice(CATS), text(rng), rng.randint(-300, 300)])
    elif r < 0.30:
        db.put(1, u, [u, rng.choice(CATS), text(rng), rng.randint(-300, 300), rng.randint(0, 3)], shape_id=2)
    elif r < 0.38:
        n = rng.randint(2, 30)
        u0 = rng.randint(1, N_U - n)
        db.put_dense_batch(1, u0, [[x, rng.choice(CATS), text(rng), rng.randint(-300, 300)] for x in range(u0, u0 + n)],
                           shape_id=1)
    elif r < 0.50:
        db.put_cell(1, u, 2, rng.choice(CATS))           # sparse state of a BASE field
    elif r < 0.56:
        db.put_cell(1, u, 3, text(rng), type="utf8")      # sparse text (TOKEN/TRIGRAM)
    elif r < 0.66:
        db.put_cell(1, u, 10, rng.randint(0, 3))          # extension cell (also without record)
    elif r < 0.72:
        try:
            db.put_extensions(1, u, {"tag": rng.randint(0, 3)})
        except KeyError:
            pass
    elif r < 0.78:
        # T-01: partial delete – the BASE fields of shape 1 only; v=5 (shape
        # 2), the extension v=10 and sparse cells outside v 1..4 survive
        # (tombstones without a DELETE block when something survives)
        db.delete_area(1, u, u, 1, 4)
    elif r < 0.82:
        # T-01: a partial delete of a few u in one field (tombstones only)
        n = rng.randint(1, 8)
        db.delete_area(1, u, min(u + n, N_U), 2, 2)
    elif r < 0.88:
        db.delete_record(1, u)                             # record and all its cells
    else:
        db.put_cell(1, u, 4, rng.randint(-300, 300), type="varint")


@pytest.mark.parametrize("seed,column", [(11, True), (23, True), (37, True), (41, False), (53, False)])
def test_overlay_equals_rebuild(tmp_path, seed, column):
    path = tmp_path / "w" / "ov.nhydb"
    db = setup(path, column)
    rng = random.Random(seed)
    checks = 0
    for step in range(600):
        random_step(db, rng)
        r = rng.random()
        if r < 0.05:
            db.rollback()
            continue
        if r < 0.13 or step == 599:
            db.commit()
            if rng.random() < 0.25:                        # reopen: overlay recomputed from the blocks
                db.close()
                db = nhydb.NHyDB(str(path))
            mine = answers(db)
            ro = nhydb.NHyDB(str(path), readonly=True)
            other = answers(ro)
            ro.close()
            want = reference(path, tmp_path, checks)
            assert mine == want, f"seed {seed} step {step}: writer differs from rebuild"
            assert other == want, f"seed {seed} step {step}: reader differs from rebuild"
            checks += 1
    assert checks >= 30
    db.close()


def test_writer_sees_uncommitted_writes(tmp_path):
    db = setup(tmp_path / "w" / "ov.nhydb")
    before = list(db.index_candidates(1, "eq", 5, None))
    db.put(1, 350, [350, 5, "omega", 1])                  # not committed
    assert list(db.index_candidates(1, "eq", 5, None)) == before + [350]
    assert 350 in list(db.index_candidates(5, "token", "omega", None))
    db.rollback()                                          # block ids are handed out again
    assert list(db.index_candidates(1, "eq", 5, None)) == before
    db.put(1, 351, [351, 0, "pi", 2])                      # reuses the block id of the rollback
    assert 350 not in list(db.index_candidates(1, "eq", 5, None))
    assert 351 in list(db.index_candidates(1, "eq", 0, None))
    db.close()


def test_queryengine_uses_overlay_and_stays_correct(tmp_path):
    # without a COLUMN layout (in a mixed realm the QueryEngine always scans)
    db = setup(tmp_path / "w" / "ov.nhydb", column=False)
    db.put(1, 7, [7, 4, "delta pi", 99], commit=True)
    db.put_cell(1, 8, 2, 4, commit=True)
    res = db.sql("SELECT id FROM c WHERE cat = 4")
    ids = sorted(r["id"] for r in res["rows"])
    want = sorted(u for u in range(1, 301) if (u % 6 == 4 and u not in (7, 8))) + [7, 8]
    assert ids == sorted(want)
    from nhydb.query import Q
    access = db.explain_query(Q.field("cat") == 4, realm=1)["plan"]["access"]
    assert "INDEX" in access, access                        # the stale index is used, not a scan
    db.close()
