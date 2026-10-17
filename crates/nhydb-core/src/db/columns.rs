//! Spaltenorientierte Batch-Operationen (Grundlage der NumPy-API) und
//! logisches Löschen von Objekten.
//!
//! Alle Schleifen laufen in Rust; die Eingabe sind typisierte Slices.

use crate::address::{self, Lane};
use crate::codec::{decode_value, skip_value, TypeCode};
use crate::error::{Error, Result};
use crate::nat::Nat;
use crate::schema::LayoutKind;
use crate::val::Val;
use crate::vmap;

use super::dense::{ColData, DenseRows};
use super::layout::LayoutRef;
use super::sparse::CellInput;
use super::{Db, SVal};

impl ColData<'_> {
    /// Teilbereich `[a, b)`.
    pub fn slice(&self, a: usize, b: usize) -> ColData<'_> {
        match self {
            ColData::U8(x) => ColData::U8(&x[a..b]),
            ColData::U16(x) => ColData::U16(&x[a..b]),
            ColData::U32(x) => ColData::U32(&x[a..b]),
            ColData::U64(x) => ColData::U64(&x[a..b]),
            ColData::I8(x) => ColData::I8(&x[a..b]),
            ColData::I16(x) => ColData::I16(&x[a..b]),
            ColData::I32(x) => ColData::I32(&x[a..b]),
            ColData::I64(x) => ColData::I64(&x[a..b]),
            ColData::F32(x) => ColData::F32(&x[a..b]),
            ColData::F64(x) => ColData::F64(&x[a..b]),
            ColData::Bool(x) => ColData::Bool(&x[a..b]),
            ColData::Var { offsets, data, utf8 } => ColData::Var { offsets: &offsets[a..=b], data, utf8: *utf8 },
            ColData::Vals(x) => ColData::Vals(&x[a..b]),
        }
    }
}

/// Typisierte Ausgabespalte für `read_columns`.
#[derive(Clone, Debug)]
pub enum OutCol {
    I64(Vec<i64>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Bool(Vec<u8>),
    Vals(Vec<Val>),
}

impl OutCol {
    fn for_type(t: TypeCode, enum_: bool, n: usize) -> OutCol {
        if enum_ {
            return OutCol::Vals(Vec::with_capacity(n));
        }
        match t {
            TypeCode::U64 | TypeCode::VarUInt => OutCol::U64(Vec::with_capacity(n)),
            TypeCode::U8 | TypeCode::U16 | TypeCode::U32 | TypeCode::I8 | TypeCode::I16 | TypeCode::I32 | TypeCode::I64 | TypeCode::VarInt => {
                OutCol::I64(Vec::with_capacity(n))
            }
            TypeCode::F32 => OutCol::F32(Vec::with_capacity(n)),
            TypeCode::F64 => OutCol::F64(Vec::with_capacity(n)),
            TypeCode::Bool => OutCol::Bool(Vec::with_capacity(n)),
            _ => OutCol::Vals(Vec::with_capacity(n)),
        }
    }

    fn to_vals(&self) -> Vec<Val> {
        match self {
            OutCol::I64(v) => v.iter().map(|x| Val::Int(*x)).collect(),
            OutCol::U64(v) => v.iter().map(|x| Val::int(*x)).collect(),
            OutCol::F32(v) => v.iter().map(|x| Val::Float(*x as f64)).collect(),
            OutCol::F64(v) => v.iter().map(|x| Val::Float(*x)).collect(),
            OutCol::Bool(v) => v.iter().map(|x| Val::Bool(*x != 0)).collect(),
            OutCol::Vals(v) => v.clone(),
        }
    }

