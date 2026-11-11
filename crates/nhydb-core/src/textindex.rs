//! Index families and text semantics.
//!
//! Stateless: normalization of family/options, text normalization,
//! tokenizer (`\w+` as in CPython `re`), n-grams, index/query keys,
//! predicate verification, FULLTEXT payload codecs, phrase and BM25.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::error::{Error, Result};
use crate::nat::{get_vu, put_vu64, Nat};
use crate::unicode;
use crate::val::Val;
use crate::vmap;

pub const KIND_VALUE: &str = "VALUE";
pub const KIND_TOKEN: &str = "TOKEN";
pub const KIND_TRIGRAM: &str = "TRIGRAM";
pub const KIND_FULLTEXT: &str = "FULLTEXT";
pub const TOKENIZER_SIMPLE: &str = "simple";

/// Payload codes in the SECONDARY directory (in addition to 0/4/8).
pub const PAYLOAD_FULLTEXT_POSITIONS: u8 = 1;
/// All document lengths in one posting (indexes of 0.28.0/0.28.1, still read).
pub const PAYLOAD_FULLTEXT_DOCLENS: u8 = 2;
/// 0.28.2: document count, token total and segment count under
/// `FULLTEXT_DOCLEN_KEY`; the lengths follow as segments.
pub const PAYLOAD_FULLTEXT_DOCLEN_SUMMARY: u8 = 3;
/// 0.28.2: lengths of a u range of documents (`doclen_segment_key`).
pub const PAYLOAD_FULLTEXT_DOCLEN_SEGMENT: u8 = 5;
pub const FULLTEXT_DOCLEN_KEY: &str = "\u{0}NHYDB_FULLTEXT_DOCUMENT_LENGTHS";
/// Separator between `FULLTEXT_DOCLEN_KEY` and the last u of a segment.
const DOCLEN_SEGMENT_SEP: char = '\u{1}';

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IndexKind {
    Value,
    Token,
    Trigram,
    Fulltext,
}

impl IndexKind {
    pub fn name(self) -> &'static str {
        match self {
            IndexKind::Value => KIND_VALUE,
            IndexKind::Token => KIND_TOKEN,
            IndexKind::Trigram => KIND_TRIGRAM,
            IndexKind::Fulltext => KIND_FULLTEXT,
        }
    }
}

fn cfg_err(msg: String) -> Error {
    Error::IndexConfig(msg)
}

/// `normalize_index_kind`
pub fn normalize_kind(kind: &Val) -> Result<IndexKind> {
    if kind.is_none() {
        return Ok(IndexKind::Value);
    }
    let s = unicode::upper(unicode::strip(&kind.py_str()));
    Ok(match s.as_str() {
        KIND_VALUE => IndexKind::Value,
        KIND_TOKEN => IndexKind::Token,
        KIND_TRIGRAM => IndexKind::Trigram,
        KIND_FULLTEXT => IndexKind::Fulltext,
        _ => {
            return Err(cfg_err(format!(
                "unknown index family {}; expected VALUE/TOKEN/TRIGRAM/FULLTEXT",
                kind.py_repr()
            )))
        }
    })
}

/// Python `int(x)` for option values (int, bool, truncated float, str).
fn py_int(v: &Val) -> Result<i64> {
    match v {
        Val::Bool(b) => Ok(*b as i64),
        Val::Int(i) => Ok(*i),
        Val::Float(f) if f.is_finite() => Ok(f.trunc() as i64),
        Val::Str(s) => unicode::strip(s)
            .replace('_', "")
            .parse::<i64>()
            .map_err(|_| Error::Value(format!("invalid literal for int() with base 10: {}", Val::str(s.as_str()).py_repr()))),
        _ => Err(Error::Type(format!("int() argument must be a string, a bytes-like object or a real number, not {}", v.py_repr()))),
    }
}

fn opt<'a>(raw: &'a Val, key: &str) -> Option<&'a Val> {
    raw.get(key)
}

fn tokenizer_of(raw: &Val, family: &str) -> Result<String> {
    let t = opt(raw, "tokenizer").map(|v| v.py_str()).unwrap_or_else(|| TOKENIZER_SIMPLE.to_string());
    let t = unicode::lower(unicode::strip(&t));
    if t != TOKENIZER_SIMPLE {
        return Err(cfg_err(format!("{family} tokenizer {} is not supported", Val::str(t.as_str()).py_repr())));
    }
    Ok(t)
}

fn bool_of(raw: &Val, key: &str, default: bool) -> bool {
    opt(raw, key).map(|v| v.is_truthy()).unwrap_or(default)
}

/// `normalize_index_options`
pub fn normalize_options(kind: IndexKind, raw: &Val) -> Result<Val> {
    let empty = Val::map();
    let raw = if raw.is_none() { &empty } else { raw };
    Ok(match kind {
        IndexKind::Value => Val::map(),
        IndexKind::Token => {
            let t = tokenizer_of(raw, KIND_TOKEN)?;
            vmap! {"tokenizer" => t.as_str(), "casefold" => bool_of(raw, "casefold", true)}
        }
        IndexKind::Trigram => {
            let n = match opt(raw, "ngram") {
                Some(v) => py_int(v)?,
                None => 3,
            };
            if !(2..=16).contains(&n) {
                return Err(cfg_err("TRIGRAM ngram must be between 2 and 16".into()));
            }
            vmap! {"ngram" => n, "casefold" => bool_of(raw, "casefold", false)}
        }
        IndexKind::Fulltext => {
            let t = tokenizer_of(raw, KIND_FULLTEXT)?;
            vmap! {"tokenizer" => t.as_str(), "casefold" => bool_of(raw, "casefold", true), "positions" => true}
        }
    })
}

