//! Sparse cell store (`SPARSE_CELLS`, tag `SP10`).
//!
//! Block layout:
//! `SP10 | vu realm | vu u_start | vu u_end | vu groups | vu cells | stream`
//! stream per u group: `vu u_delta | vu rec_len | record`
//! record: `vu count | (vu (v_delta<<3 | flags) [typecode] [value])*`
//! flags: 1 = tombstone, 2 = self-typed, 4 = null.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::address::{lane_flags, lane_name, require_positive, validate_local_v, Lane};
use crate::codec::{encode_value, FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, get_vu_u64, put_vu64, Nat};
use crate::schema::{LayoutKind, ShapeDef};
use crate::val::Val;
use crate::vmap;

use super::blockcache::{bounded_insert, SPARSE_CELLS_BUDGET, SPARSE_INDEX_BUDGET};
use super::{Db, SVal, SparseCellMeta, SparseMap, SparseSegment};

/// Larger decoded/merged u groups are not cached
/// (otherwise several GB with COLUMN/PACKED; targeted accesses go through
/// `vindex.rs`).
const SPARSE_CACHE_MAX_CELLS: usize = 65_536;

/// A sparse cell to be written.
#[derive(Clone, Debug)]
pub struct CellInput {
    pub u: Nat,
    pub v: Nat,
    pub value: Val,
    /// explicit type (`type=...`) → self-typed cell
    pub typ: Option<TypeCode>,
    pub tomb: bool,
}

impl CellInput {
    pub fn new(u: Nat, v: Nat, value: Val) -> Self {
        CellInput { u, v, value, typ: None, tomb: false }
    }
    pub fn typed(u: Nat, v: Nat, value: Val, typ: Option<TypeCode>) -> Self {
        CellInput { u, v, value, typ, tomb: false }
    }
    pub fn tombstone(u: Nat, v: Nat) -> Self {
        CellInput { u, v, value: Val::None, typ: None, tomb: true }
    }
}

/// Fully encoded sparse block that has not been written yet.
pub(crate) struct EncodedSparse {
    pub payload: Vec<u8>,
    pub prefix_len: usize,
    pub u_start: Nat,
    pub u_end: Nat,
    pub group_count: u64,
    pub cell_count: u64,
    /// (u, relative offset in the stream, length)
    pub local_index: Vec<(Nat, u64, u32)>,
}

/// A materialized point (`iter_materialized_sparse_points`).
#[derive(Clone, Debug)]
pub struct MaterializedPoint {
    pub realm: Nat,
    pub lane: Lane,
    pub u: Nat,
    pub v: Nat,
    pub value: Val,
    pub typ: Option<TypeCode>,
}

impl MaterializedPoint {
    pub fn to_val(&self) -> Val {
        vmap! {
            "realm" => &self.realm, "lane" => self.lane as i64, "u" => &self.u, "v" => &self.v,
            "value" => self.value.clone(), "type" => self.typ.map(|t| t.name()),
        }
    }
}

enum Declared<'a> {
    Ext,
    Classic(Option<&'a ShapeDef>),
    Column(Option<FieldDef>),
    Packed(Nat, Vec<(num_bigint::BigInt, FieldDef)>),
    Other,
}


/// Bytes of a u group and their position in the block payload.
pub(crate) type RecordSlice = (super::blockcache::BlockBytes, usize);