    /// Hängt einen Wert an; `None`/fehlend → Nullwert (0 / NaN / None).
    /// Passt der Wert nicht in den Spaltentyp, wird auf `Vals` umgestellt.
    fn push(&mut self, v: Option<&Val>) {
        let ok = match (&mut *self, v) {
            (OutCol::I64(c), None | Some(Val::None)) => {
                c.push(0);
                true
            }
            (OutCol::U64(c), None | Some(Val::None)) => {
                c.push(0);
                true
            }
            (OutCol::F32(c), None | Some(Val::None)) => {
                c.push(f32::NAN);
                true
            }
            (OutCol::F64(c), None | Some(Val::None)) => {
                c.push(f64::NAN);
                true
            }
            (OutCol::Bool(c), None | Some(Val::None)) => {
                c.push(0);
                true
            }
            (OutCol::I64(c), Some(Val::Int(i))) => {
                c.push(*i);
                true
            }
            (OutCol::U64(c), Some(x)) if x.as_nat().and_then(|n| n.as_u64()).is_some() => {
                c.push(x.as_nat().unwrap().as_u64().unwrap());
                true
            }
            (OutCol::F32(c), Some(Val::Float(f))) => {
                c.push(*f as f32);
                true
            }
            (OutCol::F64(c), Some(Val::Float(f))) => {
                c.push(*f);
                true
            }
            (OutCol::Bool(c), Some(Val::Bool(b))) => {
                c.push(*b as u8);
                true
            }
            (OutCol::Vals(c), x) => {
                c.push(x.cloned().unwrap_or(Val::None));
                true
            }
            _ => false,
        };
        if !ok {
            let mut vals = self.to_vals();
            vals.push(v.cloned().unwrap_or(Val::None));
            *self = OutCol::Vals(vals);
        }
    }
}

/// Ergebnis von `read_columns`.
#[derive(Clone, Debug)]
pub struct ColumnsOut {
    pub names: Vec<String>,
    pub cols: Vec<OutCol>,
    pub present: Vec<u8>,
}

impl Db {
    /// Schreibt Objekte spaltenweise über ein Layout. `oids` müssen streng
    /// aufsteigend sein. CLASSIC nutzt den Dense-Spaltenpfad (zusammenhängende
    /// Läufe), COLUMN/PACKED/CUSTOM erzeugen Sparse-Zellen.
    pub fn put_object_columns(
        &mut self,
        r: &LayoutRef,
        oids: &[Nat],
        cols: &[(String, ColData)],
        lane: Lane,
        commit: bool,
        u_window: u64,
    ) -> Result<Val> {
        let lay = self.layout_by_ref(r)?.clone();
        let n = oids.len();
        for (name, c) in cols {
            if c.len() != n {
                return Err(Error::Value(format!("Spalte '{name}' hat {} Werte, erwartet {n}", c.len())));
            }
        }
        for (i, o) in oids.iter().enumerate() {
            address::require_positive("object_id", o)?;
            if i > 0 && oids[i - 1] >= *o {
                return Err(Error::Schema("object_ids muessen streng aufsteigend sein (keine Duplikate)".into()));
            }
        }
        if n == 0 {
            if commit {
                self.commit()?;
            }
            return Ok(vmap! {"objects" => 0i64, "cells" => 0i64, "blocks" => 0i64, "bytes" => 0i64});
        }
        if lay.kind == LayoutKind::Classic {
            let sid = lay.shape_id.ok_or_else(|| Error::Schema("CLASSIC Layout ohne Shape".into()))?;
            let shape = self.shapes.get(&sid).ok_or_else(|| Error::Key(sid.to_string()))?.clone();
            let mut ordered: Vec<&ColData> = Vec::with_capacity(shape.fields.len());
            for f in &shape.fields {
                let c = cols
                    .iter()
                    .find(|(nm, _)| *nm == f.name)
                    .map(|(_, c)| c)
                    .ok_or_else(|| Error::Schema(format!("Pflichtfeld '{}' fehlt", f.name)))?;
                ordered.push(c);
            }
            let extra: Vec<&(String, ColData)> = cols.iter().filter(|(nm, _)| shape.by_name(nm).is_none()).collect();
            let (mut blocks, mut bytes, mut cells) = (0i64, 0i64, 0i64);
            let mut a = 0usize;
            while a < n {
                let mut b = a + 1;
                while b < n && oids[b] == oids[b - 1].add_u64(1) {
                    b += 1;
                }
                let run: Vec<ColData> = ordered.iter().map(|c| c.slice(a, b)).collect();
                let mut ext: Vec<(Nat, Vec<(Val, Val)>)> = Vec::new();
                if !extra.is_empty() {
                    for (i, oid) in oids.iter().enumerate().take(b).skip(a) {
                        let items: Vec<(Val, Val)> = extra
                            .iter()
                            .filter_map(|(nm, c)| {
                                let v = c.get_val(i);
                                if v.is_none() { None } else { Some((Val::str(nm), v)) }
                            })
                            .collect();
                        if !items.is_empty() {
                            ext.push((oid.clone(), items));
                        }
                    }
                }
                let res = self.put_dense_batch(
                    &lay.realm_id,
                    &oids[a],
                    &DenseRows::Columns { cols: &run, count: b - a },
                    Some(sid),
                    &ext,
                    lane,
                    false,
                )?;
                blocks += 1;
                bytes += res.get("bytes").and_then(|v| v.as_i64()).unwrap_or(0);
                cells += ((b - a) * shape.fields.len()) as i64;
                if let Some(s) = res.get("sparse").filter(|s| s.is_truthy()) {
                    blocks += 1;
                    bytes += s.get("bytes").and_then(|v| v.as_i64()).unwrap_or(0);
                    cells += s.get("cells").and_then(|v| v.as_i64()).unwrap_or(0);
                }
                a = b;
            }
            if commit {
                self.commit()?;
            }
            return Ok(vmap! {"objects" => n as i64, "cells" => cells, "blocks" => blocks, "bytes" => bytes, "kind" => "CLASSIC"});
        }
        let mut out_cells: Vec<CellInput> = Vec::with_capacity(n * cols.len());
        match lay.kind {
            LayoutKind::Column => {
                let fields = lay.fields();
                let mut col_u: Vec<Nat> = Vec::with_capacity(cols.len());
                for (name, _) in cols {
                    let mut hit = None;
                    for f in &fields {
                        if &f.name() == name {
                            hit = Some(f.u()?);
                        }
                    }
                    col_u.push(hit.ok_or_else(|| Error::Schema(format!("Layout '{}': Feld '{}' unbekannt", lay.name, name)))?);
                }
                for (i, oid) in oids.iter().enumerate() {
                    for ((_, c), u) in cols.iter().zip(col_u.iter()) {
                        out_cells.push(CellInput::new(u.clone(), oid.clone(), c.get_val(i)));
                    }
                }
            }
            LayoutKind::Packed => {
                let u = lay.packed_u()?;
                let span = lay.packed_span()?;
                let fields = lay.fields();
                let mut col_off: Vec<Nat> = Vec::with_capacity(cols.len());
                for (name, _) in cols {
                    let mut hit = None;
                    for f in &fields {
                        if &f.name() == name {
                            hit = Some(f.offset()?);
                        }
                    }
                    let off = hit.ok_or_else(|| Error::Schema(format!("Layout '{}': Feld '{}' unbekannt", lay.name, name)))?;
                    match off.to_biguint().map(Nat::from) {
                        Some(o) if o < span => col_off.push(o),
                        _ => return Err(Error::Schema("PACKED offset ausserhalb span".into())),
                    }
                }
                for (i, oid) in oids.iter().enumerate() {
                    let anchor = oid.sub_u64(1).unwrap().mul(&span).add_u64(1);
                    if &anchor > address::realm_span() {
                        return Err(Error::RealmLimit("PACKED record liegt ausserhalb dieses Realms".into()));
                    }
                    for ((_, c), off) in cols.iter().zip(col_off.iter()) {
                        out_cells.push(CellInput::new(u.clone(), anchor.add(off), c.get_val(i)));
                    }
                }
            }
            LayoutKind::Custom => {
                for (i, oid) in oids.iter().enumerate() {
                    for (name, c) in cols {
                        let a = self.logical_address_in(&lay, oid, &Val::str(name), lane)?;
                        out_cells.push(CellInput::new(a.u().clone(), a.v_local().clone(), c.get_val(i)));
                    }
                }
            }
            LayoutKind::Classic => unreachable!(),
        }
        let mut res = self.put_points(&lay.realm_id, out_cells, lane, commit, u_window)?;
        res.set("objects", Val::Int(n as i64));
        res.set("kind", Val::str(lay.kind.name()));
        Ok(res)
    }

