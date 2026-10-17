//! Python API of the MATH_MAP language and the realm planner
//! (`nhydb.math`, `nhydb.mathlayout`).
//!
//! The classes behave like frozen dataclasses: constructors,
//! attributes, equality/ordering/hash, `repr`, `to_dict`/`from_dict`.

use std::collections::HashMap;

use num_bigint::BigInt;
use pyo3::basic::CompareOp;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use nhydb_core::db::math::MathLayoutState;
use nhydb_core::mathexpr::{self as mx, MathExpression, MathMap, MathMapLayoutSpec, MathMapTarget, Side, MV};
use nhydb_core::mathlayout::{self as ml, CoreLayoutSnapshot, MathAllocationPlan, MathRealmExtent, MathRealmMap, OwnedRealmExtent, ResolvedMathCell};
use nhydb_core::{address, Val};

use crate::conv::{from_val, to_val, IntoPy};

fn span_default() -> BigInt {
    address::realm_span().to_bigint()
}

fn strs(obj: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    obj.try_iter()?.map(|x| x?.str().map(|s| s.to_string())).collect()
}

fn bigint_tuple<'py>(py: Python<'py>, v: &[BigInt]) -> PyResult<Bound<'py, PyTuple>> {
    PyTuple::new(py, v)
}

fn mv_py(py: Python<'_>, v: MV) -> PyResult<Py<PyAny>> {
    Ok(match v {
        MV::Int(i) => i.into_pyobject(py)?.into_any().unbind(),
        MV::Bool(b) => pyo3::types::PyBool::new(py, b).to_owned().into_any().unbind(),
    })
}

fn tuple_repr(items: &[String]) -> String {
    if items.len() == 1 { format!("({},)", items[0]) } else { format!("({})", items.join(", ")) }
}

fn cmp_result<T: PartialOrd>(a: &T, b: &T, op: CompareOp) -> bool {
    match op {
        CompareOp::Lt => a < b,
        CompareOp::Le => a <= b,
        CompareOp::Eq => a == b,
        CompareOp::Ne => a != b,
        CompareOp::Gt => a > b,
        CompareOp::Ge => a >= b,
    }
}

// ----------------------------------------------------------------------
// MathExpression
// ----------------------------------------------------------------------

#[pyclass(name = "MathExpression", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyMathExpression {
    pub inner: MathExpression,
}

