//! Column-oriented batch operations (basis of the NumPy API) and
//! logical deletion of objects.
//!
//! All loops run in Rust; inputs are typed slices.

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
    /// Subrange `[a, b)`.
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

/// Typed output column for `read_columns`.
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
    /// Empty column for a field type (the dtype depends only on the type,
    /// never on the stored values).
    fn for_type(t: TypeCode, enum_: bool) -> OutCol {
        if enum_ {
            return OutCol::Vals(Vec::new());
        }
        match t {
            TypeCode::U64 | TypeCode::VarUInt => OutCol::U64(Vec::new()),
            TypeCode::U8 | TypeCode::U16 | TypeCode::U32 | TypeCode::I8 | TypeCode::I16 | TypeCode::I32 | TypeCode::I64 | TypeCode::VarInt => {
                OutCol::I64(Vec::new())
            }
            TypeCode::F32 => OutCol::F32(Vec::new()),
            TypeCode::F64 => OutCol::F64(Vec::new()),
            TypeCode::Bool => OutCol::Bool(Vec::new()),
            _ => OutCol::Vals(Vec::new()),
        }
    }

    /// Bytes per row of this column (for the memory estimate).
    fn row_bytes(&self) -> usize {
        match self {
            OutCol::I64(_) | OutCol::U64(_) | OutCol::F64(_) => 8,
            OutCol::F32(_) => 4,
            OutCol::Bool(_) => 1,
            OutCol::Vals(_) => std::mem::size_of::<Val>(),
        }
    }

    /// Fallible reservation of `n` rows (F018: no abort on huge counts).
    fn try_reserve(&mut self, n: usize) -> Result<()> {
        let r = match self {
            OutCol::I64(v) => v.try_reserve_exact(n),
            OutCol::U64(v) => v.try_reserve_exact(n),
            OutCol::F32(v) => v.try_reserve_exact(n),
            OutCol::F64(v) => v.try_reserve_exact(n),
            OutCol::Bool(v) => v.try_reserve_exact(n),
            OutCol::Vals(v) => v.try_reserve_exact(n),
        };
        r.map_err(|e| Error::Memory(format!("cannot allocate {n} result rows: {e}")))
    }

    /// NumPy dtype name (for error messages).
    pub fn dtype_name(&self) -> &'static str {
        match self {
            OutCol::I64(_) => "int64",
            OutCol::U64(_) => "uint64",
            OutCol::F32(_) => "float32",
            OutCol::F64(_) => "float64",
            OutCol::Bool(_) => "bool",
            OutCol::Vals(_) => "object",
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

    /// Appends a value; `None`/missing → null value (0 / NaN / None).
    /// If the value does not fit the column type, the column switches to
    /// `Vals` when `allow_object` is set; otherwise nothing is appended and
    /// `false` is returned (deterministic dtype, F033).
    fn push(&mut self, v: Option<&Val>, allow_object: bool) -> bool {
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
            if !allow_object {
                return false;
            }
            let mut vals = self.to_vals();
            vals.push(v.cloned().unwrap_or(Val::None));
            *self = OutCol::Vals(vals);
        }
        true
    }
}

impl OutCol {
    /// Removes the last row (undo of a direct decode, P018).
    fn pop(&mut self) {
        match self {
            OutCol::I64(v) => drop(v.pop()),
            OutCol::U64(v) => drop(v.pop()),
            OutCol::F32(v) => drop(v.pop()),
            OutCol::F64(v) => drop(v.pop()),
            OutCol::Bool(v) => drop(v.pop()),
            OutCol::Vals(v) => drop(v.pop()),
        }
    }
}

