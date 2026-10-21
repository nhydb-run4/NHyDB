//! Secondary content indexes (`SECONDARY`, tag `SI10`).
//!
//! Coupled to BASE/dense records: dense records are indexed
//! (BASE fields from the stored raw values, extension fields
//! from the merged record view). Posting lists are stored zlib-compressed as
//! u32/u64 arrays or (for u > 2^64) as VarUInt deltas.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;

use crate::address::{lane_flags, lane_name, Lane};
use crate::codec::{decode_value, skip_value};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, get_vu_u64, put_vu64, Nat};
use crate::schema::{IndexDef, LayoutKind};
use crate::textindex::{self, FtCorpus, FtPosting, IndexKind, FULLTEXT_DOCLEN_KEY, PAYLOAD_FULLTEXT_DOCLENS, PAYLOAD_FULLTEXT_POSITIONS};
use crate::val::{Val, ValKey};
use crate::vmap;

use super::{BlockInfo, Db, PostingRef, SecondaryDir, SVal};

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
    fn push(&mut self, u: &Nat) -> Result<()> {
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

    fn len(&self) -> usize {
        match self {
            UList::Tiny(v) => v.len(),
            UList::Small(v) => v.len(),
            UList::Big(v) => v.len(),
        }
    }

    fn last(&self) -> Option<Nat> {
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

    fn to_nats(&self) -> Vec<Nat> {
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
    fn len(&self) -> usize {
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
}

impl ValueMap {
    fn bucket_str(&mut self, key: &str, make: impl FnOnce() -> Bucket) -> &mut Bucket {
        let i = match self.spos.get(key) {
            Some(&i) => i,
            None => {
                let i = self.items.len();
                self.spos.insert(key.to_string(), i);
                self.items.push((Val::str(key), make()));
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
                self.pos.insert(k, self.items.len());
                self.items.push((key, make()));
                self.items.len() - 1
            }
        };
        &mut self.items[i].1
    }
    fn get(&self, key: &str) -> Option<&Bucket> {
        self.spos.get(key).or_else(|| self.pos.get(&ValKey(Val::str(key)))).map(|&i| &self.items[i].1)
    }
}

/// Header of a SECONDARY block (the directory is loaded only when needed).
#[derive(Clone, Debug)]
pub struct SecondaryMeta {
    pub index_id: u64,
    pub lane: Lane,
    pub source_block_id: u64,
    pub entry_count: u64,
    pub dir_offset: u64,
    pub dir_len: u64,
    pub postings_base: u64,
    pub payload_end: u64,
    pub block_id: u64,
    /// Offset of the block header (for the one-time payload CRC check).
    pub block_offset: u64,
    /// Payload CRC already verified in this session (F011). Shared between
    /// clones; a later per-block "verified" registry can replace it.
    pub verified: Arc<AtomicBool>,
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

/// I001: bytes per posting reference assumed when planning a build: the
/// list element for the largest u of the lane, the worst-case growth
/// reserve of a doubling `Vec` and the write buffers (measured with KQ-10M:
/// about 8.6 bytes per reference for `u32` lists).
fn ref_bytes_estimate(max_u: &Nat) -> u64 {
    match max_u {
        Nat::S(x) if *x <= u32::MAX as u64 => 10,
        Nat::S(_) => 20,
        Nat::B(_) => 80,
    }
}
/// Positional postings per query term (shared, without copying).
pub type TermPostingsOwned = Vec<(String, Arc<Vec<FtPosting>>)>;

impl Db {
    // ------------------------------------------------------------------
    // SECONDARY blocks: header on open, directory on demand
    // ------------------------------------------------------------------

    /// `_register_secondary_header`
    pub(crate) fn parse_secondary_directory(&mut self, info: &BlockInfo) -> Result<()> {
        let lane = crate::address::lane_from_flags(info.flags);
        let payload_off = info.offset + BLOCK_HEADER_SIZE as u64;
        let raw = self.store.read_upto(payload_off, 128.min(info.payload_len as usize))?;
        if !raw.starts_with(SEC_TAG) {
            return Err(Error::Corruption(format!("SECONDARY block {}: wrong tag", info.block_id)));
        }
        let mut p = 4;
        let index_id = get_vu_u64(&raw, &mut p, "SECONDARY index")?;
        let source_bid = get_vu_u64(&raw, &mut p, "SECONDARY source")?;
        let entry_count = get_vu_u64(&raw, &mut p, "SECONDARY entries")?;
        let dir_len = get_vu_u64(&raw, &mut p, "SECONDARY dirlen")?;
        let dir_off = payload_off + p as u64;
        let payload_end = payload_off + info.payload_len;
        let postings_base = dir_off.saturating_add(dir_len);
        if postings_base > payload_end {
            return Err(Error::Corruption("SECONDARY directory outside the block".into()));
        }
        let meta = SecondaryMeta {
            index_id,
            lane,
            source_block_id: source_bid,
            entry_count,
            dir_offset: dir_off,
            dir_len,
            postings_base,
            payload_end,
            block_id: info.block_id,
            block_offset: info.offset,
            verified: Arc::new(AtomicBool::new(false)),
        };
        self.secondary_blocks.insert((index_id, lane), meta);
        self.secondary_source_block.insert((index_id, lane), source_bid);
        let c = self.caches.get_mut().unwrap_or_else(|e| e.into_inner());
        c.postings.retain(|k, _| !(k.0 == index_id && k.1 == lane));
        c.sec_dirs.remove(&(index_id, lane));
        c.sec_sorted.remove(&(index_id, lane));
        c.ft_postings.retain(|k, _| !(k.0 == index_id && k.1 == lane));
        c.ft_corpus.remove(&(index_id, lane));
        Ok(())
    }

    /// `_ensure_secondary_directory`
    pub(crate) fn secondary_dir(&self, index_id: u64, lane: Lane) -> Result<Arc<SecondaryDir>> {
        if let Some(d) = self.cache().sec_dirs.get(&(index_id, lane)) {
            return Ok(d.clone());
        }
        let Some(meta) = self.secondary_blocks.get(&(index_id, lane)) else {
            return Ok(Arc::new(SecondaryDir::default()));
        };
        // F011: the whole SECONDARY payload is CRC-checked once before its
        // first use (directory and postings); the directory comes from the
        // verified payload.
        let payload = self.verified_secondary_payload(meta)?;
        let payload_off = meta.block_offset + BLOCK_HEADER_SIZE as u64;
        let rel = (meta.dir_offset - payload_off) as usize;
        let dbytes = payload
            .get(rel..rel.saturating_add(meta.dir_len as usize))
            .ok_or_else(|| Error::Corruption("SECONDARY directory outside the block".into()))?;
        let mut q = 0usize;
        let mut dir = SecondaryDir::default();
        for _ in 0..meta.entry_count {
            let klen = get_vu_u64(dbytes, &mut q, "SECONDARY key")? as usize;
            if q.checked_add(klen).is_none_or(|e| e > dbytes.len()) {
                return Err(Error::Corruption("Length value ends beyond payload".into()));
            }
            let value = json_key_decode(&dbytes[q..q + klen])?;
            q += klen;
            let rel = get_vu_u64(dbytes, &mut q, "SECONDARY rel")?;
            let clen = get_vu_u64(dbytes, &mut q, "SECONDARY clen")?;
            let cnt = get_vu_u64(dbytes, &mut q, "SECONDARY cnt")?;
            let Some(&width) = dbytes.get(q) else {
                return Err(Error::Corruption("SECONDARY width missing".into()));
            };
            q += 1;
            if !matches!(width, 0 | 4 | 8 | PAYLOAD_FULLTEXT_POSITIONS | PAYLOAD_FULLTEXT_DOCLENS) {
                return Err(Error::Corruption("SECONDARY payload codec invalid".into()));
            }
            if meta.postings_base.checked_add(rel).and_then(|a| a.checked_add(clen)).map(|e| e > meta.payload_end).unwrap_or(true) {
                return Err(Error::Corruption("SECONDARY posting list points beyond the block".into()));
            }
            let r = PostingRef { value: value.clone(), abs_offset: meta.postings_base + rel, compressed_len: clen, raw_count: cnt, width };
            match dir.by_key.get(&ValKey(value.clone())) {
                Some(&i) => dir.entries[i] = r,
                None => {
                    dir.by_key.insert(ValKey(value), dir.entries.len());
                    dir.entries.push(r);
                }
            }
        }
        if q != dbytes.len() {
            return Err(Error::Corruption("SECONDARY directory trailing bytes".into()));
        }
        let arc = Arc::new(dir);
        self.cache().sec_dirs.insert((index_id, lane), arc.clone());
        Ok(arc)
    }

    /// Reads the payload of a SECONDARY block with CRC check and marks the
    /// block as verified (F011).
    fn verified_secondary_payload(&self, meta: &SecondaryMeta) -> Result<Vec<u8>> {
        let (info, payload) = self.read_block(meta.block_offset)?;
        if info.block_type != BT_SECONDARY || info.block_id != meta.block_id {
            return Err(Error::Corruption(format!("SECONDARY block {}: header changed", meta.block_id)));
        }
        meta.verified.store(true, AtomicOrdering::Release);
        Ok(payload)
    }

    /// Directory keys ordered for range lookups (F025): numbers (without
    /// NaN), then `str`, then `bytes`, each in natural order; all other keys
    /// (None, NaN, lists, dicts) follow unordered. Range bounds of one of the
    /// three ordered classes only ever match keys of the same class.
    fn secondary_keys_sorted(&self, index_id: u64, lane: Lane) -> Result<Arc<Vec<Val>>> {
        if let Some(k) = self.cache().sec_sorted.get(&(index_id, lane)) {
            return Ok(k.clone());
        }
        let dir = self.secondary_dir(index_id, lane)?;
        let mut keys: Vec<Val> = dir.entries.iter().map(|e| e.value.clone()).collect();
        keys.sort_by(|a, b| {
            let (ca, cb) = (range_class(a), range_class(b));
            ca.cmp(&cb).then_with(|| if ca < 3 { crate::val::py_cmp(a, b).unwrap_or(Ordering::Equal) } else { Ordering::Equal })
        });
        let arc = Arc::new(keys);
        self.cache().sec_sorted.insert((index_id, lane), arc.clone());
        Ok(arc)
    }

    /// Public view of the (lazily loaded) directory.
    pub fn secondary_directory(&self, index_id: u64, lane: Lane) -> Result<Arc<SecondaryDir>> {
        self.check_open()?;
        self.secondary_dir(index_id, lane)
    }

    pub fn secondary_source_block_id(&self, index_id: u64, lane: Lane) -> Option<u64> {
        self.secondary_source_block.get(&(index_id, lane)).copied()
    }

    /// Number of directory entries of the current SECONDARY block.
    pub(crate) fn secondary_entry_count(&self, index_id: u64, lane: Lane) -> usize {
        self.secondary_blocks.get(&(index_id, lane)).map(|m| m.entry_count as usize).unwrap_or(0)
    }

    pub(crate) fn secondary_directory_loaded(&self, index_id: u64, lane: Lane) -> bool {
        self.cache().sec_dirs.contains_key(&(index_id, lane))
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
            }
            IndexKind::Fulltext => {
                let tokens = textindex::simple_tokens(value, kg.casefold);
                if let Bucket::Doc(v) = map.bucket_str(FULLTEXT_DOCLEN_KEY, || Bucket::Doc(Vec::new())) {
                    v.push((u.clone(), tokens.len() as u64));
                }
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
                    if let Bucket::Ft(v) = map.bucket_str(t, || Bucket::Ft(Vec::new())) {
                        v.push((u.clone(), p));
                    }
                }
            }
            IndexKind::Token | IndexKind::Trigram => {
                if value.is_none() {
                    return Ok(());
                }
                let keys = if idef.kind == IndexKind::Token {
                    textindex::simple_tokens(value, kg.casefold)
                } else {
                    textindex::ngrams(value, kg.ngram, kg.casefold)
                };
                let mut seen: HashSet<&str> = HashSet::with_capacity(keys.len());
                for k in &keys {
                    if seen.insert(k.as_str()) {
                        if let Bucket::Us(v) = map.bucket_str(k, || Bucket::Us(UList::default())) {
                            v.push(u)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Value lists of all selected indexes (`_build_maps_fast`):
    /// BASE fields via the dense path, extensions directly from sparse blocks.
    fn build_maps(&self, selected: &[IndexDef]) -> Result<(IndexMaps, u64, u64)> {
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
                let mut overlay: HashMap<(Nat, Nat), SVal> = HashMap::new();
                if !base_idxs.is_empty() {
                    if let Some(sidx) = self.sparse.get(&key) {
                        for seg in &sidx.segs {
                            let bi = self.sparse_block_index(seg)?;
                            let payload = self.block_payload_once(seg.block_offset)?;
                            for (u, abs, len) in bi.iter() {
                                let rel = (abs - seg.payload_offset) as usize;
                                for (v, meta) in self.decode_sparse_record(&realm, u, &payload[rel..rel + *len as usize])? {
                                    if base_vs_wanted.contains(&v) {
                                        overlay.insert((u.clone(), v), meta.value);
                                    }
                                }
                            }
                        }
                    }
                }
                if !base_idxs.is_empty() {
                    if let Some(didx) = self.dense.get(&key).filter(|d| !d.segs.is_empty()) {
                        let mut by_start: Vec<&super::DenseSegment> = didx.segs.iter().collect();
                        by_start.sort_by(|a, b| a.u_start.cmp(&b.u_start).then(a.block_id.cmp(&b.block_id)));
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
                        let newest_wins = dense_overlap || self.deletes.keys().any(|(r, l, _)| r == &realm && *l == lane);
                        let segs: Vec<&super::DenseSegment> = if newest_wins {
                            let mut s: Vec<_> = didx.segs.iter().collect();
                            s.sort_by_key(|x| std::cmp::Reverse(x.block_id));
                            s
                        } else {
                            by_start
                        };
                        let mut seen: HashSet<Nat> = HashSet::new();
                        for seg in segs {
                            let shape = self.shape_of(seg.shape_id)?;
                            let mut wanted_here: Vec<(usize, Vec<&IndexDef>)> = Vec::new();
                            for (fi, f) in shape.fields.iter().enumerate() {
                                let ids: Vec<&IndexDef> = base_idxs.iter().copied().filter(|i| i.field_v() == &f.v).collect();
                                if !ids.is_empty() {
                                    wanted_here.push((fi, ids));
                                }
                            }
                            if wanted_here.is_empty() {
                                continue;
                            }
                            let payload = self.block_payload_once(seg.block_offset)?;
                            let toff = (seg.offsets_offset - seg.payload_offset) as usize;
                            let rbase = (seg.records_offset - seg.payload_offset) as usize;
                            let max_fi = wanted_here.last().unwrap().0;
                            for i in 0..seg.count as usize {
                                let u = seg.u_start.add_u64(i as u64);
                                if newest_wins {
                                    if !seen.insert(u.clone()) {
                                        continue;
                                    }
                                    if self.deletes.get(&(realm.clone(), lane, u.clone())).is_some_and(|d| *d > seg.block_id) {
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
                                let mut p = 0usize;
                                let mut wi = 0;
                                for (fi, f) in shape.fields.iter().enumerate().take(max_fi + 1) {
                                    if wi < wanted_here.len() && wanted_here[wi].0 == fi {
                                        let mut val = decode_value(f, rec, &mut p)?;
                                        if !overlay.is_empty() {
                                            match overlay.get(&(u.clone(), f.v.clone())) {
                                                Some(SVal::Tomb) => {
                                                    wi += 1;
                                                    continue;
                                                }
                                                Some(SVal::V(x)) => val = x.clone(),
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
                        }
                    }
                }
                if !overlay.is_empty() {
                    // pure sparse records (no live dense record at u)
                    let mut extra: Vec<(&(Nat, Nat), &Val)> = overlay
                        .iter()
                        .filter_map(|(k, st)| st.val().map(|v| (k, v)))
                        .filter(|((u, _), _)| self.resolve_segment(&realm, u, lane).is_none() && !foreign(u))
                        .collect();
                    extra.sort_by(|a, b| a.0.cmp(b.0));
                    for ((u, v), val) in extra {
                        for idef in base_idxs.iter().filter(|i| i.field_v() == v) {
                            Self::append_index_document(maps.get_mut(&(idef.index_id, lane)).unwrap(), idef, &kgs[&idef.index_id], u, val)?;
                        }
                    }
                }
                if !ext_idxs.is_empty() {
                    if let Some(sidx) = self.sparse.get(&key).filter(|s| !s.segs.is_empty()) {
                        let mut segs: Vec<_> = sidx.segs.iter().collect();
                        segs.sort_by_key(|x| std::cmp::Reverse(x.block_id));
                        let wanted: HashSet<&Nat> = ext_idxs.iter().map(|i| i.field_v()).collect();
                        let max_wanted: Nat = wanted.iter().map(|v| (*v).clone()).max().unwrap_or_default();
                        let mut seen_cells: HashSet<(Nat, Nat)> = HashSet::new();
                        for seg in segs {
                            let bi = self.sparse_block_index(seg)?;
                            let payload = self.block_payload_once(seg.block_offset)?;
                            for (u, abs, len) in bi.iter() {
                                sparse_groups += 1;
                                let rel = (abs - seg.payload_offset) as usize;
                                if foreign(u) {
                                    continue;
                                }
                                let vals = self.decode_sparse_selected(&realm, u, &payload[rel..rel + *len as usize], &max_wanted)?;
                                // F026: `delete(realm, u)` hides only the dense
                                // record; extension cells stay visible to the
                                // read path and therefore stay indexed.
                                for (v, meta) in vals {
                                    if !wanted.contains(&v) {
                                        continue;
                                    }
                                    if !seen_cells.insert((u.clone(), v.clone())) {
                                        continue;
                                    }
                                    let SVal::V(val) = meta.value else { continue };
                                    for idef in ext_idxs.iter().filter(|i| i.field_v() == &v) {
                                        Self::append_index_document(maps.get_mut(&(idef.index_id, lane)).unwrap(), idef, &kgs[&idef.index_id], u, &val)?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Persisted postings must be sorted by document u.
        for vm in maps.values_mut() {
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
        Ok((maps, dense_scanned, sparse_groups))
    }

    fn persist_secondary(&mut self, idx: &IndexDef, value_map: &ValueMap, source_bid: u64, lane: Lane, lane_max_u: &Nat) -> Result<(u64, u64, usize)> {
        // F009: the posting width covers the largest document u actually
        // indexed (sparse u may exceed the dense maximum of the lane); the
        // lane maximum keeps the width of the reference for ordinary data.
        let mut max_u = lane_max_u.clone();
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
        let too_wide = || Error::Value("posting u exceeds the posting width".into());
        let mut items: Vec<&(Val, Bucket)> = value_map.items.iter().collect();
        crate::val::py_sort_keys(&mut items, |kv| &kv.0);
        let mut postings: Vec<u8> = Vec::new();
        let mut directory: Vec<u8> = Vec::new();
        for (value, bucket) in items {
            let (w, raw): (u8, Vec<u8>) = match bucket {
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
            };
            let comp = crate::zlib::compress(&raw, 1);
            let rel = postings.len() as u64;
            postings.extend_from_slice(&comp);
            let k = json_key_encode(value)?;
            put_vu64(&mut directory, k.len() as u64);
            directory.extend_from_slice(k.as_bytes());
            put_vu64(&mut directory, rel);
            put_vu64(&mut directory, comp.len() as u64);
            put_vu64(&mut directory, bucket.len() as u64);
            directory.push(w);
        }
        let mut prefix = Vec::new();
        prefix.extend_from_slice(SEC_TAG);
        put_vu64(&mut prefix, idx.index_id);
        put_vu64(&mut prefix, source_bid);
        put_vu64(&mut prefix, value_map.items.len() as u64);
        put_vu64(&mut prefix, directory.len() as u64);
        let plen = prefix.len() + directory.len() + postings.len();
        let (off, bid) = self.append_block_parts(BT_SECONDARY, &[&prefix, &directory, &postings], lane_flags(lane))?;
        let info = self.read_header_at(off)?;
        self.parse_secondary_directory(&info)?;
        Ok((off, bid, plen))
    }

    /// Rebuilds indexes (`build_indexes`).
    ///
    /// A lane with any data blocks (even only sparse or
    /// deletes) always receives a SECONDARY block, so that indexes of
    /// pure sparse realms also become fresh.
    /// I001: estimated memory of the posting lists of one index during the
    /// build – one reference per dense record and per sparse u group of its
    /// realm (exact for VALUE indexes; a lower bound for TOKEN, TRIGRAM and
    /// FULLTEXT, which can produce several references per record).
    pub(crate) fn index_build_estimate(&self, idx: &IndexDef) -> u64 {
        let mut refs = 0u64;
        let mut per_ref = 10u64;
        for lane in [0u8, 1] {
            let key = (idx.realm_id.clone(), lane);
            if let Some(d) = self.dense.get(&key) {
                refs = refs.saturating_add(d.segs.iter().map(|s| s.count).sum::<u64>());
            }
            if let Some(sp) = self.sparse.get(&key) {
                refs = refs.saturating_add(sp.segs.iter().map(|s| s.group_count).sum::<u64>());
            }
            if let Some(m) = self.max_u.get(&key) {
                per_ref = per_ref.max(ref_bytes_estimate(m));
            }
        }
        refs.saturating_mul(per_ref)
    }

    /// I001: splits the selected indexes into scan passes. All indexes of a
    /// pass are built from one scan and held in memory together, so a pass
    /// may use at most half of the physical memory; consecutive indexes are
    /// grouped (order kept). Normally that is a single pass, as before; only
    /// for very large databases the build falls back to fewer indexes per
    /// scan. An index that alone would need more than the physical memory
    /// is refused with `Error::Memory` before anything is read or written.
    fn plan_index_passes(&self, selected: &[IndexDef]) -> Result<Vec<Vec<IndexDef>>> {
        let Some(phys) = self.index_build_memory_override.or_else(super::columns::physical_memory_bytes) else {
            return Ok(vec![selected.to_vec()]);
        };
        let limit = phys / 2;
        let mut passes: Vec<Vec<IndexDef>> = Vec::new();
        let (mut cur, mut cur_bytes): (Vec<IndexDef>, u64) = (Vec::new(), 0);
        for idx in selected {
            let need = self.index_build_estimate(idx);
            if need > phys {
                return Err(Error::Memory(format!(
                    "build_indexes: index '{}' would need about {} MiB of memory during the build, more than the physical memory ({} MiB)",
                    idx.name,
                    need >> 20,
                    phys >> 20
                )));
            }
            if !cur.is_empty() && cur_bytes.saturating_add(need) > limit {
                passes.push(std::mem::take(&mut cur));
                cur_bytes = 0;
            }
            cur.push(idx.clone());
            cur_bytes = cur_bytes.saturating_add(need);
        }
        if !cur.is_empty() {
            passes.push(cur);
        }
        Ok(passes)
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
        let passes = self.plan_index_passes(&selected)?;
        let (mut scan_s, mut persist_s) = (0.0f64, 0.0f64);
        let (mut dense_scanned, mut sparse_groups) = (0u64, 0u64);
        let mut scanned_realms: HashSet<Nat> = HashSet::new();
        let mut stats = Vec::new();
        for pass in &passes {
            let t0 = Instant::now();
            let (maps, pass_dense, pass_sparse) = self.build_maps(pass)?;
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
                    let (off, bid, nbytes) = self.persist_secondary(idx, vm, source, lane, &max_u)?;
                    let mut row = vmap! {
                        "index_id" => idx.index_id as i64, "name" => idx.name.as_str(), "kind" => idx.kind.name(),
                        "lane" => lane as i64, "lane_name" => lane_name(lane),
                    };
                    if idx.kind == IndexKind::Fulltext {
                        let docs = vm.get(FULLTEXT_DOCLEN_KEY).map(|b| b.len()).unwrap_or(0);
                        let has_doc = vm.get(FULLTEXT_DOCLEN_KEY).is_some();
                        let refs: usize = vm.items.iter().filter(|(k, _)| k.as_str() != Some(FULLTEXT_DOCLEN_KEY)).map(|(_, b)| b.len()).sum();
                        row.set("distinct_values", Val::Int(vm.items.len() as i64 - has_doc as i64));
                        row.set("u_references", Val::Int(refs as i64));
                        row.set("block_offset", Val::Int(off as i64));
                        row.set("block_id", Val::Int(bid as i64));
                        row.set("payload_bytes", Val::Int(nbytes as i64));
                        row.set("documents", Val::Int(docs as i64));
                    } else {
                        let refs: usize = vm.items.iter().map(|(_, b)| b.len()).sum();
                        row.set("distinct_values", Val::Int(vm.items.len() as i64));
                        row.set("u_references", Val::Int(refs as i64));
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
        {
            let c = self.caches.get_mut().unwrap_or_else(|e| e.into_inner());
            c.postings.clear();
            c.ft_postings.clear();
            c.ft_corpus.clear();
        }
        if commit {
            self.commit()?;
        }
        Ok(vmap! {
            "scan_s" => scan_s, "persist_s" => persist_s, "total_s" => scan_s + persist_s,
            "records_scanned" => (dense_scanned + sparse_groups) as i64,
            "scan_detail" => vmap! {"dense_records_scanned" => dense_scanned as i64, "sparse_groups_scanned" => sparse_groups as i64},
            "passes" => passes.len() as i64,
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

    fn read_payload(&self, r: &PostingRef) -> Result<Vec<u8>> {
        let comp = self.store.read_vec(r.abs_offset, r.compressed_len as usize)?;
        // R010: `decompress` is bounded by deflate's maximum ratio
        crate::zlib::decompress(&comp).map_err(|e| Error::Corruption(format!("Index payload zlib-corrupt: {e}")))
    }

    pub(crate) fn load_posting(&self, index_id: u64, value: &Val, lane: Lane) -> Result<Arc<Vec<Nat>>> {
        let ck = (index_id, lane, ValKey(value.clone()));
        if let Some(c) = self.cache().postings.get(&ck) {
            return Ok(c.clone());
        }
        let dir = self.secondary_dir(index_id, lane)?;
        let Some(&i) = dir.by_key.get(&ValKey(value.clone())) else {
            return Ok(Arc::new(Vec::new()));
        };
        let r = &dir.entries[i];
        if r.width == PAYLOAD_FULLTEXT_POSITIONS || r.width == PAYLOAD_FULLTEXT_DOCLENS {
            return Err(Error::Schema("typed FULLTEXT payload is not a plain document ID posting list".into()));
        }
        let raw = self.read_payload(r)?;
        let list: Vec<Nat> = match r.width {
            0 => decode_big_posting(&raw, r.raw_count)?,
            4 => raw.as_chunks::<4>().0.iter().map(|c| Nat::S(u32::from_le_bytes(*c) as u64)).collect(),
            _ => raw.as_chunks::<8>().0.iter().map(|c| Nat::S(u64::from_le_bytes(*c))).collect(),
        };
        if list.len() as u64 != r.raw_count {
            return Err(Error::Corruption("Posting count mismatch".into()));
        }
        let arc = Arc::new(list);
        let mut c = self.cache();
        let c = &mut *c;
        super::blockcache::bounded_insert(&mut c.postings, &mut c.used.postings, ck, arc.clone(), |v| v.len(), super::blockcache::POSTINGS_BUDGET, usize::MAX);
        Ok(arc)
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
        let ck = (index_id, lane, term.to_string());
        if let Some(c) = self.cache().ft_postings.get(&ck) {
            return Ok(c.clone());
        }
        let dir = self.secondary_dir(index_id, lane)?;
        let out = match dir.by_key.get(&ValKey(Val::str(term))) {
            None => Vec::new(),
            Some(&i) => {
                let r = &dir.entries[i];
                if r.width != PAYLOAD_FULLTEXT_POSITIONS {
                    return Err(Error::Corruption("FULLTEXT term has wrong payload codec".into()));
                }
                let raw = self.read_payload(r)?;
                textindex::decode_fulltext_posting(&raw, Some(r.raw_count))
                    .map_err(|e| Error::Corruption(format!("FULLTEXT term payload corrupt: {e}")))?
            }
        };
        let arc = Arc::new(out);
        let mut c = self.cache();
        let c = &mut *c;
        super::blockcache::bounded_insert(&mut c.ft_postings, &mut c.used.ft_postings, ck, arc.clone(), |v| v.len(), super::blockcache::FT_POSTINGS_BUDGET, usize::MAX);
        Ok(arc)
    }

    /// `fulltext_corpus(index_id, lane=…)`
    pub fn fulltext_corpus(&self, index_id: u64, lane: Lane) -> Result<Arc<FtCorpus>> {
        self.check_open()?;
        self.require_fulltext(index_id)?;
        if let Some(c) = self.cache().ft_corpus.get(&(index_id, lane)) {
            return Ok(c.clone());
        }
        let dir = self.secondary_dir(index_id, lane)?;
        let corpus = match dir.by_key.get(&ValKey(Val::str(FULLTEXT_DOCLEN_KEY))) {
            None => FtCorpus::default(),
            Some(&i) => {
                let r = &dir.entries[i];
                if r.width != PAYLOAD_FULLTEXT_DOCLENS {
                    return Err(Error::Corruption("FULLTEXT document lengths have wrong payload codec".into()));
                }
                let raw = self.read_payload(r)?;
                textindex::decode_fulltext_doc_lengths(&raw, Some(r.raw_count))
                    .map_err(|e| Error::Corruption(format!("FULLTEXT document lengths corrupt: {e}")))?
            }
        };
        let arc = Arc::new(corpus);
        self.cache().ft_corpus.insert((index_id, lane), arc.clone());
        Ok(arc)
    }

    /// `fulltext_payloads(index_id, terms)`: unique terms in order.
    pub fn fulltext_payloads(&self, index_id: u64, terms: &[String], lane: Lane) -> Result<(TermPostingsOwned, Arc<FtCorpus>)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for t in terms {
            if seen.insert(t.clone()) {
                out.push((t.clone(), self.fulltext_term_posting(index_id, t, lane)?));
            }
        }
        Ok((out, self.fulltext_corpus(index_id, lane)?))
    }

    /// Keys `k` with `lo <= k <= hi` under the scan semantics
    /// (`match_raw(k, "between", lo, hi)`): incomparable values (None, NaN,
    /// other types) never match and never raise (F025).
    fn range_keys(&self, index_id: u64, lane: Lane, lo: &Val, hi: &Val) -> Result<Vec<Val>> {
        let keys = self.secondary_keys_sorted(index_id, lane)?;
        let c = range_class(lo);
        if c >= 3 || range_class(hi) != c {
            // Unusual bounds (None, NaN, lists, mixed classes): evaluate the
            // scan predicate on every key.
            let mut out = Vec::new();
            for k in keys.iter() {
                if crate::query::match_raw(k, "between", lo, hi)? {
                    out.push(k.clone());
                }
            }
            return Ok(out);
        }
        let first = keys.partition_point(|k| range_class(k) < c);
        let last = keys.partition_point(|k| range_class(k) <= c);
        let seg = &keys[first..last];
        let cmp = |a: &Val, b: &Val| crate::val::py_cmp(a, b).unwrap_or(Ordering::Equal);
        let start = seg.partition_point(|k| cmp(k, lo) == Ordering::Less);
        let end = seg.partition_point(|k| cmp(hi, k) != Ordering::Less);
        Ok(if end > start { seg[start..end].to_vec() } else { Vec::new() })
    }

    /// Keys equal to `value` under Python equality (F045: NaN never matches).
    fn eq_keys(&self, dir: &SecondaryDir, value: &Val) -> Vec<Val> {
        if contains_nan(value) {
            return Vec::new();
        }
        match value {
            Val::List(_) | Val::Map(_) => dir.entries.iter().filter(|e| &e.value == value).map(|e| e.value.clone()).collect(),
            _ => dir.by_key.get(&ValKey(value.clone())).map(|&i| vec![dir.entries[i].value.clone()]).unwrap_or_default(),
        }
    }

    /// `posting_count`: exact cardinality from directory metadata.
    pub fn posting_count(&self, index_id: u64, cond: &Cond, lane: Lane) -> Result<u64> {
        self.check_open()?;
        let dir = self.secondary_dir(index_id, lane)?;
        Ok(match cond {
            Cond::Range(lo, hi) => self
                .range_keys(index_id, lane, lo, hi)?
                .iter()
                .map(|k| dir.by_key.get(&ValKey(k.clone())).map(|&i| dir.entries[i].raw_count).unwrap_or(0))
                .sum(),
            Cond::Eq(v) => self
                .eq_keys(&dir, v)
                .iter()
                .map(|k| dir.by_key.get(&ValKey(k.clone())).map(|&i| dir.entries[i].raw_count).unwrap_or(0))
                .sum(),
        })
    }

    pub(crate) fn posting_for_condition(&self, index_id: u64, cond: &Cond, lane: Lane) -> Result<Arc<Vec<Nat>>> {
        let matched = match cond {
            Cond::Eq(v) => {
                let dir = self.secondary_dir(index_id, lane)?;
                self.eq_keys(&dir, v)
            }
            Cond::Range(lo, hi) => self.range_keys(index_id, lane, lo, hi)?,
        };
        match matched.len() {
            0 => Ok(Arc::new(Vec::new())),
            1 => self.load_posting(index_id, &matched[0], lane),
            _ => {
                let postings: Vec<Arc<Vec<Nat>>> = matched.iter().map(|v| self.load_posting(index_id, v, lane)).collect::<Result<_>>()?;
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
        let dir = self.secondary_dir(index_id, lane)?;
        let op = crate::unicode::lower(op);
        let count_of = |k: &str| dir.by_key.get(&ValKey(Val::str(k))).map(|&i| dir.entries[i].raw_count).unwrap_or(0);
        let none = || vmap! {"estimated_rows" => Val::None, "exact" => false, "key_count" => 0i64};
        Ok(match idx.kind {
            IndexKind::Value => {
                let cond = match op.as_str() {
                    "eq" => Cond::Eq(value.clone()),
                    "between" => Cond::Range(value.clone(), value2.clone()),
                    _ => return Ok(none()),
                };
                vmap! {"estimated_rows" => self.posting_count(index_id, &cond, lane)? as i64, "exact" => true, "key_count" => 1i64}
            }
            IndexKind::Token => {
                if !matches!(op.as_str(), "token" | "tokens_all" | "tokens_any") {
                    return Ok(none());
                }
                let keys = self.token_query_keys(&op, value, &idx.options)?;
                let counts: Vec<u64> = keys.iter().map(|k| count_of(k)).collect();
                let est = if counts.is_empty() {
                    0
                } else if op == "tokens_any" {
                    counts.iter().sum()
                } else {
                    *counts.iter().min().unwrap()
                };
                vmap! {"estimated_rows" => est as i64, "exact" => op == "token" && keys.len() == 1, "key_count" => keys.len() as i64}
            }
            IndexKind::Trigram if op == "contains" || op == "icontains" => {
                let keys = textindex::query_trigram_keys(value, &idx.options)?;
                let est = keys.iter().map(|k| count_of(k)).min().unwrap_or(0);
                vmap! {"estimated_rows" => est as i64, "exact" => false, "key_count" => keys.len() as i64}
            }
            IndexKind::Fulltext if op == "fulltext" || op == "phrase" => {
                let keys = textindex::query_fulltext_terms(value, &idx.options)?;
                let counts: Vec<u64> = keys.iter().map(|k| count_of(k)).collect();
                let mode = if op == "phrase" {
                    "all".to_string()
                } else {
                    crate::unicode::lower(&predicate_options.get("mode").map(|m| m.py_str()).unwrap_or_else(|| "all".into()))
                };
                let est = if counts.is_empty() {
                    0
                } else if mode == "any" {
                    let sum: u64 = counts.iter().sum();
                    match dir.by_key.get(&ValKey(Val::str(FULLTEXT_DOCLEN_KEY))) {
                        Some(&i) => sum.min(dir.entries[i].raw_count),
                        None => sum,
                    }
                } else {
                    *counts.iter().min().unwrap()
                };
                vmap! {
                    "estimated_rows" => est as i64, "exact" => op == "fulltext" && keys.len() == 1,
                    "key_count" => keys.len() as i64, "mode" => mode, "positions" => true,
                }
            }
            _ => none(),
        })
    }

    /// `index_candidates`: sorted record u for an index predicate.
    pub fn index_candidates(&self, index_id: u64, op: &str, value: &Val, value2: &Val, lane: Lane, predicate_options: &Val) -> Result<Vec<Nat>> {
        self.check_open()?;
        let idx = self.index_defs.get(&index_id).ok_or_else(|| Error::Schema(format!("Index {index_id} missing")))?;
        if !self.index_is_fresh(index_id, lane) {
            return Err(Error::StaleIndex(format!("Index {}/{} is not fresh", Val::str(idx.name.as_str()).py_repr(), lane_name(lane))));
        }
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
                "eq" => Ok(self.posting_for_condition(index_id, &Cond::Eq(value.clone()), lane)?.to_vec()),
                "between" => Ok(self.posting_for_condition(index_id, &Cond::Range(value.clone(), value2.clone()), lane)?.to_vec()),
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
                    keys.iter().map(|k| self.load_posting(index_id, &Val::str(k.as_str()), lane)).collect::<Result<_>>()?;
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
                    return Ok(Vec::new());
                }
                let postings: Vec<Arc<Vec<Nat>>> =
                    keys.iter().map(|k| self.load_posting(index_id, &Val::str(k.as_str()), lane)).collect::<Result<_>>()?;
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
                let (owned, _corpus) = self.fulltext_payloads(index_id, &seq, lane)?;
                let postings: Vec<(String, &[FtPosting])> = owned.iter().map(|(t, p)| (t.clone(), &p[..])).collect();
                let mode = crate::unicode::lower(&predicate_options.get("mode").map(|m| m.py_str()).unwrap_or_else(|| "all".into()));
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
            if !self.index_is_fresh(iid, lane) {
                return Err(Error::StaleIndex(format!(
                    "Index '{name}'/{} is missing or older than the data. Run build_indexes().",
                    lane_name(lane)
                )));
            }
            let p = self.posting_for_condition(iid, cond, lane)?;
            if p.is_empty() {
                return Ok(Vec::new());
            }
            postings.push(p);
        }
        let mut result: Vec<Nat> = if postings.is_empty() {
            let mut r = Vec::new();
            if let Some(d) = self.dense.get(&key) {
                if !self.overlay_keys.contains(&key) {
                    for seg in d.sorted() {
                        for i in 0..seg.count {
                            r.push(seg.u_start.add_u64(i));
                        }
                    }
                } else {
                    let mut seen = HashSet::new();
                    let mut segs: Vec<_> = d.segs.iter().collect();
                    segs.sort_by_key(|x| std::cmp::Reverse(x.block_id));
                    for seg in segs {
                        for i in 0..seg.count {
                            let u = seg.u_start.add_u64(i);
                            if !seen.insert(u.clone()) {
                                continue;
                            }
                            if self.deletes.get(&(realm.clone(), lane, u.clone())).map(|d| *d > seg.block_id).unwrap_or(false) {
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
        Ok(result)
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

#[cfg(test)]
mod perf_tests {
    use super::*;

    /// I001: under memory pressure the build splits into scan passes with
    /// identical postings; an index that cannot fit is refused before
    /// anything is written.
    #[test]
    fn build_passes_follow_the_memory_budget_i001() {
        use crate::address::NL;
        use crate::codec::{FieldDef, TypeCode};
        use crate::db::dense::DenseRows;
        use crate::db::OpenOptions;
        let d = std::env::temp_dir().join(format!("nhydb_i001_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let r = Nat::S(1);
        let mut db = Db::create(d.join("i.nhydb"), true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        let fields = ["a", "b", "c"].iter().enumerate().map(|(i, n)| FieldDef::new(Nat::S(i as u64 + 1), *n, TypeCode::VarUInt)).collect();
        db.create_shape(1, &r, "s", fields, "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
        for (id, v) in [(1u64, 1i64), (2, 2), (3, 3)] {
            db.create_index(id, &r, ["a", "b", "c"][id as usize - 1], &Val::Int(v), "").unwrap();
        }
        let rows: Vec<Vec<Val>> = (1..=2000u64).map(|u| vec![Val::Int((u % 7) as i64), Val::Int((u % 13) as i64), Val::Int((u % 101) as i64)]).collect();
        db.put_dense_batch(&r, &Nat::S(1), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
        // every index: 2000 references x 10 bytes
        assert_eq!(db.index_build_estimate(&db.index_defs[&1].clone()), 20_000);
        let finds = |db: &Db| -> Vec<Vec<Nat>> {
            [("a", 3i64), ("b", 5), ("c", 42)]
                .iter()
                .map(|(f, v)| db.find_u(&r, &[(f.to_string(), Cond::Eq(Val::Int(*v)))], NL, None, None).unwrap())
                .collect()
        };
        let one = db.build_indexes(None, true).unwrap();
        assert_eq!(one.get("passes"), Some(&Val::Int(1)));
        let want = finds(&db);
        assert_eq!(want[0].len(), 286);
        // half of 50 000 bytes holds one index per pass
        db.index_build_memory_override = Some(50_000);
        let three = db.build_indexes(None, true).unwrap();
        assert_eq!(three.get("passes"), Some(&Val::Int(3)));
        assert_eq!(three.get("records_scanned"), one.get("records_scanned"));
        assert_eq!(finds(&db), want);
        // one index alone needs more than the assumed memory: refused, nothing written
        db.index_build_memory_override = Some(15_000);
        let len = db.store.len();
        assert!(matches!(db.build_indexes(None, true), Err(Error::Memory(_))));
        assert_eq!(db.store.len(), len);
        assert_eq!(finds(&db), want);
        let _ = std::fs::remove_dir_all(&d);
    }

    fn big(shift: u32, add: u64) -> Nat {
        Nat::from_bigint(&((num_bigint::BigInt::from(1u8) << shift) + add)).unwrap()
    }

    /// P014: the u64 union equals the general `Nat` union; a single u > 2^64
    /// switches to the exact path.
    #[test]
    fn union_postings_small_and_big() {
        let a = Arc::new(vec![Nat::S(1), Nat::S(5), Nat::S(9), Nat::S(u64::MAX)]);
        let b = Arc::new(vec![Nat::S(2), Nat::S(5), Nat::S(u32::MAX as u64 + 3)]);
        let c = Arc::new(Vec::new());
        let got = union_postings(&[a.clone(), b.clone(), c.clone()]);
        assert_eq!(got, vec![Nat::S(1), Nat::S(2), Nat::S(5), Nat::S(9), Nat::S(u32::MAX as u64 + 3), Nat::S(u64::MAX)]);
        let d = Arc::new(vec![Nat::S(3), big(64, 0), big(70, 1)]);
        let got = union_postings(&[d, a, b, c]);
        assert_eq!(got.len(), 9);
        assert!(got.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(got[6], Nat::S(u64::MAX));
        assert_eq!(got[7], big(64, 0));
        assert_eq!(got[8], big(70, 1));
    }

    /// P020: build lists stay `u64` until the first u > 2^64, keep order and
    /// produce the same values as `Nat` lists.
    #[test]
    fn ulist_upgrades_exactly() {
        let mut l = UList::default();
        for u in [Nat::S(7), Nat::S(3), Nat::S(u32::MAX as u64)] {
            l.push(&u).unwrap();
        }
        assert!(matches!(l, UList::Tiny(_)));
        l.push(&Nat::S(u64::MAX)).unwrap();
        assert!(matches!(l, UList::Small(_)));
        assert_eq!(l.last(), Some(Nat::S(u64::MAX)));
        l.push(&big(64, 5)).unwrap();
        l.push(&Nat::S(1)).unwrap();
        assert!(matches!(l, UList::Big(_)));
        assert_eq!(l.len(), 6);
        l.sort_check();
        assert_eq!(l.to_nats(), vec![Nat::S(1), Nat::S(3), Nat::S(7), Nat::S(u32::MAX as u64), Nat::S(u64::MAX), big(64, 5)]);
        // straight from u32 to BigInt
        let mut t = UList::default();
        t.push(&Nat::S(9)).unwrap();
        t.push(&big(70, 1)).unwrap();
        assert!(matches!(t, UList::Big(_)));
        assert_eq!(t.to_nats(), vec![Nat::S(9), big(70, 1)]);
        let mut w = UList::Tiny(vec![9, 2, 4]);
        w.sort_check();
        assert_eq!(w.to_nats(), vec![Nat::S(2), Nat::S(4), Nat::S(9)]);
        let mut s = UList::Small(vec![9, 2, 4]);
        s.sort_check();
        assert_eq!(s.to_nats(), vec![Nat::S(2), Nat::S(4), Nat::S(9)]);
    }
}
