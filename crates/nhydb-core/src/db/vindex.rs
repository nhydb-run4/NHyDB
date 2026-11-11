//! Targeted access to the sparse cells of a u group without a full merge.
//!
//! A naive implementation (`_sparse_updates_for_u`) decodes, for every cell access, all
//! sparse records of all segments containing `u` and merges them into one
//! map. With COLUMN (u = field, v = object) and PACKED (all objects
//! on one u) that is the entire data set of the layout.
//!
//! Here an in-memory v index is built once per `(realm, lane, u)`
//! (file format unchanged), with two parts:
//! * **single-cell records** (one cell of `u` in a block – individual
//!   writes): a sorted map `v → newest record`. A newer single-cell record
//!   of the same v replaces the older one, so overwritten versions cost
//!   nothing (G3) and a live cell costs one map entry (G2);
//! * **multi-cell records**: their v range `[v_first, v_last]` in an
//!   interval index and, for records of more than `CK_EVERY` cells, jump
//!   marks every `CK_EVERY` cells (byte position, v and previous v, because
//!   v is delta-encoded).
//!
//! A point access takes the single-cell candidate and the multi-cell
//! records whose range contains `v` from the newest to the oldest block
//! (the newest state wins, as in the merge), jumps via a mark close to `v`
//! and decodes only the matching cell. Range accesses work analogously.
//!
//! The cached indexes are bounded in bytes (`[cache] vindex_max_marks`, in
//! units of one jump mark) and in number (`vindex_max_indexes`), evicted
//! least recently used; an index larger than the whole budget is kept
//! alone. An index is brought up to date for blocks written later
//! (directory flushes included) instead of being rebuilt (G4).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::address::{validate_local_v, Lane};
use crate::codec::{decode_value, skip_raw, FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::nat::{get_vu, get_vu_u64, put_vu64, Nat};

use super::interval::IntervalIndex;
use super::layout::FieldResolver;
use super::{Db, SVal, SparseCellMeta, SparseSegment};

/// Distance between checkpoints in cells.
const CK_EVERY: u64 = 128;
/// Bytes of one budget unit of `[cache] vindex_max_marks` (one jump mark).
pub(crate) const MARK_BYTES: usize = std::mem::size_of::<Ck>();
/// Maximum number of u groups of one block indexed together.
const MAX_SIBLINGS: usize = 64;
/// Up to this many segments of a u are kept from the first pass of an
/// index build (no second visit of the directory).
const FEW_SEGMENTS: usize = 64;

/// Checkpoint: cell no. `idx` starts at `pos` (key byte), has the
/// value `v`, its predecessor is `prev`.
#[derive(Clone, Debug)]
struct Ck {
    v: Nat,
    prev: Nat,
    pos: u32,
    idx: u64,
}

/// A multi-cell sparse record (one u group in one block) in the v index.
#[derive(Clone, Debug)]
pub(crate) struct RecSpan {
    block_offset: u64,
    bid: u64,
    rel: u32,
    len: u32,
    count: u64,
    v_first: Nat,
    v_last: Nat,
    /// marks every `CK_EVERY` cells; empty for records of at most
    /// `CK_EVERY` cells (the window then starts at the first cell)
    ck: Box<[Ck]>,
}

/// A single-cell record: where the cell of one v lies.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Single {
    block_offset: u64,
    bid: u64,
    rel: u32,
    len: u32,
}

/// Estimated heap bytes per entry (record value, map/interval node share).
const SINGLE_BYTES: usize = std::mem::size_of::<Single>() + std::mem::size_of::<Nat>() + 24;
const REC_BYTES: usize = std::mem::size_of::<RecSpan>() + 4 * std::mem::size_of::<Nat>() + 8;

/// v index of a u group across all segments.
#[derive(Clone, Debug, Default)]
pub(crate) struct UIndex {
    /// newest single-cell record per v: sorted at the build, plus a map of
    /// the records added later (they win over the sorted part)
    singles: Vec<(Nat, Single)>,
    singles_new: BTreeMap<Nat, Single>,
    /// multi-cell records in block order (id = position)
    recs: Vec<RecSpan>,
    iv: IntervalIndex,
    /// registrations of the segment index taken into account
    segs_seen: usize,
    /// largest block id taken into account
    bid_seen: u64,
    ck_total: usize,
}

