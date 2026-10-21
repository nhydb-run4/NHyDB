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
        Error::NotFound(m) => file_err(2, "No such file or directory", &m),
        Error::Exists(m) => file_err(17, "File exists", &m),
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
        Error::Memory(m) => PyMemoryError::new_err(m),
        Error::Io(e) => io_err(e, None),
        Error::IoPath(e, p) => io_err(e, Some(p.display().to_string())),
        Error::Foreign(any, m) => match any.downcast::<PyErr>() {
            Ok(pe) => *pe,
            Err(_) => NHyDBError::new_err(m),
        },
    }
}

/// `OSError(errno, strerror[, filename])`: Python picks the matching
/// subclass (`FileNotFoundError`, `PermissionError`, `IsADirectoryError`, …)
/// and sets `errno`/`strerror`/`filename` (F048).
fn os_error(errno: i32, strerror: String, filename: Option<String>) -> PyErr {
    match filename {
        Some(f) => PyOSError::new_err((errno, strerror, f)),
        None => PyOSError::new_err((errno, strerror)),
    }
}

/// `NotFound`/`Exists` messages are either `"[Errno N] text: 'path'"`, a bare
/// path or a free text; the path (if any) becomes `filename`.
fn file_err(errno: i32, default_text: &str, m: &str) -> PyErr {
    if let Some(rest) = m.strip_prefix("[Errno ") {
        if let Some((_, tail)) = rest.split_once("] ") {
            if let Some((text, quoted)) = tail.rsplit_once(": '") {
                if let Some(path) = quoted.strip_suffix('\'') {
                    return os_error(errno, text.to_string(), Some(path.to_string()));
                }
            }
            return os_error(errno, tail.to_string(), None);
        }
    }
    let looks_like_path = m.contains('/') || m.contains(std::path::MAIN_SEPARATOR) || (!m.is_empty() && !m.contains(' '));
    if looks_like_path {
        os_error(errno, default_text.to_string(), Some(m.to_string()))
    } else {
        os_error(errno, m.to_string(), None)
    }
}

/// `std::io::Error` → OSError subclass with errno and, if known, the
/// affected file as `filename` (F048).
pub(crate) fn io_err(e: std::io::Error, filename: Option<String>) -> PyErr {
    match e.raw_os_error() {
        Some(code) => {
            let text = e.to_string();
            let suffix = format!(" (os error {code})");
            let text = text.strip_suffix(&suffix).unwrap_or(&text).to_string();
            os_error(code, text, filename)
        }
        None => match filename {
            Some(f) => PyOSError::new_err(format!("{e}: '{f}'")),
            None => PyErr::from(e),
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

/// Maximum nesting depth of list/dict values (R009): deeper values raise
/// `ValueError` instead of overflowing the native stack.
pub const MAX_VALUE_DEPTH: usize = 256;

/// Nesting limit for converting engine results to Python (N5). Stored
/// values are already limited to [`MAX_VALUE_DEPTH`] on input and when
/// decoded; results built by the engine itself nest deeper – the plan of an
/// expression at the query limit (`MAX_EXPR_DEPTH` = 256, e.g. alternating
/// `(q & a) | b`) has two to three value levels per expression level
/// (`explain=True`). The conversion recursion is shallow per level, so this
/// limit only guards the native stack.
pub const MAX_OUTPUT_DEPTH: usize = 1024;

fn too_deep() -> PyErr {
    PyValueError::new_err(format!("value nesting too deep (limit {MAX_VALUE_DEPTH} levels)"))
}

/// Python object → `Val`.
pub fn to_val(obj: &Bound<'_, PyAny>) -> PyResult<Val> {
    to_val_d(obj, 0)
}

fn to_val_d(obj: &Bound<'_, PyAny>, depth: usize) -> PyResult<Val> {
    if depth > MAX_VALUE_DEPTH {
        return Err(too_deep());
    }
    let to_val = |x: &Bound<'_, PyAny>| to_val_d(x, depth + 1);
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
    // NumPy scalars and other number objects (arrays with ndim > 0 go to tolist)
    let is_array = obj.hasattr("ndim")? && obj.getattr("ndim")?.extract::<usize>().map(|n| n > 0).unwrap_or(false);
    if !is_array && obj.hasattr("__index__")? {
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
    from_val_d(py, v, 0)
}

fn from_val_d(py: Python<'_>, v: &Val, depth: usize) -> PyResult<Py<PyAny>> {
    if depth > MAX_OUTPUT_DEPTH {
        return Err(PyValueError::new_err(format!("value nesting too deep (limit {MAX_OUTPUT_DEPTH} levels)")));
    }
    let from_val = |py: Python<'_>, x: &Val| from_val_d(py, x, depth + 1);
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

/// Exact integer conversion for coordinates, IDs and lanes (F040):
/// `int`/`bool`, objects with `__index__` (NumPy integers) and decimal
/// strings (like `int("3")` in the reference). Other numbers (float,
/// `Decimal`, NumPy floats) are accepted only with an integral value;
/// `2.7` raises `TypeError` instead of being truncated silently.
pub fn to_int(obj: &Bound<'_, PyAny>) -> PyResult<BigInt> {
    if obj.is_instance_of::<PyInt>() {
        return obj.extract::<BigInt>();
    }
    let builtins = obj.py().import("builtins")?;
    if obj.is_instance_of::<PyString>() {
        return builtins.getattr("int")?.call1((obj,))?.extract();
    }
    if obj.hasattr("__index__")? {
        return obj.py().import("operator")?.getattr("index")?.call1((obj,))?.extract();
    }
    let i = builtins.getattr("int")?.call1((obj,))?;
    if !obj.eq(&i)? {
        return Err(PyTypeError::new_err(format!(
            "an integer is required, got {} of type {} (non-integral values are not truncated)",
            obj.repr()?,
            obj.get_type().name()?
        )));
    }
    i.extract()
}

/// Coordinate → Nat; negative values → 0 (so the subsequent positivity
/// checks report the usual error message).
pub fn to_nat(obj: &Bound<'_, PyAny>) -> PyResult<Nat> {
    if let Ok(v) = obj.extract::<u64>() {
        return Ok(Nat::S(v));
    }
    Ok(Nat::from_bigint(&to_int(obj)?).unwrap_or(Nat::ZERO))
}

/// Non-negative count (e.g. `read_numpy(count)`): negative → `ValueError`,
/// larger than the address space → `MemoryError`.
pub fn to_count(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<usize> {
    use num_traits::{Signed, ToPrimitive};
    let b = to_int(obj)?;
    if b.is_negative() {
        return Err(PyValueError::new_err(format!("{what} must be >= 0")));
    }
    b.to_usize().ok_or_else(|| PyMemoryError::new_err(format!("{what}={b} is too large")))
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
        Some(v) if v <= i64::MAX as u64 => Ok(v),
        _ => Err(PyValueError::new_err(format!("{what} too large (max 2^63-1)"))),
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
    let i: BigInt = to_int(o)?;
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
