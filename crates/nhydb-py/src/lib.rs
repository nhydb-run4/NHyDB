//! PyO3 facade of the native NHyDB (`nhydb._native`).
//!
//! The `NHyDB` class exposes the public Python API (method names, keyword
//! arguments, result dicts). In addition:
//! * KISS single-item API: `put_cell`, `put_record`, `get_value`, … with `commit=`
//! * NumPy batch API: `put_numpy_batch`, `put_cells_numpy`,
//!   `put_objects_numpy`, `read_numpy` (loops run entirely in Rust).
//!
//! No database logic lives here; everything is delegated to `nhydb-core`.

// Nested conditions are kept on purpose: they mirror the structure of the
// Python API logic and keep the code easy to compare line by line.
#![allow(clippy::collapsible_if, clippy::collapsible_match)]
mod conv;
mod exceptions;
mod iter;
mod kernel;
mod mathapi;
mod queryapi;
mod numpy_io;
mod pyplugin;
mod textapi;

use std::path::PathBuf;
use std::sync::Arc;

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple, PyWeakrefReference};
use pyo3::IntoPyObjectExt;

use nhydb_core::address;
use nhydb_core::codec::{FieldDef, TypeCode};
use nhydb_core::db::layout::{EvolveSpec, LayoutRef, LayoutSpec};
use nhydb_core::db::maint::CompactOptions;
use nhydb_core::schema::field_from_val;
use nhydb_core::{CellInput, Cond, Db, DenseRows, GetMode, Nat, OpenOptions, SyncMode, Val};

use conv::*;
use exceptions::*;
use pyplugin::{PyLayoutDef, PyPlugin};

// ----------------------------------------------------------------------
// Argument helpers
// ----------------------------------------------------------------------

pub(crate) fn layout_ref(obj: &Bound<'_, PyAny>) -> PyResult<LayoutRef> {
    if let Ok(s) = obj.cast::<PyString>() {
        let s = s.to_str()?;
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(LayoutRef::Id(s.parse().map_err(|_| PyValueError::new_err("layout_id too large"))?));
        }
        return Ok(LayoutRef::Name(s.to_owned()));
    }
    Ok(LayoutRef::Id(to_id(obj, "layout_id")?))
}

fn type_code(name: &str) -> PyResult<TypeCode> {
    TypeCode::from_name(name).ok_or_else(|| SchemaError::new_err(format!("unknown data type '{name}'")))
}

/// Fields for `create_shape`: dicts or objects with `to_dict()`.
fn shape_fields(fields: &Bound<'_, PyAny>) -> PyResult<Vec<FieldDef>> {
    let mut out = Vec::new();
    for f in fields.try_iter()? {
        let f = f?;
        let d = if f.hasattr("to_dict")? && !f.is_instance_of::<PyDict>() { f.call_method0("to_dict")? } else { f };
        out.push(field_from_val(&to_val(&d)?).py()?);
    }
    Ok(out)
}

/// Extension argument (`_normalize_extensions` input forms) → pairs.
fn ext_items(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<(Val, Val)>> {
    let Some(obj) = obj else { return Ok(vec![]) };
    if obj.is_none() {
        return Ok(vec![]);
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        return d.iter().map(|(k, v)| Ok((to_val(&k)?, to_val(&v)?))).collect();
    }
    let is_key = |o: &Bound<'_, PyAny>| o.is_instance_of::<pyo3::types::PyInt>() || o.is_instance_of::<PyString>();
    if is_key(obj) {
        return Ok(vec![(to_val(obj)?, Val::None)]);
    }
    if let Ok(t) = obj.cast::<PyTuple>() {
        if t.len() == 2 && is_key(&t.get_item(0)?) {
            return Ok(vec![(to_val(&t.get_item(0)?)?, to_val(&t.get_item(1)?)?)]);
        }
    }
    let mut out = Vec::new();
    for item in obj.try_iter()? {
        let item = item?;
        if let Ok(t) = item.cast::<PyTuple>() {
            if t.len() == 2 {
                out.push((to_val(&t.get_item(0)?)?, to_val(&t.get_item(1)?)?));
                continue;
            }
        }
        out.push((to_val(&item)?, Val::None));
    }
    Ok(out)
}

/// Cells for `put_points`: dicts `{u,v,value,type}` or tuples `(u,v,value[,type])`.
fn cells_from_py(cells: &Bound<'_, PyAny>) -> PyResult<Vec<CellInput>> {
    let mut out = Vec::new();
    for item in cells.try_iter()? {
        let item = item?;
        let (u, v, value, typ) = if let Ok(d) = item.cast::<PyDict>() {
            let get = |k: &str| -> PyResult<Option<Bound<'_, PyAny>>> { d.get_item(k) };
            let u = get("u")?.ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("u"))?;
            let v = get("v")?.ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("v"))?;
            let value = get("value")?.map(|x| to_val(&x)).transpose()?.unwrap_or(Val::None);
            let typ = get("type")?.filter(|t| !t.is_none()).map(|t| -> PyResult<String> { Ok(t.str()?.to_str()?.to_owned()) }).transpose()?;
            (to_nat(&u)?, to_nat(&v)?, value, typ)
        } else {
            let n = item.len()?;
            if n != 3 && n != 4 {
                return Err(PyValueError::new_err("cell expects (u,v,value[,type])"));
            }
            let typ = if n == 4 {
                let t = item.get_item(3)?;
                if t.is_none() { None } else { Some(t.str()?.to_str()?.to_owned()) }
            } else {
                None
            };
            (to_nat(&item.get_item(0)?)?, to_nat(&item.get_item(1)?)?, to_val(&item.get_item(2)?)?, typ)
        };
        let typ = typ.map(|t| type_code(&t)).transpose()?;
        out.push(CellInput::typed(u, v, value, typ));
    }
    Ok(out)
}

fn val_list(py: Python<'_>, vals: Vec<Val>) -> PyResult<Py<PyAny>> {
    from_val(py, &Val::List(vals))
}

fn record_dict(py: Python<'_>, rec: &nhydb_core::DenseRecord) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("realm", nat_py(py, &rec.realm)?)?;
    d.set_item("shape_id", rec.shape_id)?;
    d.set_item("values", from_val(py, &Val::Map(rec.values.clone()))?)?;
    let bv = |m: &std::collections::BTreeMap<Nat, Val>| -> PyResult<Bound<'_, PyDict>> {
        let x = PyDict::new(py);
        for (k, v) in m {
            x.set_item(nat_py(py, k)?, from_val(py, v)?)?;
        }
        Ok(x)
    };
    d.set_item("base_by_v", bv(&rec.base_by_v)?)?;
    d.set_item("extensions_by_v", bv(&rec.extensions_by_v)?)?;
    d.set_item("by_v", bv(&rec.by_v())?)?;
    d.set_item("u", nat_py(py, &rec.u)?)?;
    d.set_item("lane", rec.lane as i64)?;
    Ok(d.into_any().unbind())
}

fn filters_from_py(filters: &Bound<'_, PyDict>) -> PyResult<Vec<(String, Cond)>> {
    let mut out = Vec::new();
    for (k, v) in filters.iter() {
        let name = k.str()?.to_str()?.to_owned();
        let cond = match v.cast::<PyTuple>() {
            Ok(t) if t.len() == 2 => Cond::Range(to_val(&t.get_item(0)?)?, to_val(&t.get_item(1)?)?),
            _ => Cond::Eq(to_val(&v)?),
        };
        out.push((name, cond));
    }
    Ok(out)
}

fn evolve_spec(
    name: Option<String>,
    description: Option<String>,
    u_start: Option<&Bound<'_, PyAny>>,
    u_end: Option<Option<Nat>>,
    priority: Option<i64>,
    add_fields: Option<&Bound<'_, PyAny>>,
    spacemap_update: Option<&Bound<'_, PyAny>>,
) -> PyResult<EvolveSpec> {
    let add = match add_fields {
        None => vec![],
        Some(a) if a.is_none() => vec![],
        Some(a) => match to_val(a)? {
            Val::List(l) => l,
            other => vec![other],
        },
    };
    Ok(EvolveSpec {
        name,
        description,
        u_start: opt_nat(u_start)?,
        u_end,
        priority,
        add_fields: add,
        spacemap_update: spacemap_update.filter(|s| !s.is_none()).map(|s| to_val(s)).transpose()?,
    })
}

/// Open options from the Python keyword arguments.
#[allow(clippy::too_many_arguments)]
fn open_opts(
    strict: bool,
    durability: Option<&str>,
    layout_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
    config: Option<PathBuf>,
    lock: Option<bool>,
    lock_timeout_ms: Option<u64>,
    base_lock: nhydb_core::LockConfig,
) -> PyResult<OpenOptions> {
    let sync_mode = durability.map(SyncMode::parse).transpose().py()?;
    let lock = if lock.is_some() || lock_timeout_ms.is_some() {
        Some(nhydb_core::LockConfig {
            enabled: lock.unwrap_or(base_lock.enabled),
            timeout_ms: lock_timeout_ms.unwrap_or(base_lock.timeout_ms),
        })
    } else {
        None
    };
    Ok(OpenOptions { strict, sync_mode, layout_path, key_path, config: None, config_path: config, lock, internal_compaction: false })
}

// ----------------------------------------------------------------------
// NHyDB
// ----------------------------------------------------------------------

#[pyclass(name = "NHyDB", module = "nhydb", weakref, dict)]
pub struct NHyDB {
    pub(crate) db: Db,
    /// Position of the file view `db.f` (seek/tell across multiple accesses).
    fpos: Arc<std::sync::atomic::AtomicU64>,
}

impl NHyDB {
    fn rlookup(&self, obj: &Bound<'_, PyAny>) -> PyResult<LayoutRef> {
        layout_ref(obj)
    }
}