// ----------------------------------------------------------------------
// Text, tokens, n-grams, keys
// ----------------------------------------------------------------------

/// `normalize_text`: `None` → "", otherwise `str(value)`, optionally casefolded.
pub fn normalize_text(value: &Val, casefold: bool) -> String {
    let t = if value.is_none() { String::new() } else { value.py_str() };
    if casefold { unicode::casefold(&t) } else { t }
}

/// `simple_tokens`: `re.finditer(r"\w+", text)`.
pub fn simple_tokens(value: &Val, casefold: bool) -> Vec<String> {
    let text = normalize_text(value, casefold);
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if unicode::is_word(c) {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unique_ordered(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.into_iter().filter(|v| seen.insert(v.clone())).collect()
}

/// `ngrams` over code points.
pub fn ngrams(value: &Val, n: usize, casefold: bool) -> Vec<String> {
    let text = normalize_text(value, casefold);
    ngram_slices(&text, n).into_iter().map(str::to_string).collect()
}

/// The n-grams of [`ngrams`] as slices of the normalized text (RUN9 V4:
/// the index build needs no string per position, only per new key).
pub fn ngram_slices(text: &str, n: usize) -> Vec<&str> {
    let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
    let chars = bounds.len() - 1;
    if chars < n {
        return Vec::new();
    }
    (0..=chars - n).map(|i| &text[bounds[i]..bounds[i + n]]).collect()
}

fn opt_bool(opts: &Val, key: &str) -> bool {
    opts.get(key).map(|v| v.is_truthy()).unwrap_or(false)
}

fn opt_ngram(opts: &Val) -> usize {
    opts.get("ngram").and_then(|v| v.as_i64()).unwrap_or(3).max(1) as usize
}

/// `fulltext_tokens`
pub fn fulltext_tokens(value: &Val, options: &Val) -> Result<Vec<String>> {
    let o = normalize_options(IndexKind::Fulltext, options)?;
    Ok(simple_tokens(value, opt_bool(&o, "casefold")))
}

/// Terms of a document with their positions (order of first occurrence).
pub type TermPositions = Vec<(String, Vec<u64>)>;

/// `fulltext_document`: (document length, term → positions) in
/// order of first occurrence.
pub fn fulltext_document(value: &Val, options: &Val) -> Result<(u64, TermPositions)> {
    let tokens = fulltext_tokens(value, options)?;
    let mut order: Vec<String> = Vec::new();
    let mut pos: HashMap<String, Vec<u64>> = HashMap::new();
    for (i, t) in tokens.iter().enumerate() {
        pos.entry(t.clone())
            .or_insert_with(|| {
                order.push(t.clone());
                Vec::new()
            })
            .push(i as u64);
    }
    Ok((tokens.len() as u64, order.into_iter().map(|t| { let p = pos.remove(&t).unwrap(); (t, p) }).collect()))
}

pub fn query_fulltext_sequence(value: &Val, options: &Val) -> Result<Vec<String>> {
    fulltext_tokens(value, options)
}

pub fn query_fulltext_terms(value: &Val, options: &Val) -> Result<Vec<String>> {
    Ok(unique_ordered(query_fulltext_sequence(value, options)?))
}

/// `index_keys` (VALUE: the value itself; text families: key strings).
pub fn index_keys(kind: IndexKind, value: &Val, options: &Val) -> Result<Vec<Val>> {
    let o = normalize_options(kind, options)?;
    if kind == IndexKind::Value {
        return Ok(vec![value.clone()]);
    }
    if value.is_none() {
        return Ok(Vec::new());
    }
    let keys = match kind {
        IndexKind::Token => unique_ordered(simple_tokens(value, opt_bool(&o, "casefold"))),
        IndexKind::Trigram => unique_ordered(ngrams(value, opt_ngram(&o), opt_bool(&o, "casefold"))),
        _ => query_fulltext_terms(value, &o)?,
    };
    Ok(keys.into_iter().map(Val::Str).collect())
}

pub fn query_token_keys(value: &Val, options: &Val) -> Result<Vec<String>> {
    let o = normalize_options(IndexKind::Token, options)?;
    Ok(unique_ordered(simple_tokens(value, opt_bool(&o, "casefold"))))
}

pub fn query_trigram_keys(value: &Val, options: &Val) -> Result<Vec<String>> {
    let o = normalize_options(IndexKind::Trigram, options)?;
    Ok(unique_ordered(ngrams(value, opt_ngram(&o), opt_bool(&o, "casefold"))))
}

/// `index_supports_predicate`
pub fn index_supports_predicate(kind: IndexKind, op: &str, options: &Val, predicate_options: &Val, value: &Val) -> Result<bool> {
    let op = unicode::lower(op);
    let idx_opts = normalize_options(kind, options)?;
    Ok(match kind {
        IndexKind::Value => op == "eq" || op == "between",
        IndexKind::Token => {
            if !matches!(op.as_str(), "token" | "tokens_all" | "tokens_any") {
                return Ok(false);
            }
            idx_opts == normalize_options(IndexKind::Token, predicate_options)?
        }
        IndexKind::Trigram => {
            if op != "contains" && op != "icontains" {
                return Ok(false);
            }
            let ngram = predicate_options.get("ngram").or_else(|| idx_opts.get("ngram")).cloned().unwrap_or(Val::Int(3));
            let expected = normalize_options(IndexKind::Trigram, &vmap! {"ngram" => ngram, "casefold" => op == "icontains"})?;
            if idx_opts != expected {
                return Ok(false);
            }
            !query_trigram_keys(value, &idx_opts)?.is_empty()
        }
        IndexKind::Fulltext => {
            if op != "fulltext" && op != "phrase" {
                return Ok(false);
            }
            if idx_opts != normalize_options(IndexKind::Fulltext, predicate_options)? {
                return Ok(false);
            }
            !query_fulltext_sequence(value, &idx_opts)?.is_empty()
        }
    })
}

fn as_values(value: &Val) -> Vec<Val> {
    match value {
        Val::List(l) => l.clone(),
        other => vec![other.clone()],
    }
}

/// `predicate_matches`: verification/scan fallback for text predicates.
pub fn predicate_matches(actual: &Val, op: &str, value: &Val, options: &Val) -> Result<bool> {
    let op = unicode::lower(op);
    match op.as_str() {
        "contains" | "icontains" => {
            if actual.is_none() {
                return Ok(false);
            }
            let cf = op == "icontains";
            Ok(normalize_text(actual, cf).contains(&normalize_text(value, cf)))
        }
        "token" | "tokens_all" | "tokens_any" => {
            let to = normalize_options(IndexKind::Token, options)?;
            let actual_set: HashSet<String> = simple_tokens(actual, opt_bool(&to, "casefold")).into_iter().collect();
            if op == "token" {
                let w = query_token_keys(value, &to)?;
                return Ok(w.len() == 1 && actual_set.contains(&w[0]));
            }
            let mut wanted: HashSet<String> = HashSet::new();
            for raw in as_values(value) {
                wanted.extend(query_token_keys(&raw, &to)?);
            }
            if wanted.is_empty() {
                return Ok(false);
            }
            Ok(if op == "tokens_all" { wanted.is_subset(&actual_set) } else { !wanted.is_disjoint(&actual_set) })
        }
        "fulltext" | "phrase" => {
            let fo = normalize_options(IndexKind::Fulltext, options)?;
            let at = fulltext_tokens(actual, &fo)?;
            let q = query_fulltext_sequence(value, &fo)?;
            if q.is_empty() {
                return Ok(false);
            }
            if op == "phrase" {
                let n = q.len();
                return Ok(at.len() >= n && (0..=at.len() - n).any(|i| at[i..i + n] == q[..]));
            }
            let mode = unicode::lower(&options.get("mode").map(|m| m.py_str()).unwrap_or_else(|| "all".into()));
            if mode != "all" && mode != "any" {
                return Err(cfg_err("FULLTEXT mode must be 'all' or 'any'".into()));
            }
            let a: HashSet<&String> = at.iter().collect();
            let w: HashSet<&String> = q.iter().collect();
            Ok(if mode == "all" { w.is_subset(&a) } else { !w.is_disjoint(&a) })
        }
        _ => Err(cfg_err(format!("predicate_matches does not know operator {}", Val::str(op.as_str()).py_repr()))),
    }
}

// ----------------------------------------------------------------------
// FULLTEXT payload codecs
// ----------------------------------------------------------------------

const FT_POST_MAGIC: &[u8; 4] = b"FTP1";
const FT_DOC_MAGIC: &[u8; 4] = b"FTD1";
const FT_SUM_MAGIC: &[u8; 4] = b"FTS1";

/// Entry of a positional posting list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FtPosting {
    pub u: Nat,
    pub positions: Vec<u64>,
}

/// Document lengths and corpus statistics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FtCorpus {
    pub lengths: BTreeMap<Nat, u64>,
    pub total_docs: u64,
    pub total_tokens: u64,
}

