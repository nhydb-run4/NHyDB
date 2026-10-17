"""Differential tests of the index semantics against ``nhydb_index_v027`` (external reference implementation, not included)."""
import random
import sys

import pytest

import nhydb
import nhydb.index as rx


@pytest.fixture(scope="module")
def px(ref):
    return sys.modules["nhydb_index_v027"]


ALPHABET = list("abcdefgh ABC äöüß ÄÖÜ ẞ ǅ ΣσςΑ İıſ fi ﬁ ① ² ٣ _-.,;!?\t\n€😀 ") + ["́", "　", "​"]


def _texts(seed, n=300):
    r = random.Random(seed)
    out = ["", " ", "Red FOX red", "Straße STRASSE", "quick brown fox quick", "x_1 x-1", None, 17, 3.5, True, b"ab"]
    for _ in range(n):
        out.append("".join(r.choice(ALPHABET) for _ in range(r.randint(0, 30))))
    return out


@pytest.mark.parametrize("seed", [1, 2, 3])
def test_text_functions_identical(px, seed):
    for t in _texts(seed):
        for cf in (True, False):
            assert rx.simple_tokens(t, casefold=cf) == px.simple_tokens(t, casefold=cf), t
            assert rx.normalize_text(t, casefold=cf) == px.normalize_text(t, casefold=cf), t
            for n in (2, 3, 4):
                assert rx.ngrams(t, n=n, casefold=cf) == px.ngrams(t, n=n, casefold=cf), t
        for kind, opts in [("VALUE", None), ("TOKEN", {"casefold": False}), ("TRIGRAM", {"ngram": 2, "casefold": True}), ("FULLTEXT", None)]:
            assert rx.index_keys(kind, t, opts) == px.index_keys(kind, t, opts), (kind, t)
        assert rx.fulltext_document(t) == px.fulltext_document(t), t
        assert rx.query_fulltext_terms(t) == px.query_fulltext_terms(t), t


def test_options_and_errors(px):
    cases = [("token", {"tokenizer": " SIMPLE "}), ("trigram", {"ngram": "5"}), ("trigram", {"ngram": 3.9, "casefold": 1}),
             ("fulltext", {"casefold": 0}), (None, None), ("value", {"x": 1})]
    for k, o in cases:
        assert rx.normalize_index_options(k, o) == px.normalize_index_options(k, o)
    for k, o in [("bogus", None), ("trigram", {"ngram": 17}), ("token", {"tokenizer": "icu"})]:
        with pytest.raises(px.IndexConfigError) as e1:
            px.normalize_index_options(k, o)
        with pytest.raises(rx.IndexConfigError) as e2:
            rx.normalize_index_options(k, o)
        assert str(e1.value) == str(e2.value)
    assert issubclass(rx.IndexConfigError, ValueError)


@pytest.mark.parametrize("seed", [4, 5])
def test_predicates_identical(px, seed):
    r = random.Random(seed)
    texts = [t for t in _texts(seed, 120) if isinstance(t, str)]
    ops = [("contains", {}), ("icontains", {}), ("token", {"casefold": True}), ("tokens_all", {}), ("tokens_any", {}),
           ("fulltext", {"mode": "all"}), ("fulltext", {"mode": "any"}), ("phrase", {})]
    for _ in range(600):
        a = r.choice(texts + [None]); q = r.choice(texts)
        op, opt = r.choice(ops)
        val = (q, r.choice(texts)) if op in {"tokens_all", "tokens_any"} else q
        assert rx.predicate_matches(a, op, val, None, opt) == px.predicate_matches(a, op, val, None, opt), (a, op, val)
        for kind, iopt in [("TOKEN", None), ("TRIGRAM", None), ("TRIGRAM", {"casefold": True}), ("FULLTEXT", None)]:
            assert (rx.index_supports_predicate(kind, op, iopt, opt, q)
                    == px.index_supports_predicate(kind, op, iopt, opt, q)), (kind, op, q)


def test_fulltext_codecs_phrase_bm25(px):
    r = random.Random(9)
    docs = {u: " ".join(r.choice(["quick", "fox", "brown", "the", "a", "b"]) for _ in range(r.randint(1, 12))) for u in range(1, 60)}
    postings_px, lengths = {}, []
    for u, text in docs.items():
        dl, pos = px.fulltext_document(text)
        lengths.append((u, dl))
        for term, p in pos.items():
            postings_px.setdefault(term, []).append(px.FullTextPosting(u, p))
    raw_px = {t: px.encode_fulltext_posting(v) for t, v in postings_px.items()}
    raw_rx = {t: rx.encode_fulltext_posting([(p.u, p.positions) for p in v]) for t, v in postings_px.items()}
    assert raw_px == raw_rx
    dl_px = px.encode_fulltext_doc_lengths(lengths)
    assert dl_px == rx.encode_fulltext_doc_lengths(lengths)
    corpus_px = px.decode_fulltext_doc_lengths(dl_px, len(lengths))
    corpus_rx = rx.decode_fulltext_doc_lengths(dl_px, len(lengths))
    assert dict(corpus_px.lengths) == dict(corpus_rx.lengths) and corpus_px.avg_doc_len == corpus_rx.avg_doc_len
    post_rx = {t: rx.decode_fulltext_posting(raw_rx[t]) for t in raw_rx}
    for q in ["quick fox", "the quick", "fox fox", "a b a", "zz"]:
        seq = px.query_fulltext_sequence(q)
        sub_px = {t: postings_px.get(t, ()) for t in dict.fromkeys(seq)}
        sub_rx = {t: post_rx.get(t, ()) for t in dict.fromkeys(seq)}
        assert px.phrase_candidate_docs(seq, sub_px) == rx.phrase_candidate_docs(seq, sub_rx)
        for mode in ("all", "any"):
            assert px.fulltext_candidate_docs(sub_px, mode=mode) == rx.fulltext_candidate_docs(sub_rx, mode=mode)
        s_px = px.bm25_scores(sub_px, corpus_px)
        s_rx = rx.bm25_scores(sub_rx, corpus_rx)
        assert list(s_px.items()) == list(s_rx.items())  # bit-identical floats, same order