    /// Liest `count` CLASSIC-Records ab `u_start` spaltenweise
    /// (inkl. Sparse-Overlays, Tombstones, Deletes).
    pub fn read_columns(&self, realm: &Nat, u_start: &Nat, count: usize, fields: Option<&[Val]>, lane: Lane) -> Result<ColumnsOut> {
        self.require_realm(realm)?;
        address::require_positive("u_start", u_start)?;
        let base = self.current_base_shape(realm);
        let wanted: Vec<(String, Nat)> = match fields {
            Some(fs) => {
                let mut out = Vec::new();
                for f in fs {
                    let v = self.resolve_v(realm, f)?;
                    let name = match f {
                        Val::Str(s) => s.clone(),
                        _ => base.and_then(|b| b.by_v(&v)).map(|x| x.name.clone()).unwrap_or_else(|| self.extension_label(realm, &v)),
                    };
                    out.push((name, v));
                }
                out
            }
            None => base
                .ok_or_else(|| Error::Schema("kein Shape angegeben/default".into()))?
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.v.clone()))
                .collect(),
        };
        let mut cols: Vec<OutCol> = wanted
            .iter()
            .map(|(_, v)| {
                let fd = base.and_then(|b| b.by_v(v)).cloned().or_else(|| self.extension_defs.get(&(realm.clone(), v.clone())).map(|e| e.as_field()));
                match fd {
                    Some(f) => OutCol::for_type(f.typ, f.enum_map.is_some(), count),
                    None => OutCol::Vals(Vec::with_capacity(count)),
                }
            })
            .collect();
        let mut present = Vec::with_capacity(count);
        let key = (realm.clone(), lane);
        let sparse_idx = self.sparse.get(&key);
        let overlay = self.dense_overlap_keys.contains(&key);
        let check_deletes = !self.deletes.is_empty();
        let ncols = wanted.len();
        let mut row: Vec<Option<Val>> = vec![None; ncols];
        let mut hits = Vec::new();
        let mut u = u_start.clone();
        let mut done = 0usize;
        while done < count {
            // Lauf innerhalb eines Segments bestimmen
            let seg = self.resolve_segment(realm, &u, lane);
            let run = match seg {
                Some(s) if !overlay => {
                    let left = s.u_end.checked_sub(&u).and_then(|d| d.as_u64()).unwrap_or(0) as usize + 1;
                    left.min(count - done)
                }
                _ => 1,
            };
            match seg {
                Some(seg) => {
                    let shape = self.shape_of(seg.shape_id)?;
                    let plan: Vec<Option<usize>> =
                        shape.fields.iter().map(|f| wanted.iter().position(|(_, v)| *v == f.v)).collect();
                    let payload = self.block_payload(seg.block_offset)?;
                    let toff = (seg.offsets_offset - seg.payload_offset) as usize;
                    let rbase = (seg.records_offset - seg.payload_offset) as usize;
                    let first = u.checked_sub(&seg.u_start).and_then(|d| d.as_u64()).unwrap() as usize;
                    for k in 0..run {
                        let i = first + k;
                        let st = u32::from_le_bytes(payload[toff + i * 4..toff + i * 4 + 4].try_into().unwrap()) as usize;
                        let en = u32::from_le_bytes(payload[toff + i * 4 + 4..toff + i * 4 + 8].try_into().unwrap()) as usize;
                        if en < st || rbase + en > payload.len() {
                            return Err(Error::Corruption("DATA Recordoffset ungueltig".into()));
                        }
                        let uu = if k == 0 { u.clone() } else { u.add_u64(k as u64) };
                        if check_deletes {
                            if let Some(d) = self.deletes.get(&(realm.clone(), lane, uu.clone())) {
                                if *d > seg.block_id {
                                    present.push(0);
                                    for c in cols.iter_mut() {
                                        c.push(None);
                                    }
                                    continue;
                                }
                            }
                        }
                        let raw = &payload[rbase + st..rbase + en];
                        row.iter_mut().for_each(|x| *x = None);
                        let mut p = 0usize;
                        for (f, ci) in shape.fields.iter().zip(plan.iter()) {
                            match ci {
                                Some(ci) => row[*ci] = Some(decode_value(f, raw, &mut p)?),
                                None => skip_value(f, raw, &mut p)?,
                            }
                        }
                        if p < raw.len() {
                            let (ext, _) = self.decode_extensions(realm, raw, p)?;
                            for (v, x) in ext {
                                if let Some(ci) = wanted.iter().position(|(_, w)| *w == v) {
                                    row[ci] = Some(x);
                                }
                            }
                        }
                        if let Some(si) = sparse_idx {
                            hits.clear();
                            si.iv.stab(&uu, &mut hits);
                            if !hits.is_empty() {
                                hits.sort_unstable();
                                let upd = self.sparse_states_direct(realm, &uu, si, &hits)?;
                                for (ci, (_, v)) in wanted.iter().enumerate() {
                                    match upd.get(v) {
                                        Some(SVal::V(x)) => row[ci] = Some(x.clone()),
                                        Some(SVal::Tomb) => row[ci] = None,
                                        None => {}
                                    }
                                }
                            }
                        }
                        present.push(1);
                        for (ci, c) in cols.iter_mut().enumerate() {
                            c.push(row[ci].as_ref());
                        }
                    }
                }
                None => {
                    present.push(0);
                    for c in cols.iter_mut() {
                        c.push(None);
                    }
                }
            }
            u = u.add_u64(run as u64);
            done += run;
        }
        Ok(ColumnsOut { names: wanted.into_iter().map(|(n, _)| n).collect(), cols, present })
    }

    /// Sparse-Zustand einer u-Gruppe ohne Merge-/Record-Cache (Batch-Lesen).
    fn sparse_states_direct(
        &self,
        realm: &Nat,
        u: &Nat,
        idx: &super::SparseKeyIndex,
        hits: &[usize],
    ) -> Result<super::SparseMap> {
        let mut out = super::SparseMap::new();
        for &i in hits {
            let seg = &idx.segs[i];
            if let Some((payload, rel, len)) = self.read_sparse_record_bytes(seg, u)? {
                for (v, m) in self.decode_sparse_record(realm, u, &payload[rel..rel + len])? {
                    out.insert(v, m.value);
                }
            }
        }
        Ok(out)
    }

    /// Logisches Löschen eines Objekts. CLASSIC: BASE-Record (DELETE-Block)
    /// und lebende Sparse-Zellen der u-Gruppe; sonst alle belegten Zellen
    /// per Tombstone. Rückgabe: Anzahl gelöschter Records + Zellen.
    pub fn delete_object(&mut self, r: &LayoutRef, obj: &Nat, lane: Lane, commit: bool) -> Result<u64> {
        let lay = self.layout_by_ref(r)?.clone();
        address::require_positive("object_id", obj)?;
        let mut n = 0u64;
        let mut tombs: Vec<CellInput> = Vec::new();
        if lay.kind == LayoutKind::Classic {
            if self.delete(&lay.realm_id, obj, lane, false)? {
                n += 1;
            }
            let upd = self.sparse_updates_for_u(&lay.realm_id, obj, lane)?;
            for (v, s) in upd.iter() {
                if !s.is_tomb() {
                    tombs.push(CellInput::tombstone(obj.clone(), v.clone()));
                }
            }
        } else {
            for nn in self.occupied_n(r, obj, lane)? {
                let a = address::n_to_address(&nn)?;
                tombs.push(CellInput::tombstone(a.u().clone(), a.v_local().clone()));
            }
        }
        n += tombs.len() as u64;
        if !tombs.is_empty() {
            self.put_points(&lay.realm_id, tombs, lane, false, 10_000)?;
        }
        if commit {
            self.commit()?;
        }
        Ok(n)
    }
}