#[pymethods]
impl NHyDB {
    /// Opens an existing database.
    #[new]
    #[pyo3(signature = (path, *, strict=true, layout_path=None, key_path=None, config=None, durability=None, lock=None, lock_timeout_ms=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        path: PathBuf,
        strict: bool,
        layout_path: Option<PathBuf>,
        key_path: Option<PathBuf>,
        config: Option<PathBuf>,
        durability: Option<&str>,
        lock: Option<bool>,
        lock_timeout_ms: Option<u64>,
    ) -> PyResult<Self> {
        let opts = open_opts(strict, durability, layout_path, key_path, config, lock, lock_timeout_ms, Default::default())?;
        let db = py.detach(|| Db::open_with(&path, opts)).py()?;
        Ok(NHyDB { db, fpos: Default::default() })
    }

    /// Creates a new database with a layout plan and key.
    #[classmethod]
    #[pyo3(signature = (path, *, overwrite=false, layout_path=None, key_path=None, config=None, durability=None, lock=None, lock_timeout_ms=None))]
    #[allow(clippy::too_many_arguments)]
    fn create(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        path: PathBuf,
        overwrite: bool,
        layout_path: Option<PathBuf>,
        key_path: Option<PathBuf>,
        config: Option<PathBuf>,
        durability: Option<&str>,
        lock: Option<bool>,
        lock_timeout_ms: Option<u64>,
    ) -> PyResult<Self> {
        let opts = open_opts(true, durability, layout_path, key_path, config, lock, lock_timeout_ms, Default::default())?;
        let db = py.detach(|| Db::create(&path, overwrite, opts)).py()?;
        Ok(NHyDB { db, fpos: Default::default() })
    }

    /// `get_system_metadata()`: R0,u1 (read-only).
    fn get_system_metadata(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.get_system_metadata())
    }

    /// `get_system_cell(v)`
    fn get_system_cell(&self, py: Python<'_>, v: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let n = match to_nat(v) {
            Ok(n) => n,
            Err(_) => return Err(AddressError::new_err("system v outside R0,u1")),
        };
        from_val(py, &self.db.get_system_cell(&n).py()?)
    }

    #[getter]
    fn layout_path(&self) -> PathBuf {
        self.db.layout_path().to_path_buf()
    }
    #[getter]
    fn key_path(&self) -> PathBuf {
        self.db.key_path().to_path_buf()
    }
    #[getter]
    fn max_db_size_bytes(&self) -> u64 {
        self.db.max_db_size_bytes
    }
    #[getter]
    fn next_block_id(&self) -> u64 {
        self.db.next_block_id
    }
    #[getter]
    fn config_source(&self) -> Option<PathBuf> {
        self.db.config.source.clone()
    }

    /// `set_max_db_size(size_bytes, *, commit=False)`
    #[pyo3(signature = (size_bytes, *, commit=false))]
    fn set_max_db_size(&mut self, size_bytes: &Bound<'_, PyAny>, commit: bool) -> PyResult<u64> {
        if size_bytes.is_instance_of::<pyo3::types::PyBool>() || !size_bytes.is_instance_of::<pyo3::types::PyInt>() {
            return Err(PyValueError::new_err("size_bytes must be a positive integer"));
        }
        let n: i128 = size_bytes.extract().map_err(|_| PyValueError::new_err("size_bytes must be a positive integer"))?;
        if n <= 0 || n > u64::MAX as i128 {
            return Err(PyValueError::new_err("size_bytes must be a positive integer"));
        }
        self.db.set_max_db_size(n as u64, commit).py()
    }

    /// `iter_materialized_cells(realm_id=None, *, lane=None)` (Iterator).
    #[pyo3(signature = (realm_id=None, *, lane=None))]
    fn iter_materialized_cells(slf: &Bound<'_, Self>, realm_id: Option<&Bound<'_, PyAny>>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<iter::ResultIterator> {
        let lane = match lane {
            Some(l) if !l.is_none() => Some(to_lane(Some(l))?),
            _ => None,
        };
        Ok(iter::ResultIterator::new(slf.clone().unbind(), iter::Pending::Cells { realm: opt_nat(realm_id)?, lane }))
    }

    /// `iter_materialized_record_ns(realm_id=None, *, lane=None)` (Iterator).
    #[pyo3(signature = (realm_id=None, *, lane=None))]
    fn iter_materialized_record_ns(slf: &Bound<'_, Self>, realm_id: Option<&Bound<'_, PyAny>>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<iter::ResultIterator> {
        let lane = match lane {
            Some(l) if !l.is_none() => Some(to_lane(Some(l))?),
            _ => None,
        };
        Ok(iter::ResultIterator::new(slf.clone().unbind(), iter::Pending::RecordNs { realm: opt_nat(realm_id)?, lane }))
    }

    /// `record_n_for_address(realm_id, u, v, *, lane=0)`
    #[pyo3(signature = (realm_id, u, v, *, lane=None))]
    fn record_n_for_address(&self, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, v: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Option<String>> {
        self.db.record_n_for_address(&to_nat(realm_id)?, &to_nat(u)?, &to_nat(v)?, to_lane(lane)?).py()
    }

    /// `record_n_from_n(value)`
    fn record_n_from_n(&self, value: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
        self.db.record_n_from_n(&to_n(value)?).py()
    }

    /// `query(expr, *, realm=None, lane=0, return_mode="record", select=None, explain=False)`
    #[pyo3(signature = (expr, *, realm=None, lane=None, return_mode=None, select=None, explain=None))]
    #[allow(clippy::too_many_arguments)]
    fn query(
        &self,
        py: Python<'_>,
        expr: &Bound<'_, PyAny>,
        realm: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        return_mode: Option<&Bound<'_, PyAny>>,
        select: Option<&Bound<'_, PyAny>>,
        explain: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let e = queryapi::py_to_expr(expr)?;
        let opts = nhydb_core::query::ExecOptions {
            realm: match realm {
                Some(r) if !r.is_none() => Some(r.extract::<num_bigint::BigInt>().or_else(|_| py.import("builtins")?.getattr("int")?.call1((r,))?.extract())?),
                _ => None,
            },
            lane: to_lane(lane)?,
            return_mode: match return_mode {
                Some(r) => r.str()?.to_string(),
                None => "record".into(),
            },
            select: match select {
                Some(s) if !s.is_none() => Some(to_val(s)?),
                _ => None,
            },
            explain: explain.map(|x| x.is_truthy()).transpose()?.unwrap_or(false),
            ..Default::default()
        };
        let r = queryapi::run_core(&self.db, |eng| eng.execute(&e, &opts))?;
        from_val(py, &r)
    }

    /// `sql(text, parameters=None, *, lane=0)`: a read-only NHySQL SELECT.
    #[pyo3(signature = (text, parameters=None, *, lane=None))]
    fn sql(&self, py: Python<'_>, text: &Bound<'_, PyAny>, parameters: Option<&Bound<'_, PyAny>>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let (t, p, l) = (text.str()?.to_string(), queryapi::sql_params(parameters)?, to_lane(lane)?);
        let r = queryapi::run_core(&self.db, |eng| nhydb_core::sql::execute_sql(eng, &t, &p, l))?;
        from_val(py, &r)
    }

    /// `sql_numpy(text, parameters=None, *, lane=0)`: result of a SELECT as
    /// `{column: numpy.ndarray}` in column order (bool, int64, float64 or
    /// object, whichever is the most compact exact representation).
    #[pyo3(signature = (text, parameters=None, *, lane=None))]
    fn sql_numpy(&self, py: Python<'_>, text: &Bound<'_, PyAny>, parameters: Option<&Bound<'_, PyAny>>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let (t, p, l) = (text.str()?.to_string(), queryapi::sql_params(parameters)?, to_lane(lane)?);
        let r = queryapi::run_core(&self.db, |eng| nhydb_core::sql::execute_sql(eng, &t, &p, l))?;
        queryapi::rows_to_numpy(py, &r)
    }

    /// `compile_sql(text, parameters=None, *, lane=0)` → `SQLCompiledQuery`
    #[pyo3(signature = (text, parameters=None, *, lane=None))]
    fn compile_sql(&self, py: Python<'_>, text: &Bound<'_, PyAny>, parameters: Option<&Bound<'_, PyAny>>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let (t, p, l) = (text.str()?.to_string(), queryapi::sql_params(parameters)?, to_lane(lane)?);
        queryapi::compile_sql_py(py, &self.db, &t, &p, l)
    }

    /// `explain_query(expr, *, realm=None, lane=0, return_mode="record")`
    #[pyo3(signature = (expr, *, realm=None, lane=None, return_mode=None))]
    fn explain_query(
        &self,
        py: Python<'_>,
        expr: &Bound<'_, PyAny>,
        realm: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        return_mode: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let e = queryapi::py_to_expr(expr)?;
        let realm = match realm {
            Some(r) if !r.is_none() => Some(r.extract::<num_bigint::BigInt>().or_else(|_| py.import("builtins")?.getattr("int")?.call1((r,))?.extract())?),
            _ => None,
        };
        let rm = match return_mode {
            Some(r) => r.str()?.to_string(),
            None => "record".into(),
        };
        let lane = to_lane(lane)?;
        let r = queryapi::run_core(&self.db, |eng| eng.explain(&e, realm, lane, &rm))?;
        from_val(py, &r)
    }

    /// `describe_math(layout_id)`
    fn describe_math(&self, py: Python<'_>, layout_id: u64) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.describe_math(layout_id).py()?)
    }

    /// `create_math_layout(spec, *, layout_id=None, payload_type="bytes", commit=False)`
    #[pyo3(signature = (spec, *, layout_id=None, payload_type="bytes".to_string(), commit=false))]
    fn create_math_layout(
        &mut self,
        spec: &Bound<'_, PyAny>,
        layout_id: Option<u64>,
        payload_type: String,
        commit: bool,
    ) -> PyResult<mathapi::PyMathLayoutState> {
        let spec = spec
            .cast::<mathapi::PyMathMapLayoutSpec>()
            .map_err(|_| PyTypeError::new_err("spec must be a MathMapLayoutSpec"))?
            .get()
            .inner
            .clone();
        Ok(mathapi::PyMathLayoutState { inner: self.db.create_math_layout(spec, layout_id, &payload_type, commit).py()? })
    }

    /// `create_math_map_layout(name, parameters, *, u, V, side="NL", layout_id=None,
    /// payload_type="bytes", realm_auto_limit=None, description="", commit=False)`
    #[pyo3(signature = (name, parameters, *, u, V, side=None, layout_id=None, payload_type="bytes".to_string(), realm_auto_limit=None, description="".to_string(), commit=false))]
    #[allow(non_snake_case, clippy::too_many_arguments)]
    fn create_math_map_layout(
        &mut self,
        name: &Bound<'_, PyAny>,
        parameters: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        V: &Bound<'_, PyAny>,
        side: Option<&Bound<'_, PyAny>>,
        layout_id: Option<u64>,
        payload_type: String,
        realm_auto_limit: Option<num_bigint::BigInt>,
        description: String,
        commit: bool,
    ) -> PyResult<mathapi::PyMathLayoutState> {
        let params: Vec<String> = parameters.try_iter()?.map(|x| x?.str().map(|s| s.to_string())).collect::<PyResult<_>>()?;
        let side = match side {
            Some(s) => to_val(s)?,
            None => Val::str("NL"),
        };
        let st = self
            .db
            .create_math_map_layout(
                &name.str()?.to_string(),
                &params,
                &u.str()?.to_string(),
                &V.str()?.to_string(),
                &side,
                layout_id,
                &payload_type,
                realm_auto_limit,
                &description,
                commit,
            )
            .py()?;
        Ok(mathapi::PyMathLayoutState { inner: st })
    }

    /// `replace_math_spec(layout_id, spec, *, payload_type=None, commit=False)`
    #[pyo3(signature = (layout_id, spec, *, payload_type=None, commit=false))]
    fn replace_math_spec(&mut self, layout_id: u64, spec: &Bound<'_, PyAny>, payload_type: Option<String>, commit: bool) -> PyResult<mathapi::PyMathLayoutState> {
        let spec = spec
            .cast::<mathapi::PyMathMapLayoutSpec>()
            .map_err(|_| PyTypeError::new_err("spec must be a MathMapLayoutSpec"))?
            .get()
            .inner
            .clone();
        Ok(mathapi::PyMathLayoutState { inner: self.db.replace_math_spec(layout_id, spec, payload_type.as_deref(), commit).py()? })
    }

    /// `put_math_batch(layout_id, rows, *, overwrite=False, commit=False)`
    #[pyo3(signature = (layout_id, rows, *, overwrite=false, commit=false))]
    fn put_math_batch(&mut self, py: Python<'_>, layout_id: u64, rows: &Bound<'_, PyAny>, overwrite: bool, commit: bool) -> PyResult<Py<PyAny>> {
        let mut rs = Vec::new();
        for row in rows.try_iter()? {
            let row = row?;
            let inputs = mathapi::inputs_of(&row.get_item(0)?)?;
            rs.push((inputs, to_val(&row.get_item(1)?)?));
        }
        let db = &mut self.db;
        from_val(py, &py.detach(|| db.put_math_batch(layout_id, rs, overwrite, commit)).py()?)
    }

    /// `put_math_numpy(layout_id, inputs, payloads, *, overwrite=False, commit=False)`
    ///
    /// NumPy batch for MATH layouts: `inputs` as a 2D array (rows ×
    /// parameters), 1D array (one parameter) or list of columns per
    /// parameter; `payloads` as a 1D array/list. Same semantics as
    /// `put_math_batch` (planning, conflicts, full rollback); the
    /// loop runs in Rust without the GIL.
    #[pyo3(signature = (layout_id, inputs, payloads, *, overwrite=false, commit=false))]
    fn put_math_numpy(
        &mut self,
        py: Python<'_>,
        layout_id: u64,
        inputs: &Bound<'_, PyAny>,
        payloads: &Bound<'_, PyAny>,
        overwrite: bool,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let ndim: Option<usize> = if inputs.hasattr("ndim")? { Some(inputs.getattr("ndim")?.extract()?) } else { None };
        let columns: Vec<Vec<Val>> = match ndim {
            Some(2) => {
                let k: usize = inputs.getattr("shape")?.get_item(1)?.extract()?;
                let t = inputs.getattr("T")?;
                (0..k).map(|j| Ok(numpy_io::col_from_py(&t.get_item(j)?)?.to_vals())).collect::<PyResult<_>>()?
            }
            Some(1) => vec![numpy_io::col_from_py(inputs)?.to_vals()],
            Some(_) => return Err(PyValueError::new_err("inputs must be 1D or 2D")),
            None => inputs.try_iter()?.map(|c| Ok(numpy_io::col_from_py(&c?)?.to_vals())).collect::<PyResult<_>>()?,
        };
        let vals = numpy_io::col_from_py(payloads)?.to_vals();
        let n = vals.len();
        if columns.iter().any(|c| c.len() != n) {
            return Err(PyValueError::new_err("inputs and payloads must have the same number of rows"));
        }
        let db = &mut self.db;
        let res = py.detach(move || {
            let mut cols: Vec<std::vec::IntoIter<Val>> = columns.into_iter().map(|c| c.into_iter()).collect();
            let rows: Vec<(Vec<Val>, Val)> = vals.into_iter().map(|p| (cols.iter_mut().map(|c| c.next().unwrap()).collect(), p)).collect();
            db.put_math_batch(layout_id, rows, overwrite, commit)
        });
        from_val(py, &res.py()?)
    }

    /// `put_math(layout_id, inputs, payload, *, overwrite=False, commit=False)`
    #[pyo3(signature = (layout_id, inputs, payload, *, overwrite=false, commit=false))]
    fn put_math(&mut self, py: Python<'_>, layout_id: u64, inputs: &Bound<'_, PyAny>, payload: &Bound<'_, PyAny>, overwrite: bool, commit: bool) -> PyResult<Py<PyAny>> {
        let ins = mathapi::inputs_of(inputs)?;
        from_val(py, &self.db.put_math(layout_id, &ins, to_val(payload)?, overwrite, commit).py()?)
    }

    /// `get_math_cell(layout_id, inputs, *, dyn_address=False)`
    #[pyo3(signature = (layout_id, inputs, *, dyn_address=false))]
    fn get_math_cell(&self, py: Python<'_>, layout_id: u64, inputs: &Bound<'_, PyAny>, dyn_address: bool) -> PyResult<Py<PyAny>> {
        let ins = mathapi::inputs_of(inputs)?;
        match self.db.get_math_cell(layout_id, &ins, dyn_address).py()? {
            Some(mut v) => {
                // _math_inputs is a tuple in Python
                let d = from_val(py, &{
                    v.remove("_math_inputs");
                    v
                })?;
                let ins_t = PyTuple::new(py, ins.iter().map(|x| from_val(py, x)).collect::<PyResult<Vec<_>>>()?)?;
                d.bind(py).set_item("_math_inputs", ins_t)?;
                Ok(d)
            }
            None => Ok(py.None()),
        }
    }

    /// `get_math(layout_id, inputs)`
    fn get_math(&self, py: Python<'_>, layout_id: u64, inputs: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.get_math(layout_id, &mathapi::inputs_of(inputs)?).py()?)
    }

    /// MATH states `{layout_id: MathLayoutState}`.
    #[getter]
    fn math_layouts(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let d = PyDict::new(py);
        for (k, v) in &self.db.math_layouts {
            d.set_item(k, Py::new(py, mathapi::PyMathLayoutState { inner: v.clone() })?)?;
        }
        Ok(d.into_any().unbind())
    }

    /// Read-only file view (`seek`/`tell`) for tools and conformance tests.
    #[getter]
    fn f(&self) -> FileView {
        FileView { len: self.db.file_len(), pos: self.fpos.clone() }
    }

    /// `select_us(realm_id, us, columns, *, lane=0)` (Iterator).
    #[pyo3(signature = (realm_id, us, columns, *, lane=None))]
    fn select_us(
        slf: &Bound<'_, Self>,
        realm_id: &Bound<'_, PyAny>,
        us: &Bound<'_, PyAny>,
        columns: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<iter::ResultIterator> {
        let lane = to_lane(lane)?;
        let mut cand = Vec::new();
        for x in us.try_iter()? {
            let b: num_bigint::BigInt = x?.extract()?;
            if b > num_bigint::BigInt::from(0) {
                cand.push(Nat::from_bigint(&b).unwrap());
            }
        }
        let cols: Vec<Val> = columns.try_iter()?.map(|c| to_val(&c?)).collect::<PyResult<_>>()?;
        Ok(iter::ResultIterator::new(slf.clone().unbind(), iter::Pending::SelectUs { realm: to_nat(realm_id)?, lane, us: cand, columns: cols }))
    }

    /// `fulltext_term_posting(index_id, term, *, lane=0)`
    #[pyo3(signature = (index_id, term, *, lane=None))]
    fn fulltext_term_posting<'py>(&self, py: Python<'py>, index_id: u64, term: &Bound<'py, PyAny>, lane: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyTuple>> {
        let t = term.str()?.to_string();
        textapi::postings_tuple(py, &self.db.fulltext_term_posting(index_id, &t, to_lane(lane)?).py()?)
    }

    /// `fulltext_corpus(index_id, *, lane=0)`
    #[pyo3(signature = (index_id, *, lane=None))]
    fn fulltext_corpus(&self, index_id: u64, lane: Option<&Bound<'_, PyAny>>) -> PyResult<textapi::PyFtCorpus> {
        Ok(textapi::PyFtCorpus { inner: (*self.db.fulltext_corpus(index_id, to_lane(lane)?).py()?).clone() })
    }

    /// `fulltext_payloads(index_id, terms, *, lane=0)` → (dict term→postings, corpus)
    #[pyo3(signature = (index_id, terms, *, lane=None))]
    fn fulltext_payloads<'py>(&self, py: Python<'py>, index_id: u64, terms: &Bound<'py, PyAny>, lane: Option<&Bound<'py, PyAny>>) -> PyResult<(Bound<'py, PyDict>, textapi::PyFtCorpus)> {
        let ts: Vec<String> = terms.try_iter()?.map(|x| x?.str().map(|s| s.to_string())).collect::<PyResult<_>>()?;
        let (p, c) = self.db.fulltext_payloads(index_id, &ts, to_lane(lane)?).py()?;
        let d = PyDict::new(py);
        for (t, list) in p {
            d.set_item(t, textapi::postings_tuple(py, &list[..])?)?;
        }
        Ok((d, textapi::PyFtCorpus { inner: (*c).clone() }))
    }

    /// `posting_count(index_id, cond, *, lane=0)`; `cond` is a value or (lo, hi).
    #[pyo3(signature = (index_id, cond, *, lane=None))]
    fn posting_count(&self, index_id: u64, cond: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<u64> {
        let c = match cond.cast::<PyTuple>() {
            Ok(t) if t.len() == 2 => Cond::Range(to_val(&t.get_item(0)?)?, to_val(&t.get_item(1)?)?),
            _ => Cond::Eq(to_val(cond)?),
        };
        self.db.posting_count(index_id, &c, to_lane(lane)?).py()
    }

    /// `index_candidate_estimate(index_id, op, value=None, value2=None, *, lane=0, predicate_options=None)`
    #[pyo3(signature = (index_id, op, value=None, value2=None, *, lane=None, predicate_options=None))]
    #[allow(clippy::too_many_arguments)]
    fn index_candidate_estimate(
        &self,
        py: Python<'_>,
        index_id: u64,
        op: &str,
        value: Option<&Bound<'_, PyAny>>,
        value2: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        predicate_options: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let ov = |o: Option<&Bound<'_, PyAny>>| -> PyResult<Val> { o.map(to_val).transpose().map(|v| v.unwrap_or(Val::None)) };
        let r = self.db.index_candidate_estimate(index_id, op, &ov(value)?, &ov(value2)?, to_lane(lane)?, &ov(predicate_options)?).py()?;
        from_val(py, &r)
    }

    /// `index_candidates(index_id, op, value=None, value2=None, *, lane=0, predicate_options=None)`
    #[pyo3(signature = (index_id, op, value=None, value2=None, *, lane=None, predicate_options=None))]
    #[allow(clippy::too_many_arguments)]
    fn index_candidates<'py>(
        &self,
        py: Python<'py>,
        index_id: u64,
        op: &str,
        value: Option<&Bound<'py, PyAny>>,
        value2: Option<&Bound<'py, PyAny>>,
        lane: Option<&Bound<'py, PyAny>>,
        predicate_options: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyList>> {
        let ov = |o: Option<&Bound<'py, PyAny>>| -> PyResult<Val> { o.map(to_val).transpose().map(|v| v.unwrap_or(Val::None)) };
        let us = self.db.index_candidates(index_id, op, &ov(value)?, &ov(value2)?, to_lane(lane)?, &ov(predicate_options)?).py()?;
        let items: Vec<Py<PyAny>> = us.iter().map(|n| nat_py(py, n)).collect::<PyResult<_>>()?;
        PyList::new(py, items)
    }

    /// `_ensure_secondary_directory(index_id, lane=0)` → `{key: PostingRef}`
    #[pyo3(signature = (index_id, lane=None))]
    fn _ensure_secondary_directory<'py>(&self, py: Python<'py>, index_id: u64, lane: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyDict>> {
        let ln = to_lane(lane)?;
        let dir = self.db.secondary_directory(index_id, ln).py()?;
        let src = self.db.secondary_source_block_id(index_id, ln).unwrap_or(0);
        let d = PyDict::new(py);
        for e in &dir.entries {
            let r = textapi::PyPostingRef {
                index_id,
                value: e.value.clone(),
                abs_offset: e.abs_offset,
                compressed_len: e.compressed_len,
                raw_count: e.raw_count,
                width: e.width,
                source_block_id: src,
            };
            d.set_item(from_val(py, &e.value)?, Py::new(py, r)?)?;
        }
        Ok(d)
    }

    /// Realm definitions `{realm_id: dict}`.
    #[getter]
    fn realms(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let d = PyDict::new(py);
        for (k, v) in &self.db.realms {
            d.set_item(nat_py(py, k)?, from_val(py, &v.to_val())?)?;
        }
        Ok(d.into_any().unbind())
    }

    /// Random database ID (R0,u1,v1).
    #[getter]
    fn _db_id(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.get_system_cell(&Nat::S(1)).py()?)
    }

    /// Active sealed plan entry (generation, hash, nonce, ciphertext).
    #[getter]
    fn _active_layout_entry(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.active_layout_entry())
    }

    /// Current schema snapshot (plaintext, as it would be sealed).
    fn _export_external_schema(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.export_schema().py()?)
    }

    /// Index definitions `{index_id: IndexDef}`.
    #[getter]
    fn index_defs(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let d = PyDict::new(py);
        for (k, v) in &self.db.index_defs {
            d.set_item(k, Py::new(py, PyIndexDef::from(v))?)?;
        }
        Ok(d.into_any().unbind())
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&mut self, _args: &Bound<'_, PyTuple>) -> PyResult<bool> {
        self.db.close(true).py()?;
        Ok(false)
    }

    #[getter]
    fn path(&self) -> PathBuf {
        self.db.path.clone()
    }
    #[getter]
    fn closed(&self) -> bool {
        self.db.closed
    }
    #[getter]
    fn recovered_tail_bytes(&self) -> u64 {
        self.db.recovered_tail_bytes
    }
    #[getter]
    fn last_valid_end(&self) -> u64 {
        self.db.last_valid_end
    }
    #[getter]
    fn committed_end(&self) -> u64 {
        self.db.superblock().last_valid_end
    }
    #[getter]
    fn checkpoint_offset(&self) -> u64 {
        self.db.checkpoint_offset
    }
    #[getter]
    fn durability(&self) -> &'static str {
        self.db.sync_mode().name()
    }
    #[setter]
    fn set_durability(&mut self, mode: &str) -> PyResult<()> {
        self.db.set_sync_mode(SyncMode::parse(mode).py()?).py()
    }

    #[pyo3(signature = (*, commit=true))]
    fn close(&mut self, py: Python<'_>, commit: bool) -> PyResult<()> {
        let db = &mut self.db;
        py.detach(|| db.close(commit)).py()
    }

    #[pyo3(signature = (*, hard=false))]
    fn sync(&mut self, hard: bool) -> PyResult<()> {
        self.db.sync(hard).py()
    }

    fn commit(&mut self, py: Python<'_>) -> PyResult<u64> {
        let db = &mut self.db;
        py.detach(|| db.commit()).py()
    }

    // ------------------------------------------------------------------
    // Schema
    // ------------------------------------------------------------------

    #[pyo3(signature = (realm_id, name, *, description="".to_string(), default_shape_id=None, base_reserve_to=None))]
    fn create_realm(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        description: String,
        default_shape_id: Option<&Bound<'_, PyAny>>,
        base_reserve_to: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let ds = default_shape_id.filter(|x| !x.is_none()).map(|x| to_id(x, "shape_id")).transpose()?;
        let brt = opt_nat(base_reserve_to)?.unwrap_or(Nat::ZERO);
        let r = self.db.create_realm(&to_nat(realm_id)?, &name, &description, ds, &brt).py()?;
        from_val(py, &r.to_val())
    }

    #[pyo3(signature = (shape_id, realm_id, name, fields, *, description="".to_string()))]
    fn create_shape(
        &mut self,
        py: Python<'_>,
        shape_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        fields: &Bound<'_, PyAny>,
        description: String,
    ) -> PyResult<Py<PyAny>> {
        let realm = to_nat(realm_id)?;
        if !self.db.has_realm(&realm) {
            return Err(SchemaError::new_err("realm missing"));
        }
        let fs = shape_fields(fields)?;
        let s = self.db.create_shape(to_id(shape_id, "shape_id")?, &realm, &name, fs, &description).py()?;
        from_val(py, &s.to_val())
    }

    fn set_realm_default_shape(&mut self, py: Python<'_>, realm_id: &Bound<'_, PyAny>, shape_id: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let r = self.db.set_realm_default_shape(&to_nat(realm_id)?, to_id(shape_id, "shape_id")?).py()?;
        from_val(py, &r.to_val())
    }

    #[pyo3(signature = (realm_id, v, r#type, *, name="".to_string(), r#enum=None, description="".to_string(), replace=false))]
    #[allow(clippy::too_many_arguments)]
    fn define_extension(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        v: &Bound<'_, PyAny>,
        r#type: String,
        name: String,
        r#enum: Option<&Bound<'_, PyAny>>,
        description: String,
        replace: bool,
    ) -> PyResult<Py<PyAny>> {
        let en = r#enum.map(|e| to_val(e)).transpose()?.unwrap_or(Val::None);
        let e = self
            .db
            .define_extension(&to_nat(realm_id)?, &to_nat(v)?, &r#type, &name, &en, &description, replace)
            .py()?;
        from_val(py, &e.to_val())
    }

    fn define_extensions(&mut self, py: Python<'_>, realm_id: &Bound<'_, PyAny>, fields: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let realm = to_nat(realm_id)?;
        let mut out = Vec::new();
        for f in fields.try_iter()? {
            let f = f?;
            let d = if f.hasattr("to_dict")? && !f.is_instance_of::<PyDict>() { f.call_method0("to_dict")? } else { f };
            let d = to_val(&d)?;
            if let Some(r) = d.get("realm_id") {
                if r.as_nat().as_ref() != Some(&realm) {
                    return Err(SchemaError::new_err("extension does not match the realm"));
                }
            }
            let v = d.get("v").and_then(|x| x.as_nat()).ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("v"))?;
            let typ = d.get("type").map(|x| x.py_str()).ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("type"))?;
            let name = d.get("name").map(|x| x.py_str()).unwrap_or_default();
            let desc = d.get("description").map(|x| x.py_str()).unwrap_or_default();
            let en = d.get("enum").cloned().unwrap_or(Val::None);
            out.push(self.db.define_extension(&realm, &v, &typ, &name, &en, &desc, false).py()?.to_val());
        }
        val_list(py, out)
    }

    fn resolve_v(&self, py: Python<'_>, realm_id: &Bound<'_, PyAny>, column: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        nat_py(py, &self.db.resolve_v(&to_nat(realm_id)?, &to_val(column)?).py()?)
    }

    #[pyo3(signature = (index_id, realm_id, name, v, *, kind=None, options=None, tokenizer=None, casefold=None, ngram=None, description="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn create_index(
        &mut self,
        py: Python<'_>,
        index_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        v: &Bound<'_, PyAny>,
        kind: Option<&Bound<'_, PyAny>>,
        options: Option<&Bound<'_, PyAny>>,
        tokenizer: Option<&Bound<'_, PyAny>>,
        casefold: Option<&Bound<'_, PyAny>>,
        ngram: Option<&Bound<'_, PyAny>>,
        description: String,
    ) -> PyResult<Py<PyIndexDef>> {
        let kind = kind.map(to_val).transpose()?.unwrap_or(Val::None);
        let options = options.map(to_val).transpose()?.unwrap_or(Val::None);
        let tokenizer = tokenizer.map(to_val).transpose()?;
        let casefold = casefold.map(|c| c.is_truthy()).transpose()?;
        let ngram = ngram.map(to_val).transpose()?;
        let idx = self
            .db
            .create_index_ext(
                to_id(index_id, "index_id")?,
                &to_nat(realm_id)?,
                &name,
                &to_val(v)?,
                &kind,
                &options,
                tokenizer.as_ref(),
                casefold,
                ngram.as_ref(),
                &description,
            )
            .py()?;
        Py::new(py, PyIndexDef::from(&idx))
    }

    #[pyo3(signature = (index_id, realm_id, name, v, *, tokenizer="simple".to_string(), casefold=true, description="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn create_token_index(
        &mut self,
        py: Python<'_>,
        index_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        v: &Bound<'_, PyAny>,
        tokenizer: String,
        casefold: bool,
        description: String,
    ) -> PyResult<Py<PyIndexDef>> {
        let idx = self
            .db
            .create_index_ext(
                to_id(index_id, "index_id")?,
                &to_nat(realm_id)?,
                &name,
                &to_val(v)?,
                &Val::str("TOKEN"),
                &Val::None,
                Some(&Val::str(tokenizer)),
                Some(casefold),
                None,
                &description,
            )
            .py()?;
        Py::new(py, PyIndexDef::from(&idx))
    }

    #[pyo3(signature = (index_id, realm_id, name, v, *, ngram=None, casefold=false, description="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn create_trigram_index(
        &mut self,
        py: Python<'_>,
        index_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        v: &Bound<'_, PyAny>,
        ngram: Option<&Bound<'_, PyAny>>,
        casefold: bool,
        description: String,
    ) -> PyResult<Py<PyIndexDef>> {
        let ngram = ngram.map(to_val).transpose()?.unwrap_or(Val::Int(3));
        let idx = self
            .db
            .create_index_ext(
                to_id(index_id, "index_id")?,
                &to_nat(realm_id)?,
                &name,
                &to_val(v)?,
                &Val::str("TRIGRAM"),
                &Val::None,
                None,
                Some(casefold),
                Some(&ngram),
                &description,
            )
            .py()?;
        Py::new(py, PyIndexDef::from(&idx))
    }

    #[pyo3(signature = (index_id, realm_id, name, v, *, tokenizer="simple".to_string(), casefold=true, description="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn create_fulltext_index(
        &mut self,
        py: Python<'_>,
        index_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        v: &Bound<'_, PyAny>,
        tokenizer: String,
        casefold: bool,
        description: String,
    ) -> PyResult<Py<PyIndexDef>> {
        let idx = self
            .db
            .create_index_ext(
                to_id(index_id, "index_id")?,
                &to_nat(realm_id)?,
                &name,
                &to_val(v)?,
                &Val::str("FULLTEXT"),
                &Val::None,
                Some(&Val::str(tokenizer)),
                Some(casefold),
                None,
                &description,
            )
            .py()?;
        Py::new(py, PyIndexDef::from(&idx))
    }

    fn realm_bounds(&self, py: Python<'_>, realm_id: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.realm_bounds(&to_nat(realm_id)?).py()?)
    }

    fn dyn_address(&self, py: Python<'_>, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, v: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.dyn_address(&to_nat(realm_id)?, &to_nat(u)?, &to_nat(v)?).py()?.to_val())
    }

    #[pyo3(signature = (realm_id, u, v, *, lane=None))]
    fn cell_address(
        &self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        v: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        from_val(py, &self.db.cell_address(&to_nat(realm_id)?, &to_nat(u)?, &to_nat(v)?, lane).py()?.to_val())
    }

    fn resolve_n(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.resolve_n(&to_n(value)?).py()?)
    }

    #[pyo3(signature = (value, *, dyn_address=true))]
    fn get_cell_n(&self, py: Python<'_>, value: &Bound<'_, PyAny>, dyn_address: bool) -> PyResult<Py<PyAny>> {
        opt_from_val(py, self.db.get_cell_n(&to_n(value)?, dyn_address).py()?)
    }

    #[pyo3(signature = (mode="off", *, v=None))]
    fn set_dyn_address_output(slf: Bound<'_, Self>, mode: &str, v: Option<&Bound<'_, PyAny>>) -> PyResult<Py<Self>> {
        let v = opt_nat(v)?;
        slf.borrow_mut().db.set_dyn_address_output(mode, v).py()?;
        Ok(slf.unbind())
    }

    fn extension_catalog(&self, py: Python<'_>, realm_id: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.extension_catalog(&to_nat(realm_id)?).py()?)
    }

    // ------------------------------------------------------------------
    // Layouts
    // ------------------------------------------------------------------

    #[pyo3(signature = (layout_id, realm_id, name, kind, *, u_start=None, u_end=None, priority=0, shape_id=None,
                        base_reserve_to=None, params=None, spacemap=None, plugin="".to_string(), description="".to_string(), replace=false))]
    #[allow(clippy::too_many_arguments)]
    fn create_layout(
        &mut self,
        layout_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        kind: String,
        u_start: Option<&Bound<'_, PyAny>>,
        u_end: Option<&Bound<'_, PyAny>>,
        priority: i64,
        shape_id: Option<&Bound<'_, PyAny>>,
        base_reserve_to: Option<&Bound<'_, PyAny>>,
        params: Option<&Bound<'_, PyAny>>,
        spacemap: Option<&Bound<'_, PyAny>>,
        plugin: String,
        description: String,
        replace: bool,
    ) -> PyResult<PyLayoutDef> {
        let realm = to_nat(realm_id)?;
        if !self.db.has_realm(&realm) {
            return Err(SchemaError::new_err("realm missing"));
        }
        let spec = LayoutSpec {
            layout_id: to_id(layout_id, "layout_id")?,
            realm_id: realm,
            name,
            kind,
            u_start: opt_nat(u_start)?.unwrap_or(Nat::ONE),
            u_end: opt_nat(u_end)?,
            priority,
            shape_id: shape_id.filter(|x| !x.is_none()).map(|x| to_id(x, "shape_id")).transpose()?,
            base_reserve_to: opt_nat(base_reserve_to)?.unwrap_or(Nat::ZERO),
            params: params.filter(|x| !x.is_none()).map(|x| to_val(x)).transpose()?.unwrap_or(Val::map()),
            spacemap: spacemap.filter(|x| !x.is_none()).map(|x| to_val(x)).transpose()?,
            plugin,
            description,
            replace,
        };
        Ok(PyLayoutDef { inner: self.db.create_layout(spec).py()? })
    }

    #[pyo3(signature = (layout_id, realm_id, name, *, shape_id, base_reserve_to=None, u_start=None, u_end=None, priority=0, description="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn create_classic_layout(
        &mut self,
        layout_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        shape_id: &Bound<'_, PyAny>,
        base_reserve_to: Option<&Bound<'_, PyAny>>,
        u_start: Option<&Bound<'_, PyAny>>,
        u_end: Option<&Bound<'_, PyAny>>,
        priority: i64,
        description: String,
    ) -> PyResult<PyLayoutDef> {
        let lay = self
            .db
            .create_classic_layout(
                to_id(layout_id, "layout_id")?,
                &to_nat(realm_id)?,
                &name,
                to_id(shape_id, "shape_id")?,
                &opt_nat(base_reserve_to)?.unwrap_or(Nat::ZERO),
                &opt_nat(u_start)?.unwrap_or(Nat::ONE),
                opt_nat(u_end)?.as_ref(),
                priority,
                &description,
            )
            .py()?;
        Ok(PyLayoutDef { inner: lay })
    }

    #[pyo3(signature = (layout_id, realm_id, name, *, fields, anchor_u=None, priority=0, description="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn create_column_layout(
        &mut self,
        layout_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        fields: &Bound<'_, PyAny>,
        anchor_u: Option<&Bound<'_, PyAny>>,
        priority: i64,
        description: String,
    ) -> PyResult<PyLayoutDef> {
        let fs = match to_val(fields)? {
            Val::List(l) => l,
            _ => return Err(PyTypeError::new_err("fields must be a list")),
        };
        let lay = self
            .db
            .create_column_layout(to_id(layout_id, "layout_id")?, &to_nat(realm_id)?, &name, fs, opt_nat(anchor_u)?.as_ref(), priority, &description)
            .py()?;
        Ok(PyLayoutDef { inner: lay })
    }

    #[pyo3(signature = (layout_id, realm_id, name, *, u, span=None, fields=None, priority=0, description="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn create_packed_layout(
        &mut self,
        layout_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        u: &Bound<'_, PyAny>,
        span: Option<&Bound<'_, PyAny>>,
        fields: Option<&Bound<'_, PyAny>>,
        priority: i64,
        description: String,
    ) -> PyResult<PyLayoutDef> {
        let fs = match fields.filter(|f| !f.is_none()).map(|f| to_val(f)).transpose()? {
            Some(Val::List(l)) => l,
            None => vec![],
            _ => return Err(PyTypeError::new_err("fields must be a list")),
        };
        let lay = self
            .db
            .create_packed_layout(
                to_id(layout_id, "layout_id")?,
                &to_nat(realm_id)?,
                &name,
                &to_nat(u)?,
                &opt_nat(span)?.unwrap_or(Nat::S(100_000_000)),
                fs,
                priority,
                &description,
            )
            .py()?;
        Ok(PyLayoutDef { inner: lay })
    }

    #[pyo3(signature = (layout_id, realm_id, name, *, plugin, u_start, u_end, params=None, spacemap=None, priority=0, description="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn create_custom_layout(
        &mut self,
        layout_id: &Bound<'_, PyAny>,
        realm_id: &Bound<'_, PyAny>,
        name: String,
        plugin: String,
        u_start: &Bound<'_, PyAny>,
        u_end: Option<&Bound<'_, PyAny>>,
        params: Option<&Bound<'_, PyAny>>,
        spacemap: Option<&Bound<'_, PyAny>>,
        priority: i64,
        description: String,
    ) -> PyResult<PyLayoutDef> {
        let lay = self
            .db
            .create_custom_layout(
                to_id(layout_id, "layout_id")?,
                &to_nat(realm_id)?,
                &name,
                &plugin,
                &to_nat(u_start)?,
                opt_nat(u_end)?.as_ref(),
                params.filter(|x| !x.is_none()).map(|x| to_val(x)).transpose()?.unwrap_or(Val::map()),
                spacemap.filter(|x| !x.is_none()).map(|x| to_val(x)).transpose()?,
                priority,
                &description,
            )
            .py()?;
        Ok(PyLayoutDef { inner: lay })
    }

    /// Registers a Python object as a CUSTOM plugin (runtime only, not persisted).
    fn register_layout_plugin(slf: Bound<'_, Self>, name: String, plugin: Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let weak = PyWeakrefReference::new(slf.as_any())?.unbind();
        let p = PyPlugin { obj: plugin.clone().unbind(), db: Some(weak) };
        slf.borrow_mut().db.register_layout_plugin(&name, Arc::new(p)).py()?;
        Ok(plugin.unbind())
    }

    fn layout_at(&self, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>) -> PyResult<Option<PyLayoutDef>> {
        Ok(self.db.layout_at(&to_nat(realm_id)?, &to_nat(u)?).map(|l| PyLayoutDef { inner: l.clone() }))
    }

    fn layout_candidates(&self, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>) -> PyResult<Vec<PyLayoutDef>> {
        Ok(self.db.layout_candidates(&to_nat(realm_id)?, &to_nat(u)?).into_iter().map(|l| PyLayoutDef { inner: l.clone() }).collect())
    }

    #[pyo3(signature = (realm_id, u, v=None, *, lane=None))]
    fn route_address(
        &self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        v: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let u = to_nat(u)?;
        address::require_positive("u", &u).py()?;
        let lane = to_lane(lane)?;
        let v = opt_nat(v)?;
        from_val(py, &self.db.route_address(&to_nat(realm_id)?, &u, v.as_ref(), lane).py()?)
    }

    #[pyo3(signature = (realm_id=None))]
    fn layout_diagnostics(&self, py: Python<'_>, realm_id: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.layout_diagnostics(opt_nat(realm_id)?.as_ref()).py()?)
    }

    #[pyo3(signature = (realm_id=None, u=None, layout_id=None))]
    fn get_layout(
        &self,
        py: Python<'_>,
        realm_id: Option<&Bound<'_, PyAny>>,
        u: Option<&Bound<'_, PyAny>>,
        layout_id: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let lr = layout_id.filter(|x| !x.is_none()).map(layout_ref).transpose()?;
        from_val(py, &self.db.get_layout(opt_nat(realm_id)?.as_ref(), opt_nat(u)?.as_ref(), lr.as_ref()).py()?)
    }

    #[pyo3(signature = (layout_id, *, name=None, description=None, u_start=None, u_end=Some(unset_marker()), priority=None, add_fields=None, spacemap_update=None))]
    #[allow(clippy::too_many_arguments)]
    fn plan_layout_evolution(
        &self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        name: Option<String>,
        description: Option<String>,
        u_start: Option<&Bound<'_, PyAny>>,
        u_end: Option<UEnd>,
        priority: Option<i64>,
        add_fields: Option<&Bound<'_, PyAny>>,
        spacemap_update: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let spec = evolve_spec(name, description, u_start, ue_spec(py, u_end)?, priority, add_fields, spacemap_update)?;
        from_val(py, &self.db.plan_layout_evolution(&self.rlookup(layout_id)?, &spec).py()?)
    }

    #[pyo3(signature = (layout_id, *, name=None, description=None, u_start=None, u_end=Some(unset_marker()), priority=None, add_fields=None, spacemap_update=None, commit=false))]
    #[allow(clippy::too_many_arguments)]
    fn evolve_layout(
        &mut self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        name: Option<String>,
        description: Option<String>,
        u_start: Option<&Bound<'_, PyAny>>,
        u_end: Option<UEnd>,
        priority: Option<i64>,
        add_fields: Option<&Bound<'_, PyAny>>,
        spacemap_update: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<PyLayoutDef> {
        let spec = evolve_spec(name, description, u_start, ue_spec(py, u_end)?, priority, add_fields, spacemap_update)?;
        let r = self.rlookup(layout_id)?;
        Ok(PyLayoutDef { inner: self.db.evolve_layout(&r, &spec, commit).py()? })
    }

    #[pyo3(signature = (layout_id, object_id, field=None, *, lane=None))]
    fn logical_address(
        &self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        object_id: &Bound<'_, PyAny>,
        field: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let r = self.rlookup(layout_id)?;
        let lane = to_lane(lane)?;
        let f = field.map(|f| to_val(f)).transpose()?.unwrap_or(Val::None);
        from_val(py, &self.db.logical_address(&r, &to_nat(object_id)?, &f, lane).py()?.to_val())
    }

    #[pyo3(signature = (layout_id, object_id, *, lane=None))]
    fn record_anchor(&self, py: Python<'_>, layout_id: &Bound<'_, PyAny>, object_id: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let r = self.rlookup(layout_id)?;
        from_val(py, &self.db.record_anchor(&r, &to_nat(object_id)?, to_lane(lane)?).py()?.to_val())
    }

    // ------------------------------------------------------------------
    // Writing
    // ------------------------------------------------------------------

    #[pyo3(signature = (realm_id, cells, *, lane=None, commit=false, u_window=10_000))]
    fn put_points(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        cells: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
        u_window: i64,
    ) -> PyResult<Py<PyAny>> {
        let realm = to_nat(realm_id)?;
        if !self.db.has_realm(&realm) {
            return Err(SchemaError::new_err("realm missing"));
        }
        let lane = to_lane(lane)?;
        let cells = cells_from_py(cells)?;
        let db = &mut self.db;
        let r = py.detach(|| db.put_points(&realm, cells, lane, commit, u_window.max(1) as u64)).py()?;
        from_val(py, &r)
    }

    #[pyo3(signature = (realm_id, u, v, *, lane=None, commit=false))]
    fn delete_point(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        v: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        from_val(py, &self.db.delete_point(&to_nat(realm_id)?, &to_nat(u)?, &to_nat(v)?, lane, commit).py()?)
    }

    #[pyo3(signature = (realm_id=None, lane=None))]
    fn iter_materialized_sparse_points(
        slf: &Bound<'_, Self>,
        realm_id: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<iter::ResultIterator> {
        let lane = match lane {
            Some(l) if !l.is_none() => Some(to_lane(Some(l))?),
            _ => None,
        };
        Ok(iter::ResultIterator::new(slf.clone().unbind(), iter::Pending::Sparse { realm: opt_nat(realm_id)?, lane }))
    }

    #[pyo3(signature = (realm_id, u_start, rows, *, shape_id=None, extensions_by_u=None, lane=None, commit=false))]
    #[allow(clippy::too_many_arguments)]
    fn put_dense_batch(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u_start: &Bound<'_, PyAny>,
        rows: &Bound<'_, PyAny>,
        shape_id: Option<&Bound<'_, PyAny>>,
        extensions_by_u: Option<&Bound<'_, PyDict>>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let n = rows.len()?;
        if n == 0 {
            return from_val(py, &nhydb_core::vmap! {"records" => 0i64, "block_offset" => Val::None});
        }
        let lane = to_lane(lane)?;
        let realm = to_nat(realm_id)?;
        let us = to_nat(u_start)?;
        let sid = shape_id.filter(|x| !x.is_none()).map(|x| to_id(x, "shape_id")).transpose()?;
        let mut ext: Vec<(Nat, Vec<(Val, Val)>)> = Vec::new();
        if let Some(e) = extensions_by_u {
            for (k, v) in e.iter() {
                let items = ext_items(Some(&v))?;
                ext.push((to_nat(&k)?, items));
            }
        }
        let first = rows.get_item(0)?;
        let r = if first.is_instance_of::<PyDict>() {
            let mut dicts = Vec::with_capacity(n);
            for row in rows.try_iter()? {
                dicts.push(to_val(&row?)?);
            }
            let db = &mut self.db;
            py.detach(|| db.put_dense_batch(&realm, &us, &DenseRows::Dicts(&dicts), sid, &ext, lane, commit)).py()?
        } else {
            let width = self.dense_width(&realm, sid)?;
            let mut flat = Vec::with_capacity(n * width);
            for row in rows.try_iter()? {
                let row = row?;
                let before = flat.len();
                if let Ok(t) = row.cast::<PyTuple>() {
                    for x in t.iter() {
                        flat.push(to_val(&x)?);
                    }
                } else if let Ok(l) = row.cast::<PyList>() {
                    for x in l.iter() {
                        flat.push(to_val(&x)?);
                    }
                } else {
                    for x in row.try_iter()? {
                        flat.push(to_val(&x?)?);
                    }
                }
                if flat.len() - before != width {
                    let sid_show = sid.or_else(|| self.db.realms.get(&realm).and_then(|r| r.default_shape_id)).unwrap_or(0);
                    return Err(SchemaError::new_err(format!("shape {sid_show} expects {width} values")));
                }
            }
            let db = &mut self.db;
            py.detach(|| db.put_dense_batch(&realm, &us, &DenseRows::Flat { vals: &flat, width }, sid, &ext, lane, commit)).py()?
        };
        from_val(py, &r)
    }

    #[pyo3(signature = (realm_id, u, values, *, shape_id=None, extensions=None, lane=None, commit=false))]
    #[allow(clippy::too_many_arguments)]
    fn put(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        values: &Bound<'_, PyAny>,
        shape_id: Option<&Bound<'_, PyAny>>,
        extensions: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let sid = shape_id.filter(|x| !x.is_none()).map(|x| to_id(x, "shape_id")).transpose()?;
        let ext = match extensions {
            Some(e) if !e.is_none() => Some(ext_items(Some(e))?),
            _ => None,
        };
        let vals = match to_val(values)? {
            Val::Map(m) => Val::Map(m),
            Val::List(l) => Val::List(l),
            _ => return Err(PyTypeError::new_err("values must be a list/tuple or dict")),
        };
        let r = self.db.put(&to_nat(realm_id)?, &to_nat(u)?, vals, sid, ext, lane, commit).py()?;
        from_val(py, &r)
    }

    #[pyo3(signature = (realm_id, u, extensions, *, merge=true, lane=None, commit=false))]
    #[allow(clippy::too_many_arguments)]
    fn put_extensions(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        extensions: &Bound<'_, PyAny>,
        merge: bool,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let items = ext_items(Some(extensions))?;
        from_val(py, &self.db.put_extensions(&to_nat(realm_id)?, &to_nat(u)?, &items, merge, lane, commit).py()?)
    }

    #[pyo3(signature = (realm_id, u, vs, values=None, *, merge=true, lane=None, commit=false))]
    #[allow(clippy::too_many_arguments)]
    fn put_extension_list(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        vs: &Bound<'_, PyAny>,
        values: Option<&Bound<'_, PyAny>>,
        merge: bool,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let items = match values.filter(|v| !v.is_none()) {
            None => ext_items(Some(&PyList::new(py, vs.try_iter()?.collect::<PyResult<Vec<_>>>()?)?.into_any()))?,
            Some(vals) => {
                let a: Vec<Bound<'_, PyAny>> = vs.try_iter()?.collect::<PyResult<_>>()?;
                let b: Vec<Bound<'_, PyAny>> = vals.try_iter()?.collect::<PyResult<_>>()?;
                if a.len() != b.len() {
                    return Err(PyValueError::new_err("vs and values must have the same length"));
                }
                a.iter().zip(b.iter()).map(|(k, v)| Ok((to_val(k)?, to_val(v)?))).collect::<PyResult<_>>()?
            }
        };
        from_val(py, &self.db.put_extensions(&to_nat(realm_id)?, &to_nat(u)?, &items, merge, lane, commit).py()?)
    }

    #[pyo3(signature = (realm_id, u, start_v, values, *, merge=true, lane=None, commit=false))]
    #[allow(clippy::too_many_arguments)]
    fn put_extension_range(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        start_v: &Bound<'_, PyAny>,
        values: &Bound<'_, PyAny>,
        merge: bool,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let realm = to_nat(realm_id)?;
        let sv = to_nat(start_v)?;
        address::validate_local_v(&sv, Some(&realm)).py()?;
        let mut items = Vec::new();
        for (i, v) in values.try_iter()?.enumerate() {
            items.push((Val::nat(&sv.add_u64(i as u64)), to_val(&v?)?));
        }
        if let Some((last, _)) = items.last() {
            address::validate_local_v(&last.as_nat().unwrap(), Some(&realm)).py()?;
        }
        let lane = to_lane(lane)?;
        from_val(py, &self.db.put_extensions(&realm, &to_nat(u)?, &items, merge, lane, commit).py()?)
    }

    #[pyo3(signature = (realm_id, u, *, lane=None, commit=false))]
    fn delete(&mut self, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>, commit: bool) -> PyResult<bool> {
        let lane = to_lane(lane)?;
        self.db.delete(&to_nat(realm_id)?, &to_nat(u)?, lane, commit).py()
    }

    #[pyo3(signature = (layout_id, object_id, values, *, lane=None, commit=false))]
    fn put_object(
        &mut self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        object_id: &Bound<'_, PyAny>,
        values: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let r = self.rlookup(layout_id)?;
        let lane = to_lane(lane)?;
        let vals = to_val(values)?;
        from_val(py, &self.db.put_object(&r, &to_nat(object_id)?, &vals, lane, commit).py()?)
    }

    #[pyo3(signature = (layout_id, objects, *, lane=None, commit=false, u_window=10_000))]
    fn put_objects_batch(
        &mut self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        objects: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
        u_window: i64,
    ) -> PyResult<Py<PyAny>> {
        let r = self.rlookup(layout_id)?;
        let lane = to_lane(lane)?;
        let mut items = Vec::new();
        for item in objects.try_iter()? {
            let item = item?;
            let (oid, vals) = match item.cast::<PyDict>() {
                Ok(d) if d.contains("values")? => {
                    let oid = match d.get_item("object_id")? {
                        Some(o) => o,
                        None => d.get_item("object")?.ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("object_id"))?,
                    };
                    (oid, d.get_item("values")?.unwrap())
                }
                _ => {
                    let it: Vec<Bound<'_, PyAny>> = item.try_iter()?.collect::<PyResult<_>>()?;
                    if it.len() != 2 {
                        return Err(PyValueError::new_err("object expects (object_id, values)"));
                    }
                    (it[0].clone(), it[1].clone())
                }
            };
            items.push((to_nat(&oid)?, to_val(&vals)?));
        }
        let db = &mut self.db;
        let res = py.detach(|| db.put_objects_batch(&r, items, lane, commit, u_window.max(1) as u64));
        from_val(py, &res.py()?)
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    #[pyo3(signature = (realm_id, u, *, lane=None))]
    fn get_internal(&self, py: Python<'_>, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        match self.db.get_internal(&to_nat(realm_id)?, &to_nat(u)?, lane).py()? {
            None => Ok(py.None()),
            Some(r) => record_dict(py, &r),
        }
    }

    #[pyo3(signature = (realm_id, u, *, lane=None, dyn_address=None, dyn_v=None, include_extensions=true))]
    #[allow(clippy::too_many_arguments)]
    fn get(
        &self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        dyn_address: Option<String>,
        dyn_v: Option<&Bound<'_, PyAny>>,
        include_extensions: bool,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let dv = opt_nat(dyn_v)?;
        opt_from_val(py, self.db.get(&to_nat(realm_id)?, &to_nat(u)?, lane, dyn_address.as_deref(), dv.as_ref(), include_extensions).py()?)
    }

    #[pyo3(signature = (realm_id, u, v, *, lane=None, dyn_address=true))]
    fn get_cell(
        &self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        v: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        dyn_address: bool,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        opt_from_val(py, self.db.get_cell(&to_nat(realm_id)?, &to_nat(u)?, &to_val(v)?, lane, dyn_address).py()?)
    }

    #[pyo3(signature = (layout_id, object_id, *, lane=None, include_addresses=false))]
    fn get_object(
        &self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        object_id: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        include_addresses: bool,
    ) -> PyResult<Py<PyAny>> {
        let r = self.rlookup(layout_id)?;
        let lane = to_lane(lane)?;
        opt_from_val(py, self.db.get_object(&r, &to_nat(object_id)?, lane, include_addresses).py()?)
    }

    #[pyo3(signature = (layout_id, object_id, *, lane=None, request_n=None))]
    fn get_record_view(
        &self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        object_id: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        request_n: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let r = self.rlookup(layout_id)?;
        let lane = to_lane(lane)?;
        let rn = request_n.filter(|x| !x.is_none()).map(to_n).transpose()?;
        from_val(py, &self.db.get_record_view(&r, &to_nat(object_id)?, lane, rn.as_ref()).py()?)
    }

    #[pyo3(signature = (value, *, mode="auto"))]
    fn get_n(&self, py: Python<'_>, value: &Bound<'_, PyAny>, mode: &str) -> PyResult<Py<PyAny>> {
        let n = to_n(value)?;
        let m = GetMode::parse(mode).py()?;
        from_val(py, &self.db.get_n(&n, m).py()?)
    }

    fn get_n_cell(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        self.get_n(py, value, "cell")
    }

    fn get_n_record(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        self.get_n(py, value, "record")
    }

    #[pyo3(signature = (realm_id, u, v, *, lane=None, mode="auto"))]
    fn get_address(
        &self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        v: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        mode: &str,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let m = GetMode::parse(mode).py()?;
        from_val(py, &self.db.get_address(&to_nat(realm_id)?, &to_nat(u)?, &to_nat(v)?, lane, m).py()?)
    }

    fn describe_n(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.describe_n(&to_n(value)?).py()?)
    }

    #[pyo3(signature = (layout_id, object_id, *, lane=None))]
    fn occupied_n(&self, layout_id: &Bound<'_, PyAny>, object_id: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<String>> {
        let r = self.rlookup(layout_id)?;
        let lane = to_lane(lane)?;
        Ok(self.db.occupied_n(&r, &to_nat(object_id)?, lane).py()?.iter().map(|n| format!("N{n}")).collect())
    }

    #[pyo3(signature = (value, *, output="N"))]
    fn occupied_from_n(&self, py: Python<'_>, value: &Bound<'_, PyAny>, output: &str) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.occupied_from_n(&to_n(value)?, output).py()?)
    }

    #[pyo3(signature = (realm_id, u_from, u_to, *, lane=None, dyn_address=None, dyn_v=None, include_extensions=true))]
    #[allow(clippy::too_many_arguments)]
    fn range(
        slf: &Bound<'_, Self>,
        realm_id: &Bound<'_, PyAny>,
        u_from: &Bound<'_, PyAny>,
        u_to: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        dyn_address: Option<String>,
        dyn_v: Option<&Bound<'_, PyAny>>,
        include_extensions: bool,
    ) -> PyResult<iter::ResultIterator> {
        let lane = to_lane(lane)?;
        let mode = nhydb_core::db::cursor::RangeMode::Get { dyn_mode: dyn_address, dyn_v: opt_nat(dyn_v)?, include_extensions };
        Ok(iter::ResultIterator::new(
            slf.clone().unbind(),
            iter::Pending::Range { realm: to_nat(realm_id)?, lane, from: to_nat(u_from)?, to: to_nat(u_to)?, mode },
        ))
    }

    #[pyo3(signature = (realm_id, u_from, u_to, columns, *, lane=None))]
    fn select(
        slf: &Bound<'_, Self>,
        realm_id: &Bound<'_, PyAny>,
        u_from: &Bound<'_, PyAny>,
        u_to: &Bound<'_, PyAny>,
        columns: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<iter::ResultIterator> {
        let lane = to_lane(lane)?;
        let cols: Vec<Val> = columns.try_iter()?.map(|c| to_val(&c?)).collect::<PyResult<_>>()?;
        let mode = nhydb_core::db::cursor::RangeMode::Select { columns: cols };
        Ok(iter::ResultIterator::new(
            slf.clone().unbind(),
            iter::Pending::Range { realm: to_nat(realm_id)?, lane, from: to_nat(u_from)?, to: to_nat(u_to)?, mode },
        ))
    }

    // ------------------------------------------------------------------
    // Indexes
    // ------------------------------------------------------------------

    #[pyo3(signature = (index_ids=None, *, commit=false))]
    fn build_indexes(&mut self, py: Python<'_>, index_ids: Option<&Bound<'_, PyAny>>, commit: bool) -> PyResult<Py<PyAny>> {
        let ids: Option<Vec<u64>> = match index_ids.filter(|x| !x.is_none()) {
            None => None,
            Some(x) => Some(x.try_iter()?.map(|i| to_id(&i?, "index_id")).collect::<PyResult<_>>()?),
        };
        let db = &mut self.db;
        let r = py.detach(|| db.build_indexes(ids.as_deref(), commit)).py()?;
        from_val(py, &r)
    }

    #[pyo3(signature = (realm_id, filters, *, lane=None, u_from=None, u_to=None))]
    fn find_u(
        &self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        filters: &Bound<'_, PyDict>,
        lane: Option<&Bound<'_, PyAny>>,
        u_from: Option<&Bound<'_, PyAny>>,
        u_to: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let f = filters_from_py(filters)?;
        let us = self.db.find_u(&to_nat(realm_id)?, &f, lane, opt_nat(u_from)?.as_ref(), opt_nat(u_to)?.as_ref()).py()?;
        let l = PyList::empty(py);
        for u in us {
            l.append(nat_py(py, &u)?)?;
        }
        Ok(l.into_any().unbind())
    }

    #[pyo3(signature = (realm_id, filters, *, lane=None, u_from=None, u_to=None, dyn_address=None, dyn_v=None))]
    #[allow(clippy::too_many_arguments)]
    fn find(
        slf: &Bound<'_, Self>,
        realm_id: &Bound<'_, PyAny>,
        filters: &Bound<'_, PyDict>,
        lane: Option<&Bound<'_, PyAny>>,
        u_from: Option<&Bound<'_, PyAny>>,
        u_to: Option<&Bound<'_, PyAny>>,
        dyn_address: Option<String>,
        dyn_v: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<iter::ResultIterator> {
        let lane = to_lane(lane)?;
        let filters = filters_from_py(filters)?;
        Ok(iter::ResultIterator::new(
            slf.clone().unbind(),
            iter::Pending::Find {
                realm: to_nat(realm_id)?,
                lane,
                filters,
                u_from: opt_nat(u_from)?,
                u_to: opt_nat(u_to)?,
                dyn_mode: dyn_address,
                dyn_v: opt_nat(dyn_v)?,
            },
        ))
    }

    // ------------------------------------------------------------------
    // Maintenance
    // ------------------------------------------------------------------

    fn checkpoint(&mut self) -> PyResult<u64> {
        self.db.checkpoint().py()
    }

    #[pyo3(signature = (*, deep=false))]
    fn verify(&self, py: Python<'_>, deep: bool) -> PyResult<Py<PyAny>> {
        let db = &self.db;
        let r = py.detach(|| db.verify(deep)).py()?;
        from_val(py, &r)
    }

    #[pyo3(signature = (*, deep=false))]
    fn health(&self, py: Python<'_>, deep: bool) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.health(deep).py()?)
    }

    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.db.stats().py()?)
    }

    #[staticmethod]
    #[pyo3(signature = (path, *, output=None, make_backup=true, rebuild_indexes=true, batch_records=10_000, layout_path=None, key_path=None, config=None, durability=None, lock=None, lock_timeout_ms=None))]
    #[allow(clippy::too_many_arguments)]
    fn compact_file(
        py: Python<'_>,
        path: PathBuf,
        output: Option<PathBuf>,
        make_backup: bool,
        rebuild_indexes: bool,
        batch_records: usize,
        layout_path: Option<PathBuf>,
        key_path: Option<PathBuf>,
        config: Option<PathBuf>,
        durability: Option<&str>,
        lock: Option<bool>,
        lock_timeout_ms: Option<u64>,
    ) -> PyResult<Py<PyAny>> {
        let open = open_opts(true, durability, layout_path, key_path, config, lock, lock_timeout_ms, Default::default())?;
        let opts = CompactOptions { output, make_backup, rebuild_indexes, batch_records, open };
        let r = py.detach(|| Db::compact_file(&path, &opts)).py()?;
        from_val(py, &r)
    }

    #[staticmethod]
    fn inspect_tail(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
        from_val(py, &Db::inspect_tail(&path).py()?)
    }

    #[staticmethod]
    #[pyo3(signature = (path, *, apply=false, make_backup=true, rebuild_indexes=true, layout_path=None, key_path=None, config=None, durability=None, lock=None, lock_timeout_ms=None))]
    #[allow(clippy::too_many_arguments)]
    fn repair_file(
        py: Python<'_>,
        path: PathBuf,
        apply: bool,
        make_backup: bool,
        rebuild_indexes: bool,
        layout_path: Option<PathBuf>,
        key_path: Option<PathBuf>,
        config: Option<PathBuf>,
        durability: Option<&str>,
        lock: Option<bool>,
        lock_timeout_ms: Option<u64>,
    ) -> PyResult<Py<PyAny>> {
        let open = open_opts(true, durability, layout_path, key_path, config, lock, lock_timeout_ms, Default::default())?;
        let r = py.detach(|| Db::repair_file_with(&path, apply, make_backup, rebuild_indexes, &open)).py()?;
        from_val(py, &r)
    }

    // ------------------------------------------------------------------
    // KISS single-item API
    // ------------------------------------------------------------------

    /// Writes a single cell `(R,u,v,lane)`; `commit=True` makes it
    /// durable before returning.
    #[pyo3(signature = (realm_id, u, v, value, *, r#type=None, lane=None, commit=false))]
    #[allow(clippy::too_many_arguments)]
    fn put_cell(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        v: &Bound<'_, PyAny>,
        value: &Bound<'_, PyAny>,
        r#type: Option<String>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let typ = r#type.map(|t| type_code(&t)).transpose()?;
        let cell = CellInput::typed(to_nat(u)?, to_nat(v)?, to_val(value)?, typ);
        let realm = to_nat(realm_id)?;
        from_val(py, &self.db.put_points(&realm, vec![cell], lane, commit, 10_000).py()?)
    }

    /// Value of a cell (None if not occupied).
    #[pyo3(signature = (realm_id, u, v, *, lane=None))]
    fn get_value(&self, py: Python<'_>, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, v: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        opt_from_val(py, self.db.get_cell_value(&to_nat(realm_id)?, &to_nat(u)?, &to_nat(v)?, lane).py()?)
    }

    #[pyo3(signature = (realm_id, u, v, *, lane=None, commit=false))]
    fn delete_cell(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        v: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        self.delete_point(py, realm_id, u, v, lane, commit)
    }

    /// Writes a logical record via a layout.
    #[pyo3(signature = (layout_id, object_id, values, *, lane=None, commit=false))]
    fn put_record(
        &mut self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        object_id: &Bound<'_, PyAny>,
        values: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        self.put_object(py, layout_id, object_id, values, lane, commit)
    }

    #[pyo3(signature = (layout_id, object_id, *, lane=None))]
    fn get_record(&self, py: Python<'_>, layout_id: &Bound<'_, PyAny>, object_id: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        self.get_object(py, layout_id, object_id, lane, false)
    }

    /// Deletes a logical record (CLASSIC: BASE record + sparse cells;
    /// otherwise all occupied cells via tombstone). Returns deleted cells/records.
    #[pyo3(signature = (layout_id, object_id, *, lane=None, commit=false))]
    fn delete_record(&mut self, layout_id: &Bound<'_, PyAny>, object_id: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>, commit: bool) -> PyResult<u64> {
        let r = self.rlookup(layout_id)?;
        let lane = to_lane(lane)?;
        self.db.delete_object(&r, &to_nat(object_id)?, lane, commit).py()
    }

    // ------------------------------------------------------------------
    // NumPy batch API
    // ------------------------------------------------------------------

    /// Writes `len` contiguous BASE records starting at `u_start` from columns
    /// (dict field name→array or sequence in shape order). The entire
    /// encode and write loop runs in Rust without the GIL.
    #[pyo3(signature = (realm_id, u_start, columns, *, shape_id=None, lane=None, commit=false))]
    #[allow(clippy::too_many_arguments)]
    fn put_numpy_batch(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u_start: &Bound<'_, PyAny>,
        columns: &Bound<'_, PyAny>,
        shape_id: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let realm = to_nat(realm_id)?;
        let us = to_nat(u_start)?;
        let sid = shape_id.filter(|x| !x.is_none()).map(|x| to_id(x, "shape_id")).transpose()?;
        let r = self.db.realms.get(&realm).ok_or_else(|| SchemaError::new_err("realm missing"))?;
        let sid_eff = sid.or(r.default_shape_id).ok_or_else(|| SchemaError::new_err("no shape given/default"))?;
        let shape = match self.db.shapes.get(&sid_eff) {
            Some(s) if s.realm_id == realm => s.clone(),
            _ => return Err(SchemaError::new_err("shape missing/mismatched")),
        };
        let owned: Vec<numpy_io::OwnedCol> = if let Ok(d) = columns.cast::<PyDict>() {
            let mut v = Vec::with_capacity(shape.fields.len());
            for f in &shape.fields {
                let c = d.get_item(&f.name)?.ok_or_else(|| SchemaError::new_err(format!("required field '{}' missing", f.name)))?;
                v.push(numpy_io::col_from_py(&c)?);
            }
            v
        } else {
            columns.try_iter()?.map(|c| numpy_io::col_from_py(&c?)).collect::<PyResult<_>>()?
        };
        let count = owned.first().map(|c| c.len()).unwrap_or(0);
        if count == 0 {
            return from_val(py, &nhydb_core::vmap! {"records" => 0i64, "block_offset" => Val::None});
        }
        let cols: Vec<nhydb_core::ColData> = owned.iter().map(|c| c.as_col()).collect();
        let db = &mut self.db;
        let res = py.detach(|| db.put_dense_batch(&realm, &us, &DenseRows::Columns { cols: &cols, count }, Some(sid_eff), &[], lane, commit));
        from_val(py, &res.py()?)
    }

    /// Writes sparse cells from arrays: `u`, `v` (array or scalar),
    /// `values` (array/list). `type` forces a self-typed data type.
    #[pyo3(signature = (realm_id, u, v, values, *, r#type=None, lane=None, commit=false, u_window=10_000))]
    #[allow(clippy::too_many_arguments)]
    fn put_cells_numpy(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        v: &Bound<'_, PyAny>,
        values: &Bound<'_, PyAny>,
        r#type: Option<String>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
        u_window: i64,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let realm = to_nat(realm_id)?;
        if !self.db.has_realm(&realm) {
            return Err(SchemaError::new_err("realm missing"));
        }
        let typ = r#type.map(|t| type_code(&t)).transpose()?;
        let vals = numpy_io::col_from_py(values)?;
        let n = vals.len();
        let us = numpy_io::nats_from_py(u, Some(n), "u")?;
        let vs = numpy_io::nats_from_py(v, Some(n), "v")?;
        if us.len() != n || vs.len() != n {
            return Err(PyValueError::new_err("u, v and values must have the same length"));
        }
        let db = &mut self.db;
        let res = py.detach(move || {
            let col = vals.as_col();
            let cells: Vec<CellInput> = us
                .into_iter()
                .zip(vs)
                .enumerate()
                .map(|(i, (u, v))| CellInput::typed(u, v, col.get_val(i), typ))
                .collect();
            db.put_points(&realm, cells, lane, commit, u_window.max(1) as u64)
        });
        from_val(py, &res.py()?)
    }

    /// Writes objects column-wise via any layout.
    /// `object_ids`: increasing integer array (or start value int + column length).
    #[pyo3(signature = (layout_id, object_ids, columns, *, lane=None, commit=false, u_window=10_000))]
    #[allow(clippy::too_many_arguments)]
    fn put_objects_numpy(
        &mut self,
        py: Python<'_>,
        layout_id: &Bound<'_, PyAny>,
        object_ids: &Bound<'_, PyAny>,
        columns: &Bound<'_, PyDict>,
        lane: Option<&Bound<'_, PyAny>>,
        commit: bool,
        u_window: i64,
    ) -> PyResult<Py<PyAny>> {
        let r = self.rlookup(layout_id)?;
        let lane = to_lane(lane)?;
        let mut owned: Vec<(String, numpy_io::OwnedCol)> = Vec::new();
        for (k, v) in columns.iter() {
            owned.push((k.str()?.to_str()?.to_owned(), numpy_io::col_from_py(&v)?));
        }
        let n = owned.first().map(|(_, c)| c.len()).unwrap_or(0);
        let oids: Vec<Nat> = if object_ids.is_instance_of::<pyo3::types::PyInt>() {
            let start = to_nat(object_ids)?;
            (0..n as u64).map(|i| start.add_u64(i)).collect()
        } else {
            numpy_io::nats_from_py(object_ids, None, "object_ids")?
        };
        let db = &mut self.db;
        let res = py.detach(|| {
            let cols: Vec<(String, nhydb_core::ColData)> = owned.iter().map(|(k, c)| (k.clone(), c.as_col())).collect();
            db.put_object_columns(&r, &oids, &cols, lane, commit, u_window.max(1) as u64)
        });
        from_val(py, &res.py()?)
    }

    /// Reads `count` CLASSIC records starting at `u_start` as dict field name → NumPy array
    /// plus `"_present"` (bool array). Missing values: 0, NaN or None respectively.
    #[pyo3(signature = (realm_id, u_start, count, fields=None, *, lane=None))]
    fn read_numpy(
        &self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u_start: &Bound<'_, PyAny>,
        count: usize,
        fields: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let lane = to_lane(lane)?;
        let fs: Option<Vec<Val>> = match fields.filter(|f| !f.is_none()) {
            None => None,
            Some(f) => Some(f.try_iter()?.map(|x| to_val(&x?)).collect::<PyResult<_>>()?),
        };
        let realm = to_nat(realm_id)?;
        let us = to_nat(u_start)?;
        let db = &self.db;
        let out = py.detach(|| db.read_columns(&realm, &us, count, fs.as_deref(), lane)).py()?;
        let d = PyDict::new(py);
        for (name, col) in out.names.iter().zip(out.cols.iter()) {
            d.set_item(name, numpy_io::out_to_numpy(py, col)?)?;
        }
        d.set_item("_present", numpy_io::out_to_numpy(py, &nhydb_core::db::columns::OutCol::Bool(out.present))?)?;
        Ok(d.into_any().unbind())
    }
}

