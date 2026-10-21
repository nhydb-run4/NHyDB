//! Targeted access to the sparse cells of a u group without a full merge.
//!
//! A naive implementation (`_sparse_updates_for_u`) decodes, for every cell access, all
//! sparse records of all segments containing `u` and merges them into one
//! map. With COLUMN (u = field, v = object) and PACKED (all objects
//! on one u) that is the entire data set of the layout.
//!
//! Here an in-memory v index is built once per `(realm, lane, u)`
//! (file format unchanged):
//! * per sparse record its v range `[v_first, v_last]` in an
//!   interval index,
//! * per record checkpoints every `CK_EVERY` cells (byte position, v and
//!   previous v, because v is delta-encoded).
//!
//! A point access checks only records whose v range contains `v`, from the
//! newest to the oldest block (the newest state wins, as in the merge),
//! jumps via a checkpoint close to `v` and decodes only the
//! matching cell. Range accesses work analogously.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::address::{validate_local_v, Lane};
use crate::codec::{decode_value, skip_raw, FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::nat::{get_vu, get_vu_u64, Nat};

use super::interval::IntervalIndex;
use super::layout::FieldResolver;
use super::{Db, SVal, SparseCellMeta, SparseSegment};

/// Distance between checkpoints in cells.
const CK_EVERY: u64 = 128;
/// Upper bound of all held checkpoints (≈ 50 B per checkpoint).
pub(crate) const CK_BUDGET: usize = 4 << 20;
/// Upper bound of held v indexes.
const UINDEX_MAX_ENTRIES: usize = 4096;
/// Maximum number of u groups of one block indexed together.
const MAX_SIBLINGS: usize = 64;

/// Checkpoint: cell no. `idx` starts at `pos` (key byte), has the
/// value `v`, its predecessor is `prev`.
#[derive(Clone, Debug)]
struct Ck {
    v: Nat,
    prev: Nat,
    pos: u32,
    idx: u64,
}

/// A sparse record (one u group in one block) in the v index.
#[derive(Clone, Debug)]
pub(crate) struct RecSpan {
    block_offset: u64,
    rel: usize,
    len: usize,
    count: u64,
    v_first: Nat,
    v_last: Nat,
    ck: Vec<Ck>,
}

/// v index of a u group across all segments.
#[derive(Clone, Debug, Default)]
pub(crate) struct UIndex {
    recs: Vec<RecSpan>,
    iv: IntervalIndex,
    /// Number of segments taken into account (for consistency checks)
    segs_seen: usize,
}

impl UIndex {
    pub(crate) fn ck_count(&self) -> usize {
        self.recs.iter().map(|r| r.ck.len()).sum()
    }

    fn push(&mut self, r: RecSpan) {
        let id = self.recs.len();
        self.iv.insert(r.v_first.clone(), r.v_last.clone(), id);
        self.recs.push(r);
    }
}

/// Header of a sparse cell; `p` then points to the start of the value.
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
    fn scan_record(&self, realm: &Nat, u: &Nat, raw: &[u8], block_offset: u64, rel: usize) -> Result<Option<RecSpan>> {
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
        let mut prev = Nat::ZERO;
        let mut ck = Vec::with_capacity((count / CK_EVERY + 1).min(raw.len() as u64) as usize);
        let mut v_first = Nat::ZERO;
        for i in 0..count {
            let pos = p;
            let h = read_head(raw, &mut p, &prev, realm)?;
            if i % CK_EVERY == 0 {
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
        Ok(Some(RecSpan { block_offset, rel, len: raw.len(), count, v_first, v_last: prev, ck }))
    }

    fn rec_span_for(&self, realm: &Nat, u: &Nat, seg: &SparseSegment) -> Result<Option<RecSpan>> {
        let Some((raw, rel)) = self.read_sparse_record_bytes(seg, u)? else {
            return Ok(None);
        };
        self.scan_record(realm, u, &raw, seg.block_offset, rel)
    }

    /// v index of the u group (cached).
    pub(crate) fn sparse_uindex(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<Option<Arc<UIndex>>> {
        let Some(idx) = self.sparse.get(&(realm.clone(), lane)) else {
            return Ok(None);
        };
        let key = (realm.clone(), lane, u.clone());
        let n = idx.segs.len();
        // P019: an index built before later sparse blocks were written is
        // brought up to date here, for this u only (the write path no longer
        // touches every cached index of the realm/lane).
        let stale = {
            let mut c = self.cache();
            match c.sparse_uindex.get(&key) {
                Some(x) if x.segs_seen == n => return Ok(Some(x.clone())),
                Some(x) if x.segs_seen < n => {
                    let x = c.sparse_uindex.remove(&key).expect("present");
                    c.uindex_ck = c.uindex_ck.saturating_sub(x.ck_count());
                    Some(x)
                }
                _ => None,
            }
        };
        if let Some(mut ui) = stale {
            let m = Arc::make_mut(&mut ui);
            for seg in &idx.segs[m.segs_seen..] {
                if &seg.u_start <= u && u <= &seg.u_end {
                    if let Some(r) = self.rec_span_for(realm, u, seg)? {
                        m.push(r);
                    }
                }
            }
            m.segs_seen = n;
            self.store_uindex(key, ui.clone());
            return Ok(Some(ui));
        }
        let mut hits = Vec::new();
        idx.iv.stab(u, &mut hits);
        hits.sort_unstable();
        // Index sibling u groups with the same segment set (COLUMN: all
        // fields live in the same blocks) in the same pass,
        // so that each block is read only once.
        let mut us: Vec<Nat> = vec![u.clone()];
        if let Some(&first) = hits.first() {
            let bi = self.sparse_block_index(&idx.segs[first])?;
            if bi.len() <= MAX_SIBLINGS {
                let mut h2 = Vec::new();
                for (su, _, _) in bi.iter() {
                    if su == u {
                        continue;
                    }
                    let fresh = self
                        .cache()
                        .sparse_uindex
                        .get(&(realm.clone(), lane, su.clone()))
                        .map(|x| x.segs_seen == idx.segs.len())
                        .unwrap_or(false);
                    if fresh {
                        continue;
                    }
                    h2.clear();
                    idx.iv.stab(su, &mut h2);
                    h2.sort_unstable();
                    if h2 == hits {
                        us.push(su.clone());
                    }
                }
            }
        }
        let mut uis: Vec<UIndex> = us.iter().map(|_| UIndex { segs_seen: idx.segs.len(), ..Default::default() }).collect();
        for &i in &hits {
            let seg = &idx.segs[i];
            let bi = self.sparse_block_index(seg)?;
            let acc = self.block_access(seg.block_offset)?;
            for (k, uu) in us.iter().enumerate() {
                let Ok(j) = bi.binary_search_by(|(x, _, _)| x.cmp(uu)) else {
                    continue;
                };
                let (_, abs, len) = &bi[j];
                let rel = (abs - seg.payload_offset) as usize;
                let raw = self.access_range(&acc, seg.block_offset, rel, *len as usize)?;
                if let Some(r) = self.scan_record(realm, uu, &raw, seg.block_offset, rel)? {
                    uis[k].push(r);
                }
            }
        }
        let mut result = None;
        // store the requested u group last (survives a budget flush)
        for (k, ui) in uis.into_iter().enumerate().rev() {
            let arc = Arc::new(ui);
            self.store_uindex((realm.clone(), lane, us[k].clone()), arc.clone());
            if k == 0 {
                result = Some(arc);
            }
        }
        Ok(result)
    }

    fn store_uindex(&self, key: (Nat, Lane, Nat), ui: Arc<UIndex>) {
        let n = ui.ck_count();
        let mut c = self.cache();
        if let Some(old) = c.sparse_uindex.remove(&key) {
            c.uindex_ck = c.uindex_ck.saturating_sub(old.ck_count());
        }
        if n > CK_BUDGET {
            return;
        }
        if c.uindex_ck + n > CK_BUDGET || c.sparse_uindex.len() >= UINDEX_MAX_ENTRIES {
            c.sparse_uindex.clear();
            c.uindex_ck = 0;
        }
        c.uindex_ck += n;
        c.sparse_uindex.insert(key, ui);
    }

    /// Newest state of a sparse cell or `None` (never written).
    pub(crate) fn sparse_point(&self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<Option<SVal>> {
        let Some(ui) = self.sparse_uindex(realm, u, lane)? else {
            return Ok(None);
        };
        let mut hits = Vec::new();
        ui.iv.stab(v, &mut hits);
        hits.sort_unstable_by(|a, b| b.cmp(a));
        let mut res = LazyResolver::new(self, realm, u);
        for i in hits {
            let r = &ui.recs[i];
            if v < &r.v_first || v > &r.v_last {
                continue;
            }
            // only the bytes between the two checkpoints around v (P001)
            let w = window(r, v, v);
            let raw = self.block_range(r.block_offset, r.rel + w.start, w.end - w.start)?;
            if let Some(s) = find_in_record(&w, &raw, v, realm, &mut res)? {
                return Ok(Some(s));
            }
        }
        Ok(None)
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
        let mut hits = Vec::new();
        ui.iv.overlapping(lo, hi, &mut hits);
        hits.sort_unstable();
        let mut res = LazyResolver::new(self, realm, u);
        for i in hits {
            let r = &ui.recs[i];
            let w = window(r, lo, hi);
            let raw = self.block_range(r.block_offset, r.rel + w.start, w.end - w.start)?;
            range_in_record(&w, &raw, lo, hi, realm, &mut res, &mut out)?;
        }
        Ok(out)
    }
}

/// Checkpoint with the largest `v <= x` (or the first one).
fn start_ck<'r>(r: &'r RecSpan, x: &Nat) -> &'r Ck {
    let i = r.ck.partition_point(|c| &c.v <= x);
    &r.ck[i.saturating_sub(1)]
}

/// Part of a sparse record that can contain cells with `lo <= v <= hi`:
/// from the checkpoint at or before `lo` up to the first checkpoint with
/// `v > hi` (exclusive) or the end of the record. Byte positions are
/// relative to the record, `start` is the first byte of the window.
struct Window<'r> {
    ck: &'r Ck,
    start: usize,
    end: usize,
    end_idx: u64,
}

fn window<'r>(r: &'r RecSpan, lo: &Nat, hi: &Nat) -> Window<'r> {
    let c = start_ck(r, lo);
    let j = r.ck.partition_point(|c| &c.v <= hi);
    let (end, end_idx) = match r.ck.get(j) {
        Some(n) if n.idx > c.idx => (n.pos as usize, n.idx),
        Some(_) => (c.pos as usize, c.idx),
        None => (r.len, r.count),
    };
    Window { ck: c, start: c.pos as usize, end, end_idx }
}

fn find_in_record(w: &Window<'_>, raw: &[u8], v: &Nat, realm: &Nat, res: &mut LazyResolver<'_>) -> Result<Option<SVal>> {
    let mut p = 0usize;
    let mut prev = w.ck.prev.clone();
    for _ in w.ck.idx..w.end_idx {
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
    w: &Window<'_>,
    raw: &[u8],
    lo: &Nat,
    hi: &Nat,
    realm: &Nat,
    res: &mut LazyResolver<'_>,
    out: &mut BTreeMap<Nat, SVal>,
) -> Result<()> {
    let mut p = 0usize;
    let mut prev = w.ck.prev.clone();
    for _ in w.ck.idx..w.end_idx {
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
