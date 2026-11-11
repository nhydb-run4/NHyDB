//! Dense BASE records (`DATA_DENSE`, tag `D28D`).
//!
//! Block layout (the prefix up to `count` is sealed with its own checksum,
//! `seal_prefix`):
//! `D28D | vu realm | vu shape | vu u_start | vu count | prefix crc | u32le[count+1] offsets | records`
//! A record is the sequence of BASE field values in shape order,
//! optionally followed by the EXT area (`0xEA`, read only; extensions
//! are written as sparse blocks).

use std::collections::BTreeMap;

use num_bigint::BigInt;

use crate::address::{lane_flags, lane_name, Lane};
use crate::codec::{decode_value, encode_value, skip_value, FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{put_vi64, put_vu64, Nat};
use crate::schema::ShapeDef;
use crate::val::Val;
use crate::vmap;

use super::sparse::CellInput;
use super::{Db, DenseSegment, SVal};

/// Typed column for batch writes (e.g. NumPy arrays).
#[derive(Clone, Debug)]
pub enum ColData<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
    U64(&'a [u64]),
    I8(&'a [i8]),
    I16(&'a [i16]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    F64(&'a [f64]),
    /// NumPy bool (1 byte per value)
    Bool(&'a [u8]),
    /// Variable length: row i = `data[offsets[i]..offsets[i+1]]`
    Var { offsets: &'a [u64], data: &'a [u8], utf8: bool },
    /// Arbitrary values (fallback, e.g. enum labels, BigInts)
    Vals(&'a [Val]),
}

impl ColData<'_> {
    pub fn len(&self) -> usize {
        match self {
            ColData::U8(a) => a.len(),
            ColData::U16(a) => a.len(),
            ColData::U32(a) => a.len(),
            ColData::U64(a) => a.len(),
            ColData::I8(a) => a.len(),
            ColData::I16(a) => a.len(),
            ColData::I32(a) => a.len(),
            ColData::I64(a) => a.len(),
            ColData::F32(a) => a.len(),
            ColData::F64(a) => a.len(),
            ColData::Bool(a) => a.len(),
            ColData::Var { offsets, .. } => offsets.len().saturating_sub(1),
            ColData::Vals(a) => a.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Value as Val (for the fallback path).
    pub fn get_val(&self, i: usize) -> Val {
        match self {
            ColData::U8(a) => Val::Int(a[i] as i64),
            ColData::U16(a) => Val::Int(a[i] as i64),
            ColData::U32(a) => Val::Int(a[i] as i64),
            ColData::U64(a) => Val::int(a[i]),
            ColData::I8(a) => Val::Int(a[i] as i64),
            ColData::I16(a) => Val::Int(a[i] as i64),
            ColData::I32(a) => Val::Int(a[i] as i64),
            ColData::I64(a) => Val::Int(a[i]),
            ColData::F32(a) => Val::Float(a[i] as f64),
            ColData::F64(a) => Val::Float(a[i]),
            ColData::Bool(a) => Val::Bool(a[i] != 0),
            ColData::Var { offsets, data, utf8 } => {
                let s = &data[offsets[i] as usize..offsets[i + 1] as usize];
                if *utf8 {
                    Val::Str(String::from_utf8_lossy(s).into_owned())
                } else {
                    Val::Bytes(s.to_vec())
                }
            }
            ColData::Vals(a) => a[i].clone(),
        }
    }

    /// Integer view without allocation (None for non-integer columns).
    #[inline]
    fn get_i128(&self, i: usize) -> Option<i128> {
        Some(match self {
            ColData::U8(a) => a[i] as i128,
            ColData::U16(a) => a[i] as i128,
            ColData::U32(a) => a[i] as i128,
            ColData::U64(a) => a[i] as i128,
            ColData::I8(a) => a[i] as i128,
            ColData::I16(a) => a[i] as i128,
            ColData::I32(a) => a[i] as i128,
            ColData::I64(a) => a[i] as i128,
            ColData::Bool(a) => (a[i] != 0) as i128,
            _ => return None,
        })
    }
}

/// Input rows for `put_dense_batch`.
pub enum DenseRows<'a> {
    /// Positional rows (values in shape field order).
    Tuples(&'a [Vec<Val>]),
    /// dict rows (`Val::Map` field name → value).
    Dicts(&'a [Val]),
    /// Row-major flat: `vals[row * width + col]`; `width` = values per row.
    Flat { vals: &'a [Val], width: usize },
    /// Columns in shape field order, all with `count` rows.
    Columns { cols: &'a [ColData<'a>], count: usize },
    /// Already encoded BASE records (compaction, P002): `offsets` has one
    /// entry more than records (`offsets[0] == 0`), record `i` is
    /// `area[offsets[i]..offsets[i+1]]`. The caller copies CRC-checked bytes
    /// of the same shape; every record is decoded again by the deep verify
    /// of the compaction target.
    Raw { area: &'a [u8], offsets: &'a [u32] },
}

impl DenseRows<'_> {
    pub fn len(&self) -> usize {
        match self {
            DenseRows::Tuples(r) => r.len(),
            DenseRows::Dicts(r) => r.len(),
            DenseRows::Flat { vals, width } => if *width == 0 { 0 } else { vals.len() / width },
            DenseRows::Columns { count, .. } => *count,
            DenseRows::Raw { offsets, .. } => offsets.len().saturating_sub(1),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Decoded dense record (`get_internal`).
#[derive(Clone, Debug)]
/// R4-B: read state of `get_fields` over a sequence of u: the segment of
/// the previous u and read windows of a verified, uncached block.
#[derive(Default)]
pub(crate) struct DenseReadState {
    seg: Option<DenseSegment>,
    tab: Option<(u64, usize, Vec<u8>)>,
    rec: Option<(u64, usize, Vec<u8>)>,
    /// block and record index of the previous `with_record_bytes_win`
    last: Option<(u64, u64)>,
    /// RUN8 A7 experiment (feature `exp_block_state`): the cached payload of
    /// the previous record's block
    #[cfg(feature = "exp_block_state")]
    held: Option<(u64, std::sync::Arc<Vec<u8>>)>,
}

pub struct DenseRecord {
    pub realm: Nat,
    pub shape_id: u64,
    pub u: Nat,
    pub lane: Lane,
    /// Field name → value (shape order)
    pub values: Vec<(String, Val)>,
    pub base_by_v: BTreeMap<Nat, Val>,
    pub extensions_by_v: BTreeMap<Nat, Val>,
}

impl DenseRecord {
    pub fn by_v(&self) -> BTreeMap<Nat, Val> {
        let mut m = self.base_by_v.clone();
        for (k, v) in &self.extensions_by_v {
            m.insert(k.clone(), v.clone());
        }
        m
    }

    pub fn by_v_get(&self, v: &Nat) -> Option<&Val> {
        self.extensions_by_v.get(v).or_else(|| self.base_by_v.get(v))
    }

    pub fn to_val(&self) -> Val {
        let bv = |m: &BTreeMap<Nat, Val>| Val::Map(m.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
        vmap! {
            "realm" => &self.realm, "shape_id" => self.shape_id as i64,
            "values" => Val::Map(self.values.clone()),
            "base_by_v" => bv(&self.base_by_v), "extensions_by_v" => bv(&self.extensions_by_v),
            "by_v" => bv(&self.by_v()), "u" => &self.u, "lane" => self.lane as i64,
        }
    }
}

fn fit_err(f: &FieldDef, v: impl std::fmt::Display) -> Error {
    Error::Value(format!("{}: {v} does not fit into {}", f.name, f.typ.name()))
}

/// Fast path: encode the column value directly; `Ok(false)` → fallback needed.
#[inline]
fn encode_col_fast(f: &FieldDef, col: &ColData, i: usize, out: &mut Vec<u8>) -> Result<bool> {
    if f.enum_map.is_some() {
        // enum with integer column: raw code
        if col.get_i128(i).is_none() {
            return Ok(false);
        }
    }
    if let Some(x) = col.get_i128(i) {
        match f.typ {
            TypeCode::VarUInt => {
                if x < 0 {
                    return Err(Error::Value("varuint expects n>=0".into()));
                }
                put_vu64(out, x as u64);
            }
            TypeCode::VarInt => {
                if x >= i64::MIN as i128 && x <= i64::MAX as i128 {
                    put_vi64(out, x as i64);
                } else {
                    crate::nat::put_vi_big(out, &BigInt::from(x));
                }
            }
            TypeCode::Bool => out.push((x != 0) as u8),
            TypeCode::F32 => out.extend_from_slice(&(x as f32).to_le_bytes()),
            TypeCode::F64 => out.extend_from_slice(&(x as f64).to_le_bytes()),
            TypeCode::Utf8 | TypeCode::Bytes => return Ok(false),
            t => {
                let size = t.fixed_size().unwrap();
                let (lo, hi): (i128, i128) = match t {
                    TypeCode::U8 => (0, 255),
                    TypeCode::U16 => (0, 65535),
                    TypeCode::U32 => (0, u32::MAX as i128),
                    TypeCode::U64 => (0, u64::MAX as i128),
                    TypeCode::I8 => (-128, 127),
                    TypeCode::I16 => (-32768, 32767),
                    TypeCode::I32 => (i32::MIN as i128, i32::MAX as i128),
                    _ => (i64::MIN as i128, i64::MAX as i128),
                };
                if x < lo || x > hi {
                    return Err(fit_err(f, x));
                }
                out.extend_from_slice(&x.to_le_bytes()[..size]);
            }
        }
        return Ok(true);
    }
    match (col, f.typ) {
        (ColData::F64(a), TypeCode::F64) => out.extend_from_slice(&a[i].to_le_bytes()),
        (ColData::F32(a), TypeCode::F32) => out.extend_from_slice(&a[i].to_le_bytes()),
        (ColData::F32(a), TypeCode::F64) => out.extend_from_slice(&(a[i] as f64).to_le_bytes()),
        (ColData::F64(a), TypeCode::F32) => {
            let y = a[i] as f32;
            if a[i].is_finite() && y.is_infinite() {
                return Err(fit_err(f, a[i]));
            }
            out.extend_from_slice(&y.to_le_bytes())
        }
        (ColData::Var { offsets, data, utf8 }, TypeCode::Utf8 | TypeCode::Bytes) if f.enum_map.is_none() => {
            let s = &data[offsets[i] as usize..offsets[i + 1] as usize];
            if f.typ == TypeCode::Utf8 && !*utf8 {
                return Ok(false);
            }
            if f.typ == TypeCode::Bytes && *utf8 {
                return Err(Error::Type("string argument without an encoding".into()));
            }
            put_vu64(out, s.len() as u64);
            out.extend_from_slice(s);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

impl Db {
    /// Encodes rows according to the shape. Returns: (record area, offsets).
    /// `ignored(key)`: dict keys outside the shape that are not an error
    /// (see `ignorable_record_key`).
    ///
    /// RUN9 V1/V5: encodes the rows from `start` on into one DATA block –
    /// until the next record would make record area plus offset table exceed
    /// `budget` bytes (a block holds at least one record) – and returns the
    /// end row (exclusive) as well. `budget = usize::MAX`: all rows.
    pub(crate) fn encode_dense_rows(shape: &ShapeDef, rows: &DenseRows, ignored: &dyn Fn(&str) -> bool, start: usize, budget: usize) -> Result<(Vec<u8>, Vec<u32>, usize)> {
        let n = rows.len();
        let nf = shape.fields.len();
        let est = (n - start).min(budget / (nf * 2 + 4).max(1) + 1);
        let mut out: Vec<u8> = Vec::with_capacity(est * (nf * 2 + 4));
        let mut offsets: Vec<u32> = Vec::with_capacity(est + 1);
        offsets.push(0);
        // false: the record just encoded (from `before` on) does not fit any
        // more; it is taken out again and starts the next block
        let check = |out: &mut Vec<u8>, offsets: &mut Vec<u32>, before: usize| -> Result<bool> {
            let k = offsets.len() - 1;
            if k > 0 && out.len().saturating_add((k + 2) * 4) > budget {
                out.truncate(before);
                return Ok(false);
            }
            if out.len() > u32::MAX as usize {
                return Err(Error::Value("a DATA batch may contain at most 4 GiB of record data".into()));
            }
            offsets.push(out.len() as u32);
            Ok(true)
        };
        match rows {
            DenseRows::Tuples(rs) => {
                for (i, row) in rs.iter().enumerate().skip(start) {
                    if row.len() != nf {
                        return Err(Error::Schema(format!("Shape {} expects {} values", shape.shape_id, nf)));
                    }
                    let before = out.len();
                    for (f, v) in shape.fields.iter().zip(row.iter()) {
                        encode_value(f, v, &mut out)?;
                    }
                    if !check(&mut out, &mut offsets, before)? {
                        return Ok((out, offsets, i));
                    }
                }
            }
            DenseRows::Dicts(rs) => {
                for (i, row) in rs.iter().enumerate().skip(start) {
                    // F039: unknown keys would be dropped silently; keys starting
                    // with '_' are output metadata (`_realm`, `_u`, ...) and ignored.
                    // Extension/sparse keys are ignored only in a record that
                    // recognisably comes from get() (N4, G2-3: `_shape`, `_u` or
                    // `_realm` present); in any other dict they stay an error.
                    if let Some(m) = row.as_map() {
                        let from_get = m.iter().any(|(k, _)| matches!(k.as_str(), "_shape" | "_u" | "_realm"));
                        if let Some((k, _)) =
                            m.iter().find(|(k, _)| !k.starts_with('_') && shape.by_name(k).is_none() && !(from_get && ignored(k)))
                        {
                            return Err(Error::Schema(format!(
                                "unknown field '{k}' for shape {} (extensions are passed via extensions=)",
                                shape.shape_id
                            )));
                        }
                    }
                    let before = out.len();
                    for f in &shape.fields {
                        let v = row.get(&f.name).ok_or_else(|| Error::Schema(format!("Required field '{}' missing", f.name)))?;
                        encode_value(f, v, &mut out)?;
                    }
                    if !check(&mut out, &mut offsets, before)? {
                        return Ok((out, offsets, i));
                    }
                }
            }
            DenseRows::Flat { vals, width } => {
                if *width != nf {
                    return Err(Error::Schema(format!("Shape {} expects {} values", shape.shape_id, nf)));
                }
                for (i, row) in vals.chunks_exact(nf.max(1)).enumerate().skip(start) {
                    let before = out.len();
                    for (f, v) in shape.fields.iter().zip(row.iter()) {
                        encode_value(f, v, &mut out)?;
                    }
                    if !check(&mut out, &mut offsets, before)? {
                        return Ok((out, offsets, i));
                    }
                }
            }
            DenseRows::Columns { cols, count } => {
                if cols.len() != nf {
                    return Err(Error::Schema(format!("Shape {} expects {} values", shape.shape_id, nf)));
                }
                for (f, c) in shape.fields.iter().zip(cols.iter()) {
                    if c.len() != *count {
                        return Err(Error::Value(format!(
                            "Column '{}' has {} values, expected {}",
                            f.name,
                            c.len(),
                            count
                        )));
                    }
                }
                for i in start..*count {
                    let before = out.len();
                    for (f, c) in shape.fields.iter().zip(cols.iter()) {
                        if !encode_col_fast(f, c, i, &mut out)? {
                            encode_value(f, &c.get_val(i), &mut out)?;
                        }
                    }
                    if !check(&mut out, &mut offsets, before)? {
                        return Ok((out, offsets, i));
                    }
                }
            }
            DenseRows::Raw { area, offsets: raw_offsets } => return Self::copy_raw_rows(area, raw_offsets, start, budget),
        }
        Ok((out, offsets, n))
    }

    /// P002: already encoded BASE records (compaction) – kept out of line so
    /// that the encoding loops of `encode_dense_rows` stay as compact as
    /// before (bulk-write throughput).
    #[inline(never)]
    #[cold]
    fn copy_raw_rows(area: &[u8], raw_offsets: &[u32], start: usize, budget: usize) -> Result<(Vec<u8>, Vec<u32>, usize)> {
        if raw_offsets.first() != Some(&0)
            || raw_offsets.last().map(|e| *e as usize) != Some(area.len())
            || raw_offsets.windows(2).any(|w| w[1] < w[0])
        {
            return Err(Error::Value("raw DATA rows: offset table does not match the record area".into()));
        }
        let n = raw_offsets.len() - 1;
        let base = raw_offsets[start];
        // RUN9 V1: the same cut rule as the encoding loops
        let mut end = start + 1;
        while end < n && ((raw_offsets[end + 1] - base) as usize).saturating_add((end - start + 2) * 4) <= budget {
            end += 1;
        }
        if start == 0 && end == n {
            return Ok((area.to_vec(), raw_offsets.to_vec(), n));
        }
        let rel: Vec<u32> = raw_offsets[start..=end].iter().map(|o| o - base).collect();
        Ok((area[base as usize..raw_offsets[end] as usize].to_vec(), rel, end))
    }

    /// N4: keys of a `get()` record that `put`/`put_dense_batch` ignore
    /// besides the `_` metadata: names of extensions registered for the
    /// realm and the `v<n>` labels of unnamed extensions/sparse cells. This
    /// keeps the `put(realm, u, get(realm, u2))` round trip working; only the
    /// shape fields are written (extensions are passed via `extensions=`).
    /// G2-3: they are ignored only if the dict carries `get()` metadata
    /// (`_shape`, `_u` or `_realm`); otherwise, like any other unknown key,
    /// they raise `SchemaError` (F039).
    pub(crate) fn ignorable_record_key(&self, realm: &Nat, k: &str) -> bool {
        if let Some(d) = k.strip_prefix('v') {
            if !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) && !d.starts_with('0') {
                return true;
            }
        }
        self.extension_name_map.contains_key(&(realm.clone(), k.to_string()))
    }

    /// Writes a batch of contiguous BASE records starting at `u_start`
    /// (`put_dense_batch`). `extensions_by_u`: u → list of (key, value).
    #[allow(clippy::too_many_arguments)]
    pub fn put_dense_batch(
        &mut self,
        realm: &Nat,
        u_start: &Nat,
        rows: &DenseRows,
        shape_id: Option<u64>,
        extensions_by_u: &[(Nat, Vec<(Val, Val)>)],
        lane: Lane,
        commit: bool,
    ) -> Result<Val> {
        if rows.is_empty() {
            return Ok(vmap! {"records" => 0i64, "block_offset" => Val::None});
        }
        if u_start.is_zero() {
            return Err(Error::Value("u_start must be >0".into()));
        }
        let r = self.require_realm(realm)?;
        let sid = shape_id.or(r.default_shape_id).ok_or_else(|| Error::Schema("no shape given/default".into()))?;
        let shape = match self.shapes.get(&sid) {
            Some(s) if &s.realm_id == realm => s,
            _ => return Err(Error::Schema("Shape missing/does not match".into())),
        };
        let count = rows.len();
        let shape = shape.clone();
        // RUN9 V1: blocks of at most `[write] max_block_bytes` payload; the
        // prefix of every block is at most as long as the one of the last
        // u with the whole count
        let budget = match self.config.write_max_block_bytes {
            0 => usize::MAX,
            m => {
                let mut longest = Vec::with_capacity(32);
                longest.extend_from_slice(DATA_TAG);
                realm.put_vu(&mut longest);
                put_vu64(&mut longest, sid);
                u_start.add_u64(count as u64).put_vu(&mut longest);
                put_vu64(&mut longest, count as u64);
                seal_prefix(&mut longest);
                (m as usize).saturating_sub(longest.len())
            }
        };
        // Fully validate/encode extensions before anything is written.
        let mut sparse_enc = None;
        if !extensions_by_u.is_empty() {
            let mut by_u: BTreeMap<Nat, Vec<CellInput>> = BTreeMap::new();
            for (u, items) in extensions_by_u {
                let norm = self.normalize_extensions(realm, items)?;
                if !norm.is_empty() {
                    let cells = by_u.entry(u.clone()).or_default();
                    for (v, value) in norm {
                        cells.push(CellInput::new(u.clone(), v, value));
                    }
                }
            }
            if !by_u.is_empty() {
                sparse_enc = self.encode_sparse_segment(realm, &mut by_u)?;
            }
        }
        // F1: a record of another shape that this write replaces may hide
        // older SPARSE states of fields the new shape lacks; they get
        // tombstones, or they would show again (see `shape_change_tombstones`)
        let mut lost_tombs = self.shape_change_tombstones(realm, lane, &shape, u_start, count)?;
        // N2 (RUN8): all DATA blocks and the extension block are one unit;
        // RUN9 V5: each block is encoded, appended and freed before the next
        let (first, payload_bytes, sparse_result) = self.write_scope(|db| {
            let (mut start, mut u) = (0usize, u_start.clone());
            let (mut first, mut payload_bytes) = (None, 0usize);
            // RUN¹⁰: checkpoint of the RAM limiter between two blocks (a
            // cancel here rolls the whole call back, `write_scope`)
            let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Write);
            loop {
                let (record_area, offsets, end) = Self::encode_dense_rows(&shape, rows, &|k| db.ignorable_record_key(realm, k), start, budget)?;
                let n = end - start;
                let mut prefix = Vec::with_capacity(32);
                prefix.extend_from_slice(DATA_TAG);
                realm.put_vu(&mut prefix);
                put_vu64(&mut prefix, sid);
                u.put_vu(&mut prefix);
                put_vu64(&mut prefix, n as u64);
                seal_prefix(&mut prefix);
                let mut table = Vec::with_capacity(offsets.len() * 4);
                for o in &offsets {
                    table.extend_from_slice(&o.to_le_bytes());
                }
                payload_bytes += prefix.len() + table.len() + record_area.len();
                let ext = if end == count { sparse_enc.take() } else { None };
                let (off, bid, sparse) = db.append_dense_with_extensions(realm, &u, sid, n, [&prefix, &table, &record_area], ext, lane)?;
                first.get_or_insert((off, bid));
                if end == count {
                    if !lost_tombs.is_empty() {
                        db.put_points(realm, std::mem::take(&mut lost_tombs), lane, false, u64::MAX)?;
                    }
                    return Ok((first.unwrap(), payload_bytes, sparse));
                }
                start = end;
                u = u.add_u64(n as u64);
                meter.grow((prefix.len() + table.len() + record_area.len()) as u64)?;
            }
        })?;
        let (block_off, bid) = first;
        if commit {
            self.commit()?;
        }
        Ok(vmap! {
            "records" => count as i64, "block_offset" => block_off as i64, "block_id" => bid as i64,
            "bytes" => payload_bytes as i64, "sparse" => sparse_result,
            "lane" => lane as i64, "lane_name" => lane_name(lane),
        })
    }

    /// F1 at a shape change: `put` replaces the BASE record of u as a whole.
    /// A SPARSE state of a field of the replaced record that is older than
    /// that record is hidden by it; when the new shape lacks the field,
    /// nothing would hide the state any more and an overwritten value would
    /// show again (`put_cell(y=7)`, `put(A{x, y=20})`, `put(B{x})` must not
    /// read `y = 7`). Returns tombstones for exactly those states. Cost only
    /// when the lane has SPARSE cells and DENSE records of a shape with a
    /// field the new shape lacks; the plain write path reads nothing.
    fn shape_change_tombstones(&self, realm: &Nat, lane: Lane, shape: &ShapeDef, u_start: &Nat, count: usize) -> Result<Vec<CellInput>> {
        let key = (realm.clone(), lane);
        let mut out = Vec::new();
        if !self.sparse.contains_key(&key) {
            return Ok(out);
        }
        let Some(d) = self.dense.get(&key) else { return Ok(out) };
        let risky: Vec<u64> = d
            .shapes
            .iter()
            .copied()
            .filter(|sid| *sid != shape.shape_id && self.shapes.get(sid).is_some_and(|s| s.fields.iter().any(|f| shape.by_v(&f.v).is_none())))
            .collect();
        if risky.is_empty() {
            return Ok(out);
        }
        let mut u = u_start.clone();
        for _ in 0..count {
            if let Some(seg) = self.resolve_segment(realm, &u, lane)? {
                if risky.contains(&seg.shape_id) {
                    let old = self.shape_of(seg.shape_id)?;
                    let states = self.sparse_updates_for_u(realm, &u, lane)?;
                    for f in &old.fields {
                        if shape.by_v(&f.v).is_none() {
                            if let Some((SVal::V(_), bid)) = states.get(&f.v) {
                                if *bid < seg.block_id {
                                    out.push(CellInput::tombstone(u.clone(), f.v.clone()));
                                }
                            }
                        }
                    }
                }
            }
            u = u.add_u64(1);
        }
        Ok(out)
    }

    /// Appends and registers the DATA block of `put_dense_batch` and then its
    /// optional extension block (inside the caller's write scope).
    #[allow(clippy::too_many_arguments)]
    fn append_dense_with_extensions(
        &mut self,
        realm: &Nat,
        u_start: &Nat,
        sid: u64,
        count: usize,
        parts: [&[u8]; 3],
        sparse_enc: Option<super::sparse::EncodedSparse>,
        lane: Lane,
    ) -> Result<(u64, u64, Val)> {
        let payload_len = parts.iter().map(|p| p.len()).sum::<usize>();
        let (prefix_len, table_len) = (parts[0].len(), parts[1].len());
        debug_assert_eq!(table_len, (count + 1) * 4);
        let (block_off, bid) = self.append_block_parts(BT_DATA_DENSE, &parts, lane_flags(lane))?;
        let payload_off = block_off + BLOCK_HEADER_SIZE as u64;
        let offsets_off = payload_off + prefix_len as u64;
        let records_off = offsets_off + (count as u64 + 1) * 4;
        let seg = DenseSegment {
            realm_id: realm.clone(),
            lane,
            shape_id: sid,
            u_start: u_start.clone(),
            u_end: u_start.add_u64(count as u64 - 1),
            count: count as u64,
            block_id: bid,
            block_offset: block_off,
            payload_offset: payload_off,
            offsets_offset: offsets_off,
            records_offset: records_off,
            payload_len: payload_len as u64,
        };
        if let Err(e) = self.register_segment(seg) {
            // not registered: the block must not stay behind the next commit
            self.store.rollback_to(block_off)?;
            self.invalidate_blocks_from(block_off);
            self.next_block_id = bid;
            self.last_valid_end = block_off;
            return Err(e);
        }
        let sparse_result = match sparse_enc {
            Some(enc) => self.write_sparse_segment(realm, enc, lane)?,
            None => Val::None,
        };
        self.maybe_flush_directory(super::directory::FlushMode::AfterWrite)?;
        Ok((block_off, bid, sparse_result))
    }

    /// Single BASE record (`put`).
    #[allow(clippy::too_many_arguments)] // mirrors the Python signature (keyword arguments)
    pub fn put(
        &mut self,
        realm: &Nat,
        u: &Nat,
        values: Val,
        shape_id: Option<u64>,
        extensions: Option<Vec<(Val, Val)>>,
        lane: Lane,
        commit: bool,
    ) -> Result<Val> {
        let ext: Vec<(Nat, Vec<(Val, Val)>)> = match extensions {
            Some(e) => vec![(u.clone(), e)],
            None => vec![],
        };
        match values {
            Val::Map(_) => {
                let rows = [values];
                self.put_dense_batch(realm, u, &DenseRows::Dicts(&rows), shape_id, &ext, lane, commit)
            }
            Val::List(l) => {
                let rows = [l];
                self.put_dense_batch(realm, u, &DenseRows::Tuples(&rows), shape_id, &ext, lane, commit)
            }
            _ => Err(Error::Type("values must be a list/tuple or dict".into())),
        }
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    /// Valid dense segment for `(realm, u, lane)` (`_resolve_segment`):
    /// the segment with the highest block ID, unless deleted later.
    pub fn resolve_segment(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<Option<DenseSegment>> {
        let key = (realm.clone(), lane);
        let Some(idx) = self.dense.get(&key) else { return Ok(None) };
        let Some(best) = idx.idx.stab_newest(self, u)? else { return Ok(None) };
        if !self.deletes.is_empty() {
            if let Some(dbid) = self.delete_block_id(&key, u)? {
                if dbid > best.block_id {
                    return Ok(None);
                }
            }
        }
        Ok(Some(best))
    }

    /// Payload of a DATA block checked against its segment entry (block
    /// directory, 0.17): tag, routing prefix and offset table must be the
    /// ones the entry describes – a directory entry never routes u into a
    /// block that says otherwise.
    pub(crate) fn check_dense_prefix(&self, seg: &DenseSegment, payload: &[u8]) -> Result<()> {
        self.check_dense_prefix_parts(seg, payload, payload.len() as u64)
    }

    /// [`Db::check_dense_prefix`] on the leading bytes of a payload of
    /// `total_len` bytes (RUN9 V1: an oversized block is never read as a
    /// whole; `head` must reach the offset table the entry names).
    fn check_dense_prefix_parts(&self, seg: &DenseSegment, payload: &[u8], total_len: u64) -> Result<()> {
        let bad = || Error::Corruption(format!("DATA block {} (byte {}): block contradicts its directory entry", seg.block_id, seg.block_offset));
        if !payload.starts_with(DATA_TAG) {
            return Err(bad());
        }
        if total_len != seg.payload_len {
            return Err(bad());
        }
        // RD-01: the header must be the one of the entry (block id, type,
        // lane) – a CRC-valid block of the same shape written elsewhere
        // (swapped or misdirected block) is not accepted as this one
        let (bt, bid, flags, plen) = self.block_header_of(seg.block_offset)?;
        if bt != BT_DATA_DENSE || bid != seg.block_id || plen != seg.payload_len || crate::address::lane_from_flags(flags) != seg.lane {
            return Err(bad());
        }
        let mut p = 4usize;
        let realm = crate::nat::get_vu(payload, &mut p)?;
        let shape = crate::nat::get_vu_u64(payload, &mut p, "DATA shape")?;
        let u_start = crate::nat::get_vu(payload, &mut p)?;
        let count = crate::nat::get_vu_u64(payload, &mut p, "DATA count")?;
        check_prefix_crc(payload, &mut p)?;
        if realm != seg.realm_id
            || shape != seg.shape_id
            || u_start != seg.u_start
            || count != seg.count
            || seg.payload_offset + p as u64 != seg.offsets_offset
        {
            return Err(bad());
        }
        Ok(())
    }

    /// Block access of a DATA segment; the first access in a session checks
    /// the payload CRC (block cache) and the prefix against the entry.
    pub(crate) fn dense_block_access(&self, seg: &DenseSegment) -> Result<super::blockcache::BlockAccess> {
        let (acc, fresh) = self.block_access_fresh_len(seg.block_offset, Some(seg.payload_len))?;
        if fresh {
            // an unverified block was just CRC-checked: completely loaded, or
            // (larger than the cache takes) in chunks
            let r = match &acc {
                super::blockcache::BlockAccess::Cached(payload) => self.check_dense_prefix(seg, payload),
                super::blockcache::BlockAccess::Verified { payload_len } => self.check_dense_prefix_head(seg, *payload_len),
            };
            if let Err(e) = r {
                self.forget_verified(seg.block_offset);
                return Err(e);
            }
        }
        Ok(acc)
    }

    /// Prefix check of a verified DATA block from its leading bytes only.
    fn check_dense_prefix_head(&self, seg: &DenseSegment, payload_len: u64) -> Result<()> {
        let start = seg.block_offset + crate::format::BLOCK_HEADER_SIZE as u64;
        let bad = || Error::Corruption(format!("DATA block {} (byte {}): block contradicts its directory entry", seg.block_id, seg.block_offset));
        let head = match seg.offsets_offset.checked_sub(start) {
            Some(n) if n <= payload_len => n as usize,
            _ => return Err(bad()),
        };
        let raw = self.store.read_vec(start, head)?;
        self.check_dense_prefix_parts(seg, &raw, payload_len)
    }

    /// CRC check + prefix check of a DATA block (once per session).
    pub(crate) fn ensure_verified_dense(&self, seg: &DenseSegment) -> Result<()> {
        if self.cache().block_cache.is_verified_pub(seg.block_offset) {
            return Ok(());
        }
        // RUN9 V1: through the block access, so an oversized block is
        // checked in chunks instead of being loaded as a whole
        self.dense_block_access(seg).map(|_| ())
    }

    /// Newest DELETE block of `(realm, lane, u)`, if any.
    pub(crate) fn delete_block_id(&self, key: &(Nat, Lane), u: &Nat) -> Result<Option<u64>> {
        let Some(d) = self.deletes.get(key) else { return Ok(None) };
        let mut best = d.tail.get(u).copied();
        if !d.runs.is_empty() {
            if let Some(h) = d.runs.stab_newest(self, u)? {
                best = Some(best.map_or(h.block_id, |b| b.max(h.block_id)));
            }
        }
        Ok(best)
    }

    /// u ranges covered by the dense segments of `key`, merged (adjacent
    /// ranges joined), streamed in start order: memory grows with the number
    /// of merged ranges, not with the number of segments.
    pub(crate) fn dense_merged_ranges(&self, key: &(Nat, Lane)) -> Result<Vec<(Nat, Nat)>> {
        let mut out: Vec<(Nat, Nat)> = Vec::new();
        let Some(ix) = self.dense.get(key) else { return Ok(out) };
        let mut it = ix.idx.iter_by_start();
        while let Some(s) = it.next(self)? {
            if s.u_start > s.u_end {
                continue;
            }
            if let Some(last) = out.last_mut() {
                if s.u_start <= last.1.add_u64(1) {
                    if s.u_end > last.1 {
                        last.1 = s.u_end.clone();
                    }
                    continue;
                }
            }
            out.push((s.u_start.clone(), s.u_end.clone()));
        }
        Ok(out)
    }

    /// Whether `(realm, lane)` has any DELETE entries.
    pub(crate) fn has_deletes(&self, key: &(Nat, Lane)) -> bool {
        self.deletes.get(key).is_some_and(|d| !d.is_empty())
    }

    /// Raw bytes of a record: from the cached block payload, or – for a
    /// block CRC-verified earlier in this session – read directly (offset
    /// table entry + record, P001/P003).
    pub(crate) fn with_record_bytes<T>(&self, seg: &DenseSegment, u: &Nat, f: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        let idx = u.checked_sub(&seg.u_start).and_then(|d| d.as_u64()).filter(|d| *d < seg.count);
        let Some(idx) = idx else {
            return Err(Error::Corruption(format!(
                "DATA block {}: u={u} outside the segment (u_start={}, count={})",
                seg.block_id, seg.u_start, seg.count
            )));
        };
        let acc = self.dense_block_access(seg)?;
        let plen = acc.payload_len() as usize;
        let t = (seg.offsets_offset - seg.payload_offset) as usize + idx as usize * 4;
        if t + 8 > plen {
            return Err(Error::Corruption("DATA offset table incomplete".into()));
        }
        let tab = self.access_range(&acc, seg.block_offset, t, 8)?;
        let start = u32::from_le_bytes(tab[0..4].try_into().unwrap()) as usize;
        let end = u32::from_le_bytes(tab[4..8].try_into().unwrap()) as usize;
        if end < start {
            return Err(Error::Corruption("DATA record offsets go backwards".into()));
        }
        let rbase = (seg.records_offset - seg.payload_offset) as usize;
        if rbase + end > plen {
            return Err(Error::Corruption("DATA record incomplete".into()));
        }
        let rec = self.access_range(&acc, seg.block_offset, rbase + start, end - start)?;
        f(&rec)
    }

    /// P016: value of one BASE field of the dense record at `u` without
    /// decoding the whole record (fields before it are skipped). `None`: no
    /// fast answer – sparse overlays exist for this realm/lane or `v` is no
    /// field of the stored shape; the caller then uses the general path.
    pub(crate) fn dense_base_value(&self, seg: &DenseSegment, realm: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<Option<Val>> {
        if self.sparse.contains_key(&(realm.clone(), lane)) {
            return Ok(None);
        }
        let shape = self.shape_of(seg.shape_id)?;
        if shape.by_v(v).is_none() {
            return Ok(None);
        }
        self.with_record_bytes(seg, u, |raw| {
            let mut p = 0usize;
            for f in &shape.fields {
                if &f.v == v {
                    return decode_value(f, raw, &mut p).map(Some);
                }
                skip_value(f, raw, &mut p)?;
            }
            Ok(None)
        })
    }

    /// R4-B: `with_record_bytes` for a sequence of u: blocks that are
    /// verified but not cached (larger than a cache entry) are read through
    /// windows of `READ_WINDOW` bytes of the offset table and the records
    /// instead of two reads per record. Same checks and errors.
    fn with_record_bytes_win<T>(&self, seg: &DenseSegment, u: &Nat, st: &mut DenseReadState, f: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        const READ_WINDOW: usize = 1 << 20;
        let idx = u.checked_sub(&seg.u_start).and_then(|d| d.as_u64()).filter(|d| *d < seg.count);
        let Some(idx) = idx else {
            return Err(Error::Corruption(format!(
                "DATA block {}: u={u} outside the segment (u_start={}, count={})",
                seg.block_id, seg.u_start, seg.count
            )));
        };
        #[cfg(feature = "exp_block_state")]
        {
            if let Some((off, payload)) = st.held.as_ref().filter(|(o, _)| *o == seg.block_offset) {
                let _ = off;
                let t = (seg.offsets_offset - seg.payload_offset) as usize + idx as usize * 4;
                let tab = payload.get(t..t + 8).ok_or_else(|| Error::Corruption("DATA offset table incomplete".into()))?;
                let start = u32::from_le_bytes(tab[0..4].try_into().unwrap()) as usize;
                let end = u32::from_le_bytes(tab[4..8].try_into().unwrap()) as usize;
                let rbase = (seg.records_offset - seg.payload_offset) as usize;
                if end < start || rbase + end > payload.len() {
                    return Err(Error::Corruption("DATA record incomplete".into()));
                }
                return f(&payload[rbase + start..rbase + end]);
            }
        }
        let acc = self.dense_block_access(seg)?;
        if matches!(acc, super::blockcache::BlockAccess::Cached(_)) {
            #[cfg(feature = "exp_block_state")]
            if let super::blockcache::BlockAccess::Cached(a) = &acc {
                st.held = Some((seg.block_offset, a.clone()));
            }
            return self.with_record_bytes(seg, u, f);
        }
        let plen = acc.payload_len() as usize;
        let t = (seg.offsets_offset - seg.payload_offset) as usize + idx as usize * 4;
        if t + 8 > plen {
            return Err(Error::Corruption("DATA offset table incomplete".into()));
        }
        let base = seg.block_offset + crate::format::BLOCK_HEADER_SIZE as u64;
        // windows only for sequential access (the record follows the
        // previous one of the same block within 8 positions); scattered
        // records read just their bytes, as `with_record_bytes`
        let seq = st.last.is_some_and(|(b, i)| b == seg.block_offset && idx > i && idx <= i + 8);
        st.last = Some((seg.block_offset, idx));
        let window = |w: &mut Option<(u64, usize, Vec<u8>)>, rel: usize, len: usize| -> Result<()> {
            let ok = w.as_ref().is_some_and(|(b, s, v)| *b == seg.block_offset && *s <= rel && rel + len <= *s + v.len());
            if !ok {
                let take = if seq { len.max(READ_WINDOW).min(plen - rel) } else { len };
                *w = Some((seg.block_offset, rel, self.store.read_vec(base + rel as u64, take)?));
            }
            Ok(())
        };
        window(&mut st.tab, t, 8)?;
        let (_, ts, tv) = st.tab.as_ref().unwrap();
        let e = &tv[t - ts..t - ts + 8];
        let start = u32::from_le_bytes(e[0..4].try_into().unwrap()) as usize;
        let end = u32::from_le_bytes(e[4..8].try_into().unwrap()) as usize;
        if end < start {
            return Err(Error::Corruption("DATA record offsets go backwards".into()));
        }
        let rbase = (seg.records_offset - seg.payload_offset) as usize;
        if rbase + end > plen {
            return Err(Error::Corruption("DATA record incomplete".into()));
        }
        window(&mut st.rec, rbase + start, end - start)?;
        let (_, rs, rv) = st.rec.as_ref().unwrap();
        f(&rv[rbase + start - rs..rbase + end - rs])
    }

    /// R4-B: the values that `get_internal(realm, u, lane)` would show for
    /// the local v `vs` (`by_v_get`; `None` for v that the record does not
    /// have), without building the whole record. Without sparse overlays only
    /// the wanted BASE fields are decoded; the others are skipped
    /// structurally (as P016), extensions and trailing bytes are checked as
    /// in `decode_dense_record`. `None`: no dense record at u.
    /// `seg_cache`: the segment of the previous call; reused for the next u
    /// only when the segments of `(realm, lane)` are disjoint (no overlays:
    /// no overlapping segments, no DELETE, no sparse cells), so that it is
    /// exactly the segment `resolve_segment` would find.
    pub(crate) fn get_fields(&self, realm: &Nat, u: &Nat, lane: Lane, vs: &[Nat], st: &mut DenseReadState) -> Result<Option<Vec<Option<Val>>>> {
        let key = (realm.clone(), lane);
        if self.sparse.contains_key(&key) {
            return Ok(self.get_internal(realm, u, lane)?.map(|rec| vs.iter().map(|v| rec.by_v_get(v).cloned()).collect()));
        }
        let disjoint = !self.overlay_keys.contains(&key);
        let cached = st.seg.as_ref().filter(|s| disjoint && &s.u_start <= u && u <= &s.u_end).cloned();
        let seg = match cached {
            Some(s) => s,
            None => {
                let Some(seg) = self.resolve_segment(realm, u, lane)? else {
                    self.check_uncovered_u(realm, u, lane)?;
                    return Ok(None);
                };
                st.seg = Some(seg.clone());
                seg
            }
        };
        let shape = self
            .shapes
            .get(&seg.shape_id)
            .ok_or_else(|| Error::Corruption("Shape of the DATA segment missing".into()))?;
        let out = self.with_record_bytes_win(&seg, u, st, |payload| self.decode_fields(&seg, shape, payload, vs))?;
        Ok(Some(out))
    }

    /// The values of the local v `vs` in the record bytes `payload` of
    /// `seg` (shape `shape`) as `get_fields` shows them: wanted BASE fields
    /// decoded, the others skipped structurally, extensions and trailing
    /// bytes checked as in `decode_dense_record`.
    fn decode_fields(&self, seg: &DenseSegment, shape: &ShapeDef, payload: &[u8], vs: &[Nat]) -> Result<Vec<Option<Val>>> {
        let mut p = 0usize;
        let mut out: Vec<Option<Val>> = vec![None; vs.len()];
        for f in &shape.fields {
            // no allocation per field and record (hot loop of the scans)
            if !vs.contains(&f.v) {
                skip_value(f, payload, &mut p)?;
                continue;
            }
            let v = decode_value(f, payload, &mut p)?;
            for (i, w) in vs.iter().enumerate() {
                if *w == f.v {
                    out[i] = Some(v.clone());
                }
            }
        }
        let (ext, p) = self.decode_extensions(&seg.realm_id, payload, p)?;
        if p != payload.len() {
            return Err(Error::Corruption("Dense record trailing bytes".into()));
        }
        for (v, val) in ext {
            for (i, w) in vs.iter().enumerate() {
                if *w == v {
                    out[i] = Some(val.clone());
                }
            }
        }
        Ok(out)
    }

    /// 0.28.2/scan: the values of `vs` (as [`Db::get_fields`]) of the
    /// records `off .. off + len` of `seg`, in record order, handed to `f`
    /// with their record index. Only for a caller that already decided
    /// which segment serves these u (the winner runs of
    /// [`super::winners::WinnerView`]); no `resolve_segment` per u.
    ///
    /// Reading: the block access is taken once per run – a block that is
    /// not verified in this session is CRC-checked and prefix-checked first
    /// (`dense_block_access`, as every read). A cached payload is sliced
    /// directly; a verified block outside the cache is read through the
    /// sequential windows of `with_record_bytes_win` (never two single reads
    /// per record). Offset table and record bounds are checked as in
    /// `with_record_bytes`. `f` returns `false` to stop the run.
    pub(crate) fn scan_run_fields(&self, seg: &DenseSegment, off: u64, len: u64, vs: &[Nat], st: &mut DenseReadState, f: &mut dyn FnMut(u64, Vec<Option<Val>>) -> Result<bool>) -> Result<()> {
        let end = off.checked_add(len).filter(|e| *e <= seg.count).ok_or_else(|| {
            Error::Corruption(format!("DATA block {}: records {off}+{len} outside the segment (count={})", seg.block_id, seg.count))
        })?;
        let shape = self.shape_of(seg.shape_id)?;
        let acc = self.dense_block_access(seg)?;
        if let super::blockcache::BlockAccess::Cached(payload) = &acc {
            let plen = payload.len();
            let tab0 = (seg.offsets_offset - seg.payload_offset) as usize;
            let rbase = (seg.records_offset - seg.payload_offset) as usize;
            for idx in off..end {
                let t = tab0 + idx as usize * 4;
                if t + 8 > plen {
                    return Err(Error::Corruption("DATA offset table incomplete".into()));
                }
                let start = u32::from_le_bytes(payload[t..t + 4].try_into().unwrap()) as usize;
                let stop = u32::from_le_bytes(payload[t + 4..t + 8].try_into().unwrap()) as usize;
                if stop < start {
                    return Err(Error::Corruption("DATA record offsets go backwards".into()));
                }
                if rbase + stop > plen {
                    return Err(Error::Corruption("DATA record incomplete".into()));
                }
                let vals = self.decode_fields(seg, shape, &payload[rbase + start..rbase + stop], vs)?;
                if !f(idx, vals)? {
                    return Ok(());
                }
            }
            return Ok(());
        }
        drop(acc);
        for idx in off..end {
            let u = seg.u_start.add_u64(idx);
            let vals = self.with_record_bytes_win(seg, &u, st, |payload| self.decode_fields(seg, shape, payload, vs))?;
            if !f(idx, vals)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// `_decode_dense_record`
    pub(crate) fn decode_dense_record(&self, seg: &DenseSegment, payload: &[u8]) -> Result<DenseRecord> {
        let shape = self
            .shapes
            .get(&seg.shape_id)
            .ok_or_else(|| Error::Corruption("Shape of the DATA segment missing".into()))?;
        let mut p = 0usize;
        let mut values = Vec::with_capacity(shape.fields.len());
        let mut base_by_v = BTreeMap::new();
        for f in &shape.fields {
            let v = decode_value(f, payload, &mut p)?;
            values.push((f.name.clone(), v.clone()));
            base_by_v.insert(f.v.clone(), v);
        }
        let (ext, p) = self.decode_extensions(&seg.realm_id, payload, p)?;
        if p != payload.len() {
            return Err(Error::Corruption("Dense record trailing bytes".into()));
        }
        Ok(DenseRecord {
            realm: seg.realm_id.clone(),
            shape_id: seg.shape_id,
            u: Nat::ZERO,
            lane: seg.lane,
            values,
            base_by_v,
            extensions_by_v: ext.into_iter().collect(),
        })
    }

    /// F021 rest: the routing prefix of a DATA block is covered by its prefix
    /// checksum on open; a prefix rewritten together with that checksum is
    /// still caught here for the next block (one CRC per block and session).
    /// A u that no DATA segment covers can be the victim
    /// of a manipulated prefix of the next block (e.g. a shifted `u_start`
    /// with the same varint length), so the nearest segment above u is
    /// CRC-checked once per session (verified registry) before "not found"
    /// is answered; a mismatch raises `CorruptionError`.
    pub(crate) fn check_uncovered_u(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<()> {
        let Some(idx) = self.dense.get(&(realm.clone(), lane)) else {
            return Ok(());
        };
        if idx.idx.covers(self, u)? {
            return Ok(()); // covered (deleted records are no prefix question)
        }
        match idx.idx.first_start_after(self, u)? {
            Some(seg) => self.ensure_verified_dense(&seg),
            None => Ok(()),
        }
    }

    /// Record including sparse overlays (`get_internal`).
    pub fn get_internal(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<Option<DenseRecord>> {
        let Some(seg) = self.resolve_segment(realm, u, lane)? else {
            self.check_uncovered_u(realm, u, lane)?;
            return Ok(None);
        };
        let mut rec = self.with_record_bytes(&seg, u, |raw| self.decode_dense_record(&seg, raw))?;
        let has_sparse = self.sparse.contains_key(&(realm.clone(), lane));
        if has_sparse {
            let updates = self.sparse_updates_for_u(realm, u, lane)?;
            if !updates.is_empty() {
                let shape = self.shape_of(rec.shape_id)?;
                let current = self.current_base_shape(realm);
                for (v, (st, bid)) in updates.iter() {
                    // F1: the record is newer than this state of one of its fields
                    if super::dense_hides(shape, seg.block_id, v, *bid) {
                        continue;
                    }
                    let base_field = shape.by_v(v);
                    match st {
                        SVal::Tomb => {
                            // a tombstoned cell is no cell: it leaves the v
                            // maps, the named value reads as None
                            rec.extensions_by_v.remove(v);
                            if let Some(f) = base_field {
                                rec.base_by_v.remove(v);
                                set_value(&mut rec.values, &f.name, Val::None);
                            }
                        }
                        SVal::V(val) => {
                            if let Some(f) = base_field {
                                rec.base_by_v.insert(v.clone(), val.clone());
                                set_value(&mut rec.values, &f.name, val.clone());
                            } else if let Some(cf) = current.and_then(|b| b.by_v(v)) {
                                // BASE field of the current shape that the
                                // stored shape lacks: a BASE cell written alone
                                rec.base_by_v.insert(v.clone(), val.clone());
                                set_or_push(&mut rec.values, &cf.name, val.clone());
                            } else {
                                rec.extensions_by_v.insert(v.clone(), val.clone());
                            }
                        }
                    }
                }
            }
        }
        rec.u = u.clone();
        rec.lane = lane;
        Ok(Some(rec))
    }

    /// Live cells of a CLASSIC u group, v → value (a stored `None`
    /// included), exactly as the cell cursor and SQL see them: the DENSE
    /// record with its newer SPARSE states (F1) or, without a live record,
    /// the SPARSE cells whose newest state is a value. Empty: no live cell.
    pub(crate) fn live_cells_of_u(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<BTreeMap<Nat, Val>> {
        if let Some(rec) = self.get_internal(realm, u, lane)? {
            return Ok(rec.by_v());
        }
        let mut out = BTreeMap::new();
        if self.sparse.contains_key(&(realm.clone(), lane)) {
            for (v, (st, _)) in self.sparse_updates_for_u(realm, u, lane)?.iter() {
                if let SVal::V(x) = st {
                    out.insert(v.clone(), x.clone());
                }
            }
        }
        Ok(out)
    }

    /// Sets the extensions of an existing record (`put_extensions`).
    /// `merge=false` deletes live extension cells that are not listed.
    pub fn put_extensions(
        &mut self,
        realm: &Nat,
        u: &Nat,
        items: &[(Val, Val)],
        merge: bool,
        lane: Lane,
        commit: bool,
    ) -> Result<Val> {
        if self.resolve_segment(realm, u, lane)?.is_none() {
            return Err(Error::Key(format!("u{u}/{} does not exist", lane_name(lane))));
        }
        let norm = self.normalize_extensions(realm, items)?;
        let doomed: Vec<Nat> = if merge {
            Vec::new()
        } else {
            let current = self.sparse_updates_for_u(realm, u, lane)?;
            let boundary = self.extension_boundary(realm);
            let wanted: std::collections::HashSet<&Nat> = norm.iter().map(|(v, _)| v).collect();
            current
                .iter()
                // extension cells lie above every shape: no DENSE record hides them (F1)
                .filter(|(v, (s, _))| !wanted.contains(v) && **v > boundary && !s.is_tomb())
                .map(|(v, _)| v.clone())
                .collect()
        };
        let cells: Vec<CellInput> = norm.into_iter().map(|(v, val)| CellInput::new(u.clone(), v, val)).collect();
        // N2 (RUN8): the tombstones and the new cells are one unit
        let out = self.write_scope(|db| {
            for v in doomed {
                db.delete_point(realm, u, &v, lane, false)?;
            }
            db.put_points(realm, cells, lane, false, 10_000)
        })?;
        if commit {
            self.commit()?;
        }
        Ok(out)
    }

    /// Logically deletes a BASE record (`delete`). Returns whether it existed.
    pub fn delete(&mut self, realm: &Nat, u: &Nat, lane: Lane, commit: bool) -> Result<bool> {
        if self.get_internal(realm, u, lane)?.is_none() {
            return Ok(false);
        }
        self.delete_record_block(realm, u, lane)?;
        if commit {
            self.commit()?;
        }
        Ok(true)
    }
}

impl Db {
    /// Appends and registers the DELETE block of an existing BASE record.
    pub(crate) fn delete_record_block(&mut self, realm: &Nat, u: &Nat, lane: Lane) -> Result<()> {
        let mut payload = Vec::new();
        realm.put_vu(&mut payload);
        u.put_vu(&mut payload);
        let (_off, bid) = self.append_block(BT_DELETE, &payload, lane_flags(lane))?;
        self.register_delete(realm.clone(), lane, u.clone(), bid);
        self.maybe_flush_directory(super::directory::FlushMode::AfterWrite)
    }
}

fn set_or_push(values: &mut Vec<(String, Val)>, name: &str, v: Val) {
    match values.iter_mut().find(|(k, _)| k == name) {
        Some(slot) => slot.1 = v,
        None => values.push((name.to_string(), v)),
    }
}

fn set_value(values: &mut [(String, Val)], name: &str, v: Val) {
    if let Some(slot) = values.iter_mut().find(|(k, _)| k == name) {
        slot.1 = v;
    }
}
