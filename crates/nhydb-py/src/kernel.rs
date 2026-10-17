//! Zustandslose Kernel-Funktionen für Python (Unicode, Krypto, JSON).
//!
//! Die mit `_` beginnenden Funktionen dienen den Konformitätstests gegen
//! CPython bzw. `cryptography` und sind keine stabile Benutzer-API.

use num_bigint::BigInt;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyBytes, PyDict, PyList, PyTuple};

use nhydb_core::codec::{decode_value, encode_value, enum_from_val, skip_value, FieldDef, TypeCode};
use nhydb_core::db::index as idx;
use nhydb_core::format;
use nhydb_core::{crypto, setops, unicode, Nat, Val};

use crate::conv::{from_val, nat_py, to_val, IntoPy};
use crate::exceptions::{CorruptionError, SchemaError};

// ----------------------------------------------------------------------
// Feld-/Shape-Objekte (Duck-Typing wie Python: .v, .name, .type, .enum)
// ----------------------------------------------------------------------

fn field_of(obj: &Bound<'_, PyAny>) -> PyResult<FieldDef> {
    let typ: String = obj.getattr("type")?.extract()?;
    let t = TypeCode::from_name(&typ).ok_or_else(|| SchemaError::new_err(format!("Typ {} unbekannt", Val::str(typ.as_str()).py_repr())))?;
    let v = match obj.getattr("v") {
        Ok(x) => crate::conv::to_nat(&x).unwrap_or(Nat::ZERO),
        Err(_) => Nat::ZERO,
    };
    let name: String = obj.getattr("name").and_then(|n| n.extract()).unwrap_or_default();
    let mut f = FieldDef::new(v, name, t);
    if let Ok(e) = obj.getattr("enum") {
        if !e.is_none() {
            f.enum_map = enum_from_val(&to_val(&e)?).py()?;
        }
    }
    Ok(f)
}

fn shape_fields(shape: &Bound<'_, PyAny>) -> PyResult<(String, Vec<FieldDef>)> {
    let sid = shape.getattr("shape_id")?.str()?.to_string();
    let mut fields = Vec::new();
    for f in shape.getattr("fields")?.try_iter()? {
        fields.push(field_of(&f?)?);
    }
    Ok((sid, fields))
}

fn bigints(obj: &Bound<'_, PyAny>) -> PyResult<Vec<BigInt>> {
    obj.try_iter()?.map(|x| x?.extract::<BigInt>()).collect()
}

fn int_tuple<'py>(py: Python<'py>, v: Vec<BigInt>) -> PyResult<Bound<'py, PyTuple>> {
    PyTuple::new(py, v)
}

#[pyfunction]
fn enc_len<'py>(py: Python<'py>, raw: &[u8]) -> Bound<'py, PyBytes> {
    let mut out = Vec::with_capacity(raw.len() + 4);
    nhydb_core::nat::put_vu64(&mut out, raw.len() as u64);
    out.extend_from_slice(raw);
    PyBytes::new(py, &out)
}

#[pyfunction]
#[pyo3(signature = (buf, pos=0))]
fn dec_len<'py>(py: Python<'py>, buf: &[u8], pos: usize) -> PyResult<(Bound<'py, PyBytes>, usize)> {
    let mut p = pos;
    let ln = nhydb_core::nat::get_vu(buf, &mut p).py()?;
    let end = ln.as_u64().and_then(|l| p.checked_add(l as usize)).filter(|e| *e <= buf.len());
    let end = end.ok_or_else(|| CorruptionError::new_err("Laengenwert endet hinter Payload"))?;
    Ok((PyBytes::new(py, &buf[p..end]), end))
}

#[pyfunction]
fn json_key_encode<'py>(py: Python<'py>, value: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyBytes>> {
    Ok(PyBytes::new(py, idx::json_key_encode(&to_val(value)?).py()?.as_bytes()))
}

#[pyfunction]
fn json_key_decode(py: Python<'_>, raw: &[u8]) -> PyResult<Py<PyAny>> {
    from_val(py, &idx::json_key_decode(raw).py()?)
}

#[pyfunction]
fn encode_big_posting<'py>(py: Python<'py>, us: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyBytes>> {
    let mut v = Vec::new();
    let mut prev = BigInt::from(0);
    for x in bigints(us)? {
        if x <= prev {
            return Err(PyValueError::new_err("Posting-u muss streng aufsteigend sein"));
        }
        prev = x.clone();
        v.push(Nat::from_bigint(&x).unwrap());
    }
    Ok(PyBytes::new(py, &idx::encode_big_posting(&v).py()?))
}