impl FtCorpus {
    pub fn avg_doc_len(&self) -> f64 {
        if self.total_docs == 0 { 0.0 } else { self.total_tokens as f64 / self.total_docs as f64 }
    }
}

pub fn encode_fulltext_posting(entries: &[(Nat, Vec<u64>)]) -> Result<Vec<u8>> {
    let mut e = FtPostingEncoder::new(entries.len() as u64);
    for (u, pos) in entries {
        e.push(u, pos)?;
    }
    Ok(e.finish())
}

/// [`encode_fulltext_posting`] entry by entry (RUN9 V4: the index build
/// merges the parts of a posting straight into it); `count` is the number
/// of entries that will follow. Same bytes, same errors.
pub struct FtPostingEncoder {
    out: Vec<u8>,
    prev: Nat,
}

impl FtPostingEncoder {
    pub fn new(count: u64) -> Self {
        let mut out = FT_POST_MAGIC.to_vec();
        put_vu64(&mut out, count);
        FtPostingEncoder { out, prev: Nat::ZERO }
    }

    pub fn push(&mut self, u: &Nat, pos: &[u64]) -> Result<()> {
        if u <= &self.prev {
            return Err(cfg_err("FULLTEXT posting documents must be strictly ascending".into()));
        }
        if pos.windows(2).any(|w| w[0] >= w[1]) {
            return Err(cfg_err("FULLTEXT position list must be strictly ascending and >=0".into()));
        }
        u.checked_sub(&self.prev).unwrap().put_vu(&mut self.out);
        put_vu64(&mut self.out, pos.len() as u64);
        let mut p0 = 0u64;
        for (i, p) in pos.iter().enumerate() {
            put_vu64(&mut self.out, if i == 0 { *p } else { p - p0 });
            p0 = *p;
        }
        self.prev = u.clone();
        Ok(())
    }

    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}

