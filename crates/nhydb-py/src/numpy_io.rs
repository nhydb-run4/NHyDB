//! NumPy/buffer input and output without the `numpy` crate.
//!
//! Input: objects supporting the buffer protocol (NumPy arrays, `array.array`,
//! `memoryview`) are typed by their format code and taken over as a
//! contiguous Rust vector (controlled copy; arrays with foreign endianness
//! or strides are converted correctly). All other objects (lists,
//! object/string arrays) are converted element by element into values.
//!
//! Output: `numpy.frombuffer(ColumnBuffer, dtype)` over the Rust vector (no copy, no Python loop).

use pyo3::buffer::{PyBuffer, PyUntypedBuffer};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use nhydb_core::db::columns::OutCol;
use nhydb_core::{ColData, Nat, Val};

use crate::conv::{from_val, to_nat, to_val};

/// Owned column (data copied from Python).
pub enum OwnedCol {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Bool(Vec<u8>),
    Vals(Vec<Val>),
}

impl OwnedCol {
    pub fn as_col(&self) -> ColData<'_> {
        match self {
            OwnedCol::U8(v) => ColData::U8(v),
            OwnedCol::U16(v) => ColData::U16(v),
            OwnedCol::U32(v) => ColData::U32(v),
            OwnedCol::U64(v) => ColData::U64(v),
            OwnedCol::I8(v) => ColData::I8(v),
            OwnedCol::I16(v) => ColData::I16(v),
            OwnedCol::I32(v) => ColData::I32(v),
            OwnedCol::I64(v) => ColData::I64(v),
            OwnedCol::F32(v) => ColData::F32(v),
            OwnedCol::F64(v) => ColData::F64(v),
            OwnedCol::Bool(v) => ColData::Bool(v),
            OwnedCol::Vals(v) => ColData::Vals(v),
        }
    }

    pub fn len(&self) -> usize {
        self.as_col().len()
    }

    /// Values as natural numbers (for u/v/object_id arrays).
    pub fn to_nats(&self, what: &str) -> PyResult<Vec<Nat>> {
        let neg = || pyo3::exceptions::PyValueError::new_err(format!("{what}: negative values are not allowed"));
        Ok(match self {
            OwnedCol::U8(v) => v.iter().map(|x| Nat::S(*x as u64)).collect(),
            OwnedCol::U16(v) => v.iter().map(|x| Nat::S(*x as u64)).collect(),
            OwnedCol::U32(v) => v.iter().map(|x| Nat::S(*x as u64)).collect(),
            OwnedCol::U64(v) => v.iter().map(|x| Nat::S(*x)).collect(),
            OwnedCol::I8(v) => v.iter().map(|x| u64::try_from(*x).map(Nat::S).map_err(|_| neg())).collect::<PyResult<_>>()?,
            OwnedCol::I16(v) => v.iter().map(|x| u64::try_from(*x).map(Nat::S).map_err(|_| neg())).collect::<PyResult<_>>()?,
            OwnedCol::I32(v) => v.iter().map(|x| u64::try_from(*x).map(Nat::S).map_err(|_| neg())).collect::<PyResult<_>>()?,
            OwnedCol::I64(v) => v.iter().map(|x| u64::try_from(*x).map(Nat::S).map_err(|_| neg())).collect::<PyResult<_>>()?,
            OwnedCol::Vals(v) => v
                .iter()
                .map(|x| x.as_nat().ok_or_else(|| pyo3::exceptions::PyTypeError::new_err(format!("{what}: integers expected"))))
                .collect::<PyResult<_>>()?,
            _ => return Err(pyo3::exceptions::PyTypeError::new_err(format!("{what}: integer array expected"))),
        })
    }
}

