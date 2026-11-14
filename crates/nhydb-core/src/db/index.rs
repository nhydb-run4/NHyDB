//! Secondary content indexes (`SECONDARY`, root tag `SI28`).
//!
//! Coupled to BASE/dense records: dense records are indexed
//! (BASE fields from the stored raw values, extension fields
//! from the merged record view). Posting lists are stored as u32/u64
//! arrays or (for u > 2^64) as VarUInt deltas, inline in the leaf pages of
//! the persistent key tree or zlib-compressed in chunk blocks
//! (`secindex.rs`).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use crate::address::{lane_flags, lane_name, Lane};
use crate::codec::{decode_value, skip_value};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, Nat};
use crate::schema::{IndexDef, LayoutKind};
use crate::textindex::{
    self, FtCorpus, FtPosting, IndexKind, FULLTEXT_DOCLEN_KEY, PAYLOAD_FULLTEXT_DOCLENS, PAYLOAD_FULLTEXT_DOCLEN_SEGMENT,
    PAYLOAD_FULLTEXT_DOCLEN_SUMMARY, PAYLOAD_FULLTEXT_POSITIONS,
};
use super::{PostKey, PostVal};
use crate::val::{Val, ValKey};
use crate::vmap;

use super::secindex::{self, SecEntry};
use super::{BlockInfo, Db, SecondaryDir, SVal};

/// Filter condition for `find_u`.
#[derive(Clone, Debug)]
pub enum Cond {
    Eq(Val),
    Range(Val, Val),
}

/// Document u of one key during the build (P014/P020, I001): plain `u32`
/// as long as every u fits (4 bytes per reference, the width of an ordinary
/// posting on disk), then `u64` from the first u >= 2^32, then exact `Nat`
/// values from the first u >= 2^64, so BigInt u keep their width-0
/// postings. Growth is fallible: an allocation failure becomes
/// `Error::Memory` instead of aborting the process.
#[derive(Clone, Debug)]
pub(crate) enum UList {
    Tiny(Vec<u32>),
    Small(Vec<u64>),
    Big(Vec<Nat>),
}

impl Default for UList {
    fn default() -> Self {
        UList::Tiny(Vec::new())
    }
}

fn grow<T>(v: &mut Vec<T>) -> Result<()> {
    v.try_reserve(1).map_err(|e| Error::Memory(format!("index build: cannot grow a posting list: {e}")))
}

impl UList {
    pub(crate) fn push(&mut self, u: &Nat) -> Result<()> {
        match (&mut *self, u) {
            (UList::Tiny(v), Nat::S(x)) if *x <= u32::MAX as u64 => {
                grow(v)?;
                v.push(*x as u32);
            }
            (UList::Small(v), Nat::S(x)) => {
                grow(v)?;
                v.push(*x);
            }
            (UList::Big(v), _) => {
                grow(v)?;
                v.push(u.clone());
            }
            (UList::Tiny(v), Nat::S(x)) => {
                let mut wide: Vec<u64> = Vec::new();
                wide.try_reserve_exact(v.len() + 1).map_err(|e| Error::Memory(format!("index build: cannot widen a posting list: {e}")))?;
                wide.extend(v.iter().map(|&y| y as u64));
                wide.push(*x);
                *self = UList::Small(wide);
            }
            (UList::Tiny(_), Nat::B(_)) | (UList::Small(_), Nat::B(_)) => {
                let mut big = self.to_nats();
                grow(&mut big)?;
                big.push(u.clone());
                *self = UList::Big(big);
            }
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            UList::Tiny(v) => v.len(),
            UList::Small(v) => v.len(),
            UList::Big(v) => v.len(),
        }
    }

    pub(crate) fn last(&self) -> Option<Nat> {
        match self {
            UList::Tiny(v) => v.last().map(|x| Nat::S(*x as u64)),
            UList::Small(v) => v.last().map(|x| Nat::S(*x)),
            UList::Big(v) => v.last().cloned(),
        }
    }

    fn sort_check(&mut self) {
        match self {
            UList::Tiny(v) => sort_dedup_check(v),
            UList::Small(v) => sort_dedup_check(v),
            UList::Big(v) => sort_dedup_check(v),
        }
    }

    pub(crate) fn to_nats(&self) -> Vec<Nat> {
        match self {
            UList::Tiny(v) => v.iter().map(|x| Nat::S(*x as u64)).collect(),
            UList::Small(v) => v.iter().map(|x| Nat::S(*x)).collect(),
            UList::Big(v) => v.clone(),
        }
    }
}

/// Posting content of a key during the build.
#[derive(Clone, Debug)]
pub(crate) enum Bucket {
    /// Document u (VALUE/TOKEN/TRIGRAM).
    Us(UList),
    /// FULLTEXT: (u, positions).
    Ft(Vec<(Nat, Vec<u64>)>),
    /// FULLTEXT: (u, document length) under `FULLTEXT_DOCLEN_KEY`.
    Doc(Vec<(Nat, u64)>),
}

impl Bucket {
    pub(crate) fn len(&self) -> usize {
        match self {
            Bucket::Us(v) => v.len(),
            Bucket::Ft(v) => v.len(),
            Bucket::Doc(v) => v.len(),
        }
    }
}

#[derive(Default)]
pub(crate) struct ValueMap {
    items: Vec<(Val, Bucket)>,
    pos: HashMap<ValKey, usize>,
    /// Fast path for text keys (TOKEN/TRIGRAM/FULLTEXT).
    spos: HashMap<String, usize>,
    /// estimated heap bytes (keys, map entries, lists) for the build budget
    bytes: usize,
    /// largest document u of the buckets already spilled (posting width)
    spilled_max_u: Option<Nat>,
}

/// Estimated bytes of one key of a `ValueMap` (item, hash entry, key copies).
const KEY_COST: usize = 192;

fn key_heap(v: &Val) -> usize {
    match v {
        Val::Str(s) => s.len(),
        Val::Bytes(b) => b.len(),
        Val::Big(_) | Val::List(_) | Val::Map(_) => 64,
        _ => 0,
    }
}

impl ValueMap {
    fn bucket_str(&mut self, key: &str, make: impl FnOnce() -> Bucket) -> &mut Bucket {
        let i = match self.spos.get(key) {
            Some(&i) => i,
            None => {
                let i = self.items.len();
                self.spos.insert(key.to_string(), i);
                self.items.push((Val::str(key), make()));
                self.bytes += KEY_COST + 2 * key.len();
                i
            }
        };
        &mut self.items[i].1
    }

    fn bucket(&mut self, key: Val, make: impl FnOnce() -> Bucket) -> &mut Bucket {
        let k = ValKey(key.clone());
        let i = match self.pos.get(&k) {
            Some(&i) => i,
            None => {
                self.bytes += KEY_COST + 2 * key_heap(&key);
                self.pos.insert(k, self.items.len());
                self.items.push((key, make()));
                self.items.len() - 1
            }
        };
        &mut self.items[i].1
    }
}

/// Where the document lengths of a FULLTEXT index are.
enum DocLenSource {
    Empty,
    /// 0.28.0/0.28.1: one list (decoded through the posting pool)
    Whole(Arc<textindex::DocLenSeg>),
    /// 0.28.2: summary + segments in the key tree
    Paged { docs: u64, tokens: u64 },
}

/// Root of the key tree of an index (the pages are read only when needed).
#[derive(Clone, Debug)]
pub struct SecondaryMeta {
    pub index_id: u64,
    pub lane: Lane,
    pub source_block_id: u64,
    /// distinct keys of the index
    pub entry_count: u64,
    /// tree height (0: the root holds the only leaf)
    pub height: u64,
    pub block_id: u64,
    /// Offset of the root block header.
    pub block_offset: u64,
    /// FULLTEXT: number of document length segments (entries that are no
    /// keys), read once on first use
    pub(crate) doclen_segments: std::sync::OnceLock<u64>,
}

/// Python-style comparison of two scalars (`lo <= value`).
pub fn py_cmp(a: &Val, b: &Val) -> Result<Ordering> {
    if let (Some(x), Some(y)) = (a.as_number(), b.as_number()) {
        return x.partial_cmp(&y).ok_or_else(|| Error::Type("Range filter does not match the index type".into()));
    }
    match (a, b) {
        (Val::Str(x), Val::Str(y)) => Ok(x.cmp(y)),
        (Val::Bytes(x), Val::Bytes(y)) => Ok(x.cmp(y)),
        _ => Err(Error::Type("Range filter does not match the index type".into())),
    }
}

pub fn json_key_encode(v: &Val) -> Result<String> {
    match v {
        Val::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            vmap! {"__bytes__" => hex}.to_json()
        }
        other => other.to_json(),
    }
}

pub fn json_key_decode(raw: &[u8]) -> Result<Val> {
    let s = std::str::from_utf8(raw).map_err(|e| Error::Corruption(format!("SECONDARY key: {e}")))?;
    let v = Val::from_json(s)?;
    if let Val::Map(m) = &v {
        if m.len() == 1 && m[0].0 == "__bytes__" {
            if let Val::Str(h) = &m[0].1 {
                let bytes: Option<Vec<u8>> =
                    (0..h.len() / 2).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()).collect();
                return bytes.map(Val::Bytes).ok_or_else(|| Error::Corruption("SECONDARY bytes key".into()));
            }
        }
    }
    Ok(v)
}

pub fn encode_big_posting(us: &[Nat]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut prev = Nat::ZERO;
    for u in us {
        let d = u.checked_sub(&prev).filter(|d| !d.is_zero()).ok_or_else(|| {
            Error::Value("Big-u posting must be strictly ascending".into())
        })?;
        d.put_vu(&mut out);
        prev = u.clone();
    }
    Ok(out)
}

pub fn decode_big_posting(raw: &[u8], count: u64) -> Result<Vec<Nat>> {
    let mut p = 0;
    let mut prev = Nat::ZERO;
    // Every delta takes at least one byte: the capacity never exceeds the
    // payload, whatever `count` claims (F022).
    let mut out = Vec::with_capacity(usize::try_from(count).unwrap_or(usize::MAX).min(raw.len()));
    for _ in 0..count {
        let d = get_vu(raw, &mut p)?;
        if d.is_zero() {
            return Err(Error::Corruption("Big-u posting delta must be >0".into()));
        }
        prev = prev.add(&d);
        out.push(prev.clone());
    }
    if p != raw.len() {
        return Err(Error::Corruption("Big-u posting trailing bytes".into()));
    }
    Ok(out)
}

/// Pre-evaluated key generation of an index.
pub(crate) struct KeyGen {
    casefold: bool,
    ngram: usize,
}

impl KeyGen {
    fn of(idef: &IndexDef) -> Result<KeyGen> {
        let o = textindex::normalize_options(idef.kind, &idef.options)?;
        Ok(KeyGen {
            casefold: o.get("casefold").map(|v| v.is_truthy()).unwrap_or(false),
            ngram: o.get("ngram").and_then(|v| v.as_i64()).unwrap_or(3).max(1) as usize,
        })
    }
}

fn width_for(max_u: &Nat) -> u8 {
    if max_u <= &Nat::S(u32::MAX as u64) {
        4
    } else if max_u.as_u64().is_some() {
        8
    } else {
        0
    }
}

/// Class of a value for range lookups: 0 numbers (without NaN), 1 `str`,
/// 2 `bytes`, 3 everything else (never ordered against the bounds).
fn range_class(v: &Val) -> u8 {
    match v {
        Val::Float(f) if f.is_nan() => 3,
        Val::Bool(_) | Val::Int(_) | Val::Big(_) | Val::Float(_) => 0,
        Val::Str(_) => 1,
        Val::Bytes(_) => 2,
        _ => 3,
    }
}

/// Python `x == x` is false for NaN; containers holding NaN never compare
/// equal to a freshly decoded value either.
fn contains_nan(v: &Val) -> bool {
    match v {
        Val::Float(f) => f.is_nan(),
        Val::List(l) => l.iter().any(contains_nan),
        Val::Map(m) => m.iter().any(|(_, x)| contains_nan(x)),
        _ => false,
    }
}

