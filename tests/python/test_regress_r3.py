"""Regression pool of round 3 (0.28.2): paged FULLTEXT document lengths and
the cache budgets (per handle and per process).

* Ranked FULLTEXT search reads only the document length segments of its
  hits; results and relevance equal an independent BM25 computation over
  the whole corpus (many segments, hits far apart, phrase, mode any).
* Indexes of 0.28.0 (one document length list) stay readable.
* All caches are byte budgeted from `[cache]` in `nhydb_config.toml`; the
  process pot (`total_bytes`) bounds the sum of all handles.

Process-wide behaviour runs in subprocesses (one pot per process).
"""
import json
import math
import os
import random
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

import numpy as np
import pytest

import nhydb
from nhydb.query import Q, QueryEngine

HERE = Path(__file__).resolve().parent
K1, B = 1.2, 0.75


def _docs(n, seed=3):
    """n documents of 1..30 words from a 3000 word vocabulary; rare words
    at a few positions, some empty documents."""
    r = random.Random(seed)
    vocab = [f"w{k}" for k in range(3000)]
    docs = []
    for u in range(1, n + 1):
        k = 0 if u % 997 == 0 else r.randint(1, 30)
        docs.append(" ".join(r.choice(vocab) for _ in range(k)))
    for u, w in [(10, "rarea"), (n - 7, "rareb"), (n // 2, "rarea"), (17, "rarec rared"), (18, "rared rarec")]:
        docs[u - 1] += " " + w
    return docs


def _db(p, docs, cfg=None):
    if cfg is not None:
        (p.parent / "nhydb_config.toml").write_text(cfg)
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_fulltext_index(5, 1, "ft", 2)
    db.put_numpy_batch(1, 1, {"id": np.arange(1, len(docs) + 1, dtype=np.uint64), "body": np.array(docs, dtype=object)}, shape_id=1)
    db.build_indexes(commit=True)
    return db


def _bm25(docs, terms, mode="all", phrase=False, base=1):
    """Independent BM25 (order of the engine: terms in query order)."""
    toks = {u: d.split() for u, d in enumerate(docs, base)}
    n = len(toks)
    avg = sum(len(t) for t in toks.values()) / n
    avgdl = avg if avg else 1.0
    uniq = list(dict.fromkeys(terms))
    post = {t: [u for u in toks if t in toks[u]] for t in uniq}
    if phrase:
        cand = {u for u, ts in toks.items() if any(ts[i:i + len(terms)] == terms for i in range(len(ts)))}
    elif mode == "any":
        cand = {u for t in uniq for u in post[t]}
    else:
        cand = set(toks) if uniq else set()
        for t in uniq:
            cand &= set(post[t])
    scores = {}
    for t in uniq:
        df = len(post[t])
        if not df:
            continue
        idf = math.log(1.0 + (n - df + 0.5) / (df + 0.5))
        for u in post[t]:
            if u not in cand:
                continue
            tf = toks[u].count(t)
            dl = len(toks[u])
            norm = tf + K1 * (1.0 - B + B * (dl / avgdl))
            scores[u] = scores.get(u, 0.0) + idf * (tf * (K1 + 1.0)) / norm
    return sorted(scores.items(), key=lambda x: (-x[1], x[0]))


def _search(db, q, limit=None, **opt):
    expr = Q.field("body").phrase(q) if opt.pop("phrase", False) else Q.field("body").fulltext(q, **opt)
    r = QueryEngine(db).search(expr, realm=1, select=["id"], limit=limit)
    return [(row["id"], row["relevance"]) for row in r["rows"]]


@pytest.fixture(scope="module")
def big(tmp_path_factory):
    """300 000 documents: about 150 document length segments."""
    p = tmp_path_factory.mktemp("r3big") / "ft.nhydb"
    docs = _docs(300_000)
    _db(p, docs).close()
    return p, docs


def test_r3_paged_doc_lengths_match_the_corpus(big):
    p, docs = big
    with nhydb.NHyDB(p, readonly=True) as db:
        c = db.fulltext_corpus(5)
        want = {u: len(d.split()) for u, d in enumerate(docs, 1)}
        assert c.total_docs == len(docs) and c.total_tokens == sum(want.values())
        assert dict(c.lengths) == want
        st = db.stats()
        # the summary counts as the document length key, segments do not
        assert st["indexes"][0]["lanes"]["NL"]["distinct_values"] == len({w for d in docs for w in d.split()})
        assert db.verify(deep=True)["ok"]


@pytest.mark.parametrize("q,opt", [
    ("rarea", {}),                         # two hits far apart (segments skipped, new descent)
    ("rarea rareb", {"mode": "any"}),      # hits at both ends of the corpus
    ("rarec rared", {}),                   # adjacent documents
    ("rarec rared", {"phrase": True}),
    ("w5 w7", {"mode": "any"}),            # many hits over all segments
    ("w11", {}),
    ("nothere", {}),
])
def test_r3_ranked_search_equals_independent_bm25(big, q, opt):
    p, docs = big
    want = _bm25(docs, q.split(), mode=opt.get("mode", "all"), phrase=opt.get("phrase", False))
    with nhydb.NHyDB(p, readonly=True) as db:
        got = _search(db, q, **dict(opt))
    assert [u for u, _ in got] == [u for u, _ in want]
    for (_, a), (_, b) in zip(got, want):
        assert a == pytest.approx(b, rel=1e-12, abs=0)


def test_r3_rare_search_keeps_only_a_few_segments(big):
    """A search with two hits caches its term posting and two segments,
    not the corpus (0.28.1: about 49 bytes per document)."""
    p, docs = big
    with nhydb.NHyDB(p, readonly=True) as db:
        assert len(_search(db, "rarea")) == 2
        pool = db.stats()["cache"]["pools"]["postings"]
        assert pool["entries"] <= 4 and pool["bytes"] < 300_000, pool


def test_r3_posting_budget_bounds_the_pool_and_keeps_results(tmp_path, big):
    p, docs = big
    cfg = tmp_path / "small.toml"
    cfg.write_text("[cache]\nposting_cache_bytes = 65536\nindex_page_cache_bytes = 65536\n")
    queries = ["w5 w7", "w11", "rarea", "w1 w2 w3", "w11"]
    with nhydb.NHyDB(p, readonly=True) as big_db:
        want = [_search(big_db, q, mode="any") for q in queries]
    with nhydb.NHyDB(p, readonly=True, config=str(cfg)) as db:
        got = [_search(db, q, mode="any") for q in queries]
        pools = db.stats()["cache"]["pools"]
    assert got == want
    assert pools["postings"]["budget_bytes"] == 65536 and pools["postings"]["bytes"] <= 65536
    assert pools["index_pages"]["bytes"] <= 65536
    assert pools["postings"]["evictions"] > 0


def test_r3_doc_lengths_of_golden_db_rank_like_bm25(tmp_path):
    """The golden database (current format) holds the document lengths of
    its FULLTEXT index (`FTD1` encoding): ranked search and corpus as
    computed from its data (RUN7: formerly on the 0.16 golden database)."""
    src = HERE / "golden" / "golden.nhydb"
    p = tmp_path / "golden.nhydb"
    for s in ("", ".layout.enc", ".layout.key"):
        shutil.copyfile(str(src) + s, str(p) + s)
    os.chmod(str(p) + ".layout.key", 0o600)
    with nhydb.NHyDB(p, readonly=True) as db:
        rows = db.sql("SELECT id, body FROM records ORDER BY id")["rows"]
        assert rows
        ft = [i for i in db.stats()["indexes"] if i["kind"] == "FULLTEXT"][0]
        c = db.fulltext_corpus(ft["index_id"])
        bodies = {}
        for u, v in c.lengths.items():
            bodies[u] = db.get(1, u)["body"]
        assert c.total_docs == len(bodies)
        for q in ("gamma", "alpha beta", "row"):
            terms = [t for t in "".join(ch if ch.isalnum() else " " for ch in q.lower()).split()]
            docs = [bodies[u] for u in sorted(bodies)]
            norm = ["".join(ch if ch.isalnum() else " " for ch in (d or "").lower()) for d in docs]
            want = _bm25(norm, terms)
            # map positions back to u
            us = sorted(bodies)
            want = [(us[i - 1], s) for i, s in want]
            r = QueryEngine(db).search(Q.field("body").fulltext(q), realm=1, limit=None)
            got_n = [(row["record_N"], row["relevance"]) for row in r["rows"]]
            want_n = [(db.record_anchor(1, u)["N_ref"], s) for u, s in want]
            assert [n for n, _ in got_n] == [n for n, _ in want_n], q
            for (_, a), (_, b) in zip(got_n, want_n):
                assert a == pytest.approx(b, rel=1e-12, abs=0)


def test_r3_cache_config_keys_and_stats(tmp_path):
    cfg = tmp_path / "c.toml"
    cfg.write_text("[cache]\ntotal_bytes = 4294967296\nposting_cache_bytes = 1048576\nsparse_cache_bytes = 2097152\n"
                   "block_cache_bytes = 3145728\nindex_page_cache_bytes = 524288\n")
    db = nhydb.NHyDB.create(tmp_path / "x.nhydb", config=str(cfg))
    st = db.stats()["cache"]
    pools = st["pools"]
    assert (pools["postings"]["budget_bytes"], pools["sparse"]["budget_bytes"], pools["block"]["budget_bytes"],
            pools["index_pages"]["budget_bytes"]) == (1048576, 2097152, 3145728, 524288)
    assert set(st["process"]) >= {"limit_bytes", "used_bytes", "peak_bytes", "handles", "reclaims", "reclaimed_bytes", "warnings"}
    assert st["handle_bytes"] >= 0
    db.close()
    for bad in ("[cache]\ntotal_bytes = -1\n", "[cache]\nposting_cache_bytes = \"x\"\n", "[cache]\nnot_a_key = 1\n"):
        cfg.write_text(bad)
        with pytest.raises(ValueError):
            nhydb.NHyDB.create(tmp_path / "y.nhydb", config=str(cfg), overwrite=True)


POT_SCRIPT = textwrap.dedent(r'''
    import json, sys, numpy as np
    from pathlib import Path
    import nhydb
    from nhydb.query import Q, QueryEngine
    d = Path(sys.argv[1]); mode = sys.argv[2]
    def cfg(name, text):
        p = d / name; p.write_text(text); return str(p)
    out = {}
    if mode == "bound":
        small = cfg("small.toml", "[cache]\ntotal_bytes = 2000000\n")
        hs = [nhydb.NHyDB(d / f"db{i}" / "ft.nhydb", readonly=True, config=small) for i in range(3)]
        peak_ok = True
        for rnd in range(3):
            for h in hs:
                for q in ("w5 w7", "w11", "w1 w2", "rarea"):
                    QueryEngine(h).search(Q.field("body").fulltext(q, mode="any"), realm=1, limit=5)
                st = h.stats()["cache"]["process"]
                # bound: limit plus at most one entry of a pool in flight
                peak_ok &= st["used_bytes"] <= st["limit_bytes"] + 1_100_000
        st = hs[0].stats()["cache"]["process"]
        out.update(st, peak_ok=peak_ok, sum_handles=sum(h.stats()["cache"]["handle_bytes"] for h in hs))
        out["results"] = [[(r["record_N"], r["relevance"]) for r in QueryEngine(h).search(Q.field("body").fulltext("w11"), realm=1, limit=20)["rows"]] for h in hs]
        for h in hs: h.close()
        out["after_close"] = nhydb.NHyDB(d / "db0" / "ft.nhydb", readonly=True).stats()["cache"]["process"]
    elif mode == "warn":
        a = nhydb.NHyDB(d / "db0" / "ft.nhydb", readonly=True, config=cfg("a.toml", "[cache]\ntotal_bytes = 3000000000\n"))
        b = nhydb.NHyDB(d / "db1" / "ft.nhydb", readonly=True, config=cfg("b.toml", "[cache]\ntotal_bytes = 5000000\n"))
        out["a"] = a.stats()["cache"]["process"]; out["b"] = b.stats()["cache"]["process"]
        out["verify_b"] = [w for w in b.verify()["warnings"] if "total_bytes" in str(w)]
        a.close(); b.close()
        c = nhydb.NHyDB(d / "db1" / "ft.nhydb", readonly=True, config=str(d / "b.toml"))
        out["c"] = c.stats()["cache"]["process"]; c.close()
    print(json.dumps(out))
''')


@pytest.fixture(scope="module")
def three(tmp_path_factory):
    root = tmp_path_factory.mktemp("r3pot")
    docs = _docs(40_000, seed=5)
    for i in range(3):
        (root / f"db{i}").mkdir()
        _db(root / f"db{i}" / "ft.nhydb", docs).close()
    return root


def _run_pot(root, mode):
    r = subprocess.run([sys.executable, "-c", POT_SCRIPT, str(root), mode], capture_output=True, text=True, timeout=600,
                       env={k: v for k, v in os.environ.items() if k != "NHYDB_CONFIG"})
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout.strip().splitlines()[-1])


def test_r3_process_pot_bounds_all_handles(three):
    out = _run_pot(three, "bound")
    assert out["limit_bytes"] == 2_000_000 and out["handles"] == 3
    assert out["peak_ok"], out
    assert out["reclaims"] > 0 and out["reclaimed_bytes"] > 0
    assert out["sum_handles"] <= out["limit_bytes"] + 1_100_000
    assert out["results"][0] == out["results"][1] == out["results"][2] and out["results"][0]
    # all handles closed: the next handle defines the pot again (default)
    assert out["after_close"]["limit_bytes"] == 4 << 30 and out["after_close"]["handles"] == 1


def test_r3_first_handle_defines_the_pot_others_are_warned(three):
    out = _run_pot(three, "warn")
    assert out["a"]["limit_bytes"] == 3_000_000_000 and out["b"]["limit_bytes"] == 3_000_000_000
    assert not out["a"]["warnings"] and len(out["b"]["warnings"]) == 1
    assert "5000000" in out["b"]["warnings"][0] and "first open handle" in out["b"]["warnings"][0]
    assert out["verify_b"]
    assert out["c"]["limit_bytes"] == 5_000_000 and not out["c"]["warnings"]