#[pymethods]
impl PyMathExpression {
    #[classmethod]
    fn compile(_cls: &Bound<'_, pyo3::types::PyType>, source: &Bound<'_, PyAny>, parameters: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(PyMathExpression { inner: MathExpression::compile(&source.str()?.to_string(), &strs(parameters)?).py()? })
    }
    #[getter]
    fn source(&self) -> String {
        self.inner.source.clone()
    }
    #[getter]
    fn parameters<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.inner.parameters)
    }
    #[pyo3(signature = (**variables))]
    fn evaluate(&self, py: Python<'_>, variables: Option<&Bound<'_, PyDict>>) -> PyResult<Py<PyAny>> {
        let mut vars = HashMap::new();
        if let Some(d) = variables {
            for (k, v) in d.iter() {
                let name = k.str()?.to_string();
                if v.is_instance_of::<pyo3::types::PyBool>() || !v.is_instance_of::<pyo3::types::PyInt>() {
                    // Variables are only checked on access; unused variables
                    // are an error anyway (unknown) or integral.
                    if self.inner.parameters.contains(&name) {
                        return Err(crate::exceptions::MathDomainError::new_err(format!("variable {} must be an integer", Val::str(name.as_str()).py_repr())));
                    }
                }
                let b: BigInt = v.extract().unwrap_or_default();
                vars.insert(name, b);
            }
        }
        mv_py(py, self.inner.evaluate(&vars, mx::default_max_bits()).py()?)
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    #[classmethod]
    fn from_dict(_cls: &Bound<'_, pyo3::types::PyType>, raw: &Bound<'_, PyAny>) -> PyResult<Self> {
        let src = raw.get_item("source")?.str()?.to_string();
        Ok(PyMathExpression { inner: MathExpression::compile(&src, &strs(&raw.get_item("parameters")?)?).py()? })
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match (other.cast::<PyMathExpression>(), op) {
            (Ok(o), CompareOp::Eq) => Ok((self.inner == o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            (Ok(o), CompareOp::Ne) => Ok((self.inner != o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            _ => Ok(py.NotImplemented()),
        }
    }
    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.inner.source.hash(&mut h);
        self.inner.parameters.hash(&mut h);
        h.finish()
    }
    fn __repr__(&self) -> String {
        let ps: Vec<String> = self.inner.parameters.iter().map(|p| Val::str(p.as_str()).py_repr()).collect();
        format!("MathExpression(source={}, parameters={})", Val::str(self.inner.source.as_str()).py_repr(), tuple_repr(&ps))
    }
}

// ----------------------------------------------------------------------
// MathMapTarget
// ----------------------------------------------------------------------

#[pyclass(name = "MathMapTarget", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyMathMapTarget {
    pub inner: MathMapTarget,
}

#[pymethods]
impl PyMathMapTarget {
    #[new]
    #[allow(non_snake_case)]
    fn new(inputs: &Bound<'_, PyAny>, u: BigInt, V_global: BigInt, logical_realm: BigInt, v_local: BigInt, side: i64) -> PyResult<Self> {
        let ins: Vec<BigInt> = inputs.try_iter()?.map(|x| x?.extract()).collect::<PyResult<_>>()?;
        Ok(PyMathMapTarget { inner: MathMapTarget { inputs: ins, u, v_global: V_global, logical_realm, v_local, side } })
    }
    #[getter]
    fn inputs<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        bigint_tuple(py, &self.inner.inputs)
    }
    #[getter]
    fn u(&self) -> BigInt {
        self.inner.u.clone()
    }
    #[getter(V_global)]
    fn v_global(&self) -> BigInt {
        self.inner.v_global.clone()
    }
    #[getter]
    fn logical_realm(&self) -> BigInt {
        self.inner.logical_realm.clone()
    }
    #[getter]
    fn v_local(&self) -> BigInt {
        self.inner.v_local.clone()
    }
    #[getter]
    fn side(&self) -> i64 {
        self.inner.side
    }
    #[getter]
    fn side_name(&self) -> &'static str {
        if self.inner.side == 0 { "NL" } else { "NR" }
    }
    #[getter]
    fn logical_cell_key(&self) -> (BigInt, BigInt, BigInt, i64) {
        self.inner.logical_cell_key()
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match (other.cast::<PyMathMapTarget>(), op) {
            (Ok(o), CompareOp::Eq) => Ok((self.inner == o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            (Ok(o), CompareOp::Ne) => Ok((self.inner != o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            _ => Ok(py.NotImplemented()),
        }
    }
    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.inner.hash(&mut h);
        h.finish()
    }
    fn __repr__(&self) -> String {
        let ins: Vec<String> = self.inner.inputs.iter().map(|x| x.to_string()).collect();
        format!(
            "MathMapTarget(inputs={}, u={}, V_global={}, logical_realm={}, v_local={}, side={})",
            tuple_repr(&ins),
            self.inner.u,
            self.inner.v_global,
            self.inner.logical_realm,
            self.inner.v_local,
            self.inner.side
        )
    }
}

// ----------------------------------------------------------------------
// MathMap
// ----------------------------------------------------------------------

#[pyclass(name = "MathMap", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyMathMap {
    pub inner: MathMap,
}

pub(crate) fn inputs_of(args: &Bound<'_, PyAny>) -> PyResult<Vec<Val>> {
    args.try_iter()?.map(|x| to_val(&x?)).collect()
}

#[pymethods]
impl PyMathMap {
    #[classmethod]
    #[pyo3(signature = (name, parameters, *, u, V, side=None, realm_span=None))]
    #[allow(non_snake_case, clippy::too_many_arguments)]
    fn compile(
        _cls: &Bound<'_, pyo3::types::PyType>,
        name: &Bound<'_, PyAny>,
        parameters: &Bound<'_, PyAny>,
        u: &Bound<'_, PyAny>,
        V: &Bound<'_, PyAny>,
        side: Option<&Bound<'_, PyAny>>,
        realm_span: Option<BigInt>,
    ) -> PyResult<Self> {
        let side = match side {
            Some(s) => to_val(s)?,
            None => Val::str("NL"),
        };
        let m = MathMap::compile(
            &name.str()?.to_string(),
            &strs(parameters)?,
            &u.str()?.to_string(),
            &V.str()?.to_string(),
            &side,
            &realm_span.unwrap_or_else(span_default),
        )
        .py()?;
        Ok(PyMathMap { inner: m })
    }
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }
    #[getter]
    fn parameters<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.inner.parameters)
    }
    #[getter]
    fn u_expression(&self) -> PyMathExpression {
        PyMathExpression { inner: self.inner.u.clone() }
    }
    #[getter(V_expression)]
    fn v_expression(&self) -> PyMathExpression {
        PyMathExpression { inner: self.inner.v.clone() }
    }
    #[getter]
    fn side_fixed(&self) -> Option<i64> {
        match &self.inner.side {
            Side::Fixed(s) => Some(*s),
            Side::Expr(_) => None,
        }
    }
    #[getter]
    fn side_expression(&self) -> Option<PyMathExpression> {
        match &self.inner.side {
            Side::Expr(e) => Some(PyMathExpression { inner: e.clone() }),
            Side::Fixed(_) => None,
        }
    }
    #[getter]
    fn realm_span(&self) -> BigInt {
        self.inner.realm_span.clone()
    }
    #[pyo3(signature = (*args))]
    fn target(&self, args: &Bound<'_, PyTuple>) -> PyResult<PyMathMapTarget> {
        Ok(PyMathMapTarget { inner: self.inner.target(&inputs_of(args.as_any())?, mx::default_max_bits()).py()? })
    }
    fn targets(&self, py: Python<'_>, rows: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let mut out = Vec::new();
        for row in rows.try_iter()? {
            out.push(Py::new(py, PyMathMapTarget { inner: self.inner.target(&inputs_of(&row?)?, mx::default_max_bits()).py()? })?);
        }
        Ok(pyo3::types::PyList::new(py, out)?.try_iter()?.into_any().unbind())
    }
    fn canonical_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.canonical_dict())
    }
    #[getter]
    fn fingerprint(&self) -> String {
        self.inner.fingerprint()
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    #[classmethod]
    fn from_dict(_cls: &Bound<'_, pyo3::types::PyType>, raw: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(PyMathMap { inner: MathMap::from_val(&to_val(raw)?).py()? })
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match (other.cast::<PyMathMap>(), op) {
            (Ok(o), CompareOp::Eq) => Ok((self.inner == o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            (Ok(o), CompareOp::Ne) => Ok((self.inner != o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            _ => Ok(py.NotImplemented()),
        }
    }
    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.inner.fingerprint().hash(&mut h);
        h.finish()
    }
    fn __repr__(&self) -> String {
        let ps: Vec<String> = self.inner.parameters.iter().map(|p| Val::str(p.as_str()).py_repr()).collect();
        let ue = PyMathExpression { inner: self.inner.u.clone() }.__repr__();
        let ve = PyMathExpression { inner: self.inner.v.clone() }.__repr__();
        let (sf, se) = match &self.inner.side {
            Side::Fixed(s) => (s.to_string(), "None".to_string()),
            Side::Expr(e) => ("None".to_string(), PyMathExpression { inner: e.clone() }.__repr__()),
        };
        format!(
            "MathMap(name={}, parameters={}, u_expression={ue}, V_expression={ve}, side_fixed={sf}, side_expression={se}, realm_span={})",
            Val::str(self.inner.name.as_str()).py_repr(),
            tuple_repr(&ps),
            self.inner.realm_span
        )
    }
}

// ----------------------------------------------------------------------
// MathMapLayoutSpec
// ----------------------------------------------------------------------

#[pyclass(name = "MathMapLayoutSpec", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyMathMapLayoutSpec {
    pub inner: MathMapLayoutSpec,
}

#[pymethods]
impl PyMathMapLayoutSpec {
    #[new]
    #[pyo3(signature = (layout_name, mapping, realm_auto_limit=None, description=None))]
    fn new(layout_name: &Bound<'_, PyAny>, mapping: &Bound<'_, PyMathMap>, realm_auto_limit: Option<BigInt>, description: Option<String>) -> PyResult<Self> {
        let name = if layout_name.is_truthy()? { layout_name.str()?.to_string() } else { String::new() };
        Ok(PyMathMapLayoutSpec {
            inner: MathMapLayoutSpec::new(
                &name,
                mapping.get().inner.clone(),
                realm_auto_limit.unwrap_or_else(|| BigInt::from(nhydb_core::config::DEFAULT_REALM_AUTO_LIMIT)),
                &description.unwrap_or_default(),
            )
            .py()?,
        })
    }
    #[getter]
    fn layout_name(&self) -> String {
        self.inner.layout_name.clone()
    }
    #[getter]
    fn mapping(&self) -> PyMathMap {
        PyMathMap { inner: self.inner.mapping.clone() }
    }
    #[getter]
    fn realm_auto_limit(&self) -> BigInt {
        self.inner.realm_auto_limit.clone()
    }
    #[getter]
    fn description(&self) -> String {
        self.inner.description.clone()
    }
    #[getter]
    fn fingerprint(&self) -> String {
        self.inner.fingerprint()
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    #[classmethod]
    fn from_dict(_cls: &Bound<'_, pyo3::types::PyType>, raw: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(PyMathMapLayoutSpec { inner: MathMapLayoutSpec::from_val(&to_val(raw)?).py()? })
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match (other.cast::<PyMathMapLayoutSpec>(), op) {
            (Ok(o), CompareOp::Eq) => Ok((self.inner == o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            (Ok(o), CompareOp::Ne) => Ok((self.inner != o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            _ => Ok(py.NotImplemented()),
        }
    }
    fn __repr__(&self) -> String {
        format!(
            "MathMapLayoutSpec(layout_name={}, mapping={}, realm_auto_limit={}, description={})",
            Val::str(self.inner.layout_name.as_str()).py_repr(),
            PyMathMap { inner: self.inner.mapping.clone() }.__repr__(),
            self.inner.realm_auto_limit,
            Val::str(self.inner.description.as_str()).py_repr()
        )
    }
}

// ----------------------------------------------------------------------
// dry_run
// ----------------------------------------------------------------------

#[pyclass(name = "MathDryRunReport", module = "nhydb", frozen, get_all)]
pub struct PyDryRunReport {
    pub total: u64,
    pub valid: u64,
    pub invalid: u64,
    pub unique_cells: u64,
    pub collisions: u64,
    pub collision_examples: Py<PyAny>,
    pub logical_realms: Py<PyAny>,
    pub logical_realm_extents: Py<PyAny>,
    pub realm_count: u64,
    pub realm_budget_exceeded: bool,
    pub min_u: Option<BigInt>,
    pub max_u: Option<BigInt>,
    #[pyo3(name = "min_V")]
    pub min_v: Option<BigInt>,
    #[pyo3(name = "max_V")]
    pub max_v: Option<BigInt>,
    pub nl_count: u64,
    pub nr_count: u64,
    pub invalid_examples: Py<PyAny>,
}

#[pyfunction]
#[pyo3(signature = (mapping, rows, *, realm_auto_limit=None, track_collisions=true, max_examples=8))]
fn dry_run(
    py: Python<'_>,
    mapping: &Bound<'_, PyMathMap>,
    rows: &Bound<'_, PyAny>,
    realm_auto_limit: Option<BigInt>,
    track_collisions: bool,
    max_examples: usize,
) -> PyResult<PyDryRunReport> {
    let m = &mapping.get().inner;
    let mut dr = mx::DryRun::new(realm_auto_limit.unwrap_or_else(|| BigInt::from(4096)), track_collisions, max_examples);
    let bits = mx::default_max_bits();
    for row in rows.try_iter()? {
        dr.add(m, &inputs_of(&row?)?, bits);
    }
    let r = dr.finish();
    let row_tuple = |py: Python<'_>, row: &[Val]| -> PyResult<Py<PyAny>> {
        let items: Vec<Py<PyAny>> = row.iter().map(|v| from_val(py, v)).collect::<PyResult<_>>()?;
        Ok(PyTuple::new(py, items)?.into_any().unbind())
    };
    let coll: Vec<Py<PyAny>> = r
        .collision_examples
        .iter()
        .map(|(row, key)| -> PyResult<Py<PyAny>> { Ok(PyTuple::new(py, [row_tuple(py, row)?, key.clone().into_pyobject(py)?.into_any().unbind()])?.into_any().unbind()) })
        .collect::<PyResult<_>>()?;
    let inval: Vec<Py<PyAny>> = r
        .invalid_examples
        .iter()
        .map(|(row, msg)| -> PyResult<Py<PyAny>> { Ok(PyTuple::new(py, [row_tuple(py, row)?, msg.clone().into_pyobject(py)?.into_any().unbind()])?.into_any().unbind()) })
        .collect::<PyResult<_>>()?;
    let extents = mx::coalesce(&r.logical_realms);
    Ok(PyDryRunReport {
        total: r.total,
        valid: r.valid,
        invalid: r.invalid,
        unique_cells: r.unique_cells,
        collisions: r.collisions,
        collision_examples: PyTuple::new(py, coll)?.into_any().unbind(),
        logical_realms: bigint_tuple(py, &r.logical_realms)?.into_any().unbind(),
        logical_realm_extents: PyTuple::new(py, extents)?.into_any().unbind(),
        realm_count: r.logical_realms.len() as u64,
        realm_budget_exceeded: r.realm_budget_exceeded,
        min_u: r.min_u,
        max_u: r.max_u,
        min_v: r.min_v,
        max_v: r.max_v,
        nl_count: r.nl_count,
        nr_count: r.nr_count,
        invalid_examples: PyTuple::new(py, inval)?.into_any().unbind(),
    })
}

fn check_int(name: &str, v: &Bound<'_, PyAny>) -> PyResult<BigInt> {
    if v.is_instance_of::<pyo3::types::PyBool>() || !v.is_instance_of::<pyo3::types::PyInt>() {
        return Err(crate::exceptions::MathDomainError::new_err(format!("{name} must be an integer")));
    }
    v.extract()
}

#[pyfunction]
#[pyo3(signature = (V_global, *, realm_span=None))]
#[allow(non_snake_case)]
fn split_global_v(V_global: &Bound<'_, PyAny>, realm_span: Option<&Bound<'_, PyAny>>) -> PyResult<(BigInt, BigInt)> {
    if V_global.is_instance_of::<pyo3::types::PyBool>() || !V_global.is_instance_of::<pyo3::types::PyInt>() {
        return Err(crate::exceptions::MathDomainError::new_err("V must be an integer"));
    }
    let span = match realm_span {
        Some(s) => {
            if s.is_instance_of::<pyo3::types::PyBool>() || !s.is_instance_of::<pyo3::types::PyInt>() {
                return Err(crate::exceptions::MathDomainError::new_err("realm_span must be a positive integer"));
            }
            s.extract()?
        }
        None => span_default(),
    };
    mx::split_global_v(&V_global.extract()?, &span).py()
}

#[pyfunction]
#[pyo3(signature = (logical_realm, v_local, *, realm_span=None))]
fn join_global_v(logical_realm: &Bound<'_, PyAny>, v_local: &Bound<'_, PyAny>, realm_span: Option<BigInt>) -> PyResult<BigInt> {
    let r = check_int("logical_realm", logical_realm).map_err(|_| crate::exceptions::MathDomainError::new_err("logical_realm must be >= 1"))?;
    let v = check_int("v_local", v_local).map_err(|_| crate::exceptions::MathDomainError::new_err("v_local lies outside the realm"))?;
    mx::join_global_v(&r, &v, &realm_span.unwrap_or_else(span_default)).py()
}

#[pyfunction]
fn normalize_side(value: &Bound<'_, PyAny>) -> PyResult<i64> {
    mx::normalize_side(&to_val(value)?).py()
}

/// Process-wide safety limit of the MATH expression language (bits).
#[pyfunction]
fn set_math_max_intermediate_bits(bits: u64) {
    mx::set_default_max_bits(bits)
}

#[pyfunction]
fn get_math_max_intermediate_bits() -> u64 {
    mx::default_max_bits()
}

// ----------------------------------------------------------------------
// Realm planner
// ----------------------------------------------------------------------

#[pyclass(name = "OwnedRealmExtent", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyOwnedRealmExtent {
    pub inner: OwnedRealmExtent,
}

#[pymethods]
impl PyOwnedRealmExtent {
    #[new]
    fn new(start: BigInt, end: BigInt, owner_kind: String, owner_id: BigInt) -> PyResult<Self> {
        Ok(PyOwnedRealmExtent { inner: OwnedRealmExtent::new(start, end, &owner_kind, owner_id).py()? })
    }
    #[getter]
    fn start(&self) -> BigInt {
        self.inner.start.clone()
    }
    #[getter]
    fn end(&self) -> BigInt {
        self.inner.end.clone()
    }
    #[getter]
    fn owner_kind(&self) -> String {
        self.inner.owner_kind.clone()
    }
    #[getter]
    fn owner_id(&self) -> BigInt {
        self.inner.owner_id.clone()
    }
    #[getter]
    fn count(&self) -> BigInt {
        self.inner.count()
    }
    fn overlaps(&self, start: BigInt, end: BigInt) -> bool {
        self.inner.overlaps(&start, &end)
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match other.cast::<PyOwnedRealmExtent>() {
            Ok(o) => Ok(cmp_result(&self.inner, &o.get().inner, op).into_pyobject(py)?.to_owned().into_any().unbind()),
            Err(_) => Ok(py.NotImplemented()),
        }
    }
    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.inner.hash(&mut h);
        h.finish()
    }
    fn __repr__(&self) -> String {
        format!(
            "OwnedRealmExtent(start={}, end={}, owner_kind={}, owner_id={})",
            self.inner.start,
            self.inner.end,
            Val::str(self.inner.owner_kind.as_str()).py_repr(),
            self.inner.owner_id
        )
    }
}

#[pyclass(name = "MathRealmExtent", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyMathRealmExtent {
    pub inner: MathRealmExtent,
}

#[pymethods]
impl PyMathRealmExtent {
    #[new]
    fn new(logical_start: BigInt, logical_end: BigInt, physical_start: BigInt, physical_end: BigInt) -> PyResult<Self> {
        Ok(PyMathRealmExtent { inner: MathRealmExtent::new(logical_start, logical_end, physical_start, physical_end).py()? })
    }
    #[getter]
    fn logical_start(&self) -> BigInt {
        self.inner.logical_start.clone()
    }
    #[getter]
    fn logical_end(&self) -> BigInt {
        self.inner.logical_end.clone()
    }
    #[getter]
    fn physical_start(&self) -> BigInt {
        self.inner.physical_start.clone()
    }
    #[getter]
    fn physical_end(&self) -> BigInt {
        self.inner.physical_end.clone()
    }
    #[getter]
    fn logical_count(&self) -> BigInt {
        self.inner.logical_count()
    }
    #[getter]
    fn physical_count(&self) -> BigInt {
        &self.inner.physical_end - &self.inner.physical_start + 1
    }
    fn contains_logical(&self, r: BigInt) -> bool {
        self.inner.contains_logical(&r)
    }
    fn resolve(&self, r: BigInt) -> Option<BigInt> {
        self.inner.resolve(&r)
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    #[classmethod]
    fn from_dict(_cls: &Bound<'_, pyo3::types::PyType>, raw: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(PyMathRealmExtent { inner: MathRealmExtent::from_val(&to_val(raw)?).py()? })
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match other.cast::<PyMathRealmExtent>() {
            Ok(o) => Ok(cmp_result(&self.inner, &o.get().inner, op).into_pyobject(py)?.to_owned().into_any().unbind()),
            Err(_) => Ok(py.NotImplemented()),
        }
    }
    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.inner.hash(&mut h);
        h.finish()
    }
    fn __repr__(&self) -> String {
        format!(
            "MathRealmExtent(logical_start={}, logical_end={}, physical_start={}, physical_end={})",
            self.inner.logical_start, self.inner.logical_end, self.inner.physical_start, self.inner.physical_end
        )
    }
}

fn extents_of(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<MathRealmExtent>> {
    let mut out = Vec::new();
    if let Some(o) = obj {
        for e in o.try_iter()? {
            out.push(e?.cast::<PyMathRealmExtent>()?.get().inner.clone());
        }
    }
    Ok(out)
}

fn extents_tuple<'py>(py: Python<'py>, v: &[MathRealmExtent]) -> PyResult<Bound<'py, PyTuple>> {
    let items: Vec<Py<PyMathRealmExtent>> = v.iter().map(|e| Py::new(py, PyMathRealmExtent { inner: e.clone() })).collect::<PyResult<_>>()?;
    PyTuple::new(py, items)
}

#[pyclass(name = "MathRealmMap", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyMathRealmMap {
    pub inner: MathRealmMap,
}

#[pymethods]
impl PyMathRealmMap {
    #[new]
    #[pyo3(signature = (layout_id, mapping_fingerprint, extents=None, realm_auto_limit=None))]
    fn new(layout_id: BigInt, mapping_fingerprint: String, extents: Option<&Bound<'_, PyAny>>, realm_auto_limit: Option<BigInt>) -> PyResult<Self> {
        Ok(PyMathRealmMap {
            inner: MathRealmMap::new(layout_id, &mapping_fingerprint, extents_of(extents)?, realm_auto_limit.unwrap_or_else(|| BigInt::from(4096))).py()?,
        })
    }
    #[getter]
    fn layout_id(&self) -> BigInt {
        self.inner.layout_id.clone()
    }
    #[getter]
    fn mapping_fingerprint(&self) -> String {
        self.inner.mapping_fingerprint.clone()
    }
    #[getter]
    fn extents<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        extents_tuple(py, &self.inner.extents)
    }
    #[getter]
    fn realm_auto_limit(&self) -> BigInt {
        self.inner.realm_auto_limit.clone()
    }
    #[getter]
    fn mapped_count(&self) -> BigInt {
        self.inner.mapped_count()
    }
    fn resolve(&self, logical_realm: BigInt) -> PyResult<Option<BigInt>> {
        self.inner.resolve(&logical_realm).py()
    }
    fn contains(&self, logical_realm: BigInt) -> PyResult<bool> {
        Ok(self.inner.resolve(&logical_realm).py()?.is_some())
    }
    #[getter]
    fn state_fingerprint(&self) -> String {
        self.inner.state_fingerprint()
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    #[classmethod]
    fn from_dict(_cls: &Bound<'_, pyo3::types::PyType>, raw: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(PyMathRealmMap { inner: MathRealmMap::from_val(&to_val(raw)?).py()? })
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match (other.cast::<PyMathRealmMap>(), op) {
            (Ok(o), CompareOp::Eq) => Ok((self.inner == o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            (Ok(o), CompareOp::Ne) => Ok((self.inner != o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            _ => Ok(py.NotImplemented()),
        }
    }
    fn __repr__(&self) -> String {
        let ex: Vec<String> = self.inner.extents.iter().map(|e| PyMathRealmExtent { inner: e.clone() }.__repr__()).collect();
        format!(
            "MathRealmMap(layout_id={}, mapping_fingerprint={}, extents={}, realm_auto_limit={})",
            self.inner.layout_id,
            Val::str(self.inner.mapping_fingerprint.as_str()).py_repr(),
            tuple_repr(&ex),
            self.inner.realm_auto_limit
        )
    }
}

#[pyclass(name = "CoreLayoutSnapshot", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyCoreLayoutSnapshot {
    pub inner: CoreLayoutSnapshot,
}

#[pymethods]
impl PyCoreLayoutSnapshot {
    #[new]
    #[pyo3(signature = (revision, occupied=None))]
    fn new(revision: BigInt, occupied: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let mut occ = Vec::new();
        if let Some(o) = occupied {
            for e in o.try_iter()? {
                occ.push(e?.cast::<PyOwnedRealmExtent>()?.get().inner.clone());
            }
        }
        Ok(PyCoreLayoutSnapshot { inner: CoreLayoutSnapshot::new(revision, occ).py()? })
    }
    #[getter]
    fn revision(&self) -> BigInt {
        self.inner.revision.clone()
    }
    #[getter]
    fn occupied<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let items: Vec<Py<PyOwnedRealmExtent>> = self.inner.occupied.iter().map(|e| Py::new(py, PyOwnedRealmExtent { inner: e.clone() })).collect::<PyResult<_>>()?;
        PyTuple::new(py, items)
    }
    fn owner_at(&self, realm_id: BigInt) -> PyResult<Option<PyOwnedRealmExtent>> {
        Ok(self.inner.owner_at(&realm_id).py()?.map(|e| PyOwnedRealmExtent { inner: e.clone() }))
    }
    fn is_free(&self, start: BigInt, end: BigInt) -> PyResult<bool> {
        self.inner.is_free(&start, &end).py()
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match (other.cast::<PyCoreLayoutSnapshot>(), op) {
            (Ok(o), CompareOp::Eq) => Ok((self.inner == o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            (Ok(o), CompareOp::Ne) => Ok((self.inner != o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            _ => Ok(py.NotImplemented()),
        }
    }
    fn __repr__(&self) -> String {
        let ex: Vec<String> = self.inner.occupied.iter().map(|e| PyOwnedRealmExtent { inner: e.clone() }.__repr__()).collect();
        format!("CoreLayoutSnapshot(revision={}, occupied={})", self.inner.revision, tuple_repr(&ex))
    }
}

#[pyclass(name = "MathAllocationPlan", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyMathAllocationPlan {
    pub inner: MathAllocationPlan,
}

#[pymethods]
impl PyMathAllocationPlan {
    #[getter]
    fn allowed(&self) -> bool {
        self.inner.allowed
    }
    #[getter]
    fn reason(&self) -> String {
        self.inner.reason.clone()
    }
    #[getter]
    fn base_revision(&self) -> BigInt {
        self.inner.base_revision.clone()
    }
    #[getter]
    fn base_map_fingerprint(&self) -> String {
        self.inner.base_map_fingerprint.clone()
    }
    #[getter]
    fn layout_id(&self) -> BigInt {
        self.inner.layout_id.clone()
    }
    #[getter]
    fn mapping_fingerprint(&self) -> String {
        self.inner.mapping_fingerprint.clone()
    }
    #[getter]
    fn requested_realms<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        bigint_tuple(py, &self.inner.requested_realms)
    }
    #[getter]
    fn missing_realms<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        bigint_tuple(py, &self.inner.missing_realms)
    }
    #[getter]
    fn missing_logical_extents<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, self.inner.missing_logical_extents.clone())
    }
    #[getter]
    fn proposed_extents<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        extents_tuple(py, &self.inner.proposed_extents)
    }
    #[getter]
    fn resulting_extents<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        extents_tuple(py, &self.inner.resulting_extents)
    }
    #[getter]
    fn existing_count(&self) -> BigInt {
        self.inner.existing_count.clone()
    }
    #[getter]
    fn new_count(&self) -> BigInt {
        self.inner.new_count.clone()
    }
    #[getter]
    fn resulting_count(&self) -> BigInt {
        self.inner.resulting_count.clone()
    }
    #[getter]
    fn realm_auto_limit(&self) -> BigInt {
        self.inner.realm_auto_limit.clone()
    }
    fn require_allowed(slf: PyRef<'_, Self>) -> PyResult<PyRef<'_, Self>> {
        slf.inner.require_allowed().py()?;
        Ok(slf)
    }
}

#[pyclass(name = "ResolvedMathCell", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyResolvedMathCell {
    pub inner: ResolvedMathCell,
}