/// Sorted, duplicate-free union of several postings (P014): when every u
/// fits into 64 bits (width 4/8 postings) the union is sorted as plain
/// `u64` (no enum comparisons); postings with u > 2^64 (width 0, BigInt)
/// take the general `Nat` path, so arbitrarily large u stay exact.
/// RUN¹⁰: estimated bytes of a FULLTEXT posting list (limiter growth).
fn posting_bytes(p: &[textindex::FtPosting]) -> u64 {
    p.iter().map(|x| (std::mem::size_of::<textindex::FtPosting>() + x.positions.len() * 8) as u64).sum()
}

pub(crate) fn union_postings(postings: &[Arc<Vec<Nat>>]) -> Vec<Nat> {
    let total: usize = postings.iter().map(|p| p.len()).sum();
    let mut small: Vec<u64> = Vec::with_capacity(total);
    let mut all_small = true;
    'outer: for p in postings {
        for u in p.iter() {
            match u {
                Nat::S(x) => small.push(*x),
                Nat::B(_) => {
                    all_small = false;
                    break 'outer;
                }
            }
        }
    }
    if all_small {
        small.sort_unstable();
        small.dedup();
        return small.into_iter().map(Nat::S).collect();
    }
    let mut merged: Vec<Nat> = Vec::with_capacity(total);
    for p in postings {
        merged.extend(p.iter().cloned());
    }
    merged.sort();
    merged.dedup();
    merged
}

fn sort_dedup_check<T: Ord + Clone>(v: &mut [T]) {
    if v.windows(2).any(|w| w[0] > w[1]) {
        v.sort();
    }
}


/// Value lists per `(index_id, lane)` during the build.
type IndexMaps = HashMap<(u64, Lane), ValueMap>;

/// Range lookups over at most this many keys cache their postings.
const RANGE_CACHED_KEYS: usize = 64;

/// Estimated bytes of one document reference in a list (with growth reserve).
const REF_COST: usize = 10;
/// The build budget is checked at least after this many record bytes.
const CHECK_BYTES: usize = 64 << 10;

/// Spilled runs of one build (external sort, `idxspill.rs`).
pub(crate) struct Spill {
    budget: usize,
    dirs: Vec<std::path::PathBuf>,
    file: Option<Arc<super::idxspill::SpillFile>>,
    runs: HashMap<(u64, Lane), Vec<super::idxspill::SpillRun>>,
    pub(crate) spills: u64,
    pub(crate) bytes: u64,
    /// RUN¹⁰: elastic job of the RAM limiter; `SHRINK` spills the maps
    /// before the budget is reached
    meter: crate::limiter::Meter,
    /// bytes of the maps at the last check (growth for the meter)
    held: usize,
}

impl Spill {
    /// Writes every map as a sorted run and empties it once the maps
    /// together exceed the budget (or the RAM limiter asks for memory).
    fn check(&mut self, maps: &mut IndexMaps) -> Result<()> {
        let total = maps.values().map(|m| m.bytes).sum::<usize>();
        let grown = total.saturating_sub(self.held);
        self.held = total;
        let shrink = self.meter.grow(grown as u64)?;
        if total <= self.budget && !(shrink && total > 0) {
            return Ok(());
        }
        self.held = 0;
        if self.file.is_none() {
            let dirs: Vec<&std::path::Path> = self.dirs.iter().map(|d| d.as_path()).collect();
            self.file = Some(super::idxspill::SpillFile::create(&dirs)?);
        }
        let file = self.file.clone().unwrap();
        let mut keys: Vec<(u64, Lane)> = maps.keys().copied().collect();
        keys.sort();
        for k in keys {
            let vm = maps.get_mut(&k).unwrap();
            if vm.items.is_empty() {
                continue;
            }
            sort_buckets(vm);
            for (_, b) in &vm.items {
                if let Bucket::Us(us) = b {
                    if let Some(last) = us.last() {
                        if vm.spilled_max_u.as_ref().is_none_or(|m| &last > m) {
                            vm.spilled_max_u = Some(last);
                        }
                    }
                }
            }
            vm.items.sort_by(|a, b| secindex::cmp_vals(&a.0, &b.0));
            let mut w = super::idxspill::SpillWriter::create(&file)?;
            for (v, b) in &vm.items {
                w.push(&secindex::encode_key_vec(v)?, b)?;
            }
            let run = w.finish()?;
            self.bytes = self.bytes.max(run_end(&run));
            self.runs.entry(k).or_default().push(run);
            vm.items = Vec::new();
            vm.pos = HashMap::new();
            vm.spos = HashMap::new();
            vm.bytes = 0;
        }
        self.spills += 1;
        Ok(())
    }
}

/// Flags the ranges (inclusive bounds) that overlap at least one other
/// range: only their u need the newest-wins deduplication of the index
/// build (RA2-03: no set over all records of non-overlapping blocks).
fn overlapping<'a>(ranges: impl Iterator<Item = (&'a Nat, &'a Nat)>) -> Vec<bool> {
    let ranges: Vec<(&Nat, &Nat)> = ranges.collect();
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by(|a, b| ranges[*a].0.cmp(ranges[*b].0));
    let mut flags = vec![false; ranges.len()];
    let mut holder: Option<usize> = None;
    for i in order {
        if let Some(h) = holder {
            if ranges[i].0 <= ranges[h].1 {
                flags[i] = true;
                flags[h] = true;
            }
            if ranges[i].1 > ranges[h].1 {
                holder = Some(i);
            }
        } else {
            holder = Some(i);
        }
    }
    flags
}

/// Head of one merge source (min-heap order: key, then source index).
struct MergeHead {
    key: Vec<u8>,
    src: usize,
}

impl Ord for MergeHead {
    fn cmp(&self, o: &Self) -> Ordering {
        secindex::cmp_keys(&o.key, &self.key).then(o.src.cmp(&self.src))
    }
}

impl PartialOrd for MergeHead {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl PartialEq for MergeHead {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}

impl Eq for MergeHead {}

fn run_end(r: &super::idxspill::SpillRun) -> u64 {
    r.end_offset()
}

/// RUN9 V4: from this many entries on, a key spread over several runs is
/// merged entry by entry instead of being decoded as a whole (below: at most
/// about 64 B per entry, i.e. a few MiB).
const STREAM_MERGE_MIN: u64 = 1 << 16;

/// Test-only: the threshold of [`STREAM_MERGE_MIN`] (1 = merge every
/// spilled key entry by entry); both paths write the same bytes.
#[cfg(test)]
static STREAM_MERGE_MIN_TEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(STREAM_MERGE_MIN);

fn stream_merge_min() -> u64 {
    #[cfg(test)]
    return STREAM_MERGE_MIN_TEST.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(test))]
    STREAM_MERGE_MIN
}

/// What [`Db::persist_merged`] wrote: references, or the document count of
/// the FULLTEXT document lengths.
enum Merged {
    Refs(u64),
    Docs(u64),
}

/// RUN9 V4: k-way merge of `sources` sorted streams (`next(s)` yields the
/// next entry of source `s`): `f` receives the entries in ascending `key`
/// order, equal keys in source order and, within a source, in stream order
/// – the order of the stable sort of the concatenated sources that
/// `merge_buckets` produces. One entry per source is held.
fn stream_merge<E, K: Ord>(sources: usize, next: &mut impl FnMut(usize) -> Result<Option<E>>, key: impl Fn(&E) -> K, mut f: impl FnMut(E) -> Result<()>) -> Result<()> {
    use std::cmp::Reverse;
    let mut heads: Vec<Option<E>> = Vec::with_capacity(sources);
    let mut heap = BinaryHeap::with_capacity(sources);
    for s in 0..sources {
        let e = next(s)?;
        if let Some(e) = &e {
            heap.push(Reverse((key(e), s)));
        }
        heads.push(e);
    }
    while let Some(Reverse((_, s))) = heap.pop() {
        let e = heads[s].take().expect("head of a source in the heap");
        let n = next(s)?;
        if let Some(x) = &n {
            heap.push(Reverse((key(x), s)));
        }
        heads[s] = n;
        f(e)?;
    }
    Ok(())
}

/// Lists sorted by document u (persisted postings and spilled runs).
fn sort_buckets(vm: &mut ValueMap) {
    for (_, b) in vm.items.iter_mut() {
        match b {
            Bucket::Us(v) => v.sort_check(),
            Bucket::Ft(v) => {
                if v.windows(2).any(|w| w[0].0 > w[1].0) {
                    v.sort_by(|a, b| a.0.cmp(&b.0));
                }
            }
            Bucket::Doc(v) => {
                if v.windows(2).any(|w| w[0].0 > w[1].0) {
                    v.sort_by(|a, b| a.0.cmp(&b.0));
                }
            }
        }
    }
}

/// Raw posting bytes of a bucket in its codec (`width`: 4/8 = u32/u64
/// arrays, 0 = VarUInt deltas).
fn encode_bucket(bucket: &Bucket, width: u8) -> Result<(u8, Vec<u8>)> {
    let too_wide = || Error::Value("posting u exceeds the posting width".into());
    Ok(match bucket {
        Bucket::Ft(e) => (PAYLOAD_FULLTEXT_POSITIONS, textindex::encode_fulltext_posting(e)?),
        Bucket::Doc(e) => (PAYLOAD_FULLTEXT_DOCLENS, textindex::encode_fulltext_doc_lengths(e)?),
        Bucket::Us(UList::Tiny(us)) if width == 4 || width == 8 => {
            let mut raw = Vec::with_capacity(us.len() * width as usize);
            for &x in us {
                if width == 4 {
                    raw.extend_from_slice(&x.to_le_bytes());
                } else {
                    raw.extend_from_slice(&(x as u64).to_le_bytes());
                }
            }
            (width, raw)
        }
        Bucket::Us(UList::Small(us)) if width == 4 || width == 8 => {
            let mut raw = Vec::with_capacity(us.len() * width as usize);
            for &x in us {
                if width == 4 {
                    raw.extend_from_slice(&u32::try_from(x).map_err(|_| too_wide())?.to_le_bytes());
                } else {
                    raw.extend_from_slice(&x.to_le_bytes());
                }
            }
            (width, raw)
        }
        Bucket::Us(us) => match (width, us) {
            (4, UList::Big(us)) => {
                let mut raw = Vec::with_capacity(us.len() * 4);
                for u in us {
                    let x = u.as_u64().and_then(|x| u32::try_from(x).ok()).ok_or_else(too_wide)?;
                    raw.extend_from_slice(&x.to_le_bytes());
                }
                (4, raw)
            }
            (8, UList::Big(us)) => {
                let mut raw = Vec::with_capacity(us.len() * 8);
                for u in us {
                    raw.extend_from_slice(&u.as_u64().ok_or_else(too_wide)?.to_le_bytes());
                }
                (8, raw)
            }
            (_, us) => (0, encode_big_posting(&us.to_nats())?),
        },
    })
}

/// Result of writing one index tree.
pub(crate) struct Persisted {
    pub off: u64,
    pub bid: u64,
    pub bytes: usize,
    pub keys: u64,
    /// references without the FULLTEXT document lengths
    pub refs: u64,
    /// FULLTEXT: documents (entries of the document-length key)
    pub docs: Option<u64>,
}

/// Positional postings per query term (shared, without copying).
pub type TermPostingsOwned = Vec<(String, Arc<Vec<FtPosting>>)>;

impl Db {
    // ------------------------------------------------------------------
    // SECONDARY blocks: header on open, directory on demand
    // ------------------------------------------------------------------

    /// `_register_secondary_header`: root of the key tree.
    pub(crate) fn parse_secondary_directory(&mut self, info: &BlockInfo) -> Result<()> {
        let lane = crate::address::lane_from_flags(info.flags);
        let payload_off = info.offset + BLOCK_HEADER_SIZE as u64;
        let raw = self.store.read_upto(payload_off, 128.min(info.payload_len as usize))?;
        if !raw.starts_with(secindex::ROOT_TAG) {
            return Err(Error::Corruption(format!("SECONDARY block {}: wrong tag", info.block_id)));
        }
        let h = secindex::parse_root_head(&raw)?;
        let (index_id, source_bid, entry_count, height) = (h.index_id, h.source_bid, h.entry_count, h.height);
        let meta = SecondaryMeta {
            index_id,
            lane,
            source_block_id: source_bid,
            entry_count,
            height,
            block_id: info.block_id,
            block_offset: info.offset,
            doclen_segments: std::sync::OnceLock::new(),
        };
        self.secondary_blocks.insert((index_id, lane), meta);
        self.secondary_source_block.insert((index_id, lane), source_bid);
        self.cache().postings.retain(|k| match k {
            PostKey::Us(i, l, _) | PostKey::Ft(i, l, _) | PostKey::DocLens(i, l, _) | PostKey::Ov(i, l, _, _) => !(*i == index_id && *l == lane),
        });
        // AP9: overlays of sources no index of the realm lane uses any more
        if let Some(realm) = self.index_defs.get(&index_id).map(|i| i.realm_id.clone()) {
            self.ixm_drop_unused_overlays(&realm, lane);
        }
        Ok(())
    }