#[pyfunction]
fn decode_big_posting<'py>(py: Python<'py>, raw: &[u8], count: u64) -> PyResult<Bound<'py, PyList>> {
    let v = idx::decode_big_posting(raw, count).map_err(|e| CorruptionError::new_err(e.to_string()))?;
    let items: Vec<Py<PyAny>> = v.iter().map(|n| nat_py(py, n)).collect::<PyResult<_>>()?;
    PyList::new(py, items)
}

#[pyfunction]
fn sorted_unique_ints<'py>(py: Python<'py>, values: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyTuple>> {
    int_tuple(py, setops::sorted_unique(bigints(values)?))
}

#[pyfunction]
fn intersect_sorted_ints<'py>(py: Python<'py>, a: &Bound<'py, PyAny>, b: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyTuple>> {
    int_tuple(py, setops::intersect_sorted(&bigints(a)?, &bigints(b)?))
}

#[pyfunction]
fn union_sorted_ints<'py>(py: Python<'py>, a: &Bound<'py, PyAny>, b: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyTuple>> {
    int_tuple(py, setops::union_sorted(&bigints(a)?, &bigints(b)?))
}

#[pyfunction]
fn difference_sorted_ints<'py>(py: Python<'py>, a: &Bound<'py, PyAny>, b: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyTuple>> {
    int_tuple(py, setops::difference_sorted(&bigints(a)?, &bigints(b)?))
}

#[pyfunction]
fn enc_value<'py>(py: Python<'py>, field: &Bound<'py, PyAny>, value: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyBytes>> {
    let f = field_of(field)?;
    let mut out = Vec::new();
    encode_value(&f, &to_val(value)?, &mut out).py()?;
    Ok(PyBytes::new(py, &out))
}

#[pyfunction]
fn dec_value(py: Python<'_>, field: &Bound<'_, PyAny>, buf: &Bound<'_, PyAny>, pos: usize) -> PyResult<(Py<PyAny>, usize)> {
    let f = field_of(field)?;
    let raw: Vec<u8> = buf.call_method0("__bytes__").or_else(|_| PyBytes::new(py, &[]).get_type().call1((buf,)))?.extract()?;
    let mut p = pos;
    let v = decode_value(&f, &raw, &mut p).py()?;
    Ok((from_val(py, &v)?, p))
}

fn encode_row(sid: &str, fields: &[FieldDef], values: &Bound<'_, PyAny>, out: &mut Vec<u8>) -> PyResult<()> {
    if let Ok(d) = values.cast::<PyDict>() {
        for f in fields {
            let v = d.get_item(f.name.as_str())?.ok_or_else(|| SchemaError::new_err(format!("Pflichtfeld {} fehlt", Val::str(f.name.as_str()).py_repr())))?;
            encode_value(f, &to_val(&v)?, out).py()?;
        }
        return Ok(());
    }
    let vals: Vec<Bound<'_, PyAny>> = values.try_iter()?.collect::<PyResult<_>>()?;
    if vals.len() != fields.len() {
        return Err(SchemaError::new_err(format!("Shape {sid} erwartet {} Werte, erhalten {}", fields.len(), vals.len())));
    }
    for (f, v) in fields.iter().zip(vals.iter()) {
        encode_value(f, &to_val(v)?, out).py()?;
    }
    Ok(())
}

#[pyfunction]
fn encode_dense_row<'py>(py: Python<'py>, shape: &Bound<'py, PyAny>, values: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyBytes>> {
    let (sid, fields) = shape_fields(shape)?;
    let mut out = Vec::new();
    encode_row(&sid, &fields, values, &mut out)?;
    Ok(PyBytes::new(py, &out))
}

/// Kompilierter Batch-Encoder (`compile_dense_batch_codec(shape)`).
#[pyclass(module = "nhydb", frozen)]
pub struct DenseBatchCodec {
    sid: String,
    fields: Vec<FieldDef>,
}