/// P018: decodes a stored BASE value straight into a typed column (no
/// `Val`). Returns `Ok(false)` with `pos` unchanged when the combination of
/// field type and column needs the generic path (enums, text, numbers
/// beyond 64 bit, a field type differing from the column type, object
/// columns); the values pushed are exactly those of `decode_value` + `push`.
fn decode_into(c: &mut OutCol, f: &crate::codec::FieldDef, raw: &[u8], pos: &mut usize) -> Result<bool> {
    if f.enum_map.is_some() {
        return Ok(false);
    }
    fn fixed<'a>(raw: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
        let end = pos.checked_add(n).filter(|e| *e <= raw.len()).ok_or_else(|| Error::Corruption("fixed-width value incomplete".into()))?;
        let b = &raw[*pos..end];
        *pos = end;
        Ok(b)
    }
    match (c, f.typ) {
        (OutCol::I64(c), TypeCode::U8) => c.push(fixed(raw, pos, 1)?[0] as i64),
        (OutCol::I64(c), TypeCode::I8) => c.push(fixed(raw, pos, 1)?[0] as i8 as i64),
        (OutCol::I64(c), TypeCode::U16) => c.push(u16::from_le_bytes(fixed(raw, pos, 2)?.try_into().unwrap()) as i64),
        (OutCol::I64(c), TypeCode::I16) => c.push(i16::from_le_bytes(fixed(raw, pos, 2)?.try_into().unwrap()) as i64),
        (OutCol::I64(c), TypeCode::U32) => c.push(u32::from_le_bytes(fixed(raw, pos, 4)?.try_into().unwrap()) as i64),
        (OutCol::I64(c), TypeCode::I32) => c.push(i32::from_le_bytes(fixed(raw, pos, 4)?.try_into().unwrap()) as i64),
        (OutCol::I64(c), TypeCode::I64) => c.push(i64::from_le_bytes(fixed(raw, pos, 8)?.try_into().unwrap())),
        (OutCol::U64(c), TypeCode::U64) => c.push(u64::from_le_bytes(fixed(raw, pos, 8)?.try_into().unwrap())),
        (OutCol::F32(c), TypeCode::F32) => c.push(f32::from_le_bytes(fixed(raw, pos, 4)?.try_into().unwrap())),
        (OutCol::F64(c), TypeCode::F64) => c.push(f64::from_le_bytes(fixed(raw, pos, 8)?.try_into().unwrap())),
        (OutCol::Bool(c), TypeCode::Bool) => c.push((fixed(raw, pos, 1)?[0] != 0) as u8),
        (OutCol::U64(c), TypeCode::VarUInt) => match crate::nat::get_vu64(raw, pos)? {
            Some(v) => c.push(v),
            None => return Ok(false),
        },
        (OutCol::I64(c), TypeCode::VarInt) => match crate::nat::get_vu64(raw, pos)? {
            // zigzag: every u64 code maps into i64
            Some(z) => c.push(if z % 2 == 0 { (z / 2) as i64 } else { -((z / 2) as i64) - 1 }),
            None => return Ok(false),
        },
        _ => return Ok(false),
    }
    Ok(true)
}

/// Physical memory of the machine in bytes (None if unknown).
pub(crate) fn physical_memory_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions.
        let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
        if pages > 0 && size > 0 {
            return (pages as u64).checked_mul(size as u64);
        }
        None
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: plain C struct, dwLength set as required by the API.
        unsafe {
            let mut st: MEMORYSTATUSEX = std::mem::zeroed();
            st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut st) != 0 {
                return Some(st.ullTotalPhys);
            }
        }
        None
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Result of `read_columns`.
#[derive(Clone, Debug)]
pub struct ColumnsOut {
    pub names: Vec<String>,
    pub cols: Vec<OutCol>,
    pub present: Vec<u8>,
    /// E3: per requested field, 1 where the cell lives (a stored `None`
    /// included), 0 where it does not (never written, deleted, field missing
    /// from the stored shape, record missing); only with `masks`
    pub cell_masks: Option<Vec<Vec<u8>>>,
}