impl NHyDB {
    fn dense_width(&self, realm: &Nat, sid: Option<u64>) -> PyResult<usize> {
        let r = self.db.realms.get(realm).ok_or_else(|| SchemaError::new_err("realm missing"))?;
        let sid = sid.or(r.default_shape_id).ok_or_else(|| SchemaError::new_err("no shape given/default"))?;
        match self.db.shapes.get(&sid) {
            Some(s) if &s.realm_id == realm => Ok(s.fields.len()),
            _ => Err(SchemaError::new_err("shape missing/mismatched")),
        }
    }
}

// u_end sentinel (`_UNSET`): not passed ≠ None (= unbounded).
pub struct UEnd(Option<Py<PyAny>>);

fn unset_marker() -> UEnd {
    UEnd(None)
}

impl<'a, 'py> FromPyObject<'a, 'py> for UEnd {
    type Error = PyErr;
    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        Ok(UEnd(Some(obj.to_owned().unbind())))
    }
}

/// None (explicit) → unbounded; not passed → unchanged.
fn ue_spec(py: Python<'_>, u_end: Option<UEnd>) -> PyResult<Option<Option<Nat>>> {
    match u_end {
        None => Ok(Some(None)),
        Some(UEnd(None)) => Ok(None),
        Some(UEnd(Some(o))) => Ok(Some(Some(to_nat(o.bind(py))?))),
    }
}