#[pymethods]
impl DenseBatchCodec {
    fn __call__<'py>(&self, py: Python<'py>, rows: &Bound<'py, PyAny>) -> PyResult<(Bound<'py, PyByteArray>, Py<PyAny>)> {
        let mut out = Vec::new();
        let mut offsets: Vec<u32> = vec![0];
        for row in rows.try_iter()? {
            let row = row?;
            let vals: Vec<Bound<'_, PyAny>> = row.try_iter()?.collect::<PyResult<_>>()?;
            if vals.len() != self.fields.len() {
                return Err(SchemaError::new_err(format!("Shape {} erwartet {} Werte", self.sid, self.fields.len())));
            }
            for (f, v) in self.fields.iter().zip(vals.iter()) {
                let mut val = to_val(v)?;
                if f.enum_map.is_some() {
                    if let Val::Str(s) = &val {
                        val = Val::Int(f.enum_code(s).ok_or_else(|| PyValueError::new_err("unbekannter Enumwert"))?);
                    }
                }
                encode_value(f, &val, &mut out).py()?;
            }
            if out.len() > u32::MAX as usize {
                return Err(PyValueError::new_err("ein DATA-Batch darf maximal 4 GiB Recorddaten haben"));
            }
            offsets.push(out.len() as u32);
        }
        let arr = py.import("array")?.getattr("array")?;
        let raw: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        let a = arr.call1(("I",))?;
        a.call_method1("frombytes", (PyBytes::new(py, &raw),))?;
        Ok((PyByteArray::new(py, &out), a.unbind()))
    }
}

#[pyfunction]
fn compile_dense_batch_codec(shape: &Bound<'_, PyAny>) -> PyResult<DenseBatchCodec> {
    let (sid, fields) = shape_fields(shape)?;
    Ok(DenseBatchCodec { sid, fields })
}

/// Kompilierter selektiver Decoder (`compile_selective_decoder`).
#[pyclass(module = "nhydb", frozen)]
pub struct SelectiveDecoder {
    fields: Vec<FieldDef>,
    wanted: Vec<bool>,
    last: usize,
}

#[pymethods]
impl SelectiveDecoder {
    fn __call__<'py>(&self, py: Python<'py>, payload: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyTuple>> {
        if self.fields.is_empty() {
            return PyTuple::new(py, Vec::<Py<PyAny>>::new());
        }
        let raw: Vec<u8> = PyBytes::new(py, &[]).get_type().call1((payload,))?.extract()?;
        let mut p = 0usize;
        let mut out: Vec<Py<PyAny>> = Vec::new();
        for (i, f) in self.fields.iter().enumerate().take(self.last + 1) {
            if self.wanted[i] {
                let v = decode_value(f, &raw, &mut p).py()?;
                out.push(from_val(py, &v)?);
            } else {
                skip_value(f, &raw, &mut p).py()?;
            }
        }
        PyTuple::new(py, out)
    }
}

#[pyfunction]
fn compile_selective_decoder(shape: &Bound<'_, PyAny>, wanted_vs: &Bound<'_, PyAny>) -> PyResult<SelectiveDecoder> {
    let (_sid, fields) = shape_fields(shape)?;
    let wanted: Vec<Nat> = wanted_vs.try_iter()?.map(|x| crate::conv::to_nat(&x?)).collect::<PyResult<_>>()?;
    if wanted.is_empty() {
        return Ok(SelectiveDecoder { fields: Vec::new(), wanted: Vec::new(), last: 0 });
    }
    let missing: Vec<String> = wanted.iter().filter(|w| !fields.iter().any(|f| &f.v == *w)).map(|w| w.to_string()).collect();
    if !missing.is_empty() {
        return Err(SchemaError::new_err(format!("v nicht im Shape: [{}]", missing.join(", "))));
    }
    let max_v = wanted.iter().max().unwrap().clone();
    let flags: Vec<bool> = fields.iter().map(|f| wanted.contains(&f.v)).collect();
    let last = fields.iter().rposition(|f| f.v <= max_v).unwrap_or(0);
    Ok(SelectiveDecoder { fields, wanted: flags, last })
}

#[pyfunction]
fn pack_super<'py>(py: Python<'py>, state: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyBytes>> {
    let g = |k: &str| -> PyResult<u64> { state.get_item(k)?.extract() };
    let flags: u32 = match state.call_method1("get", ("flags", 0)) {
        Ok(v) => v.extract()?,
        Err(_) => 0,
    };
    let s = format::SuperState {
        generation: g("generation")?,
        checkpoint_offset: g("checkpoint_offset")?,
        last_valid_end: g("last_valid_end")?,
        next_block_id: g("next_block_id")?,
        created_ns: g("created_ns")?,
        updated_ns: g("updated_ns")?,
        flags,
    };
    Ok(PyBytes::new(py, &format::pack_super(&s)))
}