fn take_u64(buf: &[u8], p: &mut usize) -> Result<u64> {
    get_vu(buf, p)?.as_u64().ok_or_else(|| cfg_err("FULLTEXT value too large".into()))
}

pub fn decode_fulltext_posting(raw: &[u8], expected_count: Option<u64>) -> Result<Vec<FtPosting>> {
    if raw.len() < 4 || &raw[..4] != FT_POST_MAGIC {
        return Err(cfg_err("FULLTEXT posting magic is wrong".into()));
    }
    let mut p = 4;
    let count = take_u64(raw, &mut p)?;
    if expected_count.is_some_and(|e| e != count) {
        return Err(cfg_err("FULLTEXT posting count mismatch".into()));
    }
    let mut out = Vec::with_capacity(count.min(1 << 20) as usize);
    let mut u = Nat::ZERO;
    for _ in 0..count {
        u = u.add(&get_vu(raw, &mut p)?);
        let tf = take_u64(raw, &mut p)?;
        let mut positions = Vec::with_capacity(tf.min(1 << 16) as usize);
        let mut pos = 0u64;
        for i in 0..tf {
            let d = take_u64(raw, &mut p)?;
            pos = if i == 0 { d } else { pos.checked_add(d).ok_or_else(|| cfg_err("FULLTEXT position too large".into()))? };
            positions.push(pos);
        }
        out.push(FtPosting { u: u.clone(), positions });
    }
    if p != raw.len() {
        return Err(cfg_err("FULLTEXT posting has trailing bytes".into()));
    }
    Ok(out)
}

/// R4-A4: `decode_fulltext_posting` that keeps only the documents in
/// `wanted` (sorted ascending). The whole payload is parsed and checked as
/// by the full decoder (same errors); skipped documents allocate nothing.
pub fn decode_fulltext_posting_within(raw: &[u8], expected_count: Option<u64>, wanted: &[Nat]) -> Result<Vec<FtPosting>> {
    if raw.len() < 4 || &raw[..4] != FT_POST_MAGIC {
        return Err(cfg_err("FULLTEXT posting magic is wrong".into()));
    }
    let mut p = 4;
    let count = take_u64(raw, &mut p)?;
    if expected_count.is_some_and(|e| e != count) {
        return Err(cfg_err("FULLTEXT posting count mismatch".into()));
    }
    let mut out = Vec::with_capacity(wanted.len().min(count as usize));
    let mut u = Nat::ZERO;
    let mut j = 0usize;
    for _ in 0..count {
        u = u.add(&get_vu(raw, &mut p)?);
        let tf = take_u64(raw, &mut p)?;
        while j < wanted.len() && wanted[j] < u {
            j += 1;
        }
        let keep = j < wanted.len() && wanted[j] == u;
        let mut positions = if keep { Vec::with_capacity(tf.min(1 << 16) as usize) } else { Vec::new() };
        let mut pos = 0u64;
        for i in 0..tf {
            let d = take_u64(raw, &mut p)?;
            pos = if i == 0 { d } else { pos.checked_add(d).ok_or_else(|| cfg_err("FULLTEXT position too large".into()))? };
            if keep {
                positions.push(pos);
            }
        }
        if keep {
            out.push(FtPosting { u: u.clone(), positions });
        }
    }
    if p != raw.len() {
        return Err(cfg_err("FULLTEXT posting has trailing bytes".into()));
    }
    Ok(out)
}

pub fn encode_fulltext_doc_lengths(entries: &[(Nat, u64)]) -> Result<Vec<u8>> {
    let total: u64 = entries.iter().map(|e| e.1).sum();
    let mut out = FT_DOC_MAGIC.to_vec();
    put_vu64(&mut out, entries.len() as u64);
    put_vu64(&mut out, total);
    let mut prev = Nat::ZERO;
    for (u, dl) in entries {
        if u <= &prev {
            return Err(cfg_err("FULLTEXT document lengths must have strictly ascending u".into()));
        }
        u.checked_sub(&prev).unwrap().put_vu(&mut out);
        put_vu64(&mut out, *dl);
        prev = u.clone();
    }
    Ok(out)
}

pub fn decode_fulltext_doc_lengths(raw: &[u8], expected_count: Option<u64>) -> Result<FtCorpus> {
    if raw.len() < 4 || &raw[..4] != FT_DOC_MAGIC {
        return Err(cfg_err("FULLTEXT document lengths magic is wrong".into()));
    }
    let mut p = 4;
    let count = take_u64(raw, &mut p)?;
    let total = take_u64(raw, &mut p)?;
    if expected_count.is_some_and(|e| e != count) {
        return Err(cfg_err("FULLTEXT document count mismatch".into()));
    }
    let mut lengths = BTreeMap::new();
    let mut u = Nat::ZERO;
    let mut check: u128 = 0;
    for _ in 0..count {
        u = u.add(&get_vu(raw, &mut p)?);
        let dl = take_u64(raw, &mut p)?;
        check += dl as u128;
        lengths.insert(u.clone(), dl);
    }
    if p != raw.len() {
        return Err(cfg_err("FULLTEXT document lengths have trailing bytes".into()));
    }
    if check != total as u128 {
        return Err(cfg_err("FULLTEXT total_tokens mismatch".into()));
    }
    Ok(FtCorpus { lengths, total_docs: count, total_tokens: total })
}