impl OwnedCol {
    /// Row values as `Val` (integers exact, floats as float, bool as bool).
    pub fn to_vals(&self) -> Vec<Val> {
        match self {
            OwnedCol::U8(v) => v.iter().map(|x| Val::Int(*x as i64)).collect(),
            OwnedCol::U16(v) => v.iter().map(|x| Val::Int(*x as i64)).collect(),
            OwnedCol::U32(v) => v.iter().map(|x| Val::Int(*x as i64)).collect(),
            OwnedCol::U64(v) => v.iter().map(|x| Val::int(*x)).collect(),
            OwnedCol::I8(v) => v.iter().map(|x| Val::Int(*x as i64)).collect(),
            OwnedCol::I16(v) => v.iter().map(|x| Val::Int(*x as i64)).collect(),
            OwnedCol::I32(v) => v.iter().map(|x| Val::Int(*x as i64)).collect(),
            OwnedCol::I64(v) => v.iter().map(|x| Val::Int(*x)).collect(),
            OwnedCol::F32(v) => v.iter().map(|x| Val::Float(*x as f64)).collect(),
            OwnedCol::F64(v) => v.iter().map(|x| Val::Float(*x)).collect(),
            OwnedCol::Bool(v) => v.iter().map(|x| Val::Bool(*x != 0)).collect(),
            OwnedCol::Vals(v) => v.clone(),
        }
    }
}

/// Values of a result column → most compact exact NumPy representation:
/// only bool ⇒ bool, only i64 ⇒ int64, mixed i64/float without loss ⇒
/// float64, otherwise (None, text, big numbers …) ⇒ object.
pub fn vals_to_out(v: Vec<Val>) -> OutCol {
    if !v.is_empty() && v.iter().all(|x| matches!(x, Val::Bool(_))) {
        return OutCol::Bool(v.iter().map(|x| matches!(x, Val::Bool(true)) as u8).collect());
    }
    if v.iter().all(|x| matches!(x, Val::Int(_))) {
        return OutCol::I64(v.iter().map(|x| if let Val::Int(i) = x { *i } else { 0 }).collect());
    }
    let exact_f = |x: &Val| match x {
        Val::Float(_) => true,
        Val::Int(i) => i.unsigned_abs() <= (1u64 << 53),
        _ => false,
    };
    if v.iter().all(exact_f) {
        return OutCol::F64(v.iter().map(|x| match x {
            Val::Float(f) => *f,
            Val::Int(i) => *i as f64,
            _ => 0.0,
        }).collect());
    }
    OutCol::Vals(v)
}

/// R4-B: `vals_to_out` built value by value (`sql_numpy` streams its rows):
/// the same column for the same sequence of values, without keeping a
/// `Val` per value while the column is bool, int64 or float64.
#[derive(Default)]
pub enum ColBuilder {
    #[default]
    Empty,
    Bool(Vec<u8>),
    Int(Vec<i64>),
    /// float64 with the positions that were exact `Int` (|i| <= 2**53)
    Float { f: Vec<f64>, ints: Vec<bool> },
    Vals(Vec<Val>),
}

impl ColBuilder {
    pub fn push(&mut self, v: Val) {
        const EXACT: u64 = 1u64 << 53;
        let cur = std::mem::take(self);
        *self = match (cur, v) {
            (ColBuilder::Empty, Val::Bool(b)) => ColBuilder::Bool(vec![b as u8]),
            (ColBuilder::Empty, Val::Int(i)) => ColBuilder::Int(vec![i]),
            (ColBuilder::Empty, Val::Float(x)) => ColBuilder::Float { f: vec![x], ints: vec![false] },
            (ColBuilder::Empty, other) => ColBuilder::Vals(vec![other]),
            (ColBuilder::Bool(mut b), Val::Bool(x)) => {
                b.push(x as u8);
                ColBuilder::Bool(b)
            }
            (ColBuilder::Int(mut is), Val::Int(i)) => {
                is.push(i);
                ColBuilder::Int(is)
            }
            (ColBuilder::Int(is), Val::Float(x)) if is.iter().all(|i| i.unsigned_abs() <= EXACT) => {
                let mut f: Vec<f64> = is.iter().map(|i| *i as f64).collect();
                let mut ints = vec![true; is.len()];
                f.push(x);
                ints.push(false);
                ColBuilder::Float { f, ints }
            }
            (ColBuilder::Float { mut f, mut ints }, Val::Float(x)) => {
                f.push(x);
                ints.push(false);
                ColBuilder::Float { f, ints }
            }
            (ColBuilder::Float { mut f, mut ints }, Val::Int(i)) if i.unsigned_abs() <= EXACT => {
                f.push(i as f64);
                ints.push(true);
                ColBuilder::Float { f, ints }
            }
            (ColBuilder::Vals(mut vs), x) => {
                vs.push(x);
                ColBuilder::Vals(vs)
            }
            (other, x) => {
                let mut vs = other.into_vals();
                vs.push(x);
                ColBuilder::Vals(vs)
            }
        };
    }