impl UIndex {
    /// Estimated heap bytes.
    pub(crate) fn bytes(&self) -> usize {
        self.singles.len() * (SINGLE_BYTES - 24) + self.singles_new.len() * SINGLE_BYTES + self.recs.len() * REC_BYTES + self.ck_total * MARK_BYTES
    }

    /// Newest single-cell record of `v`.
    fn single(&self, v: &Nat) -> Option<&Single> {
        if let Some(x) = self.singles_new.get(v) {
            return Some(x);
        }
        self.singles.binary_search_by(|(k, _)| k.cmp(v)).ok().map(|i| &self.singles[i].1)
    }

    fn push_rec(&mut self, r: RecSpan) {
        let id = self.recs.len();
        self.ck_total += r.ck.len();
        self.iv.insert(r.v_first.clone(), r.v_last.clone(), id);
        self.recs.push(r);
    }

    /// Adds records in block order (newer single cells replace older ones).
    fn add(&mut self, s: Scanned) {
        match s {
            Scanned::Single(v, x) => {
                match self.single(&v) {
                    Some(old) if old.bid > x.bid => {}
                    _ => {
                        self.singles_new.insert(v, x);
                    }
                }
            }
            Scanned::Multi(r) => self.push_rec(r),
        }
    }
}

/// Result of scanning one record.
enum Scanned {
    Single(Nat, Single),
    Multi(RecSpan),
}

/// Length of the varint holding the cell count (start of the first cell).
fn count_len(count: u64) -> usize {
    let mut b = Vec::with_capacity(10);
    put_vu64(&mut b, count);
    b.len()
}

/// Header of a sparse cell; `p` then points to the start of the value.
#[derive(Clone, Debug)]
pub(crate) struct CellHead {
    pub v: Nat,
    pub tomb: bool,
    pub null: bool,
    pub selftyped: bool,
    pub typ: Option<TypeCode>,
}

/// Reads the cell header (`vu (v_delta<<3 | flags) [typecode]`).
pub(crate) fn read_head(raw: &[u8], p: &mut usize, prev: &Nat, realm: &Nat) -> Result<CellHead> {
    let key = get_vu(raw, p)?;
    let (flags, delta) = match &key {
        Nat::S(k) => (k & 7, Nat::S(k >> 3)),
        Nat::B(b) => ((&**b % 8u32).try_into().unwrap_or(0u64), Nat::from(&**b >> 3usize)),
    };
    if delta.is_zero() {
        return Err(Error::Corruption("SPARSE v delta must be >0".into()));
    }
    let v = prev.add(&delta);
    validate_local_v(&v, Some(realm))?;
    let tomb = flags & 1 != 0;
    let selftyped = flags & 2 != 0;
    let null = flags & 4 != 0;
    let mut typ = None;
    if selftyped && !tomb {
        let Some(&code) = raw.get(*p) else {
            return Err(Error::Corruption("SPARSE type code missing".into()));
        };
        *p += 1;
        typ = Some(TypeCode::from_code(code).ok_or_else(|| Error::Corruption(format!("SPARSE type code {code} unknown")))?);
    }
    Ok(CellHead { v, tomb, null, selftyped, typ })
}

/// Lazily built type resolution of a u group.
pub(crate) struct LazyResolver<'a> {
    db: &'a Db,
    realm: &'a Nat,
    u: &'a Nat,
    r: Option<FieldResolver<'a>>,
    fast: Option<FastTyp>,
}

/// Type without `FieldDef` access (only for skipping).
enum FastTyp {
    No,
    Fixed(TypeCode),
    /// span, type per offset (dense, index = offset)
    Packed(u64, Vec<Option<TypeCode>>),
}

impl<'a> LazyResolver<'a> {
    pub(crate) fn new(db: &'a Db, realm: &'a Nat, u: &'a Nat) -> Self {
        LazyResolver { db, realm, u, r: None, fast: None }
    }

