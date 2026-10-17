//! Dichte BASE-Records (`DATA_DENSE`, Tag `D10D`).
//!
//! Blockaufbau (unverändert v0.14):
//! `D10D | vu realm | vu shape | vu u_start | vu count | u32le[count+1] offsets | records`
//! Ein Record ist die Folge der BASE-Feldwerte in Shape-Reihenfolge,
//! optional gefolgt vom EXT-Bereich (`0xEA`, nur Lesen; v0.14 schreibt
//! Extensions als Sparse-Blöcke).

use std::collections::BTreeMap;

use num_bigint::BigInt;

use crate::address::{lane_flags, lane_name, Lane};
use crate::codec::{decode_value, encode_value, FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{put_vi64, put_vu64, Nat};
use crate::schema::ShapeDef;
use crate::val::Val;
use crate::vmap;

use super::sparse::CellInput;
use super::{Db, DenseSegment, SVal};

/// Typisierte Spalte für Batch-Schreibvorgänge (z. B. NumPy-Arrays).
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
    /// NumPy-bool (1 Byte je Wert)
    Bool(&'a [u8]),
    /// Variable Länge: Zeile i = `data[offsets[i]..offsets[i+1]]`
    Var { offsets: &'a [u64], data: &'a [u8], utf8: bool },
    /// Beliebige Werte (Fallback, z. B. Enum-Labels, BigInts)
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

    /// Wert als Val (für den Fallback-Pfad).
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

    /// Ganzzahlsicht ohne Allokation (None für nicht-ganzzahlige Spalten).
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

/// Eingabezeilen für `put_dense_batch`.
pub enum DenseRows<'a> {
    /// Positionsbezogene Zeilen (Werte in Shape-Feldreihenfolge).
    Tuples(&'a [Vec<Val>]),
    /// dict-Zeilen (`Val::Map` Feldname → Wert).
    Dicts(&'a [Val]),
    /// Zeilenweise flach: `vals[row * width + col]`; `width` = Werte je Zeile.
    Flat { vals: &'a [Val], width: usize },
    /// Spalten in Shape-Feldreihenfolge, alle mit `count` Zeilen.
    Columns { cols: &'a [ColData<'a>], count: usize },
}

impl DenseRows<'_> {
    pub fn len(&self) -> usize {
        match self {
            DenseRows::Tuples(r) => r.len(),
            DenseRows::Dicts(r) => r.len(),
            DenseRows::Flat { vals, width } => if *width == 0 { 0 } else { vals.len() / width },
            DenseRows::Columns { count, .. } => *count,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Dekodierter Dense-Record (`get_internal`).
#[derive(Clone, Debug)]
pub struct DenseRecord {
    pub realm: Nat,
    pub shape_id: u64,
    pub u: Nat,
    pub lane: Lane,
    /// Feldname → Wert (Shape-Reihenfolge)
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
    Error::Value(format!("{}: {v} passt nicht in {}", f.name, f.typ.name()))
}

/// Schnellpfad: Spaltenwert direkt kodieren; `Ok(false)` → Fallback nötig.
#[inline]
fn encode_col_fast(f: &FieldDef, col: &ColData, i: usize, out: &mut Vec<u8>) -> Result<bool> {
    if f.enum_map.is_some() {
        // Enum mit Ganzzahlspalte: roher Code
        if col.get_i128(i).is_none() {
            return Ok(false);
        }
    }
    if let Some(x) = col.get_i128(i) {
        match f.typ {
            TypeCode::VarUInt => {
                if x < 0 {
                    return Err(Error::Value("varuint erwartet n>=0".into()));
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
    /// Kodiert Zeilen gemäß Shape. Rückgabe: (Recordbereich, Offsets).
    pub(crate) fn encode_dense_rows(shape: &ShapeDef, rows: &DenseRows) -> Result<(Vec<u8>, Vec<u32>)> {
        let n = rows.len();
        let nf = shape.fields.len();
        let mut out: Vec<u8> = Vec::with_capacity(n * (nf * 2 + 4));
        let mut offsets: Vec<u32> = Vec::with_capacity(n + 1);
        offsets.push(0);
        let check = |out: &Vec<u8>, offsets: &mut Vec<u32>| -> Result<()> {
            if out.len() > u32::MAX as usize {
                return Err(Error::Value("ein DATA-Batch darf maximal 4 GiB Recorddaten haben".into()));
            }
            offsets.push(out.len() as u32);
            Ok(())
        };
        match rows {
            DenseRows::Tuples(rs) => {
                for row in rs.iter() {
                    if row.len() != nf {
                        return Err(Error::Schema(format!("Shape {} erwartet {} Werte", shape.shape_id, nf)));
                    }
                    for (f, v) in shape.fields.iter().zip(row.iter()) {
                        encode_value(f, v, &mut out)?;
                    }
                    check(&out, &mut offsets)?;
                }
            }
            DenseRows::Dicts(rs) => {
                for row in rs.iter() {
                    for f in &shape.fields {
                        let v = row.get(&f.name).ok_or_else(|| Error::Schema(format!("Pflichtfeld '{}' fehlt", f.name)))?;
                        encode_value(f, v, &mut out)?;
                    }
                    check(&out, &mut offsets)?;
                }
            }
            DenseRows::Flat { vals, width } => {
                if *width != nf {
                    return Err(Error::Schema(format!("Shape {} erwartet {} Werte", shape.shape_id, nf)));
                }
                for row in vals.chunks_exact(nf.max(1)) {
                    for (f, v) in shape.fields.iter().zip(row.iter()) {
                        encode_value(f, v, &mut out)?;
                    }
                    check(&out, &mut offsets)?;
                }
            }
            DenseRows::Columns { cols, count } => {
                if cols.len() != nf {
                    return Err(Error::Schema(format!("Shape {} erwartet {} Werte", shape.shape_id, nf)));
                }
                for (f, c) in shape.fields.iter().zip(cols.iter()) {
                    if c.len() != *count {
                        return Err(Error::Value(format!(
                            "Spalte '{}' hat {} Werte, erwartet {}",
                            f.name,
                            c.len(),
                            count
                        )));
                    }
                }
                for i in 0..*count {
                    for (f, c) in shape.fields.iter().zip(cols.iter()) {
                        if !encode_col_fast(f, c, i, &mut out)? {
                            encode_value(f, &c.get_val(i), &mut out)?;
                        }
                    }
                    check(&out, &mut offsets)?;
                }
            }
        }
        Ok((out, offsets))
    }

    /// Schreibt einen Batch zusammenhängender BASE-Records ab `u_start`
    /// (`put_dense_batch`). `extensions_by_u`: u → Liste (Schlüssel, Wert).
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
            return Err(Error::Value("u_start muss >0 sein".into()));
        }
        let r = self.require_realm(realm)?;
        let sid = shape_id.or(r.default_shape_id).ok_or_else(|| Error::Schema("kein Shape angegeben/default".into()))?;
        let shape = match self.shapes.get(&sid) {
            Some(s) if &s.realm_id == realm => s,
            _ => return Err(Error::Schema("Shape fehlt/passt nicht".into())),
        };
        let count = rows.len();
        let (record_area, offsets) = Self::encode_dense_rows(shape, rows)?;
        // Extensions vollständig validieren/kodieren, bevor geschrieben wird.
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
        let mut prefix = Vec::with_capacity(32);
        prefix.extend_from_slice(DATA_TAG);
        realm.put_vu(&mut prefix);
        put_vu64(&mut prefix, sid);
        u_start.put_vu(&mut prefix);
        put_vu64(&mut prefix, count as u64);
        let mut table = Vec::with_capacity(offsets.len() * 4);
        for o in &offsets {
            table.extend_from_slice(&o.to_le_bytes());
        }
        let payload_len = prefix.len() + table.len() + record_area.len();
        let (block_off, bid) =
            self.append_block_parts(BT_DATA_DENSE, &[&prefix, &table, &record_area], lane_flags(lane))?;
        let payload_off = block_off + BLOCK_HEADER_SIZE as u64;
        let offsets_off = payload_off + prefix.len() as u64;
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
        self.register_segment(seg);
        let sparse_result = match sparse_enc {
            Some(enc) => {
                let r = self.write_sparse_segment(realm, enc, lane)?;
                self.cache().sparse_record.clear();
                r
            }
            None => Val::None,
        };
        if commit {
            self.commit()?;
        }
        Ok(vmap! {
            "records" => count as i64, "block_offset" => block_off as i64, "block_id" => bid as i64,
            "bytes" => payload_len as i64, "sparse" => sparse_result,
            "lane" => lane as i64, "lane_name" => lane_name(lane),
        })
    }

    /// Einzelner BASE-Record (`put`).
    #[allow(clippy::too_many_arguments)] // bildet die Python-Signatur (Keyword-Argumente) nach
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
            _ => Err(Error::Type("values muss Liste/Tupel oder dict sein".into())),
        }
    }

    // ------------------------------------------------------------------
    // Lesen
    // ------------------------------------------------------------------

    /// Gültiges Dense-Segment für `(realm, u, lane)` (`_resolve_segment`):
    /// das Segment mit der höchsten Block-ID, sofern nicht später gelöscht.
    pub fn resolve_segment(&self, realm: &Nat, u: &Nat, lane: Lane) -> Option<&DenseSegment> {
        let idx = self.dense.get(&(realm.clone(), lane))?;
        let mut hits = Vec::new();
        idx.iv.stab(u, &mut hits);
        let best = hits.into_iter().map(|i| &idx.segs[i]).max_by_key(|s| s.block_id)?;
        if !self.deletes.is_empty() {
            if let Some(dbid) = self.deletes.get(&(realm.clone(), lane, u.clone())) {
                if *dbid > best.block_id {
                    return None;
                }
            }
        }
        Some(best)
    }

    /// Rohbytes eines Records innerhalb des (gecachten) Block-Payloads.
    pub(crate) fn with_record_bytes<T>(&self, seg: &DenseSegment, u: &Nat, f: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        let idx = u.checked_sub(&seg.u_start).and_then(|d| d.as_u64()).filter(|d| *d < seg.count);
        let Some(idx) = idx else {
            return Err(Error::Index(u.to_string()));
        };
        let payload = self.block_payload(seg.block_offset)?;
        let t = (seg.offsets_offset - seg.payload_offset) as usize + idx as usize * 4;
        if t + 8 > payload.len() {
            return Err(Error::Corruption("DATA Offsettabelle unvollstaendig".into()));
        }
        let start = u32::from_le_bytes(payload[t..t + 4].try_into().unwrap()) as usize;
        let end = u32::from_le_bytes(payload[t + 4..t + 8].try_into().unwrap()) as usize;
        if end < start {
            return Err(Error::Corruption("DATA Recordoffsets rueckwaerts".into()));
        }
        let rbase = (seg.records_offset - seg.payload_offset) as usize;
        if rbase + end > payload.len() {
            return Err(Error::Corruption("DATA Record unvollstaendig".into()));
        }
        f(&payload[rbase + start..rbase + end])
    }

    /// `_decode_dense_record`
    pub(crate) fn decode_dense_record(&self, seg: &DenseSegment, payload: &[u8]) -> Result<DenseRecord> {
        let shape = self
            .shapes
            .get(&seg.shape_id)
            .ok_or_else(|| Error::Corruption("Shape des DATA-Segments fehlt".into()))?;
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
            return Err(Error::Corruption("Dense Record Restbytes".into()));
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

    /// Record inkl. Sparse-Overlays (`get_internal`).
    pub fn get_internal(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<Option<DenseRecord>> {
        let Some(seg) = self.resolve_segment(realm, u, lane) else {
            return Ok(None);
        };
        let mut rec = self.with_record_bytes(seg, u, |raw| self.decode_dense_record(seg, raw))?;
        let has_sparse = self.sparse.contains_key(&(realm.clone(), lane));
        if has_sparse {
            let updates = self.sparse_updates_for_u(realm, u, lane)?;
            if !updates.is_empty() {
                let shape = self.shape_of(rec.shape_id)?;
                for (v, st) in updates.iter() {
                    let base_field = shape.by_v(v);
                    match st {
                        SVal::Tomb => {
                            rec.extensions_by_v.remove(v);
                            if let Some(f) = base_field {
                                rec.base_by_v.insert(v.clone(), Val::None);
                                set_value(&mut rec.values, &f.name, Val::None);
                            }
                        }
                        SVal::V(val) => {
                            if let Some(f) = base_field {
                                rec.base_by_v.insert(v.clone(), val.clone());
                                set_value(&mut rec.values, &f.name, val.clone());
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

    /// Extensions eines bestehenden Records setzen (`put_extensions`).
    /// `merge=false` löscht nicht genannte, lebende Extension-Zellen.
    pub fn put_extensions(
        &mut self,
        realm: &Nat,
        u: &Nat,
        items: &[(Val, Val)],
        merge: bool,
        lane: Lane,
        commit: bool,
    ) -> Result<Val> {
        if self.resolve_segment(realm, u, lane).is_none() {
            return Err(Error::Key(format!("u{u}/{} existiert nicht", lane_name(lane))));
        }
        let norm = self.normalize_extensions(realm, items)?;
        if !merge {
            let current = self.sparse_updates_for_u(realm, u, lane)?;
            let boundary = self.extension_boundary(realm);
            let wanted: std::collections::HashSet<&Nat> = norm.iter().map(|(v, _)| v).collect();
            let doomed: Vec<Nat> = current
                .iter()
                .filter(|(v, s)| !wanted.contains(v) && **v > boundary && !s.is_tomb())
                .map(|(v, _)| v.clone())
                .collect();
            for v in doomed {
                self.delete_point(realm, u, &v, lane, false)?;
            }
        }
        let cells: Vec<CellInput> = norm.into_iter().map(|(v, val)| CellInput::new(u.clone(), v, val)).collect();
        self.put_points(realm, cells, lane, commit, 10_000)
    }

    /// Löscht einen BASE-Record logisch (`delete`). Rückgabe: ob vorhanden.
    pub fn delete(&mut self, realm: &Nat, u: &Nat, lane: Lane, commit: bool) -> Result<bool> {
        if self.get_internal(realm, u, lane)?.is_none() {
            return Ok(false);
        }
        let mut payload = Vec::new();
        realm.put_vu(&mut payload);
        u.put_vu(&mut payload);
        let (_off, bid) = self.append_block(BT_DELETE, &payload, lane_flags(lane))?;
        self.deletes.insert((realm.clone(), lane, u.clone()), bid);
        let key = (realm.clone(), lane);
        self.note_data_block(&key, bid);
        self.has_overlays = true;
        self.overlay_keys.insert(key);
        if commit {
            self.commit()?;
        }
        Ok(true)
    }
}

fn set_value(values: &mut [(String, Val)], name: &str, v: Val) {
    if let Some(slot) = values.iter_mut().find(|(k, _)| k == name) {
        slot.1 = v;
    }
}