def test_db_index_build_and_candidates_match_reference(tmp_path, ref):
    out = {}
    for name, mod in (("rust", nhydb), ("py", ref)):
        db = mod.NHyDB.create(tmp_path / f"{name}.nhydb")
        db.create_realm(1, "r", base_reserve_to=8)
        db.create_shape(1, 1, "row", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "title", "type": "utf8"},
                                      {"v": 3, "name": "body", "type": "utf8"}])
        db.create_classic_layout(1, 1, "records", shape_id=1, base_reserve_to=8)
        db.define_extension(1, 100, "utf8", name="tag")
        db.create_index(1, 1, "i_id", "id")
        db.create_token_index(2, 1, "i_title", "title")
        db.create_trigram_index(3, 1, "i_tri", "body")
        db.create_trigram_index(4, 1, "i_tri_ci", "body", casefold=True)
        db.create_fulltext_index(5, 1, "i_ft", "body")
        db.create_token_index(6, 1, "i_tag", "tag")
        rows = [[i, f"Red Fox {i % 3}", f"the quick brown fox {i} Straße" if i % 2 else "slow ABC abcd", ] for i in range(1, 40)]
        db.put_dense_batch(1, 1, rows, shape_id=1, extensions_by_u={2: {"tag": "Alpha Beta"}, 5: {"tag": "beta"}})
        db.put_points(1, [(70, 100, "nur sparse tag", "utf8")])  # u without a dense record
        built = db.build_indexes(commit=True)
        res = {"built": [(x["name"], x["lane"], x["distinct_values"], x["u_references"], x.get("documents")) for x in built["indexes"]]}
        for iid, op, val, opts in [(1, "eq", 7, None), (1, "between", (3, 9), None), (2, "token", "FOX", None),
                                   (2, "tokens_any", ("red", "0"), None), (3, "contains", "abcd", None),
                                   (4, "icontains", "STRASSE", None), (5, "fulltext", "quick fox", {"mode": "all"}),
                                   (5, "phrase", "brown fox", None), (5, "fulltext", "slow fox", {"mode": "any"}),
                                   (6, "token", "beta", None), (6, "token", "sparse", None)]:
            v1, v2 = (val if isinstance(val, tuple) and op == "between" else (val, None))
            res[(iid, op, str(val))] = (db.index_candidates(iid, op, v1, v2, predicate_options=opts),
                                        db.index_candidate_estimate(iid, op, v1, v2, predicate_options=opts))
        c = db.fulltext_corpus(5)
        res["corpus"] = (c.total_docs, c.total_tokens, dict(c.lengths))
        res["quick"] = [(p.u, tuple(p.positions)) for p in db.fulltext_term_posting(5, "quick")]
        res["dir"] = sorted((repr(k), r.width, r.raw_count) for k, r in db._ensure_secondary_directory(5, 0).items())
        res["stats"] = [(i["name"], i["kind"], i["v"], i["options"], i["lanes"]["NL"]["distinct_values"]) for i in db.stats()["indexes"]]
        out[name] = res
        db.close()
    assert out["rust"] == out["py"]


def test_index_on_sparse_only_realm_becomes_fresh(tmp_path, ref):
    """The reference implementation writes no SECONDARY block for lanes
    without dense segments, so the index stays stale forever. The Rust
    engine builds the index and it becomes fresh."""
    res = {}
    for name, mod in (("rust", nhydb), ("py", ref)):
        db = mod.NHyDB.create(tmp_path / f"sparse_only_{name}.nhydb")
        db.create_realm(1, "r")
        db.define_extension(1, 5, "utf8", name="note")
        db.create_index(1, 1, "by_note", "note")
        db.put_points(1, [(1, 5, "a"), (2, 5, "b"), (3, 5, "a")])
        db.build_indexes(commit=True)
        try:
            res[name] = db.index_candidates(1, "eq", "a")
        except mod.StaleIndexError:
            res[name] = "stale"
        db.close()
    assert res["py"] == "stale"
    assert res["rust"] == [1, 3]