// ----------------------------------------------------------------------
// Paged document lengths (0.28.2)
// ----------------------------------------------------------------------
//
// Up to 0.28.1 a FULLTEXT index kept all document lengths in one posting
// that the first ranked search decoded into memory (about 49 bytes per
// document, per index, lane and handle). Since 0.28.2 they are segments of
// a few KiB in the key tree of the index, next to the terms:
//
// * `FULLTEXT_DOCLEN_KEY` – summary (codec 3): documents, tokens, segments;
// * `FULLTEXT_DOCLEN_KEY + "\x01" + hex(len) + hex(last u)` – one segment
//   (codec 5) in the FTD1 encoding, keyed by its last document u (keys
//   sort like the u, big `Nat` included).
//
// A search reads only the segments of its hits (page cache, block cache,
// posting cache); nothing proportional to the corpus stays in memory.

/// Target raw size of a document length segment.
pub const DOCLEN_SEGMENT_BYTES: usize = 4 << 10;

/// Decoded document length segment: sorted u and their lengths.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocLenSeg {
    pub us: Vec<Nat>,
    pub lens: Vec<u64>,
    pub tokens: u64,
}

impl DocLenSeg {
    pub fn get(&self, u: &Nat) -> Option<u64> {
        self.us.binary_search(u).ok().map(|i| self.lens[i])
    }

    pub fn last(&self) -> Option<&Nat> {
        self.us.last()
    }

    /// Estimated heap bytes.
    pub fn cost(&self) -> usize {
        96 + self.us.capacity() * std::mem::size_of::<Nat>()
            + self.us.iter().map(|u| match u {
                Nat::S(_) => 0,
                Nat::B(b) => b.bits() as usize / 8 + 32,
            }).sum::<usize>()
            + self.lens.capacity() * 8
    }
}

/// Key of the segment whose last document is `last_u`.
pub fn doclen_segment_key(last_u: &Nat) -> String {
    // RB3-05: u = 0 has no bytes (`to_bytes_be` gives [0], which the reader
    // rejects as not canonical)
    let bytes = if last_u.is_zero() { Vec::new() } else { last_u.to_big().to_bytes_be() };
    let mut k = String::with_capacity(FULLTEXT_DOCLEN_KEY.len() + 9 + bytes.len() * 2);
    k.push_str(FULLTEXT_DOCLEN_KEY);
    k.push(DOCLEN_SEGMENT_SEP);
    k.push_str(&format!("{:08x}", bytes.len()));
    for b in &bytes {
        k.push_str(&format!("{b:02x}"));
    }
    k
}

/// Last u of a segment key (`None`: not a segment key).
pub fn doclen_segment_last(key: &str) -> Option<Nat> {
    let rest = key.strip_prefix(FULLTEXT_DOCLEN_KEY)?.strip_prefix(DOCLEN_SEGMENT_SEP)?;
    let (len, hex) = (rest.get(..8)?, rest.get(8..)?);
    let n = usize::from_str_radix(len, 16).ok()?;
    if hex.len() != n * 2 || !hex.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        return None;
    }
    let bytes: Vec<u8> = (0..n).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)).collect::<std::result::Result<_, _>>().ok()?;
    if bytes.first() == Some(&0) {
        return None;
    }
    Some(Nat::from(num_bigint::BigUint::from_bytes_be(&bytes)))
}

/// Splits sorted document lengths into segments of about
/// `DOCLEN_SEGMENT_BYTES` raw bytes.
pub fn doclen_segments(entries: &[(Nat, u64)], target: usize) -> Vec<&[(Nat, u64)]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut cut = DoclenSegmenter::new(target);
    for (i, (u, dl)) in entries.iter().enumerate() {
        if cut.push(u, *dl) {
            out.push(&entries[start..=i]);
            start = i + 1;
        }
    }
    if start < entries.len() {
        out.push(&entries[start..]);
    }
    out
}

/// The cut rule of [`doclen_segments`] entry by entry (RUN9 V4): `push`
/// returns true when a segment ends with this entry.
pub struct DoclenSegmenter {
    target: usize,
    bytes: usize,
    prev: Nat,
}

impl DoclenSegmenter {
    pub fn new(target: usize) -> Self {
        DoclenSegmenter { target, bytes: 0, prev: Nat::ZERO }
    }

    pub fn push(&mut self, u: &Nat, dl: u64) -> bool {
        let d = u.checked_sub(&self.prev).unwrap_or_default();
        self.bytes += d.vu_len() + crate::nat::vu64_len(dl);
        self.prev = u.clone();
        if self.bytes >= self.target {
            self.bytes = 0;
            self.prev = Nat::ZERO;
            return true;
        }
        false
    }
}

pub fn encode_doclen_summary(docs: u64, tokens: u64, segments: u64) -> Vec<u8> {
    let mut out = FT_SUM_MAGIC.to_vec();
    put_vu64(&mut out, docs);
    put_vu64(&mut out, tokens);
    put_vu64(&mut out, segments);
    out
}

/// (documents, tokens, segments) of a summary.
pub fn decode_doclen_summary(raw: &[u8], expected_docs: Option<u64>) -> Result<(u64, u64, u64)> {
    if raw.len() < 4 || &raw[..4] != FT_SUM_MAGIC {
        return Err(cfg_err("FULLTEXT document length summary magic is wrong".into()));
    }
    let mut p = 4;
    let docs = take_u64(raw, &mut p)?;
    let tokens = take_u64(raw, &mut p)?;
    let segs = take_u64(raw, &mut p)?;
    if p != raw.len() {
        return Err(cfg_err("FULLTEXT document length summary has trailing bytes".into()));
    }
    if expected_docs.is_some_and(|e| e != docs) {
        return Err(cfg_err("FULLTEXT document count mismatch".into()));
    }
    Ok((docs, tokens, segs))
}