#[pyfunction]
fn unpack_super<'py>(py: Python<'py>, raw: &[u8]) -> PyResult<Bound<'py, PyDict>> {
    let s = format::unpack_super(raw).py()?;
    let d = PyDict::new(py);
    d.set_item("generation", s.generation)?;
    d.set_item("checkpoint_offset", s.checkpoint_offset)?;
    d.set_item("last_valid_end", s.last_valid_end)?;
    d.set_item("next_block_id", s.next_block_id)?;
    d.set_item("created_ns", s.created_ns)?;
    d.set_item("updated_ns", s.updated_ns)?;
    d.set_item("flags", s.flags)?;
    Ok(d)
}

#[pyfunction]
#[pyo3(signature = (bt, bid, payload, *, flags=0))]
fn pack_block_header<'py>(py: Python<'py>, bt: u8, bid: u64, payload: &[u8], flags: i64) -> Bound<'py, PyBytes> {
    PyBytes::new(py, &format::pack_block_header(bt, bid, payload, (flags & 0xFFFF) as u16))
}

#[pyfunction]
fn unpack_block_header(raw: &[u8]) -> PyResult<(u8, u16, u64, u64, u32)> {
    let h = format::unpack_block_header(raw).py()?;
    Ok((h.block_type, h.flags, h.block_id, h.payload_len, h.payload_crc))
}

#[pyfunction]
fn align8(n: u64) -> u64 {
    format::align8(n)
}

#[pyfunction]
fn fmt_bytes(n: u64) -> String {
    nhydb_core::db::maint::fmt_bytes(n)
}

#[pyfunction]
fn json_bytes<'py>(py: Python<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyBytes>> {
    Ok(PyBytes::new(py, to_val(obj)?.to_json().py()?.as_bytes()))
}

/// Eigenschaftsbits aller Code Points 0..0x110000 (siehe `unicode::prop_bits`).
#[pyfunction]
fn _unicode_prop_table(py: Python<'_>) -> Bound<'_, PyBytes> {
    let v: Vec<u8> = py.detach(|| (0u32..0x110000).map(unicode::prop_bits).collect());
    PyBytes::new(py, &v)
}

/// Dezimalwerte aller Code Points (255 = keiner).
#[pyfunction]
fn _unicode_decimal_table(py: Python<'_>) -> Bound<'_, PyBytes> {
    let v: Vec<u8> = (0u32..0x110000)
        .map(|cp| char::from_u32(cp).and_then(unicode::decimal_value).unwrap_or(255))
        .collect();
    PyBytes::new(py, &v)
}

#[pyfunction]
fn _unicode_map(kind: &str, s: &str) -> PyResult<String> {
    Ok(match kind {
        "casefold" => unicode::casefold(s),
        "lower" => unicode::lower(s),
        "upper" => unicode::upper(s),
        "strip" => unicode::strip(s).to_string(),
        _ => return Err(PyValueError::new_err("kind: casefold|lower|upper|strip")),
    })
}

#[pyfunction]
fn _unicode_versions() -> (&'static str, &'static str) {
    (unicode::PYTHON_VERSION, unicode::UNICODE_VERSION)
}

#[pyfunction]
fn _aes_gcm_seal<'py>(py: Python<'py>, key: &[u8], nonce: &[u8], plaintext: &[u8], aad: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    Ok(PyBytes::new(py, &crypto::aes_gcm_seal_with_nonce(key, nonce, plaintext, aad).py()?))
}

#[pyfunction]
fn _aes_gcm_open<'py>(py: Python<'py>, key: &[u8], nonce: &[u8], ciphertext: &[u8], aad: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    Ok(PyBytes::new(py, &crypto::aes_gcm_open(key, nonce, ciphertext, aad).py()?))
}

/// `layout_mac(key, payload)`
#[pyfunction]
fn layout_mac(key: &[u8], payload: &[u8]) -> String {
    crypto::layout_mac(key, payload)
}

/// `json.dumps(obj, sort_keys=True, separators=(",",":"), ensure_ascii=…)`
#[pyfunction]
#[pyo3(signature = (obj, ensure_ascii=false))]
fn _json_dumps(obj: &Bound<'_, PyAny>, ensure_ascii: bool) -> PyResult<String> {
    let v = to_val(obj)?;
    if ensure_ascii { v.to_json_ascii().py() } else { v.to_json().py() }
}

// ----------------------------------------------------------------------
// Externer Layoutplan (private Helfer der Referenz, für Tests/Werkzeuge)
// ----------------------------------------------------------------------

use nhydb_core::db::system;
use std::path::PathBuf;