    /// `_ensure_secondary_directory`
    /// All keys of an index as a directory (`_ensure_secondary_directory`;
    /// a debug view whose memory grows with the keys of the index).
    pub fn secondary_directory(&self, index_id: u64, lane: Lane) -> Result<Arc<SecondaryDir>> {
        self.check_open()?;
        Ok(Arc::new(self.sec_full_directory(index_id, lane)?))
    }

    pub fn secondary_source_block_id(&self, index_id: u64, lane: Lane) -> Option<u64> {
        self.secondary_source_block.get(&(index_id, lane)).copied()
    }

    /// Number of directory entries of the current SECONDARY block.
    /// Document length segments of a FULLTEXT index are not counted (the
    /// summary under `FULLTEXT_DOCLEN_KEY` is, like the list of 0.28.0).
    pub(crate) fn secondary_entry_count(&self, index_id: u64, lane: Lane) -> usize {
        let Some(m) = self.secondary_blocks.get(&(index_id, lane)) else {
            return 0;
        };
        let fulltext = self.index_defs.get(&index_id).is_some_and(|i| i.kind == IndexKind::Fulltext);
        let segs = if fulltext {
            // RB3-06: a read error is not remembered as a count (statistics
            // only; `verify(deep)` reports the damage)
            match m.doclen_segments.get() {
                Some(n) => *n,
                None => match self.fulltext_internal_entries(index_id, lane) {
                    Ok(n) => *m.doclen_segments.get_or_init(|| n.saturating_sub(1)),
                    Err(_) => 0,
                },
            }
        } else {
            0
        };
        m.entry_count.saturating_sub(segs) as usize
    }

    /// Whether the top page of the key tree is cached.
    pub(crate) fn secondary_directory_loaded(&self, index_id: u64, lane: Lane) -> bool {
        self.sec_top_cached(index_id, lane)
    }

    // ------------------------------------------------------------------
    // Build (`_build_maps_fast`, `_persist_secondary`, `build_indexes`)
    // ------------------------------------------------------------------

    fn append_index_document(map: &mut ValueMap, idef: &IndexDef, kg: &KeyGen, u: &Nat, value: &Val) -> Result<()> {
        match idef.kind {
            IndexKind::Value => {
                if let Bucket::Us(v) = map.bucket(value.clone(), || Bucket::Us(UList::default())) {
                    v.push(u)?;
                }
                map.bytes += REF_COST;
            }
            IndexKind::Fulltext => {
                let tokens = textindex::simple_tokens(value, kg.casefold);
                if let Bucket::Doc(v) = map.bucket_str(FULLTEXT_DOCLEN_KEY, || Bucket::Doc(Vec::new())) {
                    v.push((u.clone(), tokens.len() as u64));
                }
                map.bytes += 48;
                let mut order: Vec<&str> = Vec::new();
                let mut pos: HashMap<&str, Vec<u64>> = HashMap::new();
                for (i, t) in tokens.iter().enumerate() {
                    pos.entry(t.as_str())
                        .or_insert_with(|| {
                            order.push(t.as_str());
                            Vec::new()
                        })
                        .push(i as u64);
                }
                for t in order {
                    let p = pos.remove(t).unwrap();
                    let cost = 64 + p.len() * 8;
                    if let Bucket::Ft(v) = map.bucket_str(t, || Bucket::Ft(Vec::new())) {
                        v.push((u.clone(), p));
                    }
                    map.bytes += cost;
                }
            }
            IndexKind::Token | IndexKind::Trigram => {
                if value.is_none() {
                    return Ok(());
                }
                // RUN9 V4: n-grams as slices of the normalized text (no
                // string per position); keys are created only for new ones
                let (owned, text);
                let keys: Vec<&str> = if idef.kind == IndexKind::Token {
                    owned = textindex::simple_tokens(value, kg.casefold);
                    owned.iter().map(String::as_str).collect()
                } else {
                    text = textindex::normalize_text(value, kg.casefold);
                    textindex::ngram_slices(&text, kg.ngram)
                };
                let mut seen: HashSet<&str> = HashSet::with_capacity(keys.len());
                for &k in &keys {
                    if seen.insert(k) {
                        if let Bucket::Us(v) = map.bucket_str(k, || Bucket::Us(UList::default())) {
                            v.push(u)?;
                        }
                        map.bytes += REF_COST;
                    }
                }
            }
        }
        Ok(())
    }

    /// Value lists of all selected indexes (`_build_maps_fast`):
    /// BASE fields via the dense path, extensions directly from sparse blocks.
    fn build_maps(&self, selected: &[IndexDef], spill: &mut Spill) -> Result<(IndexMaps, u64, u64)> {
        let mut maps: HashMap<(u64, Lane), ValueMap> = HashMap::new();
        let mut kgs: HashMap<u64, KeyGen> = HashMap::new();
        for idx in selected {
            kgs.insert(idx.index_id, KeyGen::of(idx)?);
            for lane in [0u8, 1] {
                maps.insert((idx.index_id, lane), ValueMap::default());
            }
        }
        let mut by_realm: Vec<(Nat, Vec<&IndexDef>)> = Vec::new();
        for idx in selected {
            match by_realm.iter_mut().find(|(r, _)| r == &idx.realm_id) {
                Some((_, v)) => v.push(idx),
                None => by_realm.push((idx.realm_id.clone(), vec![idx])),
            }
        }
        let (mut dense_scanned, mut sparse_groups) = (0u64, 0u64);
        for (realm, idxs) in by_realm {
            let base_vs: HashSet<Nat> =
                self.current_base_shape(&realm).map(|b| b.fields.iter().map(|f| f.v.clone()).collect()).unwrap_or_default();
            let base_idxs: Vec<&IndexDef> = idxs.iter().copied().filter(|i| base_vs.contains(i.field_v())).collect();
            let ext_idxs: Vec<&IndexDef> = idxs.iter().copied().filter(|i| !base_vs.contains(i.field_v())).collect();
            // F010: cells of COLUMN/PACKED/CUSTOM layouts give v another
            // meaning; only u routed to a CLASSIC layout (or to no layout)
            // are indexed. Pure CLASSIC realms skip the per-u check.
            let mixed = self.realm_layouts(&realm).iter().any(|l| l.kind != LayoutKind::Classic);
            let foreign = |u: &Nat| mixed && self.layout_at(&realm, u).is_some_and(|l| l.kind != LayoutKind::Classic);
            for lane in [0u8, 1] {
                let key = (realm.clone(), lane);
                // Sparse states of indexed BASE fields overlay the
                // dense value as when reading (`_sparse_updates_for_u`: all
                // sparse blocks in block order). Pure sparse records are indexed
                // as well.
                let base_vs_wanted: HashSet<&Nat> = base_idxs.iter().map(|i| i.field_v()).collect();
                // (state, block id): F1 compares the id with the DENSE record
                let mut overlay: HashMap<(Nat, Nat), (SVal, u64)> = HashMap::new();
                if !base_idxs.is_empty() {
                    if let Some(sidx) = self.sparse.get(&key) {
                        for seg in &sidx.idx.collect_block_order(self)? {
                            let bi = self.sparse_block_index(seg)?;
                            let payload = self.block_payload_once(seg.block_offset)?;
                            for (u, abs, len) in bi.iter() {
                                let rel = abs.checked_sub(seg.payload_offset).ok_or_else(|| Error::Corruption(format!("SPARSE block {}: group directory outside the block", seg.block_id)))? as usize;
                                for (v, meta) in self.decode_sparse_record(&realm, u, payload.get(rel..rel + *len as usize).ok_or_else(|| Error::Corruption(format!("SPARSE block {}: record outside the block", seg.block_id)))?)? {
                                    if base_vs_wanted.contains(&v) {
                                        overlay.insert((u.clone(), v), (meta.value, seg.block_id));
                                    }
                                }
                            }
                        }
                    }
                }
                if !base_idxs.is_empty() {
                    if let Some(didx) = self.dense.get(&key).filter(|d| !d.idx.is_empty()) {
                        let all_segs = didx.idx.collect_by_start(self)?;
                        let by_start: Vec<&super::DenseSegment> = all_segs.iter().collect();
                        let mut dense_overlap = false;
                        let mut prev_end: Option<&Nat> = None;
                        for seg in &by_start {
                            if prev_end.is_some_and(|pe| seg.u_start <= *pe) {
                                dense_overlap = true;
                                break;
                            }
                            if prev_end.is_none_or(|pe| seg.u_end > *pe) {
                                prev_end = Some(&seg.u_end);
                            }
                        }
                        let newest_wins = dense_overlap || self.has_deletes(&key);
                        let segs: Vec<&super::DenseSegment> = if newest_wins {
                            let mut s: Vec<_> = all_segs.iter().collect();
                            s.sort_by_key(|x| std::cmp::Reverse(x.block_id));
                            s
                        } else {
                            by_start
                        };
                        let dedup = overlapping(segs.iter().map(|s| (&s.u_start, &s.u_end)));
                        let mut seen: HashSet<Nat> = HashSet::new();
                        for (si, seg) in segs.into_iter().enumerate() {
                            let shape = self.shape_of(seg.shape_id)?;
                            let mut wanted_here: Vec<(usize, Vec<&IndexDef>)> = Vec::new();
                            for (fi, f) in shape.fields.iter().enumerate() {
                                let ids: Vec<&IndexDef> = base_idxs.iter().copied().filter(|i| i.field_v() == &f.v).collect();
                                if !ids.is_empty() {
                                    wanted_here.push((fi, ids));
                                }
                            }
                            if wanted_here.is_empty() {
                                // a newer record of a shape without an indexed
                                // field still hides the older records of its u
                                // (newest segment first): mark them, or an older
                                // segment would index a field this record
                                // dropped (review 2026-11-09, T2 finding 2).
                                // A DELETE newer than this segment hides it
                                // in turn, then an older record may show.
                                if newest_wins && dedup[si] {
                                    for i in 0..seg.count {
                                        let u = seg.u_start.add_u64(i);
                                        if self.delete_block_id(&key, &u)?.is_some_and(|d| d > seg.block_id) {
                                            continue;
                                        }
                                        seen.insert(u);
                                    }
                                }
                                continue;
                            }
                            let payload = self.block_payload_once(seg.block_offset)?;
                            // RD-01: the block must be the one of the entry
                            self.check_dense_prefix(seg, &payload)?;
                            let toff = (seg.offsets_offset - seg.payload_offset) as usize;
                            let rbase = (seg.records_offset - seg.payload_offset) as usize;
                            let max_fi = wanted_here.last().unwrap().0;
                            // RA2-03: budget check every 256 records or 64 KiB of records
                            let mut pending = 0usize;
                            for i in 0..seg.count as usize {
                                if i & 255 == 255 || pending >= CHECK_BYTES {
                                    spill.check(&mut maps)?;
                                    pending = 0;
                                }
                                let u = seg.u_start.add_u64(i as u64);
                                if newest_wins {
                                    if dedup[si] && !seen.insert(u.clone()) {
                                        continue;
                                    }
                                    if self.delete_block_id(&key, &u)?.is_some_and(|d| d > seg.block_id) {
                                        continue;
                                    }
                                }
                                dense_scanned += 1;
                                if foreign(&u) {
                                    continue;
                                }
                                let s = u32::from_le_bytes(payload[toff + i * 4..toff + i * 4 + 4].try_into().unwrap()) as usize;
                                let e = u32::from_le_bytes(payload[toff + i * 4 + 4..toff + i * 4 + 8].try_into().unwrap()) as usize;
                                if e < s || rbase + e > payload.len() {
                                    return Err(Error::Corruption("DATA record offset invalid".into()));
                                }
                                let rec = &payload[rbase + s..rbase + e];
                                pending += rec.len();
                                let mut p = 0usize;
                                let mut wi = 0;
                                for (fi, f) in shape.fields.iter().enumerate().take(max_fi + 1) {
                                    if wi < wanted_here.len() && wanted_here[wi].0 == fi {
                                        let mut val = decode_value(f, rec, &mut p)?;
                                        if !overlay.is_empty() {
                                            match overlay.get(&(u.clone(), f.v.clone())) {
                                                // F1: older than the record → the record's value
                                                Some((_, bid)) if *bid < seg.block_id => {}
                                                Some((SVal::Tomb, _)) => {
                                                    wi += 1;
                                                    continue;
                                                }
                                                Some((SVal::V(x), _)) => val = x.clone(),
                                                None => {}
                                            }
                                        }
                                        for idef in &wanted_here[wi].1 {
                                            Self::append_index_document(maps.get_mut(&(idef.index_id, lane)).unwrap(), idef, &kgs[&idef.index_id], &u, &val)?;
                                        }
                                        wi += 1;
                                    } else {
                                        skip_value(f, rec, &mut p)?;
                                    }
                                }
                            }
                            spill.check(&mut maps)?;
                        }
                    }
                }
                if !overlay.is_empty() {
                    // cells written alone where no live dense record holds
                    // the field: pure sparse records, and BASE fields the
                    // stored shape of the record lacks
                    let mut extra: Vec<(&(Nat, Nat), &Val)> = Vec::new();
                    for (k, (st, _)) in overlay.iter() {
                        let Some(v) = st.val() else { continue };
                        if foreign(&k.0) {
                            continue;
                        }
                        let held = match self.resolve_segment(&realm, &k.0, lane)? {
                            Some(seg) => self.shape_of(seg.shape_id)?.by_v(&k.1).is_some(),
                            None => false,
                        };
                        if !held {
                            extra.push((k, v));
                        }
                    }
                    extra.sort_by(|a, b| a.0.cmp(b.0));
                    for (n, ((u, v), val)) in extra.into_iter().enumerate() {
                        for idef in base_idxs.iter().filter(|i| i.field_v() == v) {
                            Self::append_index_document(maps.get_mut(&(idef.index_id, lane)).unwrap(), idef, &kgs[&idef.index_id], u, val)?;
                        }
                        if n & 255 == 255 {
                            spill.check(&mut maps)?;
                        }
                    }
                }
                if !ext_idxs.is_empty() {
                    if let Some(sidx) = self.sparse.get(&key).filter(|s| !s.idx.is_empty()) {
                        let mut segs = sidx.idx.collect_block_order(self)?;
                        segs.reverse();
                        let wanted: HashSet<&Nat> = ext_idxs.iter().map(|i| i.field_v()).collect();
                        let max_wanted: Nat = wanted.iter().map(|v| (*v).clone()).max().unwrap_or_default();
                        let dedup = overlapping(segs.iter().map(|s| (&s.u_start, &s.u_end)));
                        let mut seen_cells: HashSet<(Nat, Nat)> = HashSet::new();
                        for (si, seg) in segs.iter().enumerate() {
                            let bi = self.sparse_block_index(seg)?;
                            let payload = self.block_payload_once(seg.block_offset)?;
                            let mut pending = 0usize;
                            for (gi, (u, abs, len)) in bi.iter().enumerate() {
                                sparse_groups += 1;
                                // RA2-03/RX-01: every 256 groups or 64 KiB of group bytes
                                pending += *len as usize;
                                if gi & 255 == 255 || pending >= CHECK_BYTES {
                                    spill.check(&mut maps)?;
                                    pending = 0;
                                }
                                let rel = abs.checked_sub(seg.payload_offset).ok_or_else(|| Error::Corruption(format!("SPARSE block {}: group directory outside the block", seg.block_id)))? as usize;
                                if foreign(u) {
                                    continue;
                                }
                                let vals = self.decode_sparse_selected(&realm, u, payload.get(rel..rel + *len as usize).ok_or_else(|| Error::Corruption(format!("SPARSE block {}: record outside the block", seg.block_id)))?, &max_wanted)?;
                                // F026: a DELETE block hides only the dense
                                // record; extension cells a delete left alive
                                // (e.g. `delete_area` over the BASE fields)
                                // stay visible and therefore stay indexed.
                                for (v, meta) in vals {
                                    if !wanted.contains(&v) {
                                        continue;
                                    }
                                    if dedup[si] && !seen_cells.insert((u.clone(), v.clone())) {
                                        continue;
                                    }
                                    let SVal::V(val) = meta.value else { continue };
                                    for idef in ext_idxs.iter().filter(|i| i.field_v() == &v) {
                                        Self::append_index_document(maps.get_mut(&(idef.index_id, lane)).unwrap(), idef, &kgs[&idef.index_id], u, &val)?;
                                    }
                                }
                            }
                            spill.check(&mut maps)?;
                        }
                    }
                }
            }
        }
        // Persisted postings must be sorted by document u.
        for vm in maps.values_mut() {
            sort_buckets(vm);
        }
        Ok((maps, dense_scanned, sparse_groups))
    }