/// Decodes a segment (FTD1 encoding) into sorted vectors.
pub fn decode_doclen_segment(raw: &[u8], expected_count: Option<u64>) -> Result<DocLenSeg> {
    if raw.len() < 4 || &raw[..4] != FT_DOC_MAGIC {
        return Err(cfg_err("FULLTEXT document lengths magic is wrong".into()));
    }
    let mut p = 4;
    let count = take_u64(raw, &mut p)?;
    let total = take_u64(raw, &mut p)?;
    if expected_count.is_some_and(|e| e != count) {
        return Err(cfg_err("FULLTEXT document count mismatch".into()));
    }
    let n = usize::try_from(count).unwrap_or(usize::MAX).min(raw.len());
    let (mut us, mut lens) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let mut u = Nat::ZERO;
    let mut check: u128 = 0;
    for i in 0..count {
        let d = get_vu(raw, &mut p)?;
        if i > 0 && d.is_zero() {
            return Err(cfg_err("FULLTEXT document lengths must have strictly ascending u".into()));
        }
        u = u.add(&d);
        let dl = take_u64(raw, &mut p)?;
        check += dl as u128;
        us.push(u.clone());
        lens.push(dl);
    }
    if p != raw.len() {
        return Err(cfg_err("FULLTEXT document lengths have trailing bytes".into()));
    }
    if check != total as u128 {
        return Err(cfg_err("FULLTEXT total_tokens mismatch".into()));
    }
    Ok(DocLenSeg { us, lens, tokens: total })
}

// ----------------------------------------------------------------------
// Candidates and ranking
// ----------------------------------------------------------------------

/// Posting lists per term (shared view, no copy).
pub type TermPostings<'a> = [(String, &'a [FtPosting])];

/// `fulltext_candidate_docs` (terms in query order).
pub fn fulltext_candidate_docs(postings: &TermPostings<'_>, mode: &str) -> Result<Vec<Nat>> {
    let mode = unicode::lower(mode);
    if mode != "all" && mode != "any" {
        return Err(cfg_err("FULLTEXT mode must be 'all' or 'any'".into()));
    }
    if postings.is_empty() {
        return Ok(Vec::new());
    }
    if mode == "any" {
        let mut all: Vec<Nat> = postings.iter().flat_map(|(_, p)| p.iter().map(|x| x.u.clone())).collect();
        all.sort();
        all.dedup();
        return Ok(all);
    }
    let mut lists: Vec<&[FtPosting]> = postings.iter().map(|(_, p)| *p).collect();
    lists.sort_by_key(|l| l.len());
    let mut out: Vec<Nat> = lists[0].iter().map(|x| x.u.clone()).collect();
    for l in &lists[1..] {
        let us: Vec<Nat> = l.iter().map(|x| x.u.clone()).collect();
        out = crate::setops::intersect_sorted(&out, &us);
        if out.is_empty() {
            break;
        }
    }
    Ok(out)
}

/// Galloping search for the first index `>= start` with `list[i].u >= u`.
fn seek(list: &[FtPosting], start: usize, u: &Nat) -> usize {
    let mut step = 1usize;
    let mut lo = start;
    let mut hi = start;
    while hi < list.len() && &list[hi].u < u {
        lo = hi + 1;
        hi += step;
        step *= 2;
    }
    let hi = hi.min(list.len());
    lo + list[lo..hi].partition_point(|x| &x.u < u)
}

/// `phrase_candidate_docs`: documents in which the token sequence occurs
/// contiguously (repeated terms position-exact). Multi-way merge over the
/// postings sorted by u, position check via bisection.
pub fn phrase_candidate_docs(seq: &[String], postings: &TermPostings<'_>) -> Vec<Nat> {
    if seq.is_empty() {
        return Vec::new();
    }
    let lookup = |t: &str| -> &[FtPosting] { postings.iter().find(|(k, _)| k == t).map(|(_, p)| *p).unwrap_or(&[]) };
    let lists: Vec<&[FtPosting]> = seq.iter().map(|t| lookup(t)).collect();
    if lists.iter().any(|l| l.is_empty()) {
        return Vec::new();
    }
    let lead = (0..lists.len()).min_by_key(|&i| lists[i].len()).unwrap();
    let mut cursors = vec![0usize; lists.len()];
    let mut out = Vec::new();
    'docs: for doc in lists[lead] {
        let u = &doc.u;
        let mut pos: Vec<&[u64]> = Vec::with_capacity(lists.len());
        for (k, l) in lists.iter().enumerate() {
            let i = seek(l, cursors[k], u);
            cursors[k] = i;
            if i >= l.len() || &l[i].u != u {
                continue 'docs;
            }
            pos.push(&l[i].positions);
        }
        if pos[0].iter().any(|&p0| (1..pos.len()).all(|k| pos[k].binary_search(&(p0 + k as u64)).is_ok())) {
            out.push(u.clone());
        }
    }
    out
}

/// R4-A2: BM25 streamed over ascending candidate u (one galloping cursor per
/// term) instead of hash maps over all hits. Per document the term scores
/// are summed in query order, exactly as `bm25_scores`, so the values are
/// bit-identical.
pub struct Bm25Stream<'p> {
    lists: Vec<&'p [FtPosting]>,
    idf: Vec<f64>,
    cursors: Vec<usize>,
    k1: f64,
    b: f64,
    avgdl: f64,
}