impl Db {
    /// Writes objects column-wise via a layout. `oids` must be strictly
    /// ascending. CLASSIC uses the dense column path (contiguous
    /// runs), COLUMN/PACKED/CUSTOM produce sparse cells.
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
                return Err(Error::Value(format!("Column '{name}' has {} values, expected {n}", c.len())));
            }
        }
        for (i, o) in oids.iter().enumerate() {
            address::require_positive("object_id", o)?;
            if i > 0 && oids[i - 1] >= *o {
                return Err(Error::Schema("object_ids must be strictly ascending (no duplicates)".into()));
            }
        }
        if n == 0 {
            if commit {
                self.commit()?;
            }
            return Ok(vmap! {"objects" => 0i64, "cells" => 0i64, "blocks" => 0i64, "bytes" => 0i64});
        }
        if lay.kind == LayoutKind::Classic {
            let sid = lay.shape_id.ok_or_else(|| Error::Schema("CLASSIC layout without shape".into()))?;
            let shape = self.shapes.get(&sid).ok_or_else(|| Error::Key(sid.to_string()))?.clone();
            let mut ordered: Vec<&ColData> = Vec::with_capacity(shape.fields.len());
            for f in &shape.fields {
                let c = cols
                    .iter()
                    .find(|(nm, _)| *nm == f.name)
                    .map(|(_, c)| c)
                    .ok_or_else(|| Error::Schema(format!("Required field '{}' missing", f.name)))?;
                ordered.push(c);
            }
            let extra: Vec<&(String, ColData)> = cols.iter().filter(|(nm, _)| shape.by_name(nm).is_none()).collect();
            // N2 (RUN8): one DATA block per contiguous run, all or none
            let (blocks, bytes, cells) = self.write_scope(|db| {
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
                    let res = db.put_dense_batch(
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
                Ok((blocks, bytes, cells))
            })?;
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
                    col_u.push(hit.ok_or_else(|| Error::Schema(format!("Layout '{}': field '{}' unknown", lay.name, name)))?);
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
                    let off = hit.ok_or_else(|| Error::Schema(format!("Layout '{}': field '{}' unknown", lay.name, name)))?;
                    match off.to_biguint().map(Nat::from) {
                        Some(o) if o < span => col_off.push(o),
                        _ => return Err(Error::Schema("PACKED offset outside span".into())),
                    }
                }
                for (i, oid) in oids.iter().enumerate() {
                    let anchor = oid.sub_u64(1).unwrap().mul(&span).add_u64(1);
                    if &anchor > address::realm_span() {
                        return Err(Error::RealmLimit("PACKED record lies outside this realm".into()));
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

    /// Reads `count` CLASSIC records starting at `u_start` column-wise
    /// (including sparse overlays, tombstones, deletes).
    ///
    /// The result always has exactly `count` rows. The dtype of every column
    /// depends only on the field type; a stored value that does not fit it
    /// (e.g. varuint > 2^64-1) raises `ValueError` unless `allow_object` is
    /// set, in which case that column becomes an object column.
    /// Memory is reserved fallibly up front; a `count` whose result would
    /// not fit into the physical memory raises `MemoryError` (F018).
    pub fn read_columns(
        &self,
        realm: &Nat,
        u_start: &Nat,
        count: usize,
        fields: Option<&[Val]>,
        lane: Lane,
        allow_object: bool,
    ) -> Result<ColumnsOut> {
        self.read_columns_ext(realm, u_start, count, fields, lane, allow_object, false)
    }

    /// `read_columns` with optional cell masks (E3, `read_numpy(masks=True)`).
    ///
    /// `present` (`_present`) follows the existence rule of the cell cursor
    /// and SQL (R9E-02): a record exists when at least one of its cells
    /// lives – a DENSE record whose cells are all tombstoned does not, a u
    /// with live SPARSE cells only does (its values are read from them).
    #[allow(clippy::too_many_arguments)]
    pub fn read_columns_ext(
        &self,
        realm: &Nat,
        u_start: &Nat,
        count: usize,
        fields: Option<&[Val]>,
        lane: Lane,
        allow_object: bool,
        masks: bool,
    ) -> Result<ColumnsOut> {
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
                .ok_or_else(|| Error::Schema("no shape given/default".into()))?
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
                    Some(f) => OutCol::for_type(f.typ, f.enum_map.is_some()),
                    None => OutCol::Vals(Vec::new()),
                }
            })
            .collect();
        // F018: plausibility check against the physical memory, then fallible reservation.
        let row_bytes: usize = 1 + cols.iter().map(|c| c.row_bytes()).sum::<usize>();
        let too_big = || Error::Memory(format!("read_numpy: count={count} needs more memory than available"));
        let need = (count as u64).checked_mul(row_bytes as u64).ok_or_else(too_big)?;
        if let Some(phys) = physical_memory_bytes() {
            if need > phys {
                return Err(Error::Memory(format!(
                    "read_numpy: count={count} would need about {} MiB, more than the physical memory ({} MiB)",
                    need >> 20,
                    phys >> 20
                )));
            }
        }
        for c in cols.iter_mut() {
            c.try_reserve(count)?;
        }
        let mut present: Vec<u8> = Vec::new();
        present.try_reserve_exact(count).map_err(|e| Error::Memory(format!("cannot allocate {count} result rows: {e}")))?;
        let mut cmask: Vec<Vec<u8>> = Vec::new();
        if masks {
            for _ in 0..wanted.len() {
                let mut m: Vec<u8> = Vec::new();
                m.try_reserve_exact(count).map_err(|e| Error::Memory(format!("cannot allocate {count} result rows: {e}")))?;
                cmask.push(m);
            }
        }
        // per row: does the requested cell live (E3)
        let mut live: Vec<bool> = vec![false; wanted.len()];
        let key = (realm.clone(), lane);
        let sparse_idx = self.sparse.get(&key);
        let overlay = self.dense_overlap_keys.contains(&key);
        let check_deletes = self.has_deletes(&key);
        let ncols = wanted.len();
        let mut row: Vec<Option<Val>> = vec![None; ncols];
        let mut direct: Vec<bool> = vec![false; ncols];
        let mut hits = Vec::new();
        let mut u = u_start.clone();
        let mut done = 0usize;
        // F021 rest (G3-03): start of the uncovered/deleted run the read is in
        let mut gap_start: Option<Nat> = None;
        while done < count {
            // determine the run within one segment
            let seg = self.resolve_segment(realm, &u, lane)?;
            match seg {
                None if gap_start.is_none() => gap_start = Some(u.clone()),
                None => {}
                Some(_) => gap_start = None,
            }
            let run = match &seg {
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
                    // Only the offset table window and the records of the
                    // run are read (whole payload from the cache, or direct
                    // range reads of a block verified earlier, P001/P003).
                    let acc = self.dense_block_access(&seg)?;
                    let plen = acc.payload_len() as usize;
                    let bad = || Error::Corruption("DATA segment offsets invalid".into());
                    let toff = seg.offsets_offset.checked_sub(seg.payload_offset).ok_or_else(bad)? as usize;
                    let rbase = seg.records_offset.checked_sub(seg.payload_offset).ok_or_else(bad)? as usize;
                    let first = u
                        .checked_sub(&seg.u_start)
                        .and_then(|d| d.as_u64())
                        .ok_or_else(|| Error::Corruption("DATA segment does not contain u".into()))? as usize;
                    // corrupted offset tables must not panic (D006: no PanicException)
                    if ((first + run) as u64) > seg.count || toff + (first + run) * 4 + 4 > plen {
                        return Err(Error::Corruption("DATA offset table incomplete".into()));
                    }
                    let tab = self.access_range(&acc, seg.block_offset, toff + first * 4, run * 4 + 4)?;
                    let at = |k: usize| u32::from_le_bytes(tab[k * 4..k * 4 + 4].try_into().unwrap()) as usize;
                    let (st0, en_last) = (at(0), at(run));
                    if en_last < st0 || rbase + en_last > plen {
                        return Err(Error::Corruption("DATA record offset invalid".into()));
                    }
                    let recs = self.access_range(&acc, seg.block_offset, rbase + st0, en_last - st0)?;
                    for k in 0..run {
                        let (st, en) = (at(k), at(k + 1));
                        if en < st || st < st0 || en > en_last {
                            return Err(Error::Corruption("DATA record offset invalid".into()));
                        }
                        let uu = if k == 0 { u.clone() } else { u.add_u64(k as u64) };
                        if check_deletes {
                            if let Some(d) = self.delete_block_id(&key, &uu)? {
                                if d > seg.block_id {
                                    // the DENSE record is deleted; newer SPARSE
                                    // cells of u may still live
                                    self.read_sparse_only_row(realm, &uu, sparse_idx, &wanted, &mut cols, &mut present, &mut cmask, &mut hits, allow_object)?;
                                    continue;
                                }
                            }
                        }
                        let raw = &recs[st - st0..en - st0];
                        row.iter_mut().for_each(|x| *x = None);
                        let has_sparse = match sparse_idx {
                            Some(si) => {
                                hits.clear();
                                si.idx.stab(self, &uu, &mut hits)?;
                                !hits.is_empty()
                            }
                            None => false,
                        };
                        // P018: records without sparse overlay and without
                        // extensions are decoded straight into the columns.
                        if !has_sparse {
                            direct.iter_mut().for_each(|x| *x = false);
                            let mut p = 0usize;
                            for (f, ci) in shape.fields.iter().zip(plan.iter()) {
                                match ci {
                                    Some(ci) => {
                                        if decode_into(&mut cols[*ci], f, raw, &mut p)? {
                                            direct[*ci] = true;
                                        } else {
                                            row[*ci] = Some(decode_value(f, raw, &mut p)?);
                                        }
                                    }
                                    None => skip_value(f, raw, &mut p)?,
                                }
                            }
                            if p == raw.len() {
                                present.push(1);
                                for (ci, m) in cmask.iter_mut().enumerate() {
                                    m.push(plan.contains(&Some(ci)) as u8);
                                }
                                for (ci, c) in cols.iter_mut().enumerate() {
                                    if !direct[ci] && !c.push(row[ci].as_ref(), allow_object) {
                                        return Err(Error::Value(format!(
                                            "read_numpy: value {} of field '{}' at u={} does not fit dtype {}; \
                                             use object_fallback=True (object column) or get()/range()",
                                            row[ci].as_ref().map(|x| x.py_repr()).unwrap_or_default(),
                                            wanted[ci].0,
                                            uu,
                                            c.dtype_name()
                                        )));
                                    }
                                }
                                continue;
                            }
                            // extensions follow: undo and take the generic path
                            for (ci, c) in cols.iter_mut().enumerate() {
                                if direct[ci] {
                                    c.pop();
                                }
                            }
                            row.iter_mut().for_each(|x| *x = None);
                        }
                        let mut p = 0usize;
                        live.iter_mut().for_each(|x| *x = false);
                        for (f, ci) in shape.fields.iter().zip(plan.iter()) {
                            match ci {
                                Some(ci) => {
                                    row[*ci] = Some(decode_value(f, raw, &mut p)?);
                                    live[*ci] = true;
                                }
                                None => skip_value(f, raw, &mut p)?,
                            }
                        }
                        // live cells of the record (existence rule, R9E-02)
                        let mut n_live = shape.fields.len();
                        let mut ext_vs: Vec<Nat> = Vec::new();
                        if p < raw.len() {
                            let (ext, _) = self.decode_extensions(realm, raw, p)?;
                            for (v, x) in ext {
                                if let Some(ci) = wanted.iter().position(|(_, w)| *w == v) {
                                    row[ci] = Some(x);
                                    live[ci] = true;
                                }
                                ext_vs.push(v);
                            }
                            n_live += ext_vs.len();
                        }
                        if let (Some(_), true) = (sparse_idx, has_sparse) {
                            {
                                hits.sort_unstable_by_key(|s| s.block_id);
                                let upd = self.sparse_states_direct(realm, &uu, &hits)?;
                                for (ci, (_, v)) in wanted.iter().enumerate() {
                                    match upd.get(v) {
                                        // F1: older than the record → the record's value
                                        Some((_, bid)) if super::dense_hides(shape, seg.block_id, v, *bid) => {}
                                        Some((SVal::V(x), _)) => {
                                            row[ci] = Some(x.clone());
                                            live[ci] = true;
                                        }
                                        Some((SVal::Tomb, _)) => {
                                            row[ci] = None;
                                            live[ci] = false;
                                        }
                                        None => {}
                                    }
                                }
                                for (v, (st, bid)) in upd.iter() {
                                    if super::dense_hides(shape, seg.block_id, v, *bid) {
                                        continue;
                                    }
                                    let in_record = shape.by_v(v).is_some() || ext_vs.contains(v);
                                    match (st, in_record) {
                                        (SVal::Tomb, true) => n_live -= 1,
                                        (SVal::V(_), false) => n_live += 1,
                                        _ => {}
                                    }
                                }
                            }
                        }
                        if n_live == 0 {
                            // every cell of the record is deleted: no record
                            present.push(0);
                            for m in cmask.iter_mut() {
                                m.push(0);
                            }
                            for c in cols.iter_mut() {
                                c.push(None, true);
                            }
                            continue;
                        }
                        present.push(1);
                        for (ci, m) in cmask.iter_mut().enumerate() {
                            m.push(live[ci] as u8);
                        }
                        for (ci, c) in cols.iter_mut().enumerate() {
                            if !c.push(row[ci].as_ref(), allow_object) {
                                return Err(Error::Value(format!(
                                    "read_numpy: value {} of field '{}' at u={} does not fit dtype {}; \
                                     use object_fallback=True (object column) or get()/range()",
                                    row[ci].as_ref().map(|x| x.py_repr()).unwrap_or_default(),
                                    wanted[ci].0,
                                    uu,
                                    c.dtype_name()
                                )));
                            }
                        }
                    }
                }
                None => {
                    // no DENSE record: live SPARSE cells of u make a record
                    // (R9E-02, as the cell cursor and SQL)
                    self.read_sparse_only_row(realm, &u, sparse_idx, &wanted, &mut cols, &mut present, &mut cmask, &mut hits, allow_object)?;
                }
            }
            u = u.add_u64(run as u64);
            done += run;
        }
        // A gap followed by a read segment needs no check (that block, the
        // nearest one above the gap, was CRC-checked before use); a read
        // that ends in a gap checks the nearest segment above it like a
        // point access.
        if let Some(g) = gap_start {
            self.check_uncovered_u(realm, &g, lane)?;
        }
        Ok(ColumnsOut { names: wanted.into_iter().map(|(n, _)| n).collect(), cols, present, cell_masks: masks.then_some(cmask) })
    }

    /// One row of `read_columns` for a u without a live DENSE record: the
    /// newest SPARSE states of u decide (a record exists when one of them is
    /// a value).
    #[allow(clippy::too_many_arguments)]
    fn read_sparse_only_row(
        &self,
        realm: &Nat,
        u: &Nat,
        sparse_idx: Option<&super::SparseKeyIndex>,
        wanted: &[(String, Nat)],
        cols: &mut [OutCol],
        present: &mut Vec<u8>,
        cmask: &mut [Vec<u8>],
        hits: &mut Vec<super::SparseSegment>,
        allow_object: bool,
    ) -> Result<()> {
        let mut upd = super::SparseMap::new();
        if let Some(si) = sparse_idx {
            hits.clear();
            si.idx.stab(self, u, hits)?;
            if !hits.is_empty() {
                hits.sort_unstable_by_key(|s| s.block_id);
                upd = self.sparse_states_direct(realm, u, hits)?;
            }
        }
        let exists = upd.values().any(|(s, _)| !s.is_tomb());
        if !exists {
            present.push(0);
            for m in cmask.iter_mut() {
                m.push(0);
            }
            for c in cols.iter_mut() {
                c.push(None, true);
            }
            return Ok(());
        }
        present.push(1);
        for (ci, (name, v)) in wanted.iter().enumerate() {
            let val = match upd.get(v) {
                Some((SVal::V(x), _)) => Some(x),
                _ => None,
            };
            if let Some(m) = cmask.get_mut(ci) {
                m.push(val.is_some() as u8);
            }
            if !cols[ci].push(val, allow_object) {
                return Err(Error::Value(format!(
                    "read_numpy: value {} of field '{}' at u={} does not fit dtype {}; \
                     use object_fallback=True (object column) or get()/range()",
                    val.map(|x| x.py_repr()).unwrap_or_default(),
                    name,
                    u,
                    cols[ci].dtype_name()
                )));
            }
        }
        Ok(())
    }

    /// Sparse state of a u group without merge/record cache (batch reads).
    fn sparse_states_direct(&self, realm: &Nat, u: &Nat, hits: &[super::SparseSegment]) -> Result<super::SparseMap> {
        let mut out = super::SparseMap::new();
        for seg in hits {
            if let Some((raw, _)) = self.read_sparse_record_bytes(seg, u)? {
                for (v, m) in self.decode_sparse_record(realm, u, &raw)? {
                    out.insert(v, (m.value, seg.block_id));
                }
            }
        }
        Ok(out)
    }

    /// Logical deletion of an object (`delete_record`): every live cell of
    /// it – BASE and extension, DENSE or SPARSE, named or not, a stored
    /// `None` included. Returns the number of live cells deleted (0: there
    /// was nothing to delete, nothing is written).
    /// The layout of a `delete_record` (T-05): a MATH layout id is named as
    /// such – its cells belong to the MATH layout, which has no delete.
    pub fn delete_layout(&self, r: &LayoutRef) -> Result<&crate::schema::LayoutDef> {
        match self.layout_by_ref(r) {
            Err(Error::Schema(_)) if matches!(r, LayoutRef::Id(id) if self.math_layouts.contains_key(id)) => {
                let LayoutRef::Id(id) = r else { unreachable!() };
                Err(Error::Schema(format!("layout {id} is a MATH layout; delete_record does not apply to MATH layouts (their cells are written by the MATH layout only)")))
            }
            other => other,
        }
    }

    pub fn delete_object(&mut self, r: &LayoutRef, obj: &Nat, lane: Lane, commit: bool) -> Result<u64> {
        let lay = self.delete_layout(r)?.clone();
        address::require_positive("object_id", obj)?;
        // N2 (RUN8): the DELETE block and the tombstones are one unit
        let n = self.write_scope(|db| {
            let mut tombs: Vec<CellInput> = Vec::new();
            let n = db.collect_object_delete(&lay, r, obj, lane, &mut tombs)?;
            if !tombs.is_empty() {
                db.put_points(&lay.realm_id, tombs, lane, false, 10_000)?;
            }
            Ok(n)
        })?;
        if commit {
            self.commit()?;
        }
        Ok(n)
    }

    /// Deletion of one object without commit: CLASSIC writes the DELETE
    /// block of a live DENSE record at once and adds the tombstones of the
    /// u to `tombs`; other layouts add the tombstones of the occupied cells.
    /// Returns the number of live cells. Must run inside a `write_scope`.
    pub(crate) fn collect_object_delete(&mut self, lay: &crate::schema::LayoutDef, r: &LayoutRef, obj: &Nat, lane: Lane, tombs: &mut Vec<CellInput>) -> Result<u64> {
        if lay.kind == LayoutKind::Classic {
            let live = self.live_cells_of_u(&lay.realm_id, obj, lane)?.len() as u64;
            if live > 0 {
                self.delete_classic_u(&lay.realm_id, obj, lane, tombs)?;
            }
            return Ok(live);
        }
        // the layout names the cells of the object (a CUSTOM plugin may
        // name empty ones too); deleted and counted is what lives
        let mut n = 0u64;
        for nn in self.occupied_n(r, obj, lane)? {
            let a = address::n_to_address(&nn)?;
            if self.live_cell_value(&lay.realm_id, a.u(), a.v_local(), lane)?.is_some() {
                n += 1;
                tombs.push(CellInput::tombstone(a.u().clone(), a.v_local().clone()));
            }
        }
        Ok(n)
    }

    /// Full delete of a CLASSIC u group: the DELETE block hides the DENSE
    /// record; every SPARSE state that is a value gets a tombstone – also
    /// the states the record hides (F1), which a DELETE alone would let
    /// show again. The tombstones go to `tombs` (written by the caller).
    pub(crate) fn delete_classic_u(&mut self, realm: &Nat, u: &Nat, lane: Lane, tombs: &mut Vec<CellInput>) -> Result<()> {
        if self.resolve_segment(realm, u, lane)?.is_some() {
            self.delete_record_block(realm, u, lane)?;
        }
        if self.sparse.contains_key(&(realm.clone(), lane)) {
            for (v, (s, _)) in self.sparse_updates_for_u(realm, u, lane)?.iter() {
                if !s.is_tomb() {
                    tombs.push(CellInput::tombstone(u.clone(), v.clone()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::NL;
    use crate::codec::FieldDef;
    use crate::db::OpenOptions;

    fn db_with(name: &str) -> (Db, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("nhydb_cols_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("c.nhydb");
        let r = Nat::S(1);
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        db.create_shape(1, &r, "s", vec![FieldDef::new(Nat::S(1), "n", TypeCode::VarUInt)], "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
        let rows = vec![vec![Val::Int(5)], vec![Val::from_bigint(num_bigint::BigInt::from(1u8) << 70)]];
        db.put_dense_batch(&r, &Nat::S(1), &DenseRows::Tuples(&rows), Some(1), &[], NL, false).unwrap();
        (db, d)
    }

    /// F018: a huge `count` is an error, never an abort/panic.
    #[test]
    fn read_columns_huge_count_is_memory_error() {
        let (db, d) = db_with("huge");
        for count in [usize::MAX, usize::MAX / 2, 1usize << 40] {
            let e = db.read_columns(&Nat::S(1), &Nat::S(1), count, None, NL, false).unwrap_err();
            assert!(matches!(e, Error::Memory(_)), "{e}");
        }
        let _ = std::fs::remove_dir_all(d);
    }

    /// F033: dtype depends on the field type; misfits raise unless allowed.
    #[test]
    fn read_columns_deterministic_dtype() {
        let (db, d) = db_with("dtype");
        let ok = db.read_columns(&Nat::S(1), &Nat::S(1), 1, None, NL, false).unwrap();
        assert!(matches!(ok.cols[0], OutCol::U64(_)));
        assert!(matches!(db.read_columns(&Nat::S(1), &Nat::S(1), 2, None, NL, false), Err(Error::Value(_))));
        let obj = db.read_columns(&Nat::S(1), &Nat::S(1), 3, None, NL, true).unwrap();
        assert!(matches!(&obj.cols[0], OutCol::Vals(v) if v.len() == 3));
        assert_eq!(obj.present, vec![1, 1, 0]);
        let _ = std::fs::remove_dir_all(d);
    }
}
