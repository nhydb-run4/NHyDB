"""Indexfamilien und Textsemantik (Gegenstück zu ``nhydb_index_v027``).

VALUE, TOKEN, TRIGRAM und positionsbasiertes FULLTEXT: Normalisierung,
Tokenizer (``\\w+`` exakt wie CPython-``re``), N-Gramme, Schlüssel,
Prädikatverifikation, FULLTEXT-Payload-Codecs, Phrase, BM25. Nativ (Rust).
"""
from ._native import (  # noqa: F401
    IndexConfigError,
    FullTextPosting,
    FullTextCorpus,
    PostingRef,
    normalize_index_kind,
    normalize_index_options,
    normalize_text,
    simple_tokens,
    ngrams,
    fulltext_tokens,
    fulltext_document,
    query_fulltext_sequence,
    query_fulltext_terms,
    index_keys,
    query_token_keys,
    query_trigram_keys,
    index_supports_predicate,
    predicate_matches,
    encode_fulltext_posting,
    decode_fulltext_posting,
    encode_fulltext_doc_lengths,
    decode_fulltext_doc_lengths,
    posting_doc_ids,
    fulltext_candidate_docs,
    phrase_candidate_docs,
    bm25_scores,
)

INDEX_KIND_VALUE = "VALUE"
INDEX_KIND_TOKEN = "TOKEN"
INDEX_KIND_TRIGRAM = "TRIGRAM"
INDEX_KIND_FULLTEXT = "FULLTEXT"
INDEX_KINDS = frozenset({INDEX_KIND_VALUE, INDEX_KIND_TOKEN, INDEX_KIND_TRIGRAM, INDEX_KIND_FULLTEXT})
TOKENIZER_SIMPLE = "simple"
PAYLOAD_FULLTEXT_POSITIONS = 1
PAYLOAD_FULLTEXT_DOCLENS = 2
FULLTEXT_DOCLEN_KEY = "\x00NHYDB_FULLTEXT_DOCUMENT_LENGTHS"

__all__ = [n for n in dir() if not n.startswith("_")]