    fn resolver(&mut self) -> &FieldResolver<'a> {
        let (db, realm, u) = (self.db, self.realm, self.u);
        self.r.get_or_insert_with(|| db.field_resolver(realm, u))
    }

    fn declared(&mut self, v: &Nat) -> Result<std::borrow::Cow<'_, FieldDef>> {
        let (db, realm, u) = (self.db, self.realm, self.u);
        let r = self.r.get_or_insert_with(|| db.field_resolver(realm, u));
        r.get(db, realm, u, v)?
            .ok_or_else(|| Error::Corruption(format!("R{realm},u{u},v{v}: type cannot be derived")))
    }

    #[inline]
    fn fast_typ(&mut self, v: &Nat) -> Option<TypeCode> {
        if self.fast.is_none() {
            let f = match self.resolver() {
                FieldResolver::Fixed(Some(f)) => FastTyp::Fixed(f.typ),
                FieldResolver::Packed { span: Nat::S(s), fields }
                    if fields.iter().all(|(o, _)| o.as_u64().map(|o| o < 4096).unwrap_or(false)) =>
                {
                    let mut tbl = Vec::new();
                    for (o, f) in fields {
                        let o = o.as_u64().unwrap() as usize;
                        if tbl.len() <= o {
                            tbl.resize(o + 1, None);
                        }
                        tbl[o] = Some(f.typ);
                    }
                    FastTyp::Packed(*s, tbl)
                }
                _ => FastTyp::No,
            };
            self.fast = Some(f);
        }
        match (self.fast.as_ref().unwrap(), v) {
            (FastTyp::Fixed(t), _) => Some(*t),
            (FastTyp::Packed(s, tbl), Nat::S(x)) => tbl.get(((x - 1) % *s) as usize).copied().flatten(),
            _ => None,
        }
    }

    /// Decodes the cell value (semantics of `_decode_sparse_record`).
    pub(crate) fn decode(&mut self, h: &CellHead, raw: &[u8], p: &mut usize) -> Result<SVal> {
        if h.tomb {
            return Ok(SVal::Tomb);
        }
        if h.null {
            return Ok(SVal::V(crate::val::Val::None));
        }
        if h.selftyped {
            let f = FieldDef::new(h.v.clone(), format!("v{}", h.v), h.typ.unwrap());
            return Ok(SVal::V(decode_value(&f, raw, p)?));
        }
        let f = self.declared(&h.v)?;
        Ok(SVal::V(decode_value(&f, raw, p)?))
    }

    /// Encoded length of the cell value at `raw[p..]` (see
    /// [`crate::codec::raw_extent`]); `Ok(None)`: more bytes are needed.
    pub(crate) fn extent(&mut self, h: &CellHead, raw: &[u8], p: usize) -> Result<Option<usize>> {
        if h.tomb || h.null {
            return Ok(Some(0));
        }
        let rest = raw.get(p..).unwrap_or(&[]);
        if h.selftyped {
            return crate::codec::raw_extent(h.typ.unwrap(), rest);
        }
        if let Some(t) = self.fast_typ(&h.v) {
            if let Ok(x) = crate::codec::raw_extent(t, rest) {
                return Ok(x);
            }
        }
        let f = self.declared(&h.v)?;
        crate::codec::raw_extent(f.typ, rest)
    }

    /// Skips the cell value.
    pub(crate) fn skip(&mut self, h: &CellHead, raw: &[u8], p: &mut usize) -> Result<()> {
        if h.tomb || h.null {
            return Ok(());
        }
        if h.selftyped {
            return skip_raw(h.typ.unwrap(), raw, p, || format!("v{}", h.v));
        }
        if let Some(t) = self.fast_typ(&h.v) {
            let mut q = *p;
            if skip_raw(t, raw, &mut q, String::new).is_ok() {
                *p = q;
                return Ok(());
            }
        }
        let f = self.declared(&h.v)?;
        skip_raw(f.typ, raw, p, || f.name.clone())
    }
}

impl Db {
    /// Decodes a u group (`_decode_sparse_record`), type resolution
    /// once per group instead of per cell.
    /// `_decode_sparse_selected` (index build): type first from the
    /// extension definition, then from the layout; stops after the first
    /// cell with `v > max_v` (without a trailing-bytes check).
    pub(crate) fn decode_sparse_selected(&self, realm: &Nat, u: &Nat, raw: &[u8], max_v: &Nat) -> Result<Vec<(Nat, SparseCellMeta)>> {
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let mut res = LazyResolver::new(self, realm, u);
        let mut p = 0usize;
        let count = get_vu_u64(raw, &mut p, "SPARSE count")?;
        let mut prev = Nat::ZERO;
        let mut out: Vec<(Nat, SparseCellMeta)> = Vec::new();
        for _ in 0..count {
            let h = read_head(raw, &mut p, &prev, realm)?;
            let value = if h.tomb {
                SVal::Tomb
            } else if h.null {
                SVal::V(crate::val::Val::None)
            } else if h.selftyped {
                let f = FieldDef::new(h.v.clone(), format!("v{}", h.v), h.typ.unwrap());
                SVal::V(decode_value(&f, raw, &mut p)?)
            } else if let Some(ext) = self.extension_defs.get(&(realm.clone(), h.v.clone())) {
                SVal::V(decode_value(&ext.field, raw, &mut p)?)
            } else {
                res.decode(&h, raw, &mut p)?
            };
            let meta = SparseCellMeta { value, typ: h.typ, selftyped: h.selftyped };
            let stop = &h.v > max_v;
            match out.last_mut() {
                Some((lv, slot)) if *lv == h.v => *slot = meta,
                _ => out.push((h.v.clone(), meta)),
            }
            prev = h.v;
            if stop {
                break;
            }
        }
        Ok(out)
    }