    /// Writes the key tree of one index: the in-memory keys merged with the
    /// spilled runs, key by key (lists of one key merged by document u).
    fn persist_secondary(&mut self, idx: &IndexDef, value_map: &ValueMap, runs: &[super::idxspill::SpillRun], source_bid: u64, lane: Lane, lane_max_u: &Nat) -> Result<Persisted> {
        // RUN¹⁰: checkpoint of the RAM limiter per MiB of merged postings
        let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::IndexBuild);
        // F009: the posting width covers the largest document u actually
        // indexed (sparse u may exceed the dense maximum of the lane); the
        // lane maximum keeps the width of the reference for ordinary data.
        let mut max_u = lane_max_u.clone();
        if let Some(m) = &value_map.spilled_max_u {
            if m > &max_u {
                max_u = m.clone();
            }
        }
        for (_, b) in &value_map.items {
            if let Bucket::Us(us) = b {
                if let Some(last) = us.last() {
                    if last > max_u {
                        max_u = last;
                    }
                }
            }
        }
        let width = width_for(&max_u);
        // key order of the tree (references only; the map holds the keys)
        let mut items: Vec<&(Val, Bucket)> = value_map.items.iter().collect();
        items.sort_by(|a, b| secindex::cmp_vals(&a.0, &b.0));
        let window = self.index_build_budget() / 4 / runs.len().max(1);
        let mut readers: Vec<super::idxspill::SpillReader> =
            runs.iter().cloned().map(|r| super::idxspill::SpillReader::open(r, window)).collect::<Result<_>>()?;
        let mut mi = 0usize;
        let doc_key = secindex::encode_key_vec(&Val::str(FULLTEXT_DOCLEN_KEY))?;
        let mut w = secindex::SecWriter::new(idx.index_id, lane);
        let (mut keys, mut refs, mut docs) = (0u64, 0u64, None);
        // RA2-06: k-way merge over a heap (smallest key, then source: the
        // in-memory map first, runs in spill order), O(log runs) per key
        let mut heap: BinaryHeap<MergeHead> = BinaryHeap::new();
        if let Some(kv) = items.first() {
            heap.push(MergeHead { key: secindex::encode_key_vec(&kv.0)?, src: 0 });
        }
        for (i, r) in readers.iter().enumerate() {
            if let Some(h) = &r.head {
                heap.push(MergeHead { key: h.key.clone(), src: i + 1 });
            }
        }
        while let Some(top) = heap.pop() {
            let key = top.key;
            let mut same = vec![top.src];
            while heap.peek().is_some_and(|h| secindex::cmp_keys(&h.key, &key) == Ordering::Equal) {
                same.push(heap.pop().unwrap().src);
            }
            let mut from_runs: Vec<usize> = Vec::new();
            let mut mem_bucket: Option<&Bucket> = None;
            for i in same {
                if i == 0 {
                    mem_bucket = Some(&items[mi].1);
                    mi += 1;
                    if let Some(kv) = items.get(mi) {
                        heap.push(MergeHead { key: secindex::encode_key_vec(&kv.0)?, src: 0 });
                    }
                } else {
                    from_runs.push(i - 1);
                }
            }
            let mut spilled: Vec<Bucket> = Vec::new();
            if !from_runs.is_empty() {
                // RUN9 V4: the entries of the runs are read one by one and
                // merged straight into the encoder; a key spread over many
                // runs is never decoded as a whole (same bytes as
                // merge_buckets + encode_bucket)
                let done = self.persist_merged(&mut w, &key, &mut readers, &from_runs, mem_bucket, key == doc_key, width)?;
                if done.is_none() {
                    for &r in &from_runs {
                        spilled.push(readers[r].take_bucket()?);
                    }
                }
                for &r in &from_runs {
                    readers[r].next_record()?;
                    if let Some(h) = &readers[r].head {
                        heap.push(MergeHead { key: h.key.clone(), src: r + 1 });
                    }
                }
                if let Some(done) = done {
                    keys += 1;
                    match done {
                        Merged::Docs(n) => docs = Some(n),
                        Merged::Refs(n) => refs += n,
                    }
                    continue;
                }
            }
            let merged;
            let bucket: &Bucket = match (mem_bucket, spilled.is_empty()) {
                (Some(b), true) => b,
                (mem, _) => {
                    spilled.extend(mem.cloned());
                    merged = super::idxspill::merge_buckets(spilled)?;
                    &merged
                }
            };
            let count = bucket.len() as u64;
            keys += 1;
            if let (Bucket::Doc(entries), true) = (bucket, key == doc_key) {
                // 0.28.2: summary + segments of about 4 KiB (`textindex.rs`)
                docs = Some(count);
                let segs = textindex::doclen_segments(entries, secindex::page_bytes().min(textindex::DOCLEN_SEGMENT_BYTES));
                let tokens: u64 = entries.iter().map(|e| e.1).sum();
                let summary = textindex::encode_doclen_summary(count, tokens, segs.len() as u64);
                w.add(self, key, count, PAYLOAD_FULLTEXT_DOCLEN_SUMMARY, &summary)?;
                for seg in segs {
                    let last = &seg.last().expect("segments are not empty").0;
                    let k = secindex::encode_key_vec(&Val::Str(textindex::doclen_segment_key(last)))?;
                    let raw = textindex::encode_fulltext_doc_lengths(seg)?;
                    w.add(self, k, seg.len() as u64, PAYLOAD_FULLTEXT_DOCLEN_SEGMENT, &raw)?;
                }
                continue;
            }
            let (code, raw) = encode_bucket(bucket, width)?;
            meter.grow(raw.len() as u64)?;
            refs += count;
            w.add(self, key, count, code, &raw)?;
        }
        let (root, pages, page_blocks) = w.finish(self, source_bid)?;
        self.dir.unlisted_blocks += page_blocks;
        let (off, bid) = self.append_block(BT_SECONDARY, &root, lane_flags(lane))?;
        let info = self.read_header_at(off)?;
        self.parse_secondary_directory(&info)?;
        Ok(Persisted { off, bid, bytes: pages as usize + root.len(), keys, refs, docs })
    }

    /// RUN9 V4: writes the key `key` whose posting is spread over the
    /// current records of the runs `runs` (in spill order) and the
    /// in-memory part `mem`, merged entry by entry into the encoder: only
    /// one entry per source is decoded at a time. `None`: a small key (below
    /// `STREAM_MERGE_MIN` entries) or a case the merge does not cover (u
    /// wider than 64 bit, document lengths under another key); nothing was
    /// read then, the caller merges as before.
    #[allow(clippy::too_many_arguments)]
    fn persist_merged(
        &mut self,
        w: &mut secindex::SecWriter,
        key: &[u8],
        readers: &mut [super::idxspill::SpillReader],
        runs: &[usize],
        mem: Option<&Bucket>,
        doc_key: bool,
        width: u8,
    ) -> Result<Option<Merged>> {
        use super::idxspill::RunEntry;
        let mem_kind = mem.map(|b| match b {
            Bucket::Us(_) => 0u8,
            Bucket::Ft(_) => 1,
            Bucket::Doc(_) => 2,
        });
        let heads: Vec<(u8, u64)> = runs.iter().map(|&r| readers[r].head.as_ref().map(|h| (h.kind, h.left)).expect("run record")).collect();
        let kind = heads[0].0;
        if heads.iter().any(|h| h.0 != kind) || mem_kind.is_some_and(|k| k != kind) {
            return Err(Error::Value("index build: one key with different posting kinds".into()));
        }
        if (kind == 0 && width != 4 && width != 8) || (kind == 2 && !doc_key) {
            return Ok(None);
        }
        let count = heads.iter().map(|h| h.1).sum::<u64>() + mem.map_or(0, |b| b.len() as u64);
        // a small key is cheaper decoded and sorted as before (a few MiB at
        // most); only large ones are merged entry by entry
        if count < stream_merge_min() {
            return Ok(None);
        }
        // sources: the runs, then the in-memory part (the order of the
        // concatenation merge_buckets sorted stably)
        let mut mi = 0usize;
        let mut next = |s: usize| -> Result<Option<RunEntry>> {
            if s < runs.len() {
                return readers[runs[s]].next_entry();
            }
            let e = match mem {
                Some(Bucket::Us(UList::Tiny(v))) => v.get(mi).map(|x| RunEntry::U(Nat::S(*x as u64))),
                Some(Bucket::Us(UList::Small(v))) => v.get(mi).map(|x| RunEntry::U(Nat::S(*x))),
                Some(Bucket::Us(UList::Big(v))) => v.get(mi).map(|x| RunEntry::U(x.clone())),
                Some(Bucket::Ft(v)) => v.get(mi).map(|(u, p)| RunEntry::Ft(u.clone(), p.clone())),
                Some(Bucket::Doc(v)) => v.get(mi).map(|(u, d)| RunEntry::Doc(u.clone(), *d)),
                None => None,
            };
            mi += 1;
            Ok(e)
        };
        let sources = runs.len() + mem.is_some() as usize;
        let u_of = |e: &RunEntry| match e {
            RunEntry::U(u) | RunEntry::Ft(u, _) | RunEntry::Doc(u, _) => u.clone(),
        };
        match kind {
            0 => {
                let too_wide = || Error::Value("posting u exceeds the posting width".into());
                let mut raw: Vec<u8> = Vec::new();
                raw.try_reserve_exact(count as usize * width as usize).map_err(|e| Error::Memory(format!("index build: {e}")))?;
                stream_merge(sources, &mut next, u_of, |e| {
                    let RunEntry::U(u) = e else { unreachable!() };
                    let x = u.as_u64().ok_or_else(too_wide)?;
                    if width == 4 {
                        raw.extend_from_slice(&u32::try_from(x).map_err(|_| too_wide())?.to_le_bytes());
                    } else {
                        raw.extend_from_slice(&x.to_le_bytes());
                    }
                    Ok(())
                })?;
                w.add(self, key.to_vec(), count, width, &raw)?;
                Ok(Some(Merged::Refs(count)))
            }
            1 => {
                let mut enc = textindex::FtPostingEncoder::new(count);
                stream_merge(sources, &mut next, u_of, |e| {
                    let RunEntry::Ft(u, pos) = e else { unreachable!() };
                    enc.push(&u, &pos)
                })?;
                w.add(self, key.to_vec(), count, PAYLOAD_FULLTEXT_POSITIONS, &enc.finish())?;
                Ok(Some(Merged::Refs(count)))
            }
            _ => {
                // 0.28.2: summary + segments of about 4 KiB; the summary
                // names the segment count, so the encoded segments (a few
                // bytes per document) are kept until the summary is written
                let target = secindex::page_bytes().min(textindex::DOCLEN_SEGMENT_BYTES);
                let mut cut = textindex::DoclenSegmenter::new(target);
                let (mut seg, mut tokens) = (Vec::new(), 0u64);
                let mut segs: Vec<(Vec<u8>, u64, Vec<u8>)> = Vec::new();
                let close = |seg: &mut Vec<(Nat, u64)>, segs: &mut Vec<(Vec<u8>, u64, Vec<u8>)>| -> Result<()> {
                    let last = &seg.last().expect("segments are not empty").0;
                    let sk = secindex::encode_key_vec(&Val::Str(textindex::doclen_segment_key(last)))?;
                    segs.push((sk, seg.len() as u64, textindex::encode_fulltext_doc_lengths(seg)?));
                    seg.clear();
                    Ok(())
                };
                stream_merge(sources, &mut next, u_of, |e| {
                    let RunEntry::Doc(u, dl) = e else { unreachable!() };
                    tokens += dl;
                    let end = cut.push(&u, dl);
                    seg.push((u, dl));
                    if end {
                        close(&mut seg, &mut segs)?;
                    }
                    Ok(())
                })?;
                if !seg.is_empty() {
                    close(&mut seg, &mut segs)?;
                }
                let summary = textindex::encode_doclen_summary(count, tokens, segs.len() as u64);
                w.add(self, key.to_vec(), count, PAYLOAD_FULLTEXT_DOCLEN_SUMMARY, &summary)?;
                for (sk, n, raw) in segs {
                    w.add(self, sk, n, PAYLOAD_FULLTEXT_DOCLEN_SEGMENT, &raw)?;
                }
                Ok(Some(Merged::Docs(count)))
            }
        }
    }

    /// Rebuilds indexes (`build_indexes`).
    ///
    /// A lane with any data blocks (even only sparse or
    /// deletes) always receives a SECONDARY block, so that indexes of
    /// pure sparse realms also become fresh.
    /// Memory budget of the index build: beyond it the collected keys are
    /// spilled as sorted runs (`idxspill.rs`).
    fn index_build_budget(&self) -> usize {
        self.index_build_memory_override.map(|b| b as usize).unwrap_or(self.config.index_build_memory_bytes)
    }

    pub fn build_indexes(&mut self, index_ids: Option<&[u64]>, commit: bool) -> Result<Val> {
        self.check_open()?;
        let selected: Vec<IndexDef> = match index_ids {
            None => self.index_defs.values().cloned().collect(),
            Some(ids) => ids
                .iter()
                .map(|k| self.index_defs.get(k).cloned().ok_or_else(|| Error::Key(k.to_string())))
                .collect::<Result<_>>()?,
        };
        if selected.is_empty() {
            if commit {
                self.commit()?;
            }
            return Ok(vmap! {"scan_s" => 0.0, "persist_s" => 0.0, "total_s" => 0.0, "records_scanned" => 0i64, "indexes" => Val::List(vec![])});
        }
        let passes = vec![selected.clone()];
        let mut spill = Spill {
            budget: self.index_build_budget(),
            dirs: vec![std::env::temp_dir(), crate::platform::parent_dir(&self.path)],
            file: None,
            runs: HashMap::new(),
            spills: 0,
            bytes: 0,
            meter: crate::limiter::Meter::new(crate::limiter::JobKind::Elastic, crate::limiter::JobPath::IndexBuild),
            held: 0,
        };
        let (mut scan_s, mut persist_s) = (0.0f64, 0.0f64);
        let (mut dense_scanned, mut sparse_groups) = (0u64, 0u64);
        let mut scanned_realms: HashSet<Nat> = HashSet::new();
        let mut stats = Vec::new();
        for pass in &passes {
            let t0 = Instant::now();
            let (maps, pass_dense, pass_sparse) = self.build_maps(pass, &mut spill)?;
            scan_s += t0.elapsed().as_secs_f64();
            // a realm scanned again in a later pass is not counted twice
            let new_realm = pass.iter().fold(false, |acc, i| scanned_realms.insert(i.realm_id.clone()) | acc);
            if new_realm {
                dense_scanned += pass_dense;
                sparse_groups += pass_sparse;
            }
            let t1 = Instant::now();
            for idx in pass {
                for lane in [0u8, 1] {
                    let rk = (idx.realm_id.clone(), lane);
                    let sk = (idx.index_id, lane);
                    let has_data = self.data_max_block_id_by_lane.get(&rk).copied().unwrap_or(0) > 0;
                    if !has_data && !self.secondary_blocks.contains_key(&sk) {
                        continue;
                    }
                    let source = self.data_max_block_id_by_lane.get(&rk).copied().unwrap_or(0);
                    let max_u = self.max_u.get(&rk).cloned().unwrap_or_default();
                    let vm = &maps[&sk];
                    let runs = spill.runs.remove(&sk).unwrap_or_default();
                    // N2 (RUN8): a failed tree leaves no unreferenced pages behind
                    let p = self.write_scope(|db| db.persist_secondary(idx, vm, &runs, source, lane, &max_u))?;
                    let (off, bid, nbytes) = (p.off, p.bid, p.bytes);
                    let mut row = vmap! {
                        "index_id" => idx.index_id as i64, "name" => idx.name.as_str(), "kind" => idx.kind.name(),
                        "lane" => lane as i64, "lane_name" => lane_name(lane),
                    };
                    if idx.kind == IndexKind::Fulltext {
                        let docs = p.docs.unwrap_or(0);
                        let has_doc = p.docs.is_some();
                        row.set("distinct_values", Val::Int(p.keys as i64 - has_doc as i64));
                        row.set("u_references", Val::Int(p.refs as i64));
                        row.set("block_offset", Val::Int(off as i64));
                        row.set("block_id", Val::Int(bid as i64));
                        row.set("payload_bytes", Val::Int(nbytes as i64));
                        row.set("documents", Val::Int(docs as i64));
                    } else {
                        row.set("distinct_values", Val::Int(p.keys as i64));
                        row.set("u_references", Val::Int(p.refs as i64));
                        row.set("block_offset", Val::Int(off as i64));
                        row.set("block_id", Val::Int(bid as i64));
                        row.set("payload_bytes", Val::Int(nbytes as i64));
                    }
                    stats.push(row);
                }
            }
            drop(maps);
            persist_s += t1.elapsed().as_secs_f64();
        }
        self.store.flush()?;
        self.cache().postings.clear();
        if commit {
            self.commit()?;
        }
        Ok(vmap! {
            "scan_s" => scan_s, "persist_s" => persist_s, "total_s" => scan_s + persist_s,
            "records_scanned" => (dense_scanned + sparse_groups) as i64,
            "scan_detail" => vmap! {"dense_records_scanned" => dense_scanned as i64, "sparse_groups_scanned" => sparse_groups as i64},
            "passes" => passes.len() as i64,
            "spill_rounds" => spill.spills as i64, "spill_bytes" => spill.bytes as i64,
            "indexes" => Val::List(stats),
        })
    }

    pub fn index_is_fresh(&self, index_id: u64, lane: Lane) -> bool {
        let Some(idx) = self.index_defs.get(&index_id) else {
            return false;
        };
        let data_bid = self.data_max_block_id_by_lane.get(&(idx.realm_id.clone(), lane)).copied().unwrap_or(0);
        if data_bid == 0 {
            return true;
        }
        self.secondary_source_block.get(&(index_id, lane)).map(|s| *s >= data_bid).unwrap_or(false)
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    /// Entry of the key `value` (key equality as in a Python dict).
    fn lookup_entry(&self, index_id: u64, lane: Lane, value: &Val) -> Result<Option<SecEntry>> {
        self.sec_lookup(index_id, lane, &secindex::encode_key_vec(value)?)
    }

    /// Posting count of the key `value` (0 if absent).
    fn key_count(&self, index_id: u64, lane: Lane, value: &Val) -> Result<u64> {
        Ok(self.lookup_entry(index_id, lane, value)?.map(|e| e.raw_count).unwrap_or(0))
    }

    /// Posting of an entry through the posting cache (point lookups; range
    /// walks decode without caching, see `posting_for_condition`).
    fn entry_posting(&self, index_id: u64, lane: Lane, e: &SecEntry) -> Result<Arc<Vec<Nat>>> {
        let ck = PostKey::Us(index_id, lane, ValKey(e.value()?));
        if let Some(PostVal::Us(c)) = self.cache().postings.get(&ck) {
            return Ok(c);
        }
        let arc = Arc::new(self.sec_posting_us(index_id, lane, e)?);
        self.posting_cache_put(ck, PostVal::Us(arc.clone()));
        Ok(arc)
    }

    /// Inserts into the posting pool (`[cache] posting_cache_bytes`).
    pub(crate) fn posting_cache_put(&self, k: PostKey, v: PostVal) {
        let cost = v.cost();
        let budget = self.config.posting_cache_bytes;
        self.cache().postings.insert(k, v, cost, budget);
    }

    pub(crate) fn load_posting(&self, index_id: u64, value: &Val, lane: Lane) -> Result<Arc<Vec<Nat>>> {
        let ck = PostKey::Us(index_id, lane, ValKey(value.clone()));
        if let Some(PostVal::Us(c)) = self.cache().postings.get(&ck) {
            return Ok(c);
        }
        match self.lookup_entry(index_id, lane, value)? {
            None => Ok(Arc::new(Vec::new())),
            Some(e) => self.entry_posting(index_id, lane, &e),
        }
    }

    fn require_fulltext(&self, index_id: u64) -> Result<&IndexDef> {
        match self.index_defs.get(&index_id) {
            Some(i) if i.kind == IndexKind::Fulltext => Ok(i),
            _ => Err(Error::Schema(format!("Index {index_id} is not a FULLTEXT index"))),
        }
    }

    /// `fulltext_term_posting(index_id, term, lane=…)`
    pub fn fulltext_term_posting(&self, index_id: u64, term: &str, lane: Lane) -> Result<Arc<Vec<FtPosting>>> {
        self.check_open()?;
        self.require_fulltext(index_id)?;
        // AP9: a stale index answers through its overlay
        if let Some(ov) = self.ixm_ft_overlay(index_id, lane)? {
            return self.ov_fulltext_term_posting(&ov, index_id, term, lane);
        }
        self.base_fulltext_term_posting(index_id, term, lane)
    }

    /// Term posting of the key tree (no overlay).
    pub(crate) fn base_fulltext_term_posting(&self, index_id: u64, term: &str, lane: Lane) -> Result<Arc<Vec<FtPosting>>> {
        let ck = PostKey::Ft(index_id, lane, term.to_string());
        if let Some(PostVal::Ft(c)) = self.cache().postings.get(&ck) {
            return Ok(c);
        }
        let out = match self.lookup_entry(index_id, lane, &Val::str(term))? {
            None => Vec::new(),
            Some(r) => {
                if r.width != PAYLOAD_FULLTEXT_POSITIONS {
                    return Err(Error::Corruption("FULLTEXT term has wrong payload codec".into()));
                }
                let raw = self.sec_posting_raw(index_id, lane, &r)?;
                textindex::decode_fulltext_posting(&raw, Some(r.raw_count))
                    .map_err(|e| Error::Corruption(format!("FULLTEXT term payload corrupt: {e}")))?
            }
        };
        let arc = Arc::new(out);
        self.posting_cache_put(ck, PostVal::Ft(arc.clone()));
        Ok(arc)
    }

    /// Document lengths of an index: `Paged` (0.28.2, summary + segments)
    /// or `Whole` (0.28.0/0.28.1: one list, decoded through the posting
    /// pool).
    fn doclen_source(&self, index_id: u64, lane: Lane) -> Result<DocLenSource> {
        let Some(e) = self.lookup_entry(index_id, lane, &Val::str(FULLTEXT_DOCLEN_KEY))? else {
            return Ok(DocLenSource::Empty);
        };
        let raw = self.sec_posting_raw(index_id, lane, &e)?;
        let bad = |x: Error| Error::Corruption(format!("FULLTEXT document lengths corrupt: {x}"));
        match e.width {
            PAYLOAD_FULLTEXT_DOCLEN_SUMMARY => {
                let (docs, tokens, _segments) = textindex::decode_doclen_summary(&raw, Some(e.raw_count)).map_err(bad)?;
                Ok(DocLenSource::Paged { docs, tokens })
            }
            PAYLOAD_FULLTEXT_DOCLENS => {
                let ck = PostKey::DocLens(index_id, lane, Vec::new());
                if let Some(PostVal::DocLens(s)) = self.cache().postings.get(&ck) {
                    return Ok(DocLenSource::Whole(s));
                }
                let seg = Arc::new(textindex::decode_doclen_segment(&raw, Some(e.raw_count)).map_err(bad)?);
                self.posting_cache_put(ck, PostVal::DocLens(seg.clone()));
                Ok(DocLenSource::Whole(seg))
            }
            _ => Err(Error::Corruption("FULLTEXT document lengths have wrong payload codec".into())),
        }
    }

    /// Decoded segment of an entry (through the posting pool unless
    /// `keep` is false); its key must name its last document.
    fn doclen_segment(&self, index_id: u64, lane: Lane, e: &SecEntry, last: &Nat, keep: bool) -> Result<Arc<textindex::DocLenSeg>> {
        let ck = PostKey::DocLens(index_id, lane, e.key.clone());
        if keep {
            if let Some(PostVal::DocLens(s)) = self.cache().postings.get(&ck) {
                return Ok(s);
            }
        }
        if e.width != PAYLOAD_FULLTEXT_DOCLEN_SEGMENT {
            return Err(Error::Corruption("FULLTEXT document length segment has wrong payload codec".into()));
        }
        let raw = self.sec_posting_raw(index_id, lane, e)?;
        let seg = textindex::decode_doclen_segment(&raw, Some(e.raw_count))
            .map_err(|x| Error::Corruption(format!("FULLTEXT document lengths corrupt: {x}")))?;
        if seg.last() != Some(last) {
            return Err(Error::Corruption("FULLTEXT document length segment does not match its key".into()));
        }
        let seg = Arc::new(seg);
        if keep {
            self.posting_cache_put(ck, PostVal::DocLens(seg.clone()));
        }
        Ok(seg)
    }

    /// Visits the document length segments with last u >= `from` (all with
    /// `None`) in u order until `f` returns false.
    fn doclen_scan(&self, index_id: u64, lane: Lane, from: Option<&Nat>, f: &mut dyn FnMut(&SecEntry, Nat) -> Result<bool>) -> Result<()> {
        let start = match from {
            Some(u) => textindex::doclen_segment_key(u),
            None => format!("{FULLTEXT_DOCLEN_KEY}\u{1}"),
        };
        let q = secindex::encode_key_vec(&Val::Str(start))?;
        self.sec_scan(index_id, lane, Some(&q), &mut |e| {
            let key = match e.value()? {
                Val::Str(k) => k,
                _ => return Ok(false),
            };
            match textindex::doclen_segment_last(&key) {
                Some(last) => f(e, last),
                None if key.starts_with(FULLTEXT_DOCLEN_KEY) && key.len() > FULLTEXT_DOCLEN_KEY.len() => {
                    Err(Error::Corruption("FULLTEXT document length segment key invalid".into()))
                }
                None => Ok(false),
            }
        })
    }

    /// Corpus statistics plus the lengths of the documents `us` (sorted,
    /// unique). Reads only the segments that hold them; memory
    /// proportional to `us`, not to the corpus.
    pub fn fulltext_doc_lengths(&self, index_id: u64, lane: Lane, us: &[Nat]) -> Result<FtCorpus> {
        self.check_open()?;
        self.require_fulltext(index_id)?;
        if let Some(ov) = self.ixm_ft_overlay(index_id, lane)? {
            return self.ov_fulltext_doc_lengths(&ov, index_id, lane, us);
        }
        self.base_fulltext_doc_lengths(index_id, lane, us)
    }

    /// Document lengths of the key tree (no overlay).
    pub(crate) fn base_fulltext_doc_lengths(&self, index_id: u64, lane: Lane, us: &[Nat]) -> Result<FtCorpus> {
        let sorted;
        let us = if us.windows(2).all(|w| w[0] < w[1]) {
            us
        } else {
            let mut v = us.to_vec();
            v.sort();
            v.dedup();
            sorted = v;
            &sorted[..]
        };
        let mut out = FtCorpus::default();
        match self.doclen_source(index_id, lane)? {
            DocLenSource::Empty => {}
            DocLenSource::Whole(seg) => {
                out.total_docs = seg.us.len() as u64;
                out.total_tokens = seg.tokens;
                for u in us {
                    if let Some(l) = seg.get(u) {
                        out.lengths.insert(u.clone(), l);
                    }
                }
            }
            DocLenSource::Paged { docs, tokens } => {
                out.total_docs = docs;
                out.total_tokens = tokens;
                let mut i = 0usize;
                while i < us.len() {
                    // walk the segments in u order; after a long run of
                    // segments without a wanted u, descend again
                    let mut restart = false;
                    let mut skipped = 0u32;
                    self.doclen_scan(index_id, lane, Some(&us[i]), &mut |e, last| {
                        if last < us[i] {
                            skipped += 1;
                            if skipped > 64 {
                                restart = true;
                                return Ok(false);
                            }
                            return Ok(true);
                        }
                        skipped = 0;
                        let seg = self.doclen_segment(index_id, lane, e, &last, true)?;
                        while i < us.len() && us[i] <= last {
                            if let Some(l) = seg.get(&us[i]) {
                                out.lengths.insert(us[i].clone(), l);
                            }
                            i += 1;
                        }
                        Ok(i < us.len())
                    })?;
                    if !restart {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    /// `fulltext_corpus(index_id, lane=…)`: all document lengths (an
    /// explicit full view: memory proportional to the corpus, not cached).
    pub fn fulltext_corpus(&self, index_id: u64, lane: Lane) -> Result<FtCorpus> {
        self.check_open()?;
        self.require_fulltext(index_id)?;
        if let Some(ov) = self.ixm_ft_overlay(index_id, lane)? {
            return self.ov_fulltext_corpus(&ov, index_id, lane);
        }
        self.base_fulltext_corpus(index_id, lane)
    }

    /// Whole corpus of the key tree (no overlay).
    pub(crate) fn base_fulltext_corpus(&self, index_id: u64, lane: Lane) -> Result<FtCorpus> {
        let mut out = FtCorpus::default();
        match self.doclen_source(index_id, lane)? {
            DocLenSource::Empty => {}
            DocLenSource::Whole(seg) => {
                out.total_docs = seg.us.len() as u64;
                out.total_tokens = seg.tokens;
                out.lengths = seg.us.iter().cloned().zip(seg.lens.iter().copied()).collect();
            }
            DocLenSource::Paged { docs, tokens } => {
                out.total_docs = docs;
                out.total_tokens = tokens;
                self.doclen_scan(index_id, lane, None, &mut |e, last| {
                    let seg = self.doclen_segment(index_id, lane, e, &last, false)?;
                    out.lengths.extend(seg.us.iter().cloned().zip(seg.lens.iter().copied()));
                    Ok(true)
                })?;
            }
        }
        Ok(out)
    }

    /// Entries of an index that are no keys: the document length summary
    /// and segments of a FULLTEXT index.
    /// Reads without filling any cache (statistics).
    pub(crate) fn fulltext_internal_entries(&self, index_id: u64, lane: Lane) -> Result<u64> {
        let q = secindex::encode_key_vec(&Val::str(FULLTEXT_DOCLEN_KEY))?;
        let Some(e) = self.sec_lookup_uncached(index_id, lane, &q)? else {
            return Ok(0);
        };
        Ok(match e.width {
            PAYLOAD_FULLTEXT_DOCLEN_SUMMARY => {
                let raw = self.sec_posting_raw(index_id, lane, &e)?;
                let (_, _, segments) = textindex::decode_doclen_summary(&raw, Some(e.raw_count))
                    .map_err(|x| Error::Corruption(format!("FULLTEXT document length summary corrupt: {x}")))?;
                1 + segments
            }
            _ => 1,
        })
    }

    /// `fulltext_payloads(index_id, terms)`: postings of the unique terms in order.
    /// R4-A4: postings of the distinct `terms` (query order) for a
    /// conjunction (mode `all` or phrase): a term with many more documents
    /// than the rarest one is decoded only for the documents of the rarest
    /// term (and not cached). Returns the postings and the document
    /// frequency of every term (for BM25). Candidates and scores are the
    /// same as with full postings.
    pub fn fulltext_payloads_conj(&self, index_id: u64, terms: &[String], lane: Lane) -> Result<(TermPostingsOwned, Vec<u64>)> {
        self.check_open()?;
        self.require_fulltext(index_id)?;
        // AP9: stale index – merged postings (no restricted decoding of the tree)
        if self.ixm_ft_overlay(index_id, lane)?.is_some() {
            let mut seen = HashSet::new();
            let uniq: Vec<String> = terms.iter().filter(|t| seen.insert((*t).clone())).cloned().collect();
            let p = self.fulltext_payloads(index_id, &uniq, lane)?;
            let dfs = p.iter().map(|(_, x)| x.len() as u64).collect();
            return Ok((p, dfs));
        }
        let mut seen = HashSet::new();
        let uniq: Vec<String> = terms.iter().filter(|t| seen.insert((*t).clone())).cloned().collect();
        if uniq.len() < 2 {
            // nothing to restrict: no extra lookups
            let p = self.fulltext_payloads(index_id, &uniq, lane)?;
            let dfs = p.iter().map(|(_, x)| x.len() as u64).collect();
            return Ok((p, dfs));
        }
        // document frequency from the cache or the leaf entry (no decoding)
        // (document frequency, cached posting, leaf entry)
        type TermInfo = (u64, Option<Arc<Vec<FtPosting>>>, Option<SecEntry>);
        let mut info: Vec<TermInfo> = Vec::with_capacity(uniq.len());
        for t in &uniq {
            let ck = PostKey::Ft(index_id, lane, t.clone());
            if let Some(PostVal::Ft(c)) = self.cache().postings.get(&ck) {
                info.push((c.len() as u64, Some(c), None));
                continue;
            }
            let e = self.lookup_entry(index_id, lane, &Val::str(t.as_str()))?;
            info.push((e.as_ref().map(|e| e.raw_count).unwrap_or(0), None, e));
        }
        let min_df = info.iter().map(|x| x.0).min().unwrap_or(0);
        let restrict = |df: u64| uniq.len() > 1 && min_df > 0 && df > min_df.saturating_mul(4);
        if !info.iter().any(|x| x.1.is_none() && restrict(x.0)) {
            let dfs = info.iter().map(|x| x.0).collect();
            return Ok((self.fulltext_payloads(index_id, &uniq, lane)?, dfs));
        }
        let lead = info.iter().position(|x| x.0 == min_df).unwrap();
        let lead_post = self.fulltext_term_posting(index_id, &uniq[lead], lane)?;
        // RUN¹⁰: checkpoint of the RAM limiter per loaded posting list
        let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Postings);
        meter.grow(posting_bytes(&lead_post))?;
        let wanted: Vec<Nat> = lead_post.iter().map(|x| x.u.clone()).collect();
        let mut out = Vec::with_capacity(uniq.len());
        let mut dfs = Vec::with_capacity(uniq.len());
        for (i, (t, (df, cached, entry))) in uniq.iter().zip(info).enumerate() {
            let post = if i == lead {
                lead_post.clone()
            } else if let Some(c) = cached {
                c
            } else if restrict(df) {
                let e = entry.unwrap();
                if e.width != PAYLOAD_FULLTEXT_POSITIONS {
                    return Err(Error::Corruption("FULLTEXT term has wrong payload codec".into()));
                }
                let raw = self.sec_posting_raw(index_id, lane, &e)?;
                Arc::new(
                    textindex::decode_fulltext_posting_within(&raw, Some(e.raw_count), &wanted)
                        .map_err(|e| Error::Corruption(format!("FULLTEXT term payload corrupt: {e}")))?,
                )
            } else {
                self.fulltext_term_posting(index_id, t, lane)?
            };
            if i != lead {
                meter.grow(posting_bytes(&post))?;
            }
            out.push((t.clone(), post));
            dfs.push(df);
        }
        Ok((out, dfs))
    }

    pub fn fulltext_payloads(&self, index_id: u64, terms: &[String], lane: Lane) -> Result<TermPostingsOwned> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Postings);
        for t in terms {
            if seen.insert(t.clone()) {
                let p = self.fulltext_term_posting(index_id, t, lane)?;
                meter.grow(posting_bytes(&p))?;
                out.push((t.clone(), p));
            }
        }
        Ok(out)
    }

    /// Entries with key `k`, `lo <= k <= hi`, under the scan semantics
    /// (`match_raw(k, "between", lo, hi)`): incomparable values (None, NaN,
    /// other types) never match and never raise (F025). Bounds of one of the
    /// ordered classes (numbers, `str`, `bytes`) seek in the key tree and
    /// walk the leaves up to `hi`; unusual bounds evaluate the predicate on
    /// every key (streamed).
    fn range_entries(&self, index_id: u64, lane: Lane, lo: &Val, hi: &Val, f: &mut dyn FnMut(&SecEntry) -> Result<()>) -> Result<()> {
        let c = range_class(lo);
        if c >= 3 || range_class(hi) != c {
            return self.sec_scan(index_id, lane, None, &mut |e| {
                if crate::query::match_raw(&e.value()?, "between", lo, hi)? {
                    f(e)?;
                }
                Ok(true)
            });
        }
        let (elo, ehi) = (secindex::encode_key_vec(lo)?, secindex::encode_key_vec(hi)?);
        if secindex::cmp_keys(&elo, &ehi) == Ordering::Greater {
            return Ok(());
        }
        self.sec_scan(index_id, lane, Some(&elo), &mut |e| {
            if secindex::range_class_of(&e.key) != c || secindex::cmp_keys(&e.key, &ehi) == Ordering::Greater {
                return Ok(false);
            }
            f(e)?;
            Ok(true)
        })
    }

    /// Entries equal to `value` under Python equality (F045: NaN never
    /// matches). Lists and dicts compare by value against the container
    /// keys (a walk over that part of the tree), everything else is a
    /// lookup.
    fn eq_entries(&self, index_id: u64, lane: Lane, value: &Val) -> Result<Vec<SecEntry>> {
        if contains_nan(value) {
            return Ok(Vec::new());
        }
        match value {
            Val::List(_) | Val::Map(_) => {
                let mut out = Vec::new();
                self.sec_scan(index_id, lane, Some(&[0x0F]), &mut |e| {
                    if !secindex::is_container_key(&e.key) {
                        return Ok(false);
                    }
                    if &e.value()? == value {
                        out.push(e.clone());
                    }
                    Ok(true)
                })?;
                Ok(out)
            }
            _ => Ok(self.lookup_entry(index_id, lane, value)?.into_iter().collect()),
        }
    }

    /// `posting_count`: exact cardinality from directory metadata.
    pub fn posting_count(&self, index_id: u64, cond: &Cond, lane: Lane) -> Result<u64> {
        self.check_open()?;
        // AP9: exact for a stale index with overlay (the planner estimate
        // keeps the counts of the base, `base_posting_count`)
        if !self.index_is_fresh(index_id, lane) {
            if let Some(ov) = self.ixm_overlay(index_id, lane)? {
                return Ok(self.ov_posting_for_condition(&ov, index_id, cond, lane)?.len() as u64);
            }
        }
        self.base_posting_count(index_id, cond, lane)
    }

    /// Posting count of the key tree (directory metadata, no overlay).
    pub(crate) fn base_posting_count(&self, index_id: u64, cond: &Cond, lane: Lane) -> Result<u64> {
        Ok(match cond {
            Cond::Range(lo, hi) => {
                let mut n = 0u64;
                self.range_entries(index_id, lane, lo, hi, &mut |e| {
                    n += e.raw_count;
                    Ok(())
                })?;
                n
            }
            Cond::Eq(v) => self.eq_entries(index_id, lane, v)?.iter().map(|e| e.raw_count).sum(),
        })
    }

    pub(crate) fn posting_for_condition(&self, index_id: u64, cond: &Cond, lane: Lane) -> Result<Arc<Vec<Nat>>> {
        if let Cond::Eq(v) = cond {
            if !matches!(v, Val::List(_) | Val::Map(_)) && !contains_nan(v) {
                // point lookup: cached posting first, then the key tree
                return self.load_posting(index_id, v, lane);
            }
        }
        let matched = match cond {
            Cond::Eq(v) => self.eq_entries(index_id, lane, v)?,
            Cond::Range(lo, hi) => {
                let mut out = Vec::new();
                self.range_entries(index_id, lane, lo, hi, &mut |e| {
                    out.push(e.clone());
                    Ok(())
                })?;
                out
            }
        };
        match matched.len() {
            0 => Ok(Arc::new(Vec::new())),
            1 => self.entry_posting(index_id, lane, &matched[0]),
            n => {
                // a few keys go through the posting cache; many keys (a wide
                // range) are decoded once without caching – the cache would
                // otherwise fill with one entry per key
                let postings: Vec<Arc<Vec<Nat>>> = if n <= RANGE_CACHED_KEYS {
                    matched.iter().map(|e| self.entry_posting(index_id, lane, e)).collect::<Result<_>>()?
                } else {
                    matched.iter().map(|e| self.sec_posting_us(index_id, lane, e).map(Arc::new)).collect::<Result<_>>()?
                };
                Ok(Arc::new(union_postings(&postings)))
            }
        }
    }

    fn token_query_keys(&self, op: &str, value: &Val, options: &Val) -> Result<Vec<String>> {
        if op == "token" {
            return textindex::query_token_keys(value, options);
        }
        let raw: Vec<Val> = match value {
            Val::List(l) => l.clone(),
            other => vec![other.clone()],
        };
        let mut tmp = Vec::new();
        for r in raw {
            tmp.extend(textindex::query_token_keys(&r, options)?);
        }
        let mut seen = HashSet::new();
        Ok(tmp.into_iter().filter(|k| seen.insert(k.clone())).collect())
    }

    /// `index_candidate_estimate`
    pub fn index_candidate_estimate(&self, index_id: u64, op: &str, value: &Val, value2: &Val, lane: Lane, predicate_options: &Val) -> Result<Val> {
        self.check_open()?;
        let idx = self.index_defs.get(&index_id).ok_or_else(|| Error::Schema(format!("Index {index_id} missing")))?;
        let op = crate::unicode::lower(op);
        let count_of = |k: &str| self.key_count(index_id, lane, &Val::str(k));
        let none = || vmap! {"estimated_rows" => Val::None, "exact" => false, "key_count" => 0i64};
        Ok(match idx.kind {
            IndexKind::Value => {
                let cond = match op.as_str() {
                    "eq" => Cond::Eq(value.clone()),
                    "between" => Cond::Range(value.clone(), value2.clone()),
                    _ => return Ok(none()),
                };
                vmap! {"estimated_rows" => self.base_posting_count(index_id, &cond, lane)? as i64, "exact" => self.index_is_fresh(index_id, lane), "key_count" => 1i64}
            }
            IndexKind::Token => {
                if !matches!(op.as_str(), "token" | "tokens_all" | "tokens_any") {
                    return Ok(none());
                }
                let keys = self.token_query_keys(&op, value, &idx.options)?;
                let counts: Vec<u64> = keys.iter().map(|k| count_of(k)).collect::<Result<_>>()?;
                let est = if counts.is_empty() {
                    0
                } else if op == "tokens_any" {
                    counts.iter().sum()
                } else {
                    *counts.iter().min().unwrap()
                };
                vmap! {"estimated_rows" => est as i64, "exact" => op == "token" && keys.len() == 1 && self.index_is_fresh(index_id, lane), "key_count" => keys.len() as i64}
            }
            IndexKind::Trigram if op == "contains" || op == "icontains" => {
                let keys = textindex::query_trigram_keys(value, &idx.options)?;
                let est = keys.iter().map(|k| count_of(k)).collect::<Result<Vec<u64>>>()?.into_iter().min().unwrap_or(0);
                vmap! {"estimated_rows" => est as i64, "exact" => false, "key_count" => keys.len() as i64}
            }
            IndexKind::Fulltext if op == "fulltext" || op == "phrase" => {
                let keys = textindex::query_fulltext_terms(value, &idx.options)?;
                let counts: Vec<u64> = keys.iter().map(|k| count_of(k)).collect::<Result<_>>()?;
                let mode = if op == "phrase" {
                    "all".to_string()
                } else {
                    crate::unicode::lower(&predicate_options.get("mode").map(|m| m.py_str()).unwrap_or_else(|| "all".into()))
                };
                let est = if counts.is_empty() {
                    0
                } else if mode == "any" {
                    let sum: u64 = counts.iter().sum();
                    match self.lookup_entry(index_id, lane, &Val::str(FULLTEXT_DOCLEN_KEY))? {
                        Some(e) => sum.min(e.raw_count),
                        None => sum,
                    }
                } else {
                    *counts.iter().min().unwrap()
                };
                vmap! {
                    "estimated_rows" => est as i64, "exact" => op == "fulltext" && keys.len() == 1 && self.index_is_fresh(index_id, lane),
                    "key_count" => keys.len() as i64, "mode" => mode, "positions" => true,
                }
            }
            _ => none(),
        })
    }

    /// `index_candidates`: sorted record u for an index predicate.
    /// R4-A4: `index_candidates` of a VALUE `eq`/`between` predicate
    /// restricted to the sorted u list `wanted` (an AND with a much smaller
    /// set): the postings are probed for the wanted u instead of being
    /// decoded and cached in full. Same checks of the posting payloads.
    /// `None`: not applicable (other index kinds or operators, stale index,
    /// cached posting) – the caller uses `index_candidates`.
    pub fn index_candidates_within(&self, index_id: u64, op: &str, value: &Val, value2: &Val, lane: Lane, wanted: &[Nat]) -> Result<Option<Vec<Nat>>> {
        self.check_open()?;
        let Some(idx) = self.index_defs.get(&index_id) else { return Ok(None) };
        if idx.kind != IndexKind::Value || !self.index_is_fresh(index_id, lane) {
            return Ok(None);
        }
        let entries = match crate::unicode::lower(op).as_str() {
            "eq" => {
                let plain = !matches!(value, Val::List(_) | Val::Map(_)) && !contains_nan(value);
                if plain && self.cache().postings.get(&PostKey::Us(index_id, lane, ValKey(value.clone()))).is_some() {
                    return Ok(None);
                }
                self.eq_entries(index_id, lane, value)?
            }
            "between" => {
                let mut out = Vec::new();
                self.range_entries(index_id, lane, value, value2, &mut |e| {
                    out.push(e.clone());
                    Ok(())
                })?;
                out
            }
            _ => return Ok(None),
        };
        let mut out: Vec<Nat> = Vec::new();
        for e in &entries {
            let hit = self.posting_within(index_id, lane, e, wanted)?;
            out = if out.is_empty() { hit } else { crate::setops::union_sorted(&out, &hit) };
        }
        Ok(Some(out))
    }

    /// u of `wanted` (sorted) contained in the posting of entry `e`; fixed
    /// width postings are searched in the raw bytes.
    fn posting_within(&self, index_id: u64, lane: Lane, e: &SecEntry, wanted: &[Nat]) -> Result<Vec<Nat>> {
        if e.width != 4 && e.width != 8 {
            let p = self.sec_posting_us(index_id, lane, e)?;
            return Ok(crate::setops::intersect_sorted(wanted, &p));
        }
        let raw = self.sec_posting_raw(index_id, lane, e)?;
        let w = e.width as usize;
        if raw.len() % w != 0 || (raw.len() / w) as u64 != e.raw_count {
            return Err(Error::Corruption("Posting count mismatch".into()));
        }
        let at = |i: usize| -> u64 {
            if w == 4 { u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap()) as u64 } else { u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap()) }
        };
        let n = raw.len() / w;
        let mut out = Vec::new();
        let mut lo = 0usize;
        for u in wanted {
            let Some(x) = u.as_u64() else { break };
            // galloping from the last position (wanted is ascending)
            let (mut a, mut step) = (lo, 1usize);
            let mut b = lo;
            while b < n && at(b) < x {
                a = b + 1;
                b += step;
                step *= 2;
            }
            let b = b.min(n);
            let (mut l, mut h) = (a, b);
            while l < h {
                let m = (l + h) / 2;
                if at(m) < x { l = m + 1 } else { h = m }
            }
            lo = l;
            if l < n && at(l) == x {
                out.push(u.clone());
            }
        }
        Ok(out)
    }

    pub fn index_candidates(&self, index_id: u64, op: &str, value: &Val, value2: &Val, lane: Lane, predicate_options: &Val) -> Result<Vec<Nat>> {
        self.check_open()?;
        let idx = self.index_defs.get(&index_id).ok_or_else(|| Error::Schema(format!("Index {index_id} missing")))?;
        // AP9: a stale index answers through its overlay (None = fresh)
        let ov = self.ixm_for_query(index_id, lane, "is not fresh")?;
        let ov = ov.as_deref();
        let op = crate::unicode::lower(op);
        let intersect_all = |mut postings: Vec<Arc<Vec<Nat>>>| -> Vec<Nat> {
            postings.sort_by_key(|p| p.len());
            let mut out = postings[0].to_vec();
            for p in &postings[1..] {
                out = crate::setops::intersect_sorted(&out, p);
                if out.is_empty() {
                    break;
                }
            }
            out
        };
        match idx.kind {
            IndexKind::Value => match op.as_str() {
                "eq" => Ok(self.ov_posting(ov, index_id, &Cond::Eq(value.clone()), lane)?.to_vec()),
                "between" => Ok(self.ov_posting(ov, index_id, &Cond::Range(value.clone(), value2.clone()), lane)?.to_vec()),
                _ => Err(Error::Schema(format!("VALUE index does not support operator {}", Val::str(op.as_str()).py_repr()))),
            },
            IndexKind::Token => {
                if !matches!(op.as_str(), "token" | "tokens_all" | "tokens_any") {
                    return Err(Error::Schema(format!("TOKEN index does not support operator {}", Val::str(op.as_str()).py_repr())));
                }
                let keys = self.token_query_keys(&op, value, &idx.options)?;
                if (op == "token" && keys.len() != 1) || keys.is_empty() {
                    return Ok(Vec::new());
                }
                let postings: Vec<Arc<Vec<Nat>>> =
                    keys.iter().map(|k| self.ov_load_posting(ov, index_id, &Val::str(k.as_str()), lane)).collect::<Result<_>>()?;
                if op != "tokens_any" && postings.iter().any(|p| p.is_empty()) {
                    return Ok(Vec::new());
                }
                let mut postings: Vec<Arc<Vec<Nat>>> = postings.into_iter().filter(|p| !p.is_empty()).collect();
                if postings.is_empty() {
                    return Ok(Vec::new());
                }
                if op == "tokens_any" {
                    postings.sort_by_key(|p| p.len());
                    let mut out = postings[0].to_vec();
                    for p in &postings[1..] {
                        out = crate::setops::union_sorted(&out, p);
                    }
                    return Ok(out);
                }
                Ok(intersect_all(postings))
            }
            IndexKind::Trigram if op == "contains" || op == "icontains" => {
                let keys = textindex::query_trigram_keys(value, &idx.options)?;
                if keys.is_empty() {
                    // R9C-H1: a pattern shorter than the n-gram length has no
                    // key – the index cannot restrict it (an empty result
                    // would be wrong); SQL scans in this case
                    return Err(Error::Value(format!(
                        "index '{}': pattern {} is shorter than its n-gram length; the TRIGRAM index cannot answer it (use sql(), which scans)",
                        idx.name,
                        value.py_repr()
                    )));
                }
                let postings: Vec<Arc<Vec<Nat>>> =
                    keys.iter().map(|k| self.ov_load_posting(ov, index_id, &Val::str(k.as_str()), lane)).collect::<Result<_>>()?;
                if postings.iter().any(|p| p.is_empty()) {
                    return Ok(Vec::new());
                }
                Ok(intersect_all(postings))
            }
            IndexKind::Fulltext if op == "fulltext" || op == "phrase" => {
                let seq = textindex::query_fulltext_sequence(value, &idx.options)?;
                if seq.is_empty() {
                    return Ok(Vec::new());
                }
                let mode = crate::unicode::lower(&predicate_options.get("mode").map(|m| m.py_str()).unwrap_or_else(|| "all".into()));
                let owned = if op == "phrase" || mode == "all" {
                    self.fulltext_payloads_conj(index_id, &seq, lane)?.0
                } else {
                    self.fulltext_payloads(index_id, &seq, lane)?
                };
                let postings: Vec<(String, &[FtPosting])> = owned.iter().map(|(t, p)| (t.clone(), &p[..])).collect();
                if postings.iter().any(|(_, p)| p.is_empty()) && (op == "phrase" || mode != "any") {
                    return Ok(Vec::new());
                }
                if op == "phrase" {
                    return Ok(textindex::phrase_candidate_docs(&seq, &postings));
                }
                textindex::fulltext_candidate_docs(&postings, &mode)
            }
            k => Err(Error::Schema(format!("Index family {} does not support operator {}", k.name(), Val::str(op.as_str()).py_repr()))),
        }
    }

    /// u values satisfying all filters (`find_u`, VALUE indexes only).
    pub fn find_u(&self, realm: &Nat, filters: &[(String, Cond)], lane: Lane, u_from: Option<&Nat>, u_to: Option<&Nat>) -> Result<Vec<Nat>> {
        self.check_open()?;
        let key = (realm.clone(), lane);
        let mut postings: Vec<Arc<Vec<Nat>>> = Vec::new();
        for (name, cond) in filters {
            let iid = *self
                .index_name_map
                .get(&(realm.clone(), name.clone()))
                .ok_or_else(|| Error::Schema(format!("no content index '{name}'")))?;
            let idef = &self.index_defs[&iid];
            if idef.kind != IndexKind::Value {
                return Err(Error::Schema(format!(
                    "find_u() accepts only VALUE indexes; {} is {}",
                    Val::str(name.as_str()).py_repr(),
                    idef.kind.name()
                )));
            }
            // AP9: a stale index answers through its overlay
            let ov = self.ixm_for_query(iid, lane, "is missing or older than the data. Run build_indexes().")?;
            let p = self.ov_posting(ov.as_deref(), iid, cond, lane)?;
            if p.is_empty() {
                return Ok(Vec::new());
            }
            postings.push(p);
        }
        let mut result: Vec<Nat> = if postings.is_empty() {
            let mut r = Vec::new();
            if let Some(d) = self.dense.get(&key) {
                if !self.overlay_keys.contains(&key) {
                    let mut it = d.idx.iter_by_start();
                    while let Some(seg) = it.next(self)? {
                        self.find_u_limit(r.len() as u64 + seg.count)?;
                        for i in 0..seg.count {
                            r.push(seg.u_start.add_u64(i));
                        }
                    }
                } else {
                    let mut seen = HashSet::new();
                    let mut segs = d.idx.collect_block_order(self)?;
                    segs.reverse();
                    for seg in &segs {
                        self.find_u_limit(seen.len() as u64 + seg.count)?;
                        for i in 0..seg.count {
                            let u = seg.u_start.add_u64(i);
                            if !seen.insert(u.clone()) {
                                continue;
                            }
                            if self.delete_block_id(&key, &u)?.is_some_and(|d| d > seg.block_id) {
                                continue;
                            }
                            r.push(u);
                        }
                    }
                    r.sort();
                }
            }
            r
        } else {
            postings.sort_by_key(|p| p.len());
            let mut result: Vec<Nat> = postings[0].to_vec();
            for p in &postings[1..] {
                result = crate::setops::intersect_sorted(&result, p);
                if result.is_empty() {
                    break;
                }
            }
            result
        };
        if let Some(f) = u_from {
            result.retain(|u| u >= f);
        }
        if let Some(t) = u_to {
            result.retain(|u| u <= t);
        }
        self.find_u_limit(result.len() as u64)?;
        Ok(result)
    }

    /// U10 (RUN8): `find_u` (and the `find` iterator built on it) collects
    /// every matching u; that list obeys `[query] max_result_bytes` like the
    /// collected results of `sql()` (16 bytes per u, 0 = no limit).
    fn find_u_limit(&self, n: u64) -> Result<()> {
        let limit = self.config.query_max_result_bytes;
        let bytes = n.saturating_mul(std::mem::size_of::<Nat>() as u64);
        if limit == 0 || bytes <= limit {
            return Ok(());
        }
        Err(Error::QueryLimit(format!(
            "find_u()/find(): about {bytes} bytes for {n} matching u exceed [query] max_result_bytes = {limit}; \
             narrow the filters or u_from/u_to, use db.sql_iter() for many records, or raise max_result_bytes (0 = no limit)"
        )))
    }

    #[allow(clippy::too_many_arguments)] // mirrors the Python signature (keyword arguments)
    pub fn find(
        &self,
        realm: &Nat,
        filters: &[(String, Cond)],
        lane: Lane,
        u_from: Option<&Nat>,
        u_to: Option<&Nat>,
        dyn_mode: Option<&str>,
        dyn_v: Option<&Nat>,
    ) -> Result<Vec<Val>> {
        self.check_open()?;
        let mut out = Vec::new();
        for u in self.find_u(realm, filters, lane, u_from, u_to)? {
            if let Some(r) = self.get(realm, &u, lane, dyn_mode, dyn_v, true)? {
                out.push(r);
            }
        }
        Ok(out)
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "index_perf_tests.rs"]
mod perf_tests;

#[cfg(all(test, nhydb_internal_tests))]
#[path = "index_doclen_tests.rs"]
mod doclen_tests;

// AP9: overlay and automatic maintenance of the indexes
pub(crate) mod ixmaint;