// ----------------------------------------------------------------------
// Module functions (address mathematics, codec)
// ----------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (lane=None))]
fn normalize_lane(lane: Option<&Bound<'_, PyAny>>) -> PyResult<u8> {
    to_lane(lane)
}

#[pyfunction]
#[pyo3(signature = (lane=None))]
fn lane_name(lane: Option<&Bound<'_, PyAny>>) -> PyResult<&'static str> {
    Ok(address::lane_name(to_lane(lane)?))
}

#[pyfunction]
#[pyo3(signature = (v, *, realm_id=None))]
fn validate_local_v(py: Python<'_>, v: &Bound<'_, PyAny>, realm_id: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
    let v = to_nat(v)?;
    address::validate_local_v(&v, opt_nat(realm_id)?.as_ref()).py()?;
    nat_py(py, &v)
}

#[pyfunction]
fn realm_v_bounds(py: Python<'_>, realm_id: &Bound<'_, PyAny>) -> PyResult<(Py<PyAny>, Py<PyAny>)> {
    let (a, b) = address::realm_v_bounds(&to_nat(realm_id)?).py()?;
    Ok((nat_py(py, &a)?, nat_py(py, &b)?))
}

#[pyfunction]
fn global_v(py: Python<'_>, realm_id: &Bound<'_, PyAny>, local_v: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    nat_py(py, &address::global_v(&to_nat(realm_id)?, &to_nat(local_v)?).py()?)
}

#[pyfunction]
fn split_global_v(py: Python<'_>, v_global: &Bound<'_, PyAny>) -> PyResult<(Py<PyAny>, Py<PyAny>)> {
    let (a, b) = address::split_global_v(&to_nat(v_global)?).py()?;
    Ok((nat_py(py, &a)?, nat_py(py, &b)?))
}

#[pyfunction]
fn dynadron_address(py: Python<'_>, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, local_v: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    from_val(py, &address::dynadron_address(&to_nat(realm_id)?, &to_nat(u)?, &to_nat(local_v)?).py()?.to_val())
}

#[pyfunction]
fn parse_n_reference(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    nat_py(py, &to_n(value)?)
}

#[pyfunction]
#[pyo3(signature = (realm_id, u, local_v, lane=None))]
fn dynadron_n(py: Python<'_>, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, local_v: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
    let (r, u, v) = (to_nat(realm_id)?, to_nat(u)?, to_nat(local_v)?);
    let lane = to_lane(lane)?;
    nat_py(py, &address::dynadron_n(&r, &u, &v, lane).py()?)
}

#[pyfunction]
fn n_to_dynadron_address(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    from_val(py, &address::n_to_address(&to_n(value)?).py()?.to_n_val())
}

#[pyfunction]
#[pyo3(signature = (realm_id, u, local_v, lane=None))]
fn cell_address(py: Python<'_>, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, local_v: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
    let lane = to_lane(lane)?;
    from_val(py, &address::cell_address(&to_nat(realm_id)?, &to_nat(u)?, &to_nat(local_v)?, lane).py()?.to_val())
}

#[pyfunction]
fn enc_vu<'py>(py: Python<'py>, n: &Bound<'py, PyAny>) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
    let b: num_bigint::BigInt = n.extract()?;
    if b.sign() == num_bigint::Sign::Minus {
        return Err(PyValueError::new_err("varuint expects n>=0"));
    }
    let mut out = Vec::new();
    Nat::from_bigint(&b).unwrap().put_vu(&mut out);
    Ok(pyo3::types::PyBytes::new(py, &out))
}