impl Db {
    /// Encodes all cells of a u group (`_encode_sparse_record`).
    fn encode_sparse_record(&self, realm: &Nat, u: &Nat, cells: &mut [CellInput], out: &mut Vec<u8>) -> Result<()> {
        if cells.is_empty() {
            return Ok(());
        }
        cells.sort_by(|a, b| a.v.cmp(&b.v));
        put_vu64(out, cells.len() as u64);
        let ext_fast = cells.iter().all(|c| {
            c.typ.is_none() && (c.tomb || self.extension_defs.contains_key(&(realm.clone(), c.v.clone())))
        });
        let lay = if ext_fast { None } else { self.layout_at(realm, u) };
        let mode = if ext_fast {
            Declared::Ext
        } else {
            match lay {
                Some(l) if l.kind == LayoutKind::Classic && l.shape_id.is_some() => {
                    Declared::Classic(self.shapes.get(&l.shape_id.unwrap()))
                }
                Some(l) if l.kind == LayoutKind::Column => {
                    let mut cf = None;
                    for f in l.fields() {
                        if &f.u()? == u {
                            cf = Some(f.to_field(Nat::ONE, f.name_or(|| format!("u{u}")))?);
                            break;
                        }
                    }
                    Declared::Column(cf)
                }
                Some(l) if l.kind == LayoutKind::Packed => {
                    let span = l.packed_span()?;
                    let mut pf = Vec::new();
                    for f in l.fields() {
                        let off = f.offset()?;
                        let nm = f.name_or(|| format!("offset_{off}"));
                        // later entries overwrite earlier ones (dict semantics)
                        pf.retain(|(o, _): &(num_bigint::BigInt, FieldDef)| *o != off);
                        pf.push((off, f.to_field(Nat::ONE, nm)?));
                    }
                    Declared::Packed(span, pf)
                }
                _ => Declared::Other,
            }
        };
        let custom = lay.map(|l| l.kind == LayoutKind::Custom).unwrap_or(false);
        let mut prev = Nat::ZERO;
        for c in cells.iter() {
            let delta = match c.v.checked_sub(&prev) {
                Some(d) if !d.is_zero() => d,
                _ => return Err(Error::Schema("Sparse v must be strictly ascending per u".into())),
            };
            let rk = realm.clone();
            let declared: Option<Cow<'_, FieldDef>> = match &mode {
                Declared::Ext => self.extension_defs.get(&(rk, c.v.clone())).map(|e| Cow::Borrowed(&e.field)),
                Declared::Classic(sh) => sh
                    .and_then(|s| s.by_v(&c.v))
                    .or_else(|| self.extension_defs.get(&(rk, c.v.clone())).map(|e| &e.field))
                    .map(Cow::Borrowed),
                Declared::Column(cf) => cf.as_ref().map(Cow::Borrowed),
                Declared::Packed(span, pf) => {
                    let off = c.v.sub_u64(1).unwrap().div_rem(span).1.to_bigint();
                    pf.iter().find(|(o, _)| *o == off).map(|(_, f)| Cow::Borrowed(f))
                }
                Declared::Other => self.field_meta_for_address(realm, u, &c.v)?.map(Cow::Owned),
            };
            let is_null = c.value.is_none() && !c.tomb;
            let custom_selftyped = custom && !c.tomb;
            let selftyped = c.typ.is_some() || (declared.is_none() && !c.tomb) || custom_selftyped;
            let typ = c.typ.or(declared.as_ref().map(|d| d.typ));
            if !c.tomb && typ.is_none() {
                return Err(Error::Schema(format!(
                    "R{realm},u{u},v{}: cannot derive a data type; specify type=...",
                    c.v
                )));
            }
            let flags = (c.tomb as u64) | ((selftyped as u64) << 1) | ((is_null as u64) << 2);
            match &delta {
                Nat::S(d) if *d < (1u64 << 60) => put_vu64(out, (d << 3) | flags),
                _ => {
                    let key = Nat::from(delta.to_big() << 3usize).add_u64(flags);
                    key.put_vu(out);
                }
            }
            if selftyped && !c.tomb {
                out.push(typ.unwrap().code());
            }
            if !c.tomb && !is_null {
                match (&declared, c.typ) {
                    (Some(d), None) => encode_value(d, &c.value, out)?,
                    _ => encode_value(&FieldDef::new(c.v.clone(), format!("v{}", c.v), typ.unwrap()), &c.value, out)?,
                }
            }
            prev = c.v.clone();
        }
        Ok(())
    }

    /// Encodes a sparse block for several u groups.
    pub(crate) fn encode_sparse_segment(
        &self,
        realm: &Nat,
        cells_by_u: &mut BTreeMap<Nat, Vec<CellInput>>,
    ) -> Result<Option<EncodedSparse>> {
        let us: Vec<Nat> = cells_by_u.iter().filter(|(_, c)| !c.is_empty()).map(|(u, _)| u.clone()).collect();
        if us.is_empty() {
            return Ok(None);
        }
        let u_start = us[0].clone();
        let u_end = us[us.len() - 1].clone();
        let mut stream: Vec<u8> = Vec::new();
        let mut rec: Vec<u8> = Vec::new();
        let mut cell_count = 0u64;
        let mut prev_u = u_start.clone();
        let mut local_index = Vec::with_capacity(us.len());
        for (i, u) in us.iter().enumerate() {
            let cells = cells_by_u.get_mut(u).unwrap();
            rec.clear();
            self.encode_sparse_record(realm, u, cells, &mut rec)?;
            if rec.is_empty() {
                continue;
            }
            let delta = if i == 0 { Nat::ZERO } else { u.checked_sub(&prev_u).unwrap() };
            delta.put_vu(&mut stream);
            put_vu64(&mut stream, rec.len() as u64);
            let rel = stream.len() as u64;
            stream.extend_from_slice(&rec);
            // R011: the in-memory index stores u32 lengths; never truncate
            let rec_len = u32::try_from(rec.len())
                .map_err(|_| Error::Value(format!("sparse cells of u={u} exceed 4 GiB in one group")))?;
            local_index.push((u.clone(), rel, rec_len));
            cell_count += cells.len() as u64;
            prev_u = u.clone();
        }
        let group_count = local_index.len() as u64;
        if group_count == 0 {
            return Ok(None);
        }
        let mut payload = Vec::with_capacity(stream.len() + 32);
        payload.extend_from_slice(SPARSE_TAG);
        realm.put_vu(&mut payload);
        u_start.put_vu(&mut payload);
        u_end.put_vu(&mut payload);
        put_vu64(&mut payload, group_count);
        put_vu64(&mut payload, cell_count);
        let prefix_len = payload.len();
        payload.extend_from_slice(&stream);
        Ok(Some(EncodedSparse { payload, prefix_len, u_start, u_end, group_count, cell_count, local_index }))
    }

    /// Writes an encoded sparse block and registers it.
    pub(crate) fn write_sparse_segment(&mut self, realm: &Nat, enc: EncodedSparse, lane: Lane) -> Result<Val> {
        let (off, bid) = self.append_block(BT_SPARSE_CELLS, &enc.payload, lane_flags(lane))?;
        let payload_off = off + BLOCK_HEADER_SIZE as u64;
        let entries_off = payload_off + enc.prefix_len as u64;
        let seg = SparseSegment {
            realm_id: realm.clone(),
            lane,
            u_start: enc.u_start,
            u_end: enc.u_end,
            group_count: enc.group_count,
            cell_count: enc.cell_count,
            block_id: bid,
            block_offset: off,
            payload_offset: payload_off,
            entries_offset: entries_off,
            payload_len: enc.payload.len() as u64,
        };
        let idx: Vec<(Nat, u64, u32)> =
            enc.local_index.iter().map(|(u, rel, ln)| (u.clone(), entries_off + rel, *ln)).collect();
        {
            let c = self.caches.get_mut().unwrap_or_else(|e| e.into_inner());
            for (u, _, _) in &idx {
                c.sparse_merged.remove(&(realm.clone(), lane, u.clone()));
            }
            if c.sparse_merged.len() > 4096 {
                c.sparse_merged.clear();
            }
            bounded_insert(&mut c.sparse_block_index, &mut c.used.sparse_block_index, bid, Arc::new(idx), |v| v.len(), SPARSE_INDEX_BUDGET, usize::MAX);
        }
        self.register_sparse_segment(seg);
        Ok(vmap! {
            "cells" => enc.cell_count as i64, "records" => enc.group_count as i64,
            "u_groups" => enc.group_count as i64, "block_offset" => off as i64,
            "block_id" => bid as i64, "bytes" => enc.payload.len() as i64,
        })
    }

    /// Validates cells and groups them by u windows (part of `put_points`).
    pub(crate) fn group_points(
        &self,
        realm: &Nat,
        cells: Vec<CellInput>,
        u_window: u64,
    ) -> Result<BTreeMap<Nat, BTreeMap<Nat, Vec<CellInput>>>> {
        let window = Nat::S(u_window.max(1));
        let mut groups: BTreeMap<Nat, BTreeMap<Nat, Vec<CellInput>>> = BTreeMap::new();
        for c in cells {
            require_positive("u", &c.u)?;
            validate_local_v(&c.v, Some(realm))?;
            let bucket = match (&c.u, &window) {
                (Nat::S(u), Nat::S(w)) => Nat::S((u - 1) / w),
                _ => c.u.sub_u64(1).unwrap().div_rem(&window).0,
            };
            groups.entry(bucket).or_default().entry(c.u.clone()).or_default().push(c);
        }
        Ok(groups)
    }

    /// Writes individual cells (`put_points`). All cells are fully validated
    /// and encoded before the first append.
    pub fn put_points(&mut self, realm: &Nat, cells: Vec<CellInput>, lane: Lane, commit: bool, u_window: u64) -> Result<Val> {
        self.put_points_as(realm, cells, lane, commit, u_window, None)
    }

    /// `put_points(..., _math_layout_id=…)`: exclusive MATH realms accept only
    /// writes of their own MATH layout (or of the internal
    /// compaction copy).
    pub(crate) fn put_points_as(
        &mut self,
        realm: &Nat,
        cells: Vec<CellInput>,
        lane: Lane,
        commit: bool,
        u_window: u64,
        math_layout_id: Option<u64>,
    ) -> Result<Val> {
        self.require_realm(realm)?;
        if let Some(owner) = self.math_owner_of_realm(realm) {
            if !self.internal_math_writes && math_layout_id != Some(owner) {
                return Err(Error::Schema(format!(
                    "Realm {realm} belongs exclusively to MATH layout {owner}; direct put_points blocked"
                )));
            }
        }
        let total_input = cells.len();
        if total_input == 0 {
            if commit {
                self.commit()?;
            }
            return Ok(vmap! {"cells" => 0i64, "blocks" => 0i64, "bytes" => 0i64});
        }
        let mut groups = self.group_points(realm, cells, u_window)?;
        let mut encoded = Vec::with_capacity(groups.len());
        for by_u in groups.values_mut() {
            encoded.push(self.encode_sparse_segment(realm, by_u)?);
        }
        let (mut cells_n, mut bytes, mut blocks, mut ugroups) = (0i64, 0i64, 0i64, 0i64);
        for enc in encoded {
            blocks += 1;
            if let Some(enc) = enc {
                let r = self.write_sparse_segment(realm, enc, lane)?;
                cells_n += r.get("cells").and_then(|v| v.as_i64()).unwrap_or(0);
                bytes += r.get("bytes").and_then(|v| v.as_i64()).unwrap_or(0);
                ugroups += r.get("u_groups").and_then(|v| v.as_i64()).unwrap_or(0);
            }
        }
        if commit {
            self.commit()?;
        }
        Ok(vmap! {
            "cells" => cells_n, "u_groups" => ugroups, "blocks" => blocks, "bytes" => bytes,
            "lane" => lane as i64, "lane_name" => lane_name(lane),
        })
    }

    /// Marks a cell as deleted (tombstone, `delete_point`).
    pub fn delete_point(&mut self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane, commit: bool) -> Result<Val> {
        self.require_realm(realm)?;
        require_positive("u", u)?;
        validate_local_v(v, Some(realm))?;
        let mut m = BTreeMap::new();
        m.insert(u.clone(), vec![CellInput::tombstone(u.clone(), v.clone())]);
        let res = match self.encode_sparse_segment(realm, &mut m)? {
            Some(enc) => self.write_sparse_segment(realm, enc, lane)?,
            None => vmap! {"cells" => 0i64, "records" => 0i64, "block_offset" => Val::None, "bytes" => 0i64},
        };
        if commit {
            self.commit()?;
        }
        Ok(res)
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    /// u → (absolute offset, length) of a sparse block (`_ensure_sparse_block_index`).
    pub(crate) fn sparse_block_index(&self, seg: &SparseSegment) -> Result<Arc<Vec<(Nat, u64, u32)>>> {
        if let Some(c) = self.cache().sparse_block_index.get(&seg.block_id) {
            return Ok(c.clone());
        }
        let payload = self.block_payload(seg.block_offset)?;
        let start = (seg.entries_offset - seg.payload_offset) as usize;
        let mv = &payload[start..];
        let bid = seg.block_id;
        let mut p = 0usize;
        let mut prev_u = seg.u_start.clone();
        // F022: group_count comes from the file; every group needs at least
        // 3 bytes (delta, length, record), so it is checked against the payload
        // before it sizes an allocation.
        if seg.group_count > (mv.len() / 3) as u64 {
            return Err(Error::Corruption(format!("SPARSE block {bid}: group count {} exceeds the payload", seg.group_count)));
        }
        let mut out: Vec<(Nat, u64, u32)> = Vec::with_capacity(seg.group_count as usize);
        for i in 0..seg.group_count {
            let delta = get_vu(mv, &mut p)?;
            if i == 0 && !delta.is_zero() {
                return Err(Error::Corruption(format!("SPARSE block {bid}: first u delta must be 0")));
            }
            if i > 0 && delta.is_zero() {
                return Err(Error::Corruption(format!("SPARSE block {bid}: u delta must be >0")));
            }
            let u = if i == 0 { seg.u_start.clone() } else { prev_u.add(&delta) };
            let rec_len = get_vu_u64(mv, &mut p, "SPARSE rec_len")?;
            let len32 = u32::try_from(rec_len).ok().filter(|l| *l > 0 && p.checked_add(*l as usize).is_some_and(|e| e <= mv.len()));
            let Some(len32) = len32 else {
                return Err(Error::Corruption(format!("SPARSE block {bid}: record length invalid")));
            };
            let rec_len = len32 as usize;
            out.push((u.clone(), seg.entries_offset + p as u64, len32));
            p += rec_len;
            prev_u = u;
        }
        if p != mv.len() {
            return Err(Error::Corruption(format!("SPARSE block {bid}: trailing bytes")));
        }
        if out.is_empty() || out[0].0 != seg.u_start || out[out.len() - 1].0 != seg.u_end {
            return Err(Error::Corruption(format!("SPARSE block {bid}: u range contradicts stream")));
        }
        let arc = Arc::new(out);
        let mut c = self.cache();
        let c = &mut *c;
        bounded_insert(&mut c.sparse_block_index, &mut c.used.sparse_block_index, bid, arc.clone(), |v| v.len(), SPARSE_INDEX_BUDGET, usize::MAX);
        Ok(arc)
    }

    /// Raw bytes of a u group in a sparse block (empty if missing).
    pub(crate) fn read_sparse_record_bytes(&self, seg: &SparseSegment, u: &Nat) -> Result<Option<RecordSlice>> {
        let idx = self.sparse_block_index(seg)?;
        let Ok(i) = idx.binary_search_by(|(x, _, _)| x.cmp(u)) else {
            return Ok(None);
        };
        let (_, abs, len) = &idx[i];
        let rel = (abs - seg.payload_offset) as usize;
        Ok(Some((self.block_range(seg.block_offset, rel, *len as usize)?, rel)))
    }

    fn sparse_record_states(&self, realm: &Nat, seg: &SparseSegment, u: &Nat) -> Result<Arc<Vec<(Nat, SVal)>>> {
        let ck = (seg.block_id, u.clone());
        if let Some(c) = self.cache().sparse_record.get(&ck) {
            return Ok(c.clone());
        }
        let vals: Vec<(Nat, SVal)> = match self.read_sparse_record_bytes(seg, u)? {
            None => Vec::new(),
            Some((raw, _)) => self
                .decode_sparse_record(realm, u, &raw)?
                .into_iter()
                .map(|(v, m)| (v, m.value))
                .collect(),
        };
        let arc = Arc::new(vals);
        if arc.len() > SPARSE_CACHE_MAX_CELLS {
            return Ok(arc);
        }
        let mut c = self.cache();
        let c = &mut *c;
        bounded_insert(&mut c.sparse_record, &mut c.used.sparse_record, ck, arc.clone(), |v| v.len(), SPARSE_CELLS_BUDGET, 4096);
        Ok(arc)
    }

    /// Merged sparse state of a u group (`_sparse_updates_for_u`).
    pub fn sparse_updates_for_u(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<Arc<SparseMap>> {
        let mk = (realm.clone(), lane, u.clone());
        if let Some(m) = self.cache().sparse_merged.get(&mk) {
            return Ok(m.clone());
        }
        let mut out = SparseMap::new();
        if let Some(idx) = self.sparse.get(&(realm.clone(), lane)) {
            let mut hits = Vec::new();
            idx.iv.stab(u, &mut hits);
            hits.sort_unstable();
            for i in hits {
                let seg = &idx.segs[i];
                let states = self.sparse_record_states(realm, seg, u)?;
                for (v, s) in states.iter() {
                    out.insert(v.clone(), s.clone());
                }
            }
        }
        let arc = Arc::new(out);
        if arc.len() > SPARSE_CACHE_MAX_CELLS {
            return Ok(arc);
        }
        let mut c = self.cache();
        let c = &mut *c;
        bounded_insert(&mut c.sparse_merged, &mut c.used.sparse_merged, mk, arc.clone(), |v| v.len(), SPARSE_CELLS_BUDGET, 4096);
        Ok(arc)
    }

    /// States in the v range `[lo, hi]` (`_sparse_updates_for_u_range`).
    pub fn sparse_updates_for_u_range(&self, realm: &Nat, u: &Nat, lo: &Nat, hi: &Nat, lane: Lane) -> Result<Vec<(Nat, SVal)>> {
        if hi < lo {
            return Ok(Vec::new());
        }
        Ok(self.sparse_range(realm, u, lo, hi, lane)?.into_iter().collect())
    }

    /// All live sparse points in their newest state, sorted.
    pub fn iter_materialized_sparse_points(&self, realm: Option<&Nat>, lane: Option<Lane>) -> Result<Vec<MaterializedPoint>> {
        let mut latest: BTreeMap<(Nat, Lane, Nat, Nat), SparseCellMeta> = BTreeMap::new();
        let mut keys: Vec<&(Nat, Lane)> = self.sparse.keys().collect();
        keys.sort();
        for key in keys {
            let (rid, ln) = key;
            if realm.map(|r| r != rid).unwrap_or(false) || lane.map(|l| l != *ln).unwrap_or(false) {
                continue;
            }
            let idx = &self.sparse[key];
            for seg in &idx.segs {
                let bi = self.sparse_block_index(seg)?;
                let payload = self.block_payload(seg.block_offset)?;
                for (u, abs, len) in bi.iter() {
                    let rel = (abs - seg.payload_offset) as usize;
                    let vals = self.decode_sparse_record(rid, u, &payload[rel..rel + *len as usize])?;
                    for (v, meta) in vals {
                        latest.insert((rid.clone(), *ln, u.clone(), v), meta);
                    }
                }
            }
        }
        Ok(latest
            .into_iter()
            .filter_map(|((r, l, u, v), m)| match m.value {
                SVal::Tomb => None,
                SVal::V(value) => Some(MaterializedPoint {
                    realm: r,
                    lane: l,
                    u,
                    v,
                    value,
                    typ: if m.selftyped { m.typ } else { None },
                }),
            })
            .collect())
    }

    /// All sparse states incl. tombstones in their newest state (compaction).
    pub(crate) fn latest_sparse_states(&self) -> Result<BTreeMap<(Nat, Lane, Nat, Nat), SparseCellMeta>> {
        let mut latest = BTreeMap::new();
        for ((rid, ln), idx) in &self.sparse {
            for seg in &idx.segs {
                let bi = self.sparse_block_index(seg)?;
                let payload = self.block_payload(seg.block_offset)?;
                for (u, abs, len) in bi.iter() {
                    let rel = (abs - seg.payload_offset) as usize;
                    for (v, meta) in self.decode_sparse_record(rid, u, &payload[rel..rel + *len as usize])? {
                        latest.insert((rid.clone(), *ln, u.clone(), v), meta);
                    }
                }
            }
        }
        Ok(latest)
    }
}