    pub(crate) fn decode_sparse_record(&self, realm: &Nat, u: &Nat, raw: &[u8]) -> Result<Vec<(Nat, SparseCellMeta)>> {
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let mut res = LazyResolver::new(self, realm, u);
        let mut p = 0usize;
        let count = get_vu_u64(raw, &mut p, "SPARSE count")?;
        let mut prev = Nat::ZERO;
        let mut out: Vec<(Nat, SparseCellMeta)> = Vec::with_capacity(count.min(raw.len() as u64) as usize);
        for _ in 0..count {
            let h = read_head(raw, &mut p, &prev, realm)?;
            let value = res.decode(&h, raw, &mut p)?;
            let meta = SparseCellMeta { value, typ: h.typ, selftyped: h.selftyped };
            match out.last_mut() {
                Some((lv, slot)) if *lv == h.v => *slot = meta,
                _ => out.push((h.v.clone(), meta)),
            }
            prev = h.v;
        }
        if p != raw.len() {
            return Err(Error::Corruption("SPARSE record trailing bytes".into()));
        }
        Ok(out)
    }

    /// Full pass over a record without decoding: v range and
    /// checkpoints. `None` for an empty record.
    fn scan_record(&self, realm: &Nat, u: &Nat, raw: &[u8], block_offset: u64, bid: u64, rel: usize) -> Result<Option<Scanned>> {
        if raw.is_empty() {
            return Ok(None);
        }
        let mut res = LazyResolver::new(self, realm, u);
        let mut p = 0usize;
        let count = get_vu_u64(raw, &mut p, "SPARSE count")?;
        if count == 0 {
            if p != raw.len() {
                return Err(Error::Corruption("SPARSE record trailing bytes".into()));
            }
            return Ok(None);
        }
        let rel32 = u32::try_from(rel).map_err(|_| Error::Corruption("SPARSE record too large".into()))?;
        let len32 = u32::try_from(raw.len()).map_err(|_| Error::Corruption("SPARSE record too large".into()))?;
        let mut prev = Nat::ZERO;
        let mut ck = Vec::new();
        let mut v_first = Nat::ZERO;
        for i in 0..count {
            let pos = p;
            let h = read_head(raw, &mut p, &prev, realm)?;
            if count > CK_EVERY && i % CK_EVERY == 0 {
                let pos = u32::try_from(pos).map_err(|_| Error::Corruption("SPARSE record too large".into()))?;
                ck.push(Ck { v: h.v.clone(), prev: prev.clone(), pos, idx: i });
            }
            if i == 0 {
                v_first = h.v.clone();
            }
            res.skip(&h, raw, &mut p)?;
            prev = h.v;
        }
        if p != raw.len() {
            return Err(Error::Corruption("SPARSE record trailing bytes".into()));
        }
        if count == 1 {
            return Ok(Some(Scanned::Single(v_first, Single { block_offset, bid, rel: rel32, len: len32 })));
        }
        Ok(Some(Scanned::Multi(RecSpan {
            block_offset,
            bid,
            rel: rel32,
            len: len32,
            count,
            v_first,
            v_last: prev,
            ck: ck.into_boxed_slice(),
        })))
    }