    fn into_vals(self) -> Vec<Val> {
        match self {
            ColBuilder::Empty => Vec::new(),
            ColBuilder::Bool(b) => b.into_iter().map(|x| Val::Bool(x != 0)).collect(),
            ColBuilder::Int(is) => is.into_iter().map(Val::Int).collect(),
            ColBuilder::Float { f, ints } => f.into_iter().zip(ints).map(|(x, i)| if i { Val::Int(x as i64) } else { Val::Float(x) }).collect(),
            ColBuilder::Vals(v) => v,
        }
    }

    pub fn finish(self) -> OutCol {
        match self {
            ColBuilder::Empty => OutCol::I64(Vec::new()),
            ColBuilder::Bool(b) => OutCol::Bool(b),
            ColBuilder::Int(i) => OutCol::I64(i),
            ColBuilder::Float { f, .. } => OutCol::F64(f),
            ColBuilder::Vals(v) => OutCol::Vals(v),
        }
    }
}

fn typed<T: pyo3::buffer::Element + Copy>(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<Vec<T>> {
    let b = PyBuffer::<T>::get(obj)?;
    b.to_vec(py)
}

/// Raw element bytes of a buffer in C order (handles strides).
fn raw_bytes(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    let mv = pyo3::types::PyMemoryView::from(obj)?;
    Ok(mv.call_method0("tobytes")?.cast::<PyBytes>()?.as_bytes().to_vec())
}

/// Decodes fixed-size elements from raw bytes with explicit byte order.
fn decode_raw<T, const N: usize>(raw: &[u8], big: bool, conv: fn([u8; N]) -> T, conv_be: fn([u8; N]) -> T) -> Vec<T> {
    raw.as_chunks::<N>().0.iter().map(|a| if big { conv_be(*a) } else { conv(*a) }).collect()
}

fn type_err(msg: String) -> PyErr {
    pyo3::exceptions::PyTypeError::new_err(msg)
}

/// Rejects objects that are iterable but not a column (F033).
fn reject_non_column(obj: &Bound<'_, PyAny>) -> PyResult<()> {
    use pyo3::types::{PyByteArray, PyDict, PyString};
    if obj.is_instance_of::<PyString>() || obj.is_instance_of::<PyBytes>() || obj.is_instance_of::<PyByteArray>() || obj.is_instance_of::<PyDict>() {
        return Err(type_err(format!(
            "a column must be a 1-D array or a sequence of values, not {}",
            obj.get_type().name()?
        )));
    }
    Ok(())
}

/// Arbitrary Python object → column.
///
/// Rules (F024/F033, documented in PYTHON_API):
/// * only 1-D arrays/buffers (`ndim != 1` → `ValueError`);
/// * NumPy dtypes bool, int8…int64, uint8…uint64, float32, float64 are taken
///   over typed (any byte order/strides); object/str/bytes arrays and Python
///   sequences are converted value by value; every other dtype (float16,
///   longdouble, complex, datetime, structured …) → `TypeError`;
/// * `str`, `bytes`, `bytearray` and `dict` are not columns → `TypeError`.
pub fn col_from_py(obj: &Bound<'_, PyAny>) -> PyResult<OwnedCol> {
    let py = obj.py();
    reject_non_column(obj)?;
    let mut numeric_numpy = false;
    if obj.hasattr("dtype")? && obj.hasattr("ndim")? {
        let ndim: usize = obj.getattr("ndim")?.extract()?;
        if ndim != 1 {
            return Err(PyValueError::new_err(format!("a column must be 1-dimensional (got ndim={ndim})")));
        }
        let dt = obj.getattr("dtype")?;
        let kind: String = dt.getattr("kind")?.extract()?;
        let size: usize = dt.getattr("itemsize")?.extract()?;
        match (kind.as_str(), size) {
            ("O" | "U" | "S", _) => return vals_from_seq(&obj.call_method0("tolist")?),
            ("b" | "i" | "u", _) | ("f", 4 | 8) => numeric_numpy = true,
            _ => return Err(type_err(format!("unsupported column dtype {} (supported: bool, int8-64, uint8-64, float32, float64, object, str)", dt.str()?))),
        }
    }
    if let Ok(ub) = PyUntypedBuffer::get(obj) {
        let fmt = ub.format().to_str().unwrap_or("").to_string();
        let size = ub.item_size();
        let ndim = ub.dimensions();
        drop(ub);
        if ndim != 1 {
            return Err(PyValueError::new_err(format!("a column must be 1-dimensional (got ndim={ndim})")));
        }
        let code = fmt.trim_start_matches(['@', '=', '<', '>', '!']);
        let big = fmt.starts_with('>') || fmt.starts_with('!');
        let swapped = big != cfg!(target_endian = "big") && (fmt.starts_with(['<', '>', '!']));
        let kind = match (code, size) {
            ("?", 1) => 'b',
            ("B" | "c", 1) => 'B',
            ("b", 1) => '1',
            ("H", 2) => 'H',
            ("h", 2) => '2',
            ("I" | "L", 4) => 'I',
            ("i" | "l", 4) => '4',
            ("L" | "Q" | "N", 8) => 'Q',
            ("l" | "q" | "n", 8) => '8',
            ("f", 4) => 'f',
            ("d", 8) => 'd',
            _ if numeric_numpy => {
                return Err(type_err(format!("unsupported column buffer format '{fmt}'")));
            }
            ("O", _) => return vals_from_seq(&obj.call_method0("tolist")?),
            _ => return Err(type_err(format!("unsupported column buffer format '{fmt}'"))),
        };
        if kind == 'b' {
            // bool: read the raw bytes (works for memoryview/array too, F033)
            return Ok(OwnedCol::Bool(raw_bytes(obj)?.into_iter().map(|x| (x != 0) as u8).collect()));
        }
        if !swapped {
            return Ok(match kind {
                'B' => OwnedCol::U8(typed(py, obj)?),
                '1' => OwnedCol::I8(typed(py, obj)?),
                'H' => OwnedCol::U16(typed(py, obj)?),
                '2' => OwnedCol::I16(typed(py, obj)?),
                'I' => OwnedCol::U32(typed(py, obj)?),
                '4' => OwnedCol::I32(typed(py, obj)?),
                'Q' => OwnedCol::U64(typed(py, obj)?),
                '8' => OwnedCol::I64(typed(py, obj)?),
                'f' => OwnedCol::F32(typed(py, obj)?),
                _ => OwnedCol::F64(typed(py, obj)?),
            });
        }
        // foreign byte order: decode explicitly (no recursion, F024)
        let raw = raw_bytes(obj)?;
        return Ok(match kind {
            'B' => OwnedCol::U8(raw),
            '1' => OwnedCol::I8(raw.into_iter().map(|x| x as i8).collect()),
            'H' => OwnedCol::U16(decode_raw(&raw, big, u16::from_le_bytes, u16::from_be_bytes)),
            '2' => OwnedCol::I16(decode_raw(&raw, big, i16::from_le_bytes, i16::from_be_bytes)),
            'I' => OwnedCol::U32(decode_raw(&raw, big, u32::from_le_bytes, u32::from_be_bytes)),
            '4' => OwnedCol::I32(decode_raw(&raw, big, i32::from_le_bytes, i32::from_be_bytes)),
            'Q' => OwnedCol::U64(decode_raw(&raw, big, u64::from_le_bytes, u64::from_be_bytes)),
            '8' => OwnedCol::I64(decode_raw(&raw, big, i64::from_le_bytes, i64::from_be_bytes)),
            'f' => OwnedCol::F32(decode_raw(&raw, big, f32::from_le_bytes, f32::from_be_bytes)),
            _ => OwnedCol::F64(decode_raw(&raw, big, f64::from_le_bytes, f64::from_be_bytes)),
        });
    }
    if numeric_numpy {
        return Err(type_err("numeric array without buffer protocol is not supported as a column".into()));
    }
    // lists, tuples, other iterables
    let src = if obj.hasattr("tolist")? { obj.call_method0("tolist")? } else { obj.clone() };
    reject_non_column(&src)?;
    vals_from_seq(&src)
}

fn vals_from_seq(src: &Bound<'_, PyAny>) -> PyResult<OwnedCol> {
    let it = match src.try_iter() {
        Ok(it) => it,
        Err(_) => return Err(type_err(format!("a column must be a 1-D array or a sequence of values, not {}", src.get_type().name()?))),
    };
    let mut vals = Vec::new();
    for x in it {
        vals.push(to_val(&x?)?);
    }
    Ok(OwnedCol::Vals(vals))
}

/// Scalar (Python int/float, NumPy scalar, 0-d array, Decimal …) – broadcast
/// value instead of a column (F033); its value is converted strictly by
/// `to_nat` (F040).
pub fn is_scalar(obj: &Bound<'_, PyAny>) -> PyResult<bool> {
    if obj.is_instance_of::<pyo3::types::PyInt>() || obj.is_instance_of::<pyo3::types::PyFloat>() {
        return Ok(true);
    }
    if obj.hasattr("ndim")? {
        let ndim: usize = obj.getattr("ndim")?.extract().unwrap_or(1);
        return Ok(ndim == 0);
    }
    Ok(!obj.hasattr("__len__")? && !obj.hasattr("__iter__")? && (obj.hasattr("__index__")? || obj.hasattr("__float__")?))
}

/// Scalar or array → vector of natural numbers of length `n`.
pub fn nats_from_py(obj: &Bound<'_, PyAny>, n: Option<usize>, what: &str) -> PyResult<Vec<Nat>> {
    if is_scalar(obj)? {
        let v = to_nat(obj)?;
        return Ok(vec![v; n.unwrap_or(1)]);
    }
    col_from_py(obj)?.to_nats(what)
}

/// Checks that a typed NumPy column suits the target field type (F033):
/// bool arrays only for bool fields, float arrays only for float fields,
/// integer arrays not for utf8/bytes fields. Object columns/sequences are
/// converted value by value like the single-item API.
pub fn check_col_type(col: &OwnedCol, typ: nhydb_core::codec::TypeCode, is_enum: bool, name: &str) -> PyResult<()> {
    use nhydb_core::codec::TypeCode as T;
    let (dtype, ok) = match col {
        OwnedCol::Vals(_) => return Ok(()),
        OwnedCol::Bool(_) => ("bool", typ == T::Bool && !is_enum),
        OwnedCol::F32(_) | OwnedCol::F64(_) => ("float", matches!(typ, T::F32 | T::F64) && !is_enum),
        _ => ("integer", !matches!(typ, T::Utf8 | T::Bytes) || is_enum),
    };
    if ok {
        Ok(())
    } else {
        Err(type_err(format!("column '{name}': {dtype} array does not match field type {}{}", typ.name(), if is_enum { " (enum)" } else { "" })))
    }
}

/// Output column → NumPy array.
/// Memory of a result column handed over to NumPy without a copy (P017):
/// the array created by `numpy.frombuffer` references this object through
/// the buffer protocol and keeps it alive; Rust never touches the data again.
#[pyclass(module = "nhydb._native")]
pub struct ColumnBuffer {
    data: OutCol,
}

impl ColumnBuffer {
    fn raw_parts(&mut self) -> (*mut std::ffi::c_void, usize) {
        fn parts<T>(v: &mut [T]) -> (*mut std::ffi::c_void, usize) {
            (v.as_mut_ptr() as *mut std::ffi::c_void, std::mem::size_of_val(v))
        }
        match &mut self.data {
            OutCol::I64(v) => parts(v),
            OutCol::U64(v) => parts(v),
            OutCol::F32(v) => parts(v),
            OutCol::F64(v) => parts(v),
            OutCol::Bool(v) => parts(v),
            OutCol::Vals(_) => (std::ptr::null_mut(), 0),
        }
    }
}

#[pymethods]
impl ColumnBuffer {
    /// Writable 1-D byte buffer over the column memory.
    unsafe fn __getbuffer__(slf: Bound<'_, Self>, view: *mut pyo3::ffi::Py_buffer, flags: std::ffi::c_int) -> PyResult<()> {
        if view.is_null() {
            return Err(pyo3::exceptions::PyBufferError::new_err("View is null"));
        }
        let (ptr, len) = slf.borrow_mut().raw_parts();
        // the vector is never resized after construction: the pointer stays
        // valid as long as this object lives (the view holds a reference)
        let rc = unsafe { pyo3::ffi::PyBuffer_FillInfo(view, slf.as_ptr(), ptr, len as pyo3::ffi::Py_ssize_t, 0, flags) };
        if rc != 0 {
            return Err(PyErr::fetch(slf.py()));
        }
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut pyo3::ffi::Py_buffer) {}
}

pub fn out_to_numpy(py: Python<'_>, col: OutCol) -> PyResult<Py<PyAny>> {
    let np = py.import("numpy")?;
    let dtype = match &col {
        OutCol::I64(_) => "=i8",
        OutCol::U64(_) => "=u8",
        OutCol::F32(_) => "=f4",
        OutCol::F64(_) => "=f8",
        OutCol::Bool(_) => "?",
        OutCol::Vals(v) => {
            let list: Vec<Py<PyAny>> = v.iter().map(|x| from_val(py, x)).collect::<PyResult<_>>()?;
            let arr = np.call_method1("empty", (list.len(), "O"))?;
            for (i, x) in list.into_iter().enumerate() {
                arr.set_item(i, x)?;
            }
            return Ok(arr.unbind());
        }
    };
    // P017: no copy – NumPy references the Rust vector (writable array)
    let buf = Bound::new(py, ColumnBuffer { data: col })?;
    Ok(np.call_method1("frombuffer", (buf, dtype))?.unbind())
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<ColumnBuffer>()?;
    Ok(())
}

#[cfg(test)]
mod col_builder_tests {
    use super::*;

    fn same(a: &OutCol, b: &OutCol) -> bool {
        match (a, b) {
            (OutCol::I64(x), OutCol::I64(y)) => x == y,
            (OutCol::F64(x), OutCol::F64(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.to_bits() == q.to_bits()),
            (OutCol::Bool(x), OutCol::Bool(y)) => x == y,
            (OutCol::Vals(x), OutCol::Vals(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| format!("{p:?}") == format!("{q:?}")),
            _ => false,
        }
    }

    /// R4-B: the incremental builder equals `vals_to_out` on random columns.
    #[test]
    fn builder_equals_vals_to_out() {
        let pool = [
            Val::Int(1), Val::Int(-7), Val::Int(1 << 53), Val::Int((1 << 53) + 1), Val::Int(i64::MIN), Val::Float(0.5),
            Val::Float(-0.0), Val::Float(f64::NAN), Val::Bool(true), Val::Bool(false), Val::None, Val::str("x"),
        ];
        let mut seed = 12345u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20_000 {
            let len = (next() % 7) as usize;
            // mostly homogeneous columns with occasional other values
            let base = (next() % pool.len() as u64) as usize;
            let col: Vec<Val> = (0..len).map(|_| if next() % 4 == 0 { pool[(next() % pool.len() as u64) as usize].clone() } else { pool[base].clone() }).collect();
            let mut b = ColBuilder::default();
            for v in col.iter().cloned() {
                b.push(v);
            }
            assert!(same(&b.finish(), &vals_to_out(col.clone())), "{col:?}");
        }
    }
}