#[pyfunction]
#[pyo3(signature = (buf, pos=0))]
fn dec_vu(py: Python<'_>, buf: &[u8], pos: usize) -> PyResult<(Py<PyAny>, usize)> {
    let mut p = pos;
    let n = nhydb_core::nat::get_vu(buf, &mut p).py()?;
    Ok((nat_py(py, &n)?, p))
}

#[pyfunction]
fn enc_vi<'py>(py: Python<'py>, n: &Bound<'py, PyAny>) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
    let b: num_bigint::BigInt = n.extract()?;
    let mut out = Vec::new();
    nhydb_core::nat::put_vi_big(&mut out, &b);
    Ok(pyo3::types::PyBytes::new(py, &out))
}

#[pyfunction]
#[pyo3(signature = (buf, pos=0))]
fn dec_vi(py: Python<'_>, buf: &[u8], pos: usize) -> PyResult<(Py<PyAny>, usize)> {
    let mut p = pos;
    let z = nhydb_core::nat::get_vu(buf, &mut p).py()?;
    Ok((nhydb_core::nat::zigzag_decode(&z).into_py_any(py)?, p))
}

/// Persisted index definition (`IndexDef`).
#[pyclass(name = "IndexDef", module = "nhydb", frozen)]
pub struct PyIndexDef {
    def: nhydb_core::schema::IndexDef,
}