#[pymethods]
impl PyResolvedMathCell {
    #[new]
    fn new(physical_realm: BigInt, u: BigInt, v_local: BigInt, side: i64) -> Self {
        PyResolvedMathCell { inner: ResolvedMathCell { physical_realm, u, v_local, side } }
    }
    #[getter]
    fn physical_realm(&self) -> BigInt {
        self.inner.physical_realm.clone()
    }
    #[getter]
    fn u(&self) -> BigInt {
        self.inner.u.clone()
    }
    #[getter]
    fn v_local(&self) -> BigInt {
        self.inner.v_local.clone()
    }
    #[getter]
    fn side(&self) -> i64 {
        self.inner.side
    }
    fn __richcmp__(&self, other: &Bound<'_, PyAny>, op: CompareOp) -> PyResult<Py<PyAny>> {
        let py = other.py();
        match (other.cast::<PyResolvedMathCell>(), op) {
            (Ok(o), CompareOp::Eq) => Ok((self.inner == o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            (Ok(o), CompareOp::Ne) => Ok((self.inner != o.get().inner).into_pyobject(py)?.to_owned().into_any().unbind()),
            _ => Ok(py.NotImplemented()),
        }
    }
    fn __repr__(&self) -> String {
        format!(
            "ResolvedMathCell(physical_realm={}, u={}, v_local={}, side={})",
            self.inner.physical_realm, self.inner.u, self.inner.v_local, self.inner.side
        )
    }
}

fn needed_of(obj: &Bound<'_, PyAny>) -> PyResult<Vec<BigInt>> {
    obj.try_iter()?.map(|x| x?.extract::<BigInt>()).collect()
}

#[pyfunction]
fn plan_math_realms(snapshot: &Bound<'_, PyCoreLayoutSnapshot>, realm_map: &Bound<'_, PyMathRealmMap>, needed_logical_realms: &Bound<'_, PyAny>) -> PyResult<PyMathAllocationPlan> {
    Ok(PyMathAllocationPlan { inner: ml::plan_math_realms(&snapshot.get().inner, &realm_map.get().inner, &needed_of(needed_logical_realms)?).py()? })
}

#[pyfunction]
fn validate_plan(snapshot: &Bound<'_, PyCoreLayoutSnapshot>, realm_map: &Bound<'_, PyMathRealmMap>, plan: &Bound<'_, PyMathAllocationPlan>) -> PyResult<()> {
    ml::validate_plan(&snapshot.get().inner, &realm_map.get().inner, &plan.get().inner).py()
}

#[pyfunction]
fn commit_plan_simulated(
    snapshot: &Bound<'_, PyCoreLayoutSnapshot>,
    realm_map: &Bound<'_, PyMathRealmMap>,
    plan: &Bound<'_, PyMathAllocationPlan>,
) -> PyResult<(PyCoreLayoutSnapshot, PyMathRealmMap)> {
    let (s, m) = ml::commit_plan_simulated(&snapshot.get().inner, &realm_map.get().inner, &plan.get().inner).py()?;
    Ok((PyCoreLayoutSnapshot { inner: s }, PyMathRealmMap { inner: m }))
}

#[pyfunction]
fn validate_map_against_snapshot(snapshot: &Bound<'_, PyCoreLayoutSnapshot>, realm_map: &Bound<'_, PyMathRealmMap>) -> PyResult<()> {
    ml::validate_map_against_snapshot(&snapshot.get().inner, &realm_map.get().inner).py()
}

#[pyfunction]
fn resolve_math_target(realm_map: &Bound<'_, PyMathRealmMap>, target: &Bound<'_, PyAny>) -> PyResult<PyResolvedMathCell> {
    let t = match target.cast::<PyMathMapTarget>() {
        Ok(t) => t.get().inner.clone(),
        Err(_) => MathMapTarget {
            inputs: Vec::new(),
            u: target.getattr("u")?.extract()?,
            v_global: BigInt::from(0),
            logical_realm: target.getattr("logical_realm")?.extract()?,
            v_local: target.getattr("v_local")?.extract()?,
            side: target.getattr("side")?.extract()?,
        },
    };
    Ok(PyResolvedMathCell { inner: ml::resolve_math_target(&realm_map.get().inner, &t).py()? })
}

// ----------------------------------------------------------------------
// MathLayoutState (Core)
// ----------------------------------------------------------------------

#[pyclass(name = "MathLayoutState", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyMathLayoutState {
    pub inner: MathLayoutState,
}

#[pymethods]
impl PyMathLayoutState {
    #[getter]
    fn layout_id(&self) -> u64 {
        self.inner.layout_id
    }
    #[getter]
    fn spec(&self) -> PyMathMapLayoutSpec {
        PyMathMapLayoutSpec { inner: self.inner.spec.clone() }
    }
    #[getter]
    fn realm_map(&self) -> PyMathRealmMap {
        PyMathRealmMap { inner: self.inner.realm_map.clone() }
    }
    #[getter]
    fn payload_type(&self) -> &'static str {
        self.inner.payload_type.name()
    }
    #[getter]
    fn frozen(&self) -> bool {
        self.inner.frozen
    }
    #[getter]
    fn created_block_id(&self) -> u64 {
        self.inner.created_block_id
    }
    #[getter]
    fn first_write_block_id(&self) -> Option<u64> {
        self.inner.first_write_block_id
    }
    #[getter]
    fn name(&self) -> String {
        self.inner.name().to_string()
    }
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.inner.to_val())
    }
    #[classmethod]
    fn from_dict(_cls: &Bound<'_, pyo3::types::PyType>, raw: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(PyMathLayoutState { inner: MathLayoutState::from_val(&to_val(raw)?).py()? })
    }
    fn __repr__(&self) -> String {
        format!(
            "MathLayoutState(layout_id={}, name={}, frozen={}, payload_type={}, mapped_realms={})",
            self.inner.layout_id,
            Val::str(self.inner.name()).py_repr(),
            if self.inner.frozen { "True" } else { "False" },
            Val::str(self.inner.payload_type.name()).py_repr(),
            self.inner.realm_map.mapped_count()
        )
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMathExpression>()?;
    m.add_class::<PyMathMapTarget>()?;
    m.add_class::<PyMathMap>()?;
    m.add_class::<PyMathMapLayoutSpec>()?;
    m.add_class::<PyDryRunReport>()?;
    m.add_class::<PyOwnedRealmExtent>()?;
    m.add_class::<PyMathRealmExtent>()?;
    m.add_class::<PyMathRealmMap>()?;
    m.add_class::<PyCoreLayoutSnapshot>()?;
    m.add_class::<PyMathAllocationPlan>()?;
    m.add_class::<PyResolvedMathCell>()?;
    m.add_class::<PyMathLayoutState>()?;
    for f in [
        wrap_pyfunction!(dry_run, m)?,
        wrap_pyfunction!(split_global_v, m)?,
        wrap_pyfunction!(join_global_v, m)?,
        wrap_pyfunction!(normalize_side, m)?,
        wrap_pyfunction!(set_math_max_intermediate_bits, m)?,
        wrap_pyfunction!(get_math_max_intermediate_bits, m)?,
        wrap_pyfunction!(plan_math_realms, m)?,
        wrap_pyfunction!(validate_plan, m)?,
        wrap_pyfunction!(commit_plan_simulated, m)?,
        wrap_pyfunction!(validate_map_against_snapshot, m)?,
        wrap_pyfunction!(resolve_math_target, m)?,
    ] {
        m.add_function(f)?;
    }
    Ok(())
}
