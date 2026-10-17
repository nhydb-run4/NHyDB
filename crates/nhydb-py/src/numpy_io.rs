//! NumPy/buffer input and output without the `numpy` crate.
//!
//! Input: objects supporting the buffer protocol (NumPy arrays, `array.array`,
//! `memoryview`) are typed by their format code and taken over as a
//! contiguous Rust vector (controlled copy; arrays with foreign endianness
//! or strides are converted correctly). All other objects (lists,
//! object/string arrays) are converted element by element into values.
//!
//! Output: `numpy.frombuffer(bytes, dtype)` (no Python loop).

use pyo3::buffer::{PyBuffer, PyUntypedBuffer};
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

fn typed<T: pyo3::buffer::Element + Copy>(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<Vec<T>> {
    let b = PyBuffer::<T>::get(obj)?;
    b.to_vec(py)
}

/// Arbitrary Python object → column.
pub fn col_from_py(obj: &Bound<'_, PyAny>) -> PyResult<OwnedCol> {
    let py = obj.py();
    if let Ok(ub) = PyUntypedBuffer::get(obj) {
        let fmt = ub.format().to_str().unwrap_or("").to_string();
        let size = ub.item_size();
        drop(ub);
        let code = fmt.trim_start_matches(['@', '=', '<', '>', '!']);
        let swapped = (fmt.starts_with('>') || fmt.starts_with('!')) && cfg!(target_endian = "little");
        if !swapped {
            let r = match (code, size) {
                ("?", 1) => {
                    let view = obj.call_method1("view", ("u1",))?;
                    Some(OwnedCol::Bool(typed::<u8>(py, &view)?))
                }
                ("B", 1) => Some(OwnedCol::U8(typed(py, obj)?)),
                ("b", 1) => Some(OwnedCol::I8(typed(py, obj)?)),
                ("H", 2) => Some(OwnedCol::U16(typed(py, obj)?)),
                ("h", 2) => Some(OwnedCol::I16(typed(py, obj)?)),
                ("I" | "L", 4) => Some(OwnedCol::U32(typed(py, obj)?)),
                ("i" | "l", 4) => Some(OwnedCol::I32(typed(py, obj)?)),
                ("L" | "Q" | "N", 8) => Some(OwnedCol::U64(typed(py, obj)?)),
                ("l" | "q" | "n", 8) => Some(OwnedCol::I64(typed(py, obj)?)),
                ("f", 4) => Some(OwnedCol::F32(typed(py, obj)?)),
                ("d", 8) => Some(OwnedCol::F64(typed(py, obj)?)),
                _ => None,
            };
            if let Some(c) = r {
                return Ok(c);
            }
        }
        // foreign endianness etc.: convert to native representation via NumPy
        if obj.hasattr("dtype")? && obj.getattr("dtype")?.getattr("kind")?.extract::<String>()? != "O" {
            let kind: String = obj.getattr("dtype")?.getattr("kind")?.extract()?;
            if matches!(kind.as_str(), "i" | "u" | "f" | "b") {
                let native = obj.call_method1("astype", (obj.getattr("dtype")?.call_method1("newbyteorder", ("=",))?,))?;
                if !native.is(obj) {
                    return col_from_py(&native);
                }
            }
        }
    }
    // lists, object/string arrays
    let src = if obj.hasattr("tolist")? { obj.call_method0("tolist")? } else { obj.clone() };
    let mut vals = Vec::new();
    for x in src.try_iter()? {
        vals.push(to_val(&x?)?);
    }
    Ok(OwnedCol::Vals(vals))
}

/// Scalar or array → vector of natural numbers of length `n`.
pub fn nats_from_py(obj: &Bound<'_, PyAny>, n: Option<usize>, what: &str) -> PyResult<Vec<Nat>> {
    if obj.is_instance_of::<pyo3::types::PyInt>() {
        let v = to_nat(obj)?;
        return Ok(vec![v; n.unwrap_or(1)]);
    }
    col_from_py(obj)?.to_nats(what)
}

/// Output column → NumPy array.
pub fn out_to_numpy(py: Python<'_>, col: &OutCol) -> PyResult<Py<PyAny>> {
    let np = py.import("numpy")?;
    fn bytes_of<T: Copy>(v: &[T]) -> &[u8] {
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
    }
    let (raw, dtype): (&[u8], &str) = match col {
        OutCol::I64(v) => (bytes_of(v), "=i8"),
        OutCol::U64(v) => (bytes_of(v), "=u8"),
        OutCol::F32(v) => (bytes_of(v), "=f4"),
        OutCol::F64(v) => (bytes_of(v), "=f8"),
        OutCol::Bool(v) => (bytes_of(v), "?"),
        OutCol::Vals(v) => {
            let list: Vec<Py<PyAny>> = v.iter().map(|x| from_val(py, x)).collect::<PyResult<_>>()?;
            let arr = np.call_method1("empty", (list.len(), "O"))?;
            for (i, x) in list.into_iter().enumerate() {
                arr.set_item(i, x)?;
            }
            return Ok(arr.unbind());
        }
    };
    let b = PyBytes::new(py, raw);
    // frombuffer returns a read-only view → copy() for a regular array
    Ok(np.call_method1("frombuffer", (b, dtype))?.call_method0("copy")?.unbind())
}

pub fn register(_m: &Bound<'_, PyModule>) -> PyResult<()> {
    Ok(())
}