#[pyfunction]
fn _layout_key_load<'py>(py: Python<'py>, path: PathBuf, db_id: &str, db_name: &str) -> PyResult<Bound<'py, PyBytes>> {
    Ok(PyBytes::new(py, &system::key_load(&path, db_id, db_name).py()?))
}

#[pyfunction]
fn _layout_seal_entry(py: Python<'_>, db_id: &str, generation: u64, snapshot: &Bound<'_, PyAny>, key: &[u8]) -> PyResult<Py<PyAny>> {
    let snap = to_val(snapshot)?.to_json().py()?;
    from_val(py, &system::seal_entry(db_id, generation, snap.as_bytes(), key).py()?)
}

#[pyfunction]
fn _layout_envelope<'py>(py: Python<'py>, db_id: &str, entries: &Bound<'py, PyAny>, key: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let e: Vec<Val> = entries.try_iter()?.map(|x| to_val(&x?)).collect::<PyResult<_>>()?;
    Ok(PyBytes::new(py, &system::envelope_bytes(db_id, &e, key).py()?))
}

#[pyfunction]
fn _layout_read_envelope(py: Python<'_>, path: PathBuf, db_id: &str, key: &[u8]) -> PyResult<Py<PyAny>> {
    from_val(py, &Val::List(system::read_envelope(&path, db_id, key).py()?))
}

#[pyfunction]
fn _layout_open_entry(py: Python<'_>, db_id: &str, entry: &Bound<'_, PyAny>, key: &[u8]) -> PyResult<Py<PyAny>> {
    from_val(py, &system::open_entry(db_id, &to_val(entry)?, key).py()?)
}

#[pyfunction]
fn _atomic_layout_replace(path: PathBuf, data: &[u8]) -> PyResult<()> {
    system::atomic_write(&path, data, nhydb_core::Durability::Fsync).py()
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    for f in [
        wrap_pyfunction!(_layout_key_load, m)?,
        wrap_pyfunction!(_layout_seal_entry, m)?,
        wrap_pyfunction!(_layout_envelope, m)?,
        wrap_pyfunction!(_layout_read_envelope, m)?,
        wrap_pyfunction!(_layout_open_entry, m)?,
        wrap_pyfunction!(_atomic_layout_replace, m)?,
    ] {
        m.add_function(f)?;
    }
    m.add_class::<DenseBatchCodec>()?;
    m.add_class::<SelectiveDecoder>()?;
    for f in [
        wrap_pyfunction!(enc_len, m)?,
        wrap_pyfunction!(dec_len, m)?,
        wrap_pyfunction!(json_key_encode, m)?,
        wrap_pyfunction!(json_key_decode, m)?,
        wrap_pyfunction!(encode_big_posting, m)?,
        wrap_pyfunction!(decode_big_posting, m)?,
        wrap_pyfunction!(sorted_unique_ints, m)?,
        wrap_pyfunction!(intersect_sorted_ints, m)?,
        wrap_pyfunction!(union_sorted_ints, m)?,
        wrap_pyfunction!(difference_sorted_ints, m)?,
        wrap_pyfunction!(enc_value, m)?,
        wrap_pyfunction!(dec_value, m)?,
        wrap_pyfunction!(encode_dense_row, m)?,
        wrap_pyfunction!(compile_dense_batch_codec, m)?,
        wrap_pyfunction!(compile_selective_decoder, m)?,
        wrap_pyfunction!(pack_super, m)?,
        wrap_pyfunction!(unpack_super, m)?,
        wrap_pyfunction!(pack_block_header, m)?,
        wrap_pyfunction!(unpack_block_header, m)?,
        wrap_pyfunction!(align8, m)?,
        wrap_pyfunction!(fmt_bytes, m)?,
        wrap_pyfunction!(json_bytes, m)?,
    ] {
        m.add_function(f)?;
    }
    m.add_function(wrap_pyfunction!(_unicode_prop_table, m)?)?;
    m.add_function(wrap_pyfunction!(_unicode_decimal_table, m)?)?;
    m.add_function(wrap_pyfunction!(_unicode_map, m)?)?;
    m.add_function(wrap_pyfunction!(_unicode_versions, m)?)?;
    m.add_function(wrap_pyfunction!(_aes_gcm_seal, m)?)?;
    m.add_function(wrap_pyfunction!(_aes_gcm_open, m)?)?;
    m.add_function(wrap_pyfunction!(layout_mac, m)?)?;
    m.add_function(wrap_pyfunction!(_json_dumps, m)?)?;
    Ok(())
}