impl<'p> Bm25Stream<'p> {
    /// Same parameter checks as `bm25_scores`; `None` for an empty corpus
    /// (no scores at all).
    /// `dfs`: document frequency of every term (the posting length, unless
    /// the posting was decoded restricted to the candidates).
    pub fn new(postings: &TermPostings<'p>, dfs: &[u64], total_docs: u64, total_tokens: u64, k1: f64, b: f64) -> Result<Option<Self>> {
        if !(k1.is_finite() && k1 > 0.0) {
            return Err(cfg_err("BM25 k1 must be > 0".into()));
        }
        if !(0.0..=1.0).contains(&b) {
            return Err(cfg_err("BM25 b must be between 0 and 1".into()));
        }
        if total_docs == 0 {
            return Ok(None);
        }
        let corpus = FtCorpus { lengths: BTreeMap::new(), total_docs, total_tokens };
        let avg = corpus.avg_doc_len();
        let n = total_docs as f64;
        let (mut lists, mut idf) = (Vec::new(), Vec::new());
        for ((_t, posting), &df) in postings.iter().zip(dfs) {
            if df == 0 {
                continue;
            }
            let df = df as f64;
            lists.push(*posting);
            idf.push((1.0 + (n - df + 0.5) / (df + 0.5)).ln());
        }
        let cursors = vec![0; lists.len()];
        Ok(Some(Bm25Stream { lists, idf, cursors, k1, b, avgdl: if avg == 0.0 { 1.0 } else { avg } }))
    }

    /// Score of document `u` with length `dl`; `None` if no term has a
    /// position in it. Calls must come in strictly ascending `u`.
    pub fn score(&mut self, u: &Nat, dl: u64) -> Option<f64> {
        let mut total: Option<f64> = None;
        let dl = dl as f64;
        for k in 0..self.lists.len() {
            let l = self.lists[k];
            let i = seek(l, self.cursors[k], u);
            self.cursors[k] = i;
            if i >= l.len() || &l[i].u != u || l[i].positions.is_empty() {
                continue;
            }
            let tf = l[i].positions.len() as f64;
            let norm = tf + self.k1 * (1.0 - self.b + self.b * (dl / self.avgdl));
            let score = self.idf[k] * (tf * (self.k1 + 1.0)) / norm;
            total = Some(match total {
                None => score,
                Some(s) => s + score,
            });
        }
        total
    }
}

