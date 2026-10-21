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
pub const PAYLOAD_FULLTEXT_DOCLENS: u8 = 2;
pub const FULLTEXT_DOCLEN_KEY: &str = "\u{0}NHYDB_FULLTEXT_DOCUMENT_LENGTHS";

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
    let chars: Vec<char> = normalize_text(value, casefold).chars().collect();
    if chars.len() < n {
        return Vec::new();
    }
    (0..=chars.len() - n).map(|i| chars[i..i + n].iter().collect()).collect()
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
    let mut out = FT_POST_MAGIC.to_vec();
    put_vu64(&mut out, entries.len() as u64);
    let mut prev = Nat::ZERO;
    for (u, pos) in entries {
        if u <= &prev {
            return Err(cfg_err("FULLTEXT posting documents must be strictly ascending".into()));
        }
        if pos.windows(2).any(|w| w[0] >= w[1]) {
            return Err(cfg_err("FULLTEXT position list must be strictly ascending and >=0".into()));
        }
        u.checked_sub(&prev).unwrap().put_vu(&mut out);
        put_vu64(&mut out, pos.len() as u64);
        let mut p0 = 0u64;
        for (i, p) in pos.iter().enumerate() {
            put_vu64(&mut out, if i == 0 { *p } else { p - p0 });
            p0 = *p;
        }
        prev = u.clone();
    }
    Ok(out)
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
