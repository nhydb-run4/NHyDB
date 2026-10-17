//! Bridge: Python objects as CUSTOM layout plugins (isolated special path).

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyWeakrefMethods, PyWeakrefReference};
use pyo3::IntoPyObjectExt;

use nhydb_core::plugin::{LayoutPlugin, PluginMethod};
use nhydb_core::schema::LayoutDef;
use nhydb_core::{Db, Lane, Nat, Result, Val};

use crate::conv::{from_pyerr, from_val, nat_py, to_val};

/// Python view of a layout (`LayoutDef`, read-only).
#[pyclass(name = "LayoutDef", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyLayoutDef {
    pub inner: LayoutDef,
}

#[pymethods]
impl PyLayoutDef {
    #[getter]
    fn layout_id(&self) -> u64 {
        self.inner.layout_id
    }
    #[getter]
    fn realm_id(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        nat_py(py, &self.inner.realm_id)
    }
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }
    #[getter]
    fn kind(&self) -> &'static str {
        self.inner.kind.name()
    }
    #[getter]
    fn u_start(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        nat_py(py, &self.inner.u_start)
    }
    #[getter]
    fn u_end(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.inner.u_end {
            Some(e) => nat_py(py, e),
            None => Ok(py.None()),
        }
    }
    #[getter]
    fn priority(&self) -> i64 {
        self.inner.priority
    }
    #[getter]
    fn shape_id(&self) -> Option<u64> {
        self.inner.shape_id
    }
    #[getter]
    fn base_reserve_to(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        nat_py(py, &self.inner.base_reserve_to)
    }
    #[getter]
    fn params(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.params)
    }
    #[getter]
    fn spacemap(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.spacemap)
    }
    #[getter]
    fn plugin(&self) -> String {
        self.inner.plugin.clone()
    }
    #[getter]
    fn description(&self) -> String {
        self.inner.description.clone()
    }
    fn contains_u(&self, u: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self.inner.contains_u(&crate::conv::to_nat(u)?))
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    fn __repr__(&self) -> String {
        format!(
            "LayoutDef(layout_id={}, realm_id={}, name={:?}, kind='{}', u_start={}, u_end={}, priority={})",
            self.inner.layout_id,
            self.inner.realm_id,
            self.inner.name,
            self.inner.kind.name(),
            self.inner.u_start,
            self.inner.u_end.as_ref().map(|e| e.to_string()).unwrap_or("None".into()),
            self.inner.priority
        )
    }
}

pub struct PyPlugin {
    pub obj: Py<PyAny>,
    /// Weak reference to the Python `NHyDB` object (for `occupied_addresses`).
    pub db: Option<Py<PyWeakrefReference>>,
}

fn method_name(m: PluginMethod) -> &'static str {
    match m {
        PluginMethod::LogicalAddress => "logical_address",
        PluginMethod::Describe => "describe",
        PluginMethod::TypeOf => "type_of",
        PluginMethod::Spacemap => "spacemap",
        PluginMethod::OccupiedAddresses => "occupied_addresses",
    }
}

fn layout_obj<'py>(py: Python<'py>, lay: &LayoutDef) -> PyResult<Bound<'py, PyLayoutDef>> {
    Bound::new(py, PyLayoutDef { inner: lay.clone() })
}

impl LayoutPlugin for PyPlugin {
    fn has(&self, m: PluginMethod) -> bool {
        Python::attach(|py| self.obj.bind(py).hasattr(method_name(m)).unwrap_or(false))
    }

    fn logical_address(&self, lay: &LayoutDef, object_id: &Nat, field: &Val, lane: Lane) -> Result<Val> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1(
                    "logical_address",
                    (layout_obj(py, lay)?, nat_py(py, object_id)?, from_val(py, field)?, lane as i64),
                )?;
            to_val(&r)
        })
        .map_err(from_pyerr)
    }

    fn describe(&self, lay: &LayoutDef, address: &Val) -> Result<Val> {
        Python::attach(|py| {
            let r = self.obj.bind(py).call_method1("describe", (layout_obj(py, lay)?, from_val(py, address)?))?;
            if r.is_none() {
                return Ok(Val::map());
            }
            if !r.is_instance_of::<PyDict>() {
                let d = py.import("builtins")?.getattr("dict")?.call1((r,))?;
                return to_val(&d);
            }
            to_val(&r)
        })
        .map_err(from_pyerr)
    }

    fn type_of(&self, lay: &LayoutDef, address: &Val) -> Result<Val> {
        Python::attach(|py| {
            let r = self.obj.bind(py).call_method1("type_of", (layout_obj(py, lay)?, from_val(py, address)?))?;
            to_val(&r)
        })
        .map_err(from_pyerr)
    }

    fn spacemap(&self, lay: &LayoutDef) -> Result<Val> {
        Python::attach(|py| {
            let r = self.obj.bind(py).call_method1("spacemap", (layout_obj(py, lay)?,))?;
            if r.is_instance_of::<PyDict>() {
                to_val(&r)
            } else {
                // not a dict → the engine reports SchemaError
                Ok(Val::List(vec![]))
            }
        })
        .map_err(from_pyerr)
    }

    fn occupied_addresses(&self, _db: &Db, lay: &LayoutDef, object_id: &Nat, lane: Lane) -> Result<Vec<Val>> {
        Python::attach(|py| {
            let db = match &self.db {
                Some(w) => w.bind(py).upgrade().map(|o| o.into_any()).unwrap_or_else(|| py.None().into_bound(py)),
                None => py.None().into_bound(py),
            };
            let r = self.obj.bind(py).call_method1(
                "occupied_addresses",
                (db, layout_obj(py, lay)?, nat_py(py, object_id)?, lane.into_py_any(py)?),
            )?;
            let mut out = Vec::new();
            for x in r.try_iter()? {
                out.push(to_val(&x?)?);
            }
            Ok(out)
        })
        .map_err(from_pyerr)
    }
}