impl From<&nhydb_core::schema::IndexDef> for PyIndexDef {
    fn from(d: &nhydb_core::schema::IndexDef) -> Self {
        PyIndexDef { def: d.clone() }
    }
}

#[pymethods]
impl PyIndexDef {
    #[getter]
    fn index_id(&self) -> u64 {
        self.def.index_id
    }
    #[getter]
    fn realm_id(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        nat_py(py, &self.def.realm_id)
    }
    #[getter]
    fn name(&self) -> String {
        self.def.name.clone()
    }
    #[getter]
    fn v(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        nat_py(py, &self.def.v)
    }
    #[getter]
    fn description(&self) -> String {
        self.def.description.clone()
    }
    #[getter]
    fn kind(&self) -> &'static str {
        self.def.kind.name()
    }
    #[getter]
    fn source_v(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.def.source_v {
            Some(n) => nat_py(py, n),
            None => Ok(py.None()),
        }
    }
    #[getter]
    fn field_v(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        nat_py(py, self.def.field_v())
    }
    #[getter]
    fn options(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.def.options)
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.def.to_val())
    }
    fn __repr__(&self) -> String {
        format!(
            "IndexDef(index_id={}, realm_id={}, name={}, v={}, description={}, kind={}, source_v={}, options={})",
            self.def.index_id,
            self.def.realm_id,
            Val::str(self.def.name.as_str()).py_repr(),
            self.def.v,
            Val::str(self.def.description.as_str()).py_repr(),
            Val::str(self.def.kind.name()).py_repr(),
            self.def.source_v.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "None".into()),
            self.def.options.py_repr()
        )
    }
}

