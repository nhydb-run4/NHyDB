//! Conversion Python ⇄ Rust (`Val`, `Nat`, lanes, errors).

use num_bigint::BigInt;
use pyo3::exceptions::*;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyInt, PyList, PyMemoryView, PyString, PyTuple};
use pyo3::IntoPyObjectExt;

use nhydb_core::{Error, Lane, Nat, Val};

use crate::exceptions::*;

/// Rust error → Python exception.
pub fn to_pyerr(e: Error) -> PyErr {
    match e {
        Error::NHyDB(m) => NHyDBError::new_err(m),
        Error::Corruption(m) => CorruptionError::new_err(m),
        Error::Schema(m) => SchemaError::new_err(m),
        Error::StaleIndex(m) => StaleIndexError::new_err(m),
        Error::RealmLimit(m) => RealmLimitError::new_err(m),
        Error::Address(m) => AddressError::new_err(m),
        Error::Value(m) => PyValueError::new_err(m),
        // Error::Key carries the text of str(KeyError) (= repr of the key);
        // Python receives the key itself.
        Error::Key(m) => match m.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
            Some(k) if m.len() >= 2 => PyKeyError::new_err(k.to_string()),
            _ => PyKeyError::new_err(m),
        },
        Error::Index(m) => PyIndexError::new_err(m),
        Error::Type(m) => PyTypeError::new_err(m),
        Error::NotFound(m) => PyFileNotFoundError::new_err(m),
        Error::Exists(m) => PyFileExistsError::new_err(m),
        Error::Locked(m) => DatabaseLockedError::new_err(m),
        Error::SizeLimit(m) => DatabaseSizeLimitError::new_err(m),
        Error::IndexConfig(m) => IndexConfigError::new_err(m),
        Error::MathLayoutNotFound(m) => MathLayoutNotFound::new_err(m),
        Error::MappingFrozen(m) => MappingFrozenError::new_err(m),
        Error::MathCellConflict(m) => MathCellConflictError::new_err(m),
        Error::MathSyntax(m) => MathSyntaxError::new_err(m),
        Error::MathDomain(m) => MathDomainError::new_err(m),
        Error::LayoutState(m) => LayoutStateError::new_err(m),
        Error::RealmBudget(m) => RealmBudgetExceeded::new_err(m),
        Error::StalePlan(m) => StalePlanError::new_err(m),
        Error::AllocationConflict(m) => AllocationConflictError::new_err(m),
        Error::Query(m) => QueryError::new_err(m),
        Error::QueryPlan(m) => QueryPlanError::new_err(m),
        Error::QueryDomain(m) => QueryDomainError::new_err(m),
        Error::SqlSyntax(m) => SQLSyntaxError::new_err(m),
        Error::Io(e) => PyOSError::new_err(e.to_string()),
        Error::Foreign(any, m) => match any.downcast::<PyErr>() {
            Ok(pe) => *pe,
            Err(_) => NHyDBError::new_err(m),
        },
    }
}

/// Python exception → Rust error (for plugin callbacks; the type is preserved).
pub fn from_pyerr(e: PyErr) -> Error {
    let msg = e.to_string();
    Error::Foreign(Box::new(e), msg)
}

pub trait IntoPy<T> {
    fn py(self) -> PyResult<T>;
}

impl<T> IntoPy<T> for nhydb_core::Result<T> {
    #[inline]
    fn py(self) -> PyResult<T> {
        self.map_err(to_pyerr)
    }
}

/// Python object → `Val`.
pub fn to_val(obj: &Bound<'_, PyAny>) -> PyResult<Val> {
    if obj.is_none() {
        return Ok(Val::None);
    }
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(Val::Bool(b.is_true()));
    }
    if obj.is_instance_of::<PyInt>() {
        if let Ok(i) = obj.extract::<i64>() {
            return Ok(Val::Int(i));
        }
        let b: BigInt = obj.extract()?;
        return Ok(Val::from_bigint(b));
    }
    if let Ok(f) = obj.cast::<PyFloat>() {
        return Ok(Val::Float(f.value()));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(Val::Str(s.to_str()?.to_owned()));
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(Val::Bytes(b.as_bytes().to_vec()));
    }
    if let Ok(b) = obj.cast::<PyByteArray>() {
        return Ok(Val::Bytes(b.to_vec()));
    }
    if obj.is_instance_of::<PyMemoryView>() {
        let b = obj.call_method0("tobytes")?;
        return to_val(&b);
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        let mut m = Vec::with_capacity(d.len());
        for (k, v) in d.iter() {
            let key = match k.cast::<PyString>() {
                Ok(s) => s.to_str()?.to_owned(),
                Err(_) => k.str()?.to_str()?.to_owned(),
            };
            m.push((key, to_val(&v)?));
        }
        return Ok(Val::Map(m));
    }
    if let Ok(l) = obj.cast::<PyList>() {
        return Ok(Val::List(l.iter().map(|x| to_val(&x)).collect::<PyResult<_>>()?));
    }
    if let Ok(t) = obj.cast::<PyTuple>() {
        return Ok(Val::List(t.iter().map(|x| to_val(&x)).collect::<PyResult<_>>()?));
    }
    // NumPy scalars and other number objects
    if obj.hasattr("__index__")? {
        let i = obj.call_method0("__index__")?;
        return to_val(&i);
    }
    if obj.hasattr("__float__")? && !obj.hasattr("__len__")? {
        return Ok(Val::Float(obj.call_method0("__float__")?.extract()?));
    }
    if obj.hasattr("tolist")? {
        return to_val(&obj.call_method0("tolist")?);
    }
    if let Ok(it) = obj.try_iter() {
        let mut out = Vec::new();
        for x in it {
            out.push(to_val(&x?)?);
        }
        return Ok(Val::List(out));
    }
    Err(PyTypeError::new_err(format!("value of type {} is not supported", obj.get_type().name()?)))
}