    /// Scans the records of the u groups `us` in one block into `out`
    /// (index k of `us` → `out[k]`).
    fn scan_block_groups(&self, realm: &Nat, us: &[Nat], seg: &SparseSegment, out: &mut [Vec<Scanned>]) -> Result<()> {
        // blocks of one u group (individual writes): decoded on the fly,
        // not cached (the cache would hold ~100 bytes per such block);
        // blocks of many groups: cached (decoded once for all their u)
        let bi = if seg.group_count <= 1 { self.sparse_block_index_uncached(seg)? } else { self.sparse_block_index(seg)? };
        let acc = self.block_access(seg.block_offset)?;
        for (k, uu) in us.iter().enumerate() {
            let Ok(j) = bi.binary_search_by(|(x, _, _)| x.cmp(uu)) else {
                continue;
            };
            let (_, abs, len) = &bi[j];
            let rel = (abs - seg.payload_offset) as usize;
            let raw = self.access_range(&acc, seg.block_offset, rel, *len as usize)?;
            if let Some(r) = self.scan_record(realm, uu, &raw, seg.block_offset, seg.block_id, rel)? {
                out[k].push(r);
            }
        }
        Ok(())
    }

    /// v index of the u group (cached).
    pub(crate) fn sparse_uindex(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<Option<Arc<UIndex>>> {
        let Some(idx) = self.sparse.get(&(realm.clone(), lane)) else {
            return Ok(None);
        };
        let key = (realm.clone(), lane, u.clone());
        let n = idx.idx.len() as usize;
        // P019: an index built before later sparse blocks were written is
        // brought up to date here, for this u only (the write path no longer
        // touches every cached index of the realm/lane). The later blocks
        // are found by block id, in the tail or – after a directory flush –
        // in the directory pages holding newer blocks (G4).
        let stale = {
            let mut c = self.cache();
            let c = &mut *c;
            c.uindex_tick += 1;
            let tick = c.uindex_tick;
            match c.sparse_uindex.get_mut(&key) {
                Some((x, t)) if x.segs_seen == n => {
                    *t = tick;
                    return Ok(Some(x.clone()));
                }
                Some((x, _)) if x.segs_seen < n => {
                    let (x, _) = c.sparse_uindex.remove(&key).expect("present");
                    c.uindex_bytes = c.uindex_bytes.saturating_sub(x.bytes());
                    Some(x)
                }
                _ => None,
            }
        };
        if let Some(mut ui) = stale {
            let mut newer: Vec<SparseSegment> = Vec::new();
            match idx.idx.tail_since(ui.segs_seen as u64) {
                Some(t) => newer.extend(t.iter().filter(|s| &s.u_start <= u && u <= &s.u_end).cloned()),
                None => idx.idx.visit_containing(self, u, ui.bid_seen, &mut |s| {
                    newer.push(s.clone());
                    Ok(())
                })?,
            }
            newer.sort_unstable_by_key(|s| s.block_id);
            let m = Arc::make_mut(&mut ui);
            let mut out = vec![Vec::new()];
            for seg in &newer {
                self.scan_block_groups(realm, std::slice::from_ref(u), seg, &mut out)?;
                for r in out[0].drain(..) {
                    m.add(r);
                }
                m.bid_seen = m.bid_seen.max(seg.block_id);
            }
            m.segs_seen = n;
            self.store_uindex(key, ui.clone(), false);
            return Ok(Some(ui));
        }
        // block ids of all segments holding u, and the oldest of them
        let mut hit_ids: Vec<u64> = Vec::new();
        let mut first: Option<SparseSegment> = None;
        // few segments (the common case) are kept for the scan below
        // instead of being visited a second time
        let mut few: Vec<SparseSegment> = Vec::new();
        idx.idx.visit_containing(self, u, 0, &mut |s| {
            hit_ids.push(s.block_id);
            if hit_ids.len() <= FEW_SEGMENTS {
                few.push(s.clone());
            } else {
                if hit_ids.len() == FEW_SEGMENTS + 1 {
                    first = few.drain(..).min_by_key(|x| x.block_id);
                }
                if first.as_ref().is_none_or(|f| s.block_id < f.block_id) {
                    first = Some(s.clone());
                }
            }
            Ok(())
        })?;
        if hit_ids.len() <= FEW_SEGMENTS {
            first = few.iter().min_by_key(|x| x.block_id).cloned();
        }
        hit_ids.sort_unstable();
        // Index sibling u groups with the same segment set (COLUMN: all
        // fields live in the same blocks) in the same pass,
        // so that each block is read only once.
        let mut us: Vec<Nat> = vec![u.clone()];
        if let Some(first) = &first {
            let bi = self.sparse_block_index(first)?;
            if bi.len() <= MAX_SIBLINGS {
                let mut ids: Vec<u64> = Vec::new();
                for (su, _, _) in bi.iter() {
                    if su == u {
                        continue;
                    }
                    let fresh = self
                        .cache()
                        .sparse_uindex
                        .get(&(realm.clone(), lane, su.clone()))
                        .map(|(x, _)| x.segs_seen == n)
                        .unwrap_or(false);
                    if fresh {
                        continue;
                    }
                    ids.clear();
                    idx.idx.visit_containing(self, su, 0, &mut |s| {
                        ids.push(s.block_id);
                        Ok(())
                    })?;
                    ids.sort_unstable();
                    if ids == hit_ids {
                        us.push(su.clone());
                    }
                }
            }
        }
        let n_hits = hit_ids.len();
        drop(hit_ids);
        // records of every segment, without keeping the segments
        let mut scanned: Vec<Vec<Scanned>> = us.iter().map(|_| Vec::new()).collect();
        if n_hits <= FEW_SEGMENTS {
            for seg in &few {
                self.scan_block_groups(realm, &us, seg, &mut scanned)?;
            }
        } else {
            idx.idx.visit_containing(self, u, 0, &mut |seg| self.scan_block_groups(realm, &us, seg, &mut scanned))?;
        }
        let bid_seen = self.next_block_id.saturating_sub(1);
        let mut result = None;
        // store the requested u group last (survives a budget eviction)
        for (k, mut recs) in scanned.into_iter().enumerate().rev() {
            recs.sort_unstable_by_key(|r| match r {
                Scanned::Single(_, x) => x.bid,
                Scanned::Multi(r) => r.bid,
            });
            let mut ui = UIndex { segs_seen: n, bid_seen, ..Default::default() };
            // single cells: newest per v, built in one sorted pass
            let mut singles: Vec<(Nat, Single)> = Vec::new();
            for r in recs {
                match r {
                    Scanned::Single(v, x) => singles.push((v, x)),
                    Scanned::Multi(r) => ui.push_rec(r),
                }
            }
            singles.sort_by(|a, b| (&a.0, a.1.bid).cmp(&(&b.0, b.1.bid)));
            singles.dedup_by(|later, earlier| {
                if later.0 == earlier.0 {
                    *earlier = later.clone();
                    true
                } else {
                    false
                }
            });
            singles.shrink_to_fit();
            ui.singles = singles;
            let arc = Arc::new(ui);
            self.store_uindex((realm.clone(), lane, us[k].clone()), arc.clone(), true);
            if k == 0 {
                result = Some(arc);
            }
        }
        Ok(result)
    }

    /// Caches an index within the byte and count budget (least recently
    /// used first out; an index above the whole budget is kept alone).
    fn store_uindex(&self, key: (Nat, Lane, Nat), ui: Arc<UIndex>, built: bool) {
        let budget = self.config.vindex_max_marks.saturating_mul(MARK_BYTES);
        let max_n = self.config.vindex_max_indexes.max(1);
        let n = ui.bytes();
        let mut c = self.cache();
        let c = &mut *c;
        if built {
            c.uindex_builds += 1;
        } else {
            c.uindex_catchups += 1;
        }
        if let Some((old, _)) = c.sparse_uindex.remove(&key) {
            c.uindex_bytes = c.uindex_bytes.saturating_sub(old.bytes());
        }
        if n > budget {
            c.sparse_uindex.clear();
            c.uindex_bytes = 0;
        }
        if !c.sparse_uindex.is_empty() && (c.uindex_bytes + n > budget || c.sparse_uindex.len() >= max_n) {
            // LRU in batches: drop the least recently used indexes until a
            // quarter of the budget and of the count is free (one sort per
            // batch instead of a scan per insert)
            let byte_limit = (budget - budget / 4).saturating_sub(n);
            let count_limit = max_n - max_n / 4;
            // the oldest quarter by tick (linear selection, no full sort);
            // repeated while the bytes are still above the limit
            while !c.sparse_uindex.is_empty() && (c.uindex_bytes > byte_limit || c.sparse_uindex.len() >= count_limit) {
                let mut ticks: Vec<u64> = c.sparse_uindex.values().map(|(_, t)| *t).collect();
                let k = (ticks.len() / 4).max(1).min(ticks.len() - 1);
                let cutoff = *ticks.select_nth_unstable(k).1;
                let mut freed = 0usize;
                c.sparse_uindex.retain(|_, (x, t)| {
                    let keep = *t > cutoff;
                    if !keep {
                        freed += x.bytes();
                    }
                    keep
                });
                c.uindex_bytes = c.uindex_bytes.saturating_sub(freed);
            }
        }
        c.uindex_tick += 1;
        c.uindex_bytes += n;
        c.sparse_uindex.insert(key, (ui, c.uindex_tick));
    }

    /// Decodes the cell of a single-cell record (`None`: it is not `v`).
    fn read_single(&self, x: &Single, realm: &Nat, u: &Nat, v: &Nat) -> Result<Option<SVal>> {
        let raw = self.block_range(x.block_offset, x.rel as usize, x.len as usize)?;
        let mut p = 0usize;
        let count = get_vu_u64(&raw, &mut p, "SPARSE count")?;
        if count != 1 {
            return Err(Error::Corruption("SPARSE record changed under the v index".into()));
        }
        let h = read_head(&raw, &mut p, &Nat::ZERO, realm)?;
        if &h.v != v {
            return Ok(None);
        }
        let mut res = LazyResolver::new(self, realm, u);
        res.decode(&h, &raw, &mut p).map(Some)
    }

    /// Newest state of a sparse cell or `None` (never written).
    pub(crate) fn sparse_point(&self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<Option<SVal>> {
        let Some(ui) = self.sparse_uindex(realm, u, lane)? else {
            return Ok(None);
        };
        let single = ui.single(v);
        let floor = single.map(|x| x.bid).unwrap_or(0);
        let mut res = LazyResolver::new(self, realm, u);
        // multi-cell records newer than the single-cell candidate, newest
        // first; the newest one usually holds v (no collecting, no sort)
        let mut best: Option<usize> = None;
        ui.iv.stab_each(v, |i| {
            if ui.recs[i].bid > floor && best.is_none_or(|b| i > b) {
                best = Some(i);
            }
        });
        if let Some(b) = best {
            if let Some(s) = self.find_in_rec(&ui.recs[b], v, realm, &mut res)? {
                return Ok(Some(s));
            }
            let mut hits = Vec::new();
            ui.iv.stab(v, &mut hits);
            hits.retain(|&i| i != b && ui.recs[i].bid > floor);
            hits.sort_unstable_by(|a, b| b.cmp(a));
            for i in hits {
                if let Some(s) = self.find_in_rec(&ui.recs[i], v, realm, &mut res)? {
                    return Ok(Some(s));
                }
            }
        }
        if let Some(x) = single {
            if let Some(s) = self.read_single(x, realm, u, v)? {
                return Ok(Some(s));
            }
            return Err(Error::Corruption("SPARSE record changed under the v index".into()));
        }
        Ok(None)
    }

    fn find_in_rec(&self, r: &RecSpan, v: &Nat, realm: &Nat, res: &mut LazyResolver<'_>) -> Result<Option<SVal>> {
        if v < &r.v_first || v > &r.v_last {
            return Ok(None);
        }
        // only the bytes between the two checkpoints around v (P001)
        let w = window(r, v, v);
        let raw = self.block_range(r.block_offset, r.rel as usize + w.start, w.end - w.start)?;
        find_in_record(&w, &raw, v, realm, res)
    }

    /// Newest states of all cells with `lo <= v <= hi` (incl. tombstones).
    pub(crate) fn sparse_range(&self, realm: &Nat, u: &Nat, lo: &Nat, hi: &Nat, lane: Lane) -> Result<BTreeMap<Nat, SVal>> {
        let mut out = BTreeMap::new();
        if hi < lo {
            return Ok(out);
        }
        let Some(ui) = self.sparse_uindex(realm, u, lane)? else {
            return Ok(out);
        };
        // all candidate records in block order: newer states overwrite
        enum Cand<'a> {
            Rec(&'a RecSpan),
            Single(&'a Nat, &'a Single),
        }
        let mut cands: Vec<(u64, Cand<'_>)> = Vec::new();
        let mut hits = Vec::new();
        ui.iv.overlapping(lo, hi, &mut hits);
        cands.extend(hits.into_iter().map(|i| (ui.recs[i].bid, Cand::Rec(&ui.recs[i]))));
        let a = ui.singles.partition_point(|(k, _)| k < lo);
        let b = ui.singles.partition_point(|(k, _)| k <= hi);
        cands.extend(
            ui.singles[a..b]
                .iter()
                .filter(|(v, _)| !ui.singles_new.contains_key(v))
                .map(|(v, x)| (x.bid, Cand::Single(v, x))),
        );
        cands.extend(ui.singles_new.range(lo.clone()..=hi.clone()).map(|(v, x)| (x.bid, Cand::Single(v, x))));
        cands.sort_unstable_by_key(|c| c.0);
        let mut res = LazyResolver::new(self, realm, u);
        for (_, c) in cands {
            match c {
                Cand::Rec(r) => {
                    let w = window(r, lo, hi);
                    let raw = self.block_range(r.block_offset, r.rel as usize + w.start, w.end - w.start)?;
                    range_in_record(&w, &raw, lo, hi, realm, &mut res, &mut out)?;
                }
                Cand::Single(v, x) => {
                    let s = self.read_single(x, realm, u, v)?.ok_or_else(|| Error::Corruption("SPARSE record changed under the v index".into()))?;
                    out.insert(v.clone(), s);
                }
            }
        }
        Ok(out)
    }
}

/// Part of a sparse record that can contain cells with `lo <= v <= hi`:
/// from the checkpoint at or before `lo` up to the first checkpoint with
/// `v > hi` (exclusive) or the end of the record. Byte positions are
/// relative to the record, `start` is the first byte of the window.
struct Window {
    start: usize,
    prev: Nat,
    start_idx: u64,
    end: usize,
    end_idx: u64,
}

fn window(r: &RecSpan, lo: &Nat, hi: &Nat) -> Window {
    if r.ck.is_empty() {
        return Window { start: count_len(r.count), prev: Nat::ZERO, start_idx: 0, end: r.len as usize, end_idx: r.count };
    }
    let i = r.ck.partition_point(|c| &c.v <= lo);
    let c = &r.ck[i.saturating_sub(1)];
    let j = r.ck.partition_point(|c| &c.v <= hi);
    let (end, end_idx) = match r.ck.get(j) {
        Some(n) if n.idx > c.idx => (n.pos as usize, n.idx),
        Some(_) => (c.pos as usize, c.idx),
        None => (r.len as usize, r.count),
    };
    Window { start: c.pos as usize, prev: c.prev.clone(), start_idx: c.idx, end, end_idx }
}

fn find_in_record(w: &Window, raw: &[u8], v: &Nat, realm: &Nat, res: &mut LazyResolver<'_>) -> Result<Option<SVal>> {
    let mut p = 0usize;
    let mut prev = w.prev.clone();
    for _ in w.start_idx..w.end_idx {
        let h = read_head(raw, &mut p, &prev, realm)?;
        if &h.v == v {
            return res.decode(&h, raw, &mut p).map(Some);
        }
        if &h.v > v {
            return Ok(None);
        }
        res.skip(&h, raw, &mut p)?;
        prev = h.v;
    }
    Ok(None)
}

fn range_in_record(
    w: &Window,
    raw: &[u8],
    lo: &Nat,
    hi: &Nat,
    realm: &Nat,
    res: &mut LazyResolver<'_>,
    out: &mut BTreeMap<Nat, SVal>,
) -> Result<()> {
    let mut p = 0usize;
    let mut prev = w.prev.clone();
    for _ in w.start_idx..w.end_idx {
        let h = read_head(raw, &mut p, &prev, realm)?;
        if &h.v > hi {
            break;
        }
        if &h.v >= lo {
            let s = res.decode(&h, raw, &mut p)?;
            out.insert(h.v.clone(), s);
        } else {
            res.skip(&h, raw, &mut p)?;
        }
        prev = h.v;
    }
    Ok(())
}

/// Process pot: evicts the least recently used v indexes until at most
/// `limit` bytes are held; returns the bytes freed.
pub(crate) fn evict_uindex_to(c: &mut super::Caches, limit: usize) -> usize {
    if c.uindex_bytes <= limit {
        return 0;
    }
    let before = c.uindex_bytes;
    let mut order: Vec<(u64, usize)> = c.sparse_uindex.values().map(|(x, t)| (*t, x.bytes())).collect();
    order.sort_unstable();
    let mut need = c.uindex_bytes - limit;
    let mut cut = 0u64;
    for (t, b) in order {
        cut = t;
        if b >= need {
            break;
        }
        need -= b;
    }
    let mut freed = 0usize;
    c.sparse_uindex.retain(|_, (x, t)| {
        let keep = *t > cut;
        if !keep {
            freed += x.bytes();
        }
        keep
    });
    c.uindex_bytes = c.uindex_bytes.saturating_sub(freed);
    before - c.uindex_bytes
}