/// Minimal read-only substitute for the Python file object `db.f`.
#[pyclass(module = "nhydb")]
pub struct FileView {
    len: u64,
    pos: Arc<std::sync::atomic::AtomicU64>,
}

#[pymethods]
impl FileView {
    #[pyo3(signature = (offset, whence=0))]
    fn seek(&self, offset: i64, whence: i32) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        let base = match whence {
            2 => self.len as i64,
            1 => self.pos.load(Relaxed) as i64,
            _ => 0,
        };
        let p = (base + offset).max(0) as u64;
        self.pos.store(p, Relaxed);
        p
    }
    fn tell(&self) -> u64 {
        self.pos.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_class::<NHyDB>()?;
    m.add_class::<PyLayoutDef>()?;
    m.add_class::<PyIndexDef>()?;
    m.add_class::<iter::ResultIterator>()?;
    m.add("NHyDBError", py.get_type::<NHyDBError>())?;
    m.add("CorruptionError", py.get_type::<CorruptionError>())?;
    m.add("SchemaError", py.get_type::<SchemaError>())?;
    m.add("StaleIndexError", py.get_type::<StaleIndexError>())?;
    m.add("RealmLimitError", py.get_type::<RealmLimitError>())?;
    m.add("AddressError", py.get_type::<AddressError>())?;
    m.add("DatabaseLockedError", py.get_type::<DatabaseLockedError>())?;
    m.add("DatabaseSizeLimitError", py.get_type::<DatabaseSizeLimitError>())?;
    m.add("IndexConfigError", py.get_type::<IndexConfigError>())?;
    m.add("MathLayoutError", py.get_type::<MathLayoutError>())?;
    m.add("MathLayoutNotFound", py.get_type::<MathLayoutNotFound>())?;
    m.add("MappingFrozenError", py.get_type::<MappingFrozenError>())?;
    m.add("MathCellConflictError", py.get_type::<MathCellConflictError>())?;
    m.add("MathError", py.get_type::<MathError>())?;
    m.add("MathSyntaxError", py.get_type::<MathSyntaxError>())?;
    m.add("MathDomainError", py.get_type::<MathDomainError>())?;
    m.add("LayoutError", py.get_type::<LayoutError>())?;
    m.add("LayoutStateError", py.get_type::<LayoutStateError>())?;
    m.add("RealmBudgetExceeded", py.get_type::<RealmBudgetExceeded>())?;
    m.add("StalePlanError", py.get_type::<StalePlanError>())?;
    m.add("AllocationConflictError", py.get_type::<AllocationConflictError>())?;
    m.add("QueryError", py.get_type::<QueryError>())?;
    m.add("QueryPlanError", py.get_type::<QueryPlanError>())?;
    m.add("QueryDomainError", py.get_type::<QueryDomainError>())?;
    m.add("SQLSyntaxError", py.get_type::<SQLSyntaxError>())?;
    m.add("REALM_SPAN", nat_py(py, address::realm_span())?)?;
    m.add("ENGINE_VERSION", nhydb_core::format::ENGINE_VERSION)?;
    m.add("FILE_FORMAT_VERSION", nhydb_core::format::FILE_FORMAT_VERSION)?;
    m.add("TYPE_CODES", {
        let d = PyDict::new(py);
        for t in nhydb_core::codec::ALL_TYPES {
            d.set_item(t.name(), t.code())?;
        }
        d
    })?;
    m.add_function(wrap_pyfunction!(normalize_lane, m)?)?;
    m.add_function(wrap_pyfunction!(lane_name, m)?)?;
    m.add_function(wrap_pyfunction!(validate_local_v, m)?)?;
    m.add_function(wrap_pyfunction!(realm_v_bounds, m)?)?;
    m.add_function(wrap_pyfunction!(global_v, m)?)?;
    m.add_function(wrap_pyfunction!(split_global_v, m)?)?;
    m.add_function(wrap_pyfunction!(dynadron_address, m)?)?;
    m.add_function(wrap_pyfunction!(parse_n_reference, m)?)?;
    m.add_function(wrap_pyfunction!(dynadron_n, m)?)?;
    m.add_function(wrap_pyfunction!(n_to_dynadron_address, m)?)?;
    m.add_function(wrap_pyfunction!(cell_address, m)?)?;
    m.add_function(wrap_pyfunction!(enc_vu, m)?)?;
    m.add_function(wrap_pyfunction!(dec_vu, m)?)?;
    m.add_function(wrap_pyfunction!(enc_vi, m)?)?;
    m.add_function(wrap_pyfunction!(dec_vi, m)?)?;
    numpy_io::register(m)?;
    kernel::register(m)?;
    textapi::register(m)?;
    mathapi::register(m)?;
    queryapi::register(m)?;
    Ok(())
}