/// `Val` → Python object.
pub fn from_val(py: Python<'_>, v: &Val) -> PyResult<Py<PyAny>> {
    Ok(match v {
        Val::None => py.None(),
        Val::Bool(b) => b.into_py_any(py)?,
        Val::Int(i) => i.into_py_any(py)?,
        Val::Big(b) => b.into_py_any(py)?,
        Val::Float(f) => f.into_py_any(py)?,
        Val::Str(s) => s.into_py_any(py)?,
        Val::Bytes(b) => PyBytes::new(py, b).into_any().unbind(),
        Val::List(l) => {
            let list = PyList::empty(py);
            for x in l {
                list.append(from_val(py, x)?)?;
            }
            list.into_any().unbind()
        }
        Val::Map(m) => {
            let d = PyDict::new(py);
            for (k, x) in m {
                d.set_item(k, from_val(py, x)?)?;
            }
            d.into_any().unbind()
        }
    })
}

pub fn opt_from_val(py: Python<'_>, v: Option<Val>) -> PyResult<Py<PyAny>> {
    match v {
        None => Ok(py.None()),
        Some(v) => from_val(py, &v),
    }
}

pub fn nat_py(py: Python<'_>, n: &Nat) -> PyResult<Py<PyAny>> {
    match n {
        Nat::S(v) => v.into_py_any(py),
        Nat::B(_) => n.to_bigint().into_py_any(py),
    }
}

/// Python `int(x)` → Nat; negative values → 0 (so the subsequent positivity
/// checks report the usual error message).
pub fn to_nat(obj: &Bound<'_, PyAny>) -> PyResult<Nat> {
    if let Ok(v) = obj.extract::<u64>() {
        return Ok(Nat::S(v));
    }
    let b: BigInt = match obj.extract::<BigInt>() {
        Ok(b) => b,
        Err(_) => {
            let builtins = obj.py().import("builtins")?;
            builtins.getattr("int")?.call1((obj,))?.extract()?
        }
    };
    Ok(Nat::from_bigint(&b).unwrap_or(Nat::ZERO))
}

pub fn opt_nat(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Nat>> {
    match obj {
        None => Ok(None),
        Some(o) if o.is_none() => Ok(None),
        Some(o) => Ok(Some(to_nat(o)?)),
    }
}

/// u64-IDs (shape_id, layout_id, index_id).
pub fn to_id(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<u64> {
    match to_nat(obj)?.as_u64() {
        Some(v) => Ok(v),
        None => Err(PyValueError::new_err(format!("{what} too large (max 2^64-1)"))),
    }
}

/// `normalize_lane`.
pub fn to_lane(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Lane> {
    let Some(o) = obj else { return Ok(0) };
    if o.is_none() {
        return Ok(0);
    }
    if let Ok(s) = o.cast::<PyString>() {
        return nhydb_core::address::normalize_lane_str(s.to_str()?).py();
    }
    let i: BigInt = match o.extract::<BigInt>() {
        Ok(i) => i,
        Err(_) => {
            let builtins = o.py().import("builtins")?;
            builtins.getattr("int")?.call1((o,))?.extract()?
        }
    };
    use num_traits::ToPrimitive;
    nhydb_core::address::normalize_lane_int(i.to_i64().unwrap_or(2)).py()
}

/// N reference (int or `"N…"` text).
pub fn to_n(obj: &Bound<'_, PyAny>) -> PyResult<Nat> {
    if let Ok(s) = obj.cast::<PyString>() {
        return nhydb_core::address::parse_n_reference(s.to_str()?).py();
    }
    let n = to_nat(obj)?;
    nhydb_core::address::require_positive("N", &n).py()?;
    Ok(n)
}