/// `bm25_scores`: summation per term in query order.
pub fn bm25_scores(postings: &TermPostings<'_>, corpus: &FtCorpus, candidates: Option<&[Nat]>, k1: f64, b: f64) -> Result<Vec<(Nat, f64)>> {
    // F056: NaN/inf would silently produce NaN scores.
    if !(k1.is_finite() && k1 > 0.0) {
        return Err(cfg_err("BM25 k1 must be > 0".into()));
    }
    if !(0.0..=1.0).contains(&b) {
        return Err(cfg_err("BM25 b must be between 0 and 1".into()));
    }
    let n = corpus.total_docs as f64;
    if corpus.total_docs == 0 {
        return Ok(Vec::new());
    }
    let avg = corpus.avg_doc_len();
    let avgdl = if avg == 0.0 { 1.0 } else { avg };
    let allowed: Option<HashSet<&Nat>> = candidates.map(|c| c.iter().collect());
    let mut order: Vec<Nat> = Vec::new();
    let mut scores: HashMap<Nat, f64> = HashMap::new();
    for (_t, posting) in postings {
        let df = posting.len() as f64;
        if posting.is_empty() {
            continue;
        }
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        for item in posting.iter() {
            if allowed.as_ref().is_some_and(|a| !a.contains(&item.u)) {
                continue;
            }
            let tf = item.positions.len() as f64;
            if item.positions.is_empty() {
                continue;
            }
            let dl = corpus.lengths.get(&item.u).copied().unwrap_or(0) as f64;
            let norm = tf + k1 * (1.0 - b + b * (dl / avgdl));
            let score = idf * (tf * (k1 + 1.0)) / norm;
            match scores.get_mut(&item.u) {
                Some(s) => *s += score,
                None => {
                    order.push(item.u.clone());
                    scores.insert(item.u.clone(), score);
                }
            }
        }
    }
    Ok(order.into_iter().map(|u| { let s = scores[&u]; (u, s) }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RB3-05: segment keys round-trip for every last u, including 0, and
    /// sort like their u.
    #[test]
    fn doclen_segment_keys_round_trip() {
        let us: Vec<Nat> = [0u64, 1, 255, 256, 65535, u64::MAX].iter().map(|u| Nat::S(*u)).chain([Nat::from(num_bigint::BigUint::from(1u8) << 200usize)]).collect();
        for w in us.windows(2) {
            let (a, b) = (doclen_segment_key(&w[0]), doclen_segment_key(&w[1]));
            assert_eq!(doclen_segment_last(&a), Some(w[0].clone()), "{a}");
            assert!(a < b, "{a} !< {b}");
        }
        assert_eq!(doclen_segment_last(&doclen_segment_key(us.last().unwrap())), us.last().cloned());
    }

    #[test]
    fn normalize() {
        assert_eq!(normalize_kind(&Val::None).unwrap(), IndexKind::Value);
        assert_eq!(normalize_kind(&Val::str(" fulltext ")).unwrap(), IndexKind::Fulltext);
        assert!(matches!(normalize_kind(&Val::str("x")), Err(Error::IndexConfig(_))));
        let o = normalize_options(IndexKind::Trigram, &vmap! {"ngram" => "4", "casefold" => 1i64}).unwrap();
        assert_eq!(o, vmap! {"ngram" => 4i64, "casefold" => true});
        assert!(normalize_options(IndexKind::Trigram, &vmap! {"ngram" => 1i64}).is_err());
        assert!(normalize_options(IndexKind::Token, &vmap! {"tokenizer" => "icu"}).is_err());
        assert_eq!(
            normalize_options(IndexKind::Fulltext, &Val::None).unwrap(),
            vmap! {"tokenizer" => "simple", "casefold" => true, "positions" => true}
        );
    }

    #[test]
    fn tokens_ngrams_codecs() {
        assert_eq!(simple_tokens(&Val::str("The quick, quick FOX_1 ä!"), true), vec!["the", "quick", "quick", "fox_1", "ä"]);
        assert_eq!(ngrams(&Val::str("äbcd"), 3, false), vec!["äbc", "bcd"]);
        let (dl, pos) = fulltext_document(&Val::str("a b a"), &Val::None).unwrap();
        assert_eq!(dl, 3);
        assert_eq!(pos, vec![("a".to_string(), vec![0, 2]), ("b".to_string(), vec![1])]);
        let e = vec![(Nat::S(3), vec![0, 5]), (Nat::S(9), vec![2])];
        let raw = encode_fulltext_posting(&e).unwrap();
        let d = decode_fulltext_posting(&raw, Some(2)).unwrap();
        assert_eq!(d[1], FtPosting { u: Nat::S(9), positions: vec![2] });
        let raw = encode_fulltext_doc_lengths(&[(Nat::S(3), 4), (Nat::S(9), 1)]).unwrap();
        let c = decode_fulltext_doc_lengths(&raw, Some(2)).unwrap();
        assert_eq!((c.total_docs, c.total_tokens), (2, 5));
        let b = [FtPosting { u: Nat::S(3), positions: vec![1] }];
        let ph = phrase_candidate_docs(&["a".into(), "b".into()], &[("a".into(), &d[..]), ("b".into(), &b[..])]);
        assert_eq!(ph, vec![Nat::S(3)]);
        assert!(predicate_matches(&Val::str("Hello World"), "icontains", &Val::str("WORLD"), &Val::None).unwrap());
    }
}

#[cfg(test)]
mod doclen_key_tests {
    use super::*;
    use crate::db::secindex::{cmp_keys, encode_key_vec};

    /// Segment keys sort like their u (big `Nat` included), round-trip, and
    /// sort after the summary key; segmentation covers every document once.
    #[test]
    fn segment_keys_order_and_split() {
        let mut us: Vec<Nat> = vec![Nat::S(1), Nat::S(2), Nat::S(255), Nat::S(256), Nat::S(u64::MAX)];
        for bits in [64u32, 65, 70, 128, 300] {
            let b = Nat::from(num_bigint::BigUint::from(1u8) << bits);
            us.push(b.clone());
            us.push(b.add_u64(1));
        }
        us.sort();
        let keys: Vec<Vec<u8>> = us.iter().map(|u| encode_key_vec(&Val::Str(doclen_segment_key(u))).unwrap()).collect();
        let summary = encode_key_vec(&Val::str(FULLTEXT_DOCLEN_KEY)).unwrap();
        for w in keys.windows(2) {
            assert_eq!(cmp_keys(&w[0], &w[1]), std::cmp::Ordering::Less);
        }
        assert_eq!(cmp_keys(&summary, &keys[0]), std::cmp::Ordering::Less);
        for u in &us {
            assert_eq!(doclen_segment_last(&doclen_segment_key(u)).as_ref(), Some(u));
        }
        for bad in ["", FULLTEXT_DOCLEN_KEY, "\u{0}NHYDB_FULLTEXT_DOCUMENT_LENGTHS\u{1}00000001", "\u{0}NHYDB_FULLTEXT_DOCUMENT_LENGTHS\u{1}0000000100", "\u{0}NHYDB_FULLTEXT_DOCUMENT_LENGTHS\u{1}000000010A"] {
            assert!(doclen_segment_last(bad).is_none(), "{bad:?}");
        }
        let entries: Vec<(Nat, u64)> = (1..=5000u64).map(|u| (Nat::S(u * 3), u % 50)).collect();
        let segs = doclen_segments(&entries, 256);
        assert!(segs.len() > 10);
        assert_eq!(segs.iter().map(|s| s.len()).sum::<usize>(), entries.len());
        let mut back = Vec::new();
        for s in &segs {
            let d = decode_doclen_segment(&encode_fulltext_doc_lengths(s).unwrap(), Some(s.len() as u64)).unwrap();
            assert_eq!(d.tokens, s.iter().map(|e| e.1).sum::<u64>());
            back.extend(d.us.into_iter().zip(d.lens));
        }
        assert_eq!(back, entries);
        assert!(decode_doclen_segment(&encode_fulltext_doc_lengths(&entries[..10]).unwrap(), Some(11)).is_err());
        assert_eq!(decode_doclen_summary(&encode_doclen_summary(7, 9, 2), Some(7)).unwrap(), (7, 9, 2));
        assert!(decode_doclen_summary(&encode_doclen_summary(7, 9, 2), Some(8)).is_err());
    }
}
