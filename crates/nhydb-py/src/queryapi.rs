//! Python API of the query layer (`nhydb.query`): native `QueryEngine`,
//! runtime providers as Python callables, conversion of expression objects.
//!
//! The expression classes (`Q`, `FieldPredicate`, …) are lightweight Python
//! dataclasses in `nhydb.query` (exact dataclass semantics); planning and
//! evaluation run entirely in the Rust core.

use std::sync::Arc;

use num_bigint::BigInt;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use nhydb_core::query::{self as q, Expr, ExecOptions, ExternalProvider, Providers, QueryContext, QueryEngine};
use nhydb_core::sql::{self, Params};
use nhydb_core::{Lane, Val};

use crate::conv::{from_pyerr, from_val, to_lane, to_val, IntoPy};
use crate::NHyDB;

fn query_mod(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    py.import("nhydb.query")
}

fn py_bigint(obj: &Bound<'_, PyAny>) -> PyResult<BigInt> {
    match obj.extract::<BigInt>() {
        Ok(b) => Ok(b),
        Err(_) => obj.py().import("builtins")?.getattr("int")?.call1((obj,))?.extract(),
    }
}

fn opt_val(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Val>> {
    match obj {
        None => Ok(None),
        Some(o) if o.is_none() => Ok(None),
        Some(o) => Ok(Some(to_val(o)?)),
    }
}

fn mapping_val(obj: &Bound<'_, PyAny>) -> PyResult<Val> {
    if obj.is_none() {
        return Ok(Val::map());
    }
    let d = obj.py().import("builtins")?.getattr("dict")?.call1((obj,))?;
    to_val(&d)
}

/// Python expression object → `Expr` (`ensure_expr`).
pub fn py_to_expr(obj: &Bound<'_, PyAny>) -> PyResult<Expr> {
    let tn = obj.get_type().name()?.to_string();
    let attr = |n: &str| obj.getattr(n);
    let s = |n: &str| -> PyResult<String> { Ok(attr(n)?.str()?.to_string()) };
    Ok(match tn.as_str() {
        "BoolExpr" => Expr::Bool {
            op: s("op")?,
            children: attr("children")?.try_iter()?.map(|c| py_to_expr(&c?)).collect::<PyResult<_>>()?,
        },
        "FieldPredicate" => Expr::Field {
            name: s("name")?,
            op: s("op")?,
            value: to_val(&attr("value")?)?,
            value2: to_val(&attr("value2")?)?,
            options: mapping_val(&attr("options")?)?,
        },
        "StructPredicate" => Expr::Struct { name: s("name")?, op: s("op")?, value: to_val(&attr("value")?)?, value2: to_val(&attr("value2")?)? },
        "MathPredicate" => Expr::Math { provider: s("provider")?, params: mapping_val(&attr("params")?)? },
        "DirectNPredicate" => Expr::DirectN {
            values: attr("values")?.try_iter()?.map(|c| to_val(&c?)).collect::<PyResult<_>>()?,
            domain: s("domain")?,
        },
        "AllPredicate" => Expr::All,
        other => return Err(PyTypeError::new_err(format!("query expression expected, got: {other}"))),
    })
}

fn is_expr(obj: &Bound<'_, PyAny>) -> PyResult<bool> {
    let base = query_mod(obj.py())?.getattr("Expr")?;
    obj.is_instance(&base)
}

/// `Expr` → Python expression object.
pub fn expr_to_py(py: Python<'_>, e: &Expr) -> PyResult<Py<PyAny>> {
    expr_to_py_ext(py, e, false)
}

/// `sql = true`: values of `tokens_all`/`tokens_any` as a tuple (as
/// produced by `FieldRef.tokens_all(*values)` in the SQL parser).
pub fn expr_to_py_ext(py: Python<'_>, e: &Expr, sql: bool) -> PyResult<Py<PyAny>> {
    let m = query_mod(py)?;
    Ok(match e {
        Expr::Bool { op, children } => {
            let kids: Vec<Py<PyAny>> = children.iter().map(|c| expr_to_py_ext(py, c, sql)).collect::<PyResult<_>>()?;
            m.getattr("BoolExpr")?.call1((op.as_str(), PyTuple::new(py, kids)?))?.unbind()
        }
        Expr::Field { name, op, value, value2, options } => {
            let v = match value {
                Val::List(items) if sql && (op == "tokens_all" || op == "tokens_any") => {
                    PyTuple::new(py, items.iter().map(|x| from_val(py, x)).collect::<PyResult<Vec<_>>>()?)?.into_any().unbind()
                }
                other => from_val(py, other)?,
            };
            m.getattr("FieldPredicate")?.call1((name.as_str(), op.as_str(), v, from_val(py, value2)?, from_val(py, options)?))?.unbind()
        }
        Expr::Struct { name, op, value, value2 } => {
            m.getattr("StructPredicate")?.call1((name.as_str(), op.as_str(), from_val(py, value)?, from_val(py, value2)?))?.unbind()
        }
        Expr::Math { provider, params } => m.getattr("MathPredicate")?.call1((provider.as_str(), from_val(py, params)?))?.unbind(),
        Expr::DirectN { values, domain } => {
            let vals: Vec<Py<PyAny>> = values.iter().map(|v| from_val(py, v)).collect::<PyResult<_>>()?;
            m.getattr("DirectNPredicate")?.call1((PyTuple::new(py, vals)?, domain.as_str()))?.unbind()
        }
        Expr::All => m.getattr("AllPredicate")?.call0()?.unbind(),
    })
}

/// `expr_from_spec` over Python objects (specifications may contain
/// ready-made expression objects).
pub fn spec_to_expr(obj: &Bound<'_, PyAny>) -> PyResult<Expr> {
    if is_expr(obj)? {
        return py_to_expr(obj);
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        if let Some(a) = d.get_item("all")? {
            if a.is(pyo3::types::PyBool::new(obj.py(), true).as_any()) {
                return Ok(Expr::All);
            }
        }
        for op in ["and", "or"] {
            if let Some(items) = d.get_item(op)? {
                let xs: Vec<Expr> = items.try_iter()?.map(|x| spec_to_expr(&x?)).collect::<PyResult<_>>()?;
                let mut it = xs.into_iter();
                let Some(mut out) = it.next() else {
                    return Err(crate::exceptions::QueryPlanError::new_err(format!("{op} must not be empty")));
                };
                for x in it {
                    out = if op == "and" { Expr::and(out, x) } else { Expr::or(out, x) };
                }
                return Ok(out);
            }
        }
        if let Some(x) = d.get_item("not")? {
            return Ok(Expr::negate(spec_to_expr(&x)?));
        }
    }
    q::expr_from_spec(&to_val(obj).map_err(|_| crate::exceptions::QueryPlanError::new_err("query expression must be an object"))?).py()
}

/// Runtime provider as a Python callable `function(engine, ctx, params)`.
struct PyProvider {
    function: Py<PyAny>,
    estimator: Option<Py<PyAny>>,
}

fn ctx_py(py: Python<'_>, ctx: &QueryContext) -> PyResult<Py<PyAny>> {
    let realm: Py<PyAny> = match &ctx.realm {
        None => py.None(),
        Some(r) => r.clone().into_pyobject(py)?.into_any().unbind(),
    };
    Ok(query_mod(py)?.getattr("QueryContext")?.call1((realm, ctx.lane as i64))?.unbind())
}

fn host_obj(py: Python<'_>, engine: &QueryEngine<'_>) -> Py<PyAny> {
    engine.host.and_then(|h| h.downcast_ref::<Py<PyAny>>()).map(|h| h.clone_ref(py)).unwrap_or_else(|| py.None())
}

impl ExternalProvider for PyProvider {
    fn call(&self, engine: &QueryEngine<'_>, ctx: &QueryContext, params: &Val) -> nhydb_core::Result<Vec<Val>> {
        Python::attach(|py| -> PyResult<Vec<Val>> {
            let res = self.function.call1(py, (host_obj(py, engine), ctx_py(py, ctx)?, from_val(py, params)?))?;
            res.bind(py).try_iter()?.map(|x| to_val(&x?)).collect()
        })
        .map_err(from_pyerr)
    }
    fn has_estimator(&self) -> bool {
        self.estimator.is_some()
    }
    fn estimate(&self, engine: &QueryEngine<'_>, ctx: &QueryContext, params: &Val) -> nhydb_core::Result<Val> {
        let Some(est) = &self.estimator else { return Ok(Val::None) };
        Python::attach(|py| -> PyResult<Val> {
            let r = est.call1(py, (host_obj(py, engine), ctx_py(py, ctx)?, from_val(py, params)?))?;
            to_val(r.bind(py))
        })
        .map_err(from_pyerr)
    }
}

#[pyclass(name = "QueryEngine", module = "nhydb", subclass, dict)]
pub struct PyQueryEngine {
    db: Py<PyAny>,
    providers: Providers,
}

impl PyQueryEngine {
    fn run<T>(slf: &Bound<'_, Self>, f: impl FnOnce(&QueryEngine<'_>) -> nhydb_core::Result<T>) -> PyResult<T> {
        let py = slf.py();
        let this = slf.borrow();
        let dbb = this.db.bind(py).cast::<NHyDB>()?.clone();
        let dbr = dbb.borrow();
        let host: Py<PyAny> = slf.clone().into_any().unbind();
        let eng = QueryEngine { db: &dbr.db, providers: &this.providers, host: Some(&host) };
        f(&eng).py()
    }
}

fn realm_arg(realm: Option<&Bound<'_, PyAny>>) -> PyResult<Option<BigInt>> {
    match realm {
        None => Ok(None),
        Some(r) if r.is_none() => Ok(None),
        Some(r) => Ok(Some(py_bigint(r)?)),
    }
}

fn lane_arg(lane: Option<&Bound<'_, PyAny>>) -> PyResult<Lane> {
    to_lane(lane)
}

#[pymethods]
impl PyQueryEngine {
    #[new]
    fn new(db: &Bound<'_, PyAny>) -> PyResult<Self> {
        db.cast::<NHyDB>().map_err(|_| PyTypeError::new_err("QueryEngine requires an NHyDB instance"))?;
        Ok(PyQueryEngine { db: db.clone().unbind(), providers: Providers::default() })
    }

    #[getter]
    fn db(&self, py: Python<'_>) -> Py<PyAny> {
        self.db.clone_ref(py)
    }

    /// Registered providers `{name: {name, domain, description, builtin}}`.
    #[getter]
    fn math_providers(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let d = PyDict::new(py);
        for name in &self.providers.order {
            let p = &self.providers.map[name];
            let e = PyDict::new(py);
            e.set_item("name", &p.name)?;
            e.set_item("domain", p.domain.name())?;
            e.set_item("description", &p.description)?;
            e.set_item("builtin", !matches!(p.kind, q::ProviderKind::External(_)))?;
            d.set_item(name, e)?;
        }
        Ok(d.into_any().unbind())
    }

    /// `register_math_provider(name, function, *, domain="cell", description="", estimator=None)`
    #[pyo3(signature = (name, function, *, domain=None, description=None, estimator=None))]
    fn register_math_provider(
        &mut self,
        name: &Bound<'_, PyAny>,
        function: &Bound<'_, PyAny>,
        domain: Option<&Bound<'_, PyAny>>,
        description: Option<&Bound<'_, PyAny>>,
        estimator: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let dom = match domain {
            Some(d) => d.str()?.to_string(),
            None => "cell".into(),
        };
        let desc = match description {
            Some(d) => d.str()?.to_string(),
            None => String::new(),
        };
        let est = estimator.filter(|e| !e.is_none()).map(|e| e.clone().unbind());
        let prov = Arc::new(PyProvider { function: function.clone().unbind(), estimator: est });
        self.providers.register(&name.str()?.to_string(), &dom, &desc, prov).py()
    }

    /// `execute(expr, *, realm=None, lane=0, return_mode="record", select=None,
    /// aggregate=None, group_by=None, order_by=None, limit=None, offset=0,
    /// explain=False, k1=1.2, b=0.75)`
    #[pyo3(signature = (expr, *, realm=None, lane=None, return_mode=None, select=None, aggregate=None, group_by=None, order_by=None, limit=None, offset=None, explain=None, k1=1.2, b=0.75))]
    #[allow(clippy::too_many_arguments)]
    fn execute(
        slf: &Bound<'_, Self>,
        expr: &Bound<'_, PyAny>,
        realm: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        return_mode: Option<&Bound<'_, PyAny>>,
        select: Option<&Bound<'_, PyAny>>,
        aggregate: Option<&Bound<'_, PyAny>>,
        group_by: Option<&Bound<'_, PyAny>>,
        order_by: Option<&Bound<'_, PyAny>>,
        limit: Option<&Bound<'_, PyAny>>,
        offset: Option<&Bound<'_, PyAny>>,
        explain: Option<&Bound<'_, PyAny>>,
        k1: f64,
        b: f64,
    ) -> PyResult<Py<PyAny>> {
        let e = py_to_expr(expr)?;
        let opts = ExecOptions {
            realm: realm_arg(realm)?,
            lane: lane_arg(lane)?,
            return_mode: match return_mode {
                Some(r) => r.str()?.to_string(),
                None => "record".into(),
            },
            select: opt_val(select)?,
            aggregate: opt_val(aggregate)?,
            group_by: opt_val(group_by)?,
            order_by: opt_val(order_by)?,
            limit: opt_val(limit)?.unwrap_or(Val::None),
            offset: opt_val(offset)?.unwrap_or(Val::Int(0)),
            explain: explain.map(|x| x.is_truthy()).transpose()?.unwrap_or(false),
            k1,
            b,
        };
        let r = Self::run(slf, |eng| eng.execute(&e, &opts))?;
        from_val(slf.py(), &r)
    }

    /// `explain(expr, *, realm=None, lane=0, return_mode="record")`
    #[pyo3(signature = (expr, *, realm=None, lane=None, return_mode=None))]
    fn explain(
        slf: &Bound<'_, Self>,
        expr: &Bound<'_, PyAny>,
        realm: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        return_mode: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let e = py_to_expr(expr)?;
        let rm = match return_mode {
            Some(r) => r.str()?.to_string(),
            None => "record".into(),
        };
        let (realm, lane) = (realm_arg(realm)?, lane_arg(lane)?);
        let r = Self::run(slf, |eng| eng.explain(&e, realm, lane, &rm))?;
        from_val(slf.py(), &r)
    }

    /// `search(expr, *, realm=None, lane=0, select=None, limit=20, offset=0,
    /// explain=False, k1=1.2, b=0.75)`
    #[pyo3(signature = (expr, *, realm=None, lane=None, select=None, limit=Some(20), offset=None, explain=None, k1=1.2, b=0.75))]
    #[allow(clippy::too_many_arguments)]
    fn search(
        slf: &Bound<'_, Self>,
        expr: &Bound<'_, PyAny>,
        realm: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        select: Option<&Bound<'_, PyAny>>,
        limit: Option<i64>,
        offset: Option<&Bound<'_, PyAny>>,
        explain: Option<&Bound<'_, PyAny>>,
        k1: f64,
        b: f64,
    ) -> PyResult<Py<PyAny>> {
        let e = py_to_expr(expr)?;
        let (realm, lane) = (realm_arg(realm)?, lane_arg(lane)?);
        let sel = opt_val(select)?;
        let lim = limit.map(Val::Int).unwrap_or(Val::None);
        let off = opt_val(offset)?.unwrap_or(Val::Int(0));
        let ex = explain.map(|x| x.is_truthy()).transpose()?.unwrap_or(false);
        let r = Self::run(slf, |eng| eng.search(&e, realm, lane, sel.as_ref(), &lim, &off, ex, k1, b))?;
        from_val(slf.py(), &r)
    }

    /// `compile_sql(text, parameters=None, *, lane=0)` → `SQLCompiledQuery`
    #[pyo3(signature = (text, parameters=None, *, lane=None))]
    fn compile_sql(slf: &Bound<'_, Self>, text: &Bound<'_, PyAny>, parameters: Option<&Bound<'_, PyAny>>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let (t, p, l) = (text.str()?.to_string(), sql_params(parameters)?, lane_arg(lane)?);
        let py = slf.py();
        let this = slf.borrow();
        let dbb = this.db.bind(py).cast::<NHyDB>()?.clone();
        let dbr = dbb.borrow();
        compile_sql_py(py, &dbr.db, &t, &p, l)
    }

    /// `execute_sql(text, parameters=None, *, lane=0)`
    #[pyo3(signature = (text, parameters=None, *, lane=None))]
    fn execute_sql(slf: &Bound<'_, Self>, text: &Bound<'_, PyAny>, parameters: Option<&Bound<'_, PyAny>>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let (t, p, l) = (text.str()?.to_string(), sql_params(parameters)?, lane_arg(lane)?);
        let r = Self::run(slf, |eng| sql::execute_sql(eng, &t, &p, l))?;
        from_val(slf.py(), &r)
    }

    /// `execute_sql_numpy(text, parameters=None, *, lane=0)` → `{column: ndarray}`
    #[pyo3(signature = (text, parameters=None, *, lane=None))]
    fn execute_sql_numpy(slf: &Bound<'_, Self>, text: &Bound<'_, PyAny>, parameters: Option<&Bound<'_, PyAny>>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let (t, p, l) = (text.str()?.to_string(), sql_params(parameters)?, lane_arg(lane)?);
        let r = Self::run(slf, |eng| sql::execute_sql(eng, &t, &p, l))?;
        rows_to_numpy(slf.py(), &r)
    }

    /// `execute_numpy(expr, *, realm=None, lane=0, select, order_by=None, limit=None,
    /// offset=0)`: projection (`fields`) as `{"record_N": …, field: ndarray}`.
    #[pyo3(signature = (expr, *, realm=None, lane=None, select, order_by=None, limit=None, offset=None))]
    #[allow(clippy::too_many_arguments)]
    fn execute_numpy(
        slf: &Bound<'_, Self>,
        expr: &Bound<'_, PyAny>,
        realm: Option<&Bound<'_, PyAny>>,
        lane: Option<&Bound<'_, PyAny>>,
        select: &Bound<'_, PyAny>,
        order_by: Option<&Bound<'_, PyAny>>,
        limit: Option<&Bound<'_, PyAny>>,
        offset: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let e = py_to_expr(expr)?;
        let opts = ExecOptions {
            realm: realm_arg(realm)?,
            lane: lane_arg(lane)?,
            return_mode: "fields".into(),
            select: Some(to_val(select)?),
            order_by: opt_val(order_by)?,
            limit: opt_val(limit)?.unwrap_or(Val::None),
            offset: opt_val(offset)?.unwrap_or(Val::Int(0)),
            ..Default::default()
        };
        let r = Self::run(slf, |eng| eng.execute(&e, &opts))?;
        rows_to_numpy(slf.py(), &r)
    }

    /// `execute_spec(spec)`
    fn execute_spec(slf: &Bound<'_, Self>, spec: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let d = slf.py().import("builtins")?.getattr("dict")?.call1((spec,))?;
        let d = d.cast::<PyDict>()?;
        let expr = match d.get_item("where")? {
            None => Expr::All,
            Some(w) => spec_to_expr(&w)?,
        };
        let rest = PyDict::new(slf.py());
        for (k, v) in d.iter() {
            if k.extract::<String>().map(|s| s != "where").unwrap_or(true) {
                rest.set_item(k, v)?;
            }
        }
        let opts = q::exec_options_from_spec(&to_val(rest.as_any())?).py()?;
        let r = Self::run(slf, |eng| eng.execute(&expr, &opts))?;
        from_val(slf.py(), &r)
    }
}

// ----------------------------------------------------------------------
// NHySQL
// ----------------------------------------------------------------------

/// Python parameters → `Params` (sequence for `?`, mapping for `:name`).
pub fn sql_params(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Params> {
    let Some(o) = obj.filter(|o| !o.is_none()) else { return Ok(Params::None) };
    let py = o.py();
    let mapping = py.import("collections.abc")?.getattr("Mapping")?;
    if o.is_instance(&mapping)? {
        let mut m = Vec::new();
        for item in o.call_method0("items")?.try_iter()? {
            let (k, v): (Bound<'_, PyAny>, Bound<'_, PyAny>) = item?.extract()?;
            if let Ok(ks) = k.extract::<String>() {
                m.push((ks, to_val(&v)?));
            }
        }
        return Ok(Params::Map(m));
    }
    if let Ok(st) = o.extract::<String>() {
        return Ok(Params::Seq(st.chars().map(|c| Val::Str(c.to_string())).collect()));
    }
    if o.hasattr("__getitem__")? && o.hasattr("__len__")? {
        let mut v = Vec::new();
        for x in o.try_iter()? {
            v.push(to_val(&x?)?);
        }
        return Ok(Params::Seq(v));
    }
    Ok(Params::Seq(Vec::new()))
}

fn opt_str(py: Python<'_>, s: &Option<String>) -> Py<PyAny> {
    match s {
        Some(x) => x.clone().into_pyobject(py).unwrap().into_any().unbind(),
        None => py.None(),
    }
}

fn items_py(py: Python<'_>, items: &[sql::SelectItem]) -> PyResult<Py<PyAny>> {
    let l = pyo3::types::PyList::empty(py);
    for it in items {
        let d = PyDict::new(py);
        d.set_item("kind", &it.kind)?;
        d.set_item("name", &it.name)?;
        d.set_item("op", &it.op)?;
        d.set_item("alias", opt_str(py, &it.alias))?;
        l.append(d)?;
    }
    Ok(l.into_any().unbind())
}

fn statement_raw(py: Python<'_>, st: &sql::SelectStatement) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("source", PyTuple::new(py, &st.source)?)?;
    d.set_item("source_alias", opt_str(py, &st.source_alias))?;
    d.set_item("select_items", items_py(py, &st.select_items)?)?;
    d.set_item("where", expr_to_py_ext(py, &st.where_, true)?)?;
    d.set_item("group_by", PyTuple::new(py, &st.group_by)?)?;
    let ob = pyo3::types::PyList::empty(py);
    for o in &st.order_by {
        ob.append((o.name.clone(), o.direction.clone()))?;
    }
    d.set_item("order_by", ob)?;
    d.set_item("limit", st.limit.clone())?;
    d.set_item("offset", st.offset.clone())?;
    d.set_item("explain", st.explain)?;
    d.set_item("text", &st.text)?;
    Ok(d.into_any().unbind())
}

fn statement_py(py: Python<'_>, st: &sql::SelectStatement) -> PyResult<Py<PyAny>> {
    Ok(query_mod(py)?.getattr("_statement_from_raw")?.call1((statement_raw(py, st)?,))?.unbind())
}

fn compiled_py(py: Python<'_>, cq: &sql::CompiledQuery) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("statement", statement_raw(py, &cq.statement)?)?;
    d.set_item("layout_id", cq.layout_id)?;
    d.set_item("layout_name", &cq.layout_name)?;
    d.set_item("realm", crate::conv::nat_py(py, &cq.realm)?)?;
    d.set_item("lane", cq.lane as i64)?;
    d.set_item("expr", expr_to_py_ext(py, &cq.expr, true)?)?;
    d.set_item("return_mode", &cq.return_mode)?;
    d.set_item("select", PyTuple::new(py, &cq.select)?)?;
    d.set_item("aggregate", from_val(py, &cq.aggregate)?)?;
    d.set_item("group_by", PyTuple::new(py, &cq.group_by)?)?;
    d.set_item("order_by", PyTuple::new(py, cq.order_by.iter().map(|(a, b)| (a.clone(), b.clone())))?)?;
    d.set_item("limit", cq.limit.clone())?;
    d.set_item("offset", cq.offset.clone())?;
    d.set_item("ranked", cq.ranked)?;
    d.set_item("output", items_py(py, &cq.output)?)?;
    d.set_item("all_fields", PyTuple::new(py, &cq.all_fields)?)?;
    Ok(query_mod(py)?.getattr("_compiled_from_raw")?.call1((d,))?.unbind())
}

/// `parse_sql(text, parameters=None)` → `SQLSelectStatement`
#[pyfunction]
#[pyo3(signature = (text, parameters=None))]
fn parse_sql(py: Python<'_>, text: &Bound<'_, PyAny>, parameters: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
    let st = sql::parse_sql(&text.str()?.to_string(), &sql_params(parameters)?).py()?;
    statement_py(py, &st)
}

/// `_sql_scan(text)` → list of `SQLToken` (for tools/tests).
#[pyfunction]
fn _sql_scan(py: Python<'_>, text: &str) -> PyResult<Py<PyAny>> {
    let cls = query_mod(py)?.getattr("SQLToken")?;
    let l = pyo3::types::PyList::empty(py);
    for t in sql::scan(text).py()? {
        l.append(cls.call1((t.kind.name(), from_val(py, &t.value)?, t.pos))?)?;
    }
    Ok(l.into_any().unbind())
}

pub fn compile_sql_py(py: Python<'_>, db: &nhydb_core::db::Db, text: &str, params: &Params, lane: Lane) -> PyResult<Py<PyAny>> {
    let cq = sql::compile_sql(db, text, params, lane).py()?;
    compiled_py(py, &cq)
}

/// Result rows (`rows`, columns from `columns` or `select`) → `{column: ndarray}`.
pub fn rows_to_numpy(py: Python<'_>, result: &Val) -> PyResult<Py<PyAny>> {
    let names: Vec<String> = match result.get("columns") {
        Some(Val::List(c)) => c.iter().map(|x| x.py_str()).collect(),
        _ => {
            let mut v = vec!["record_N".to_string()];
            if let Some(Val::List(sel)) = result.get("select") {
                v.extend(sel.iter().map(|x| x.py_str()));
            }
            v
        }
    };
    let rows = result.get("rows").and_then(|r| r.as_list()).cloned().unwrap_or_default();
    let d = PyDict::new(py);
    for name in &names {
        let col: Vec<Val> = rows.iter().map(|r| r.get(name).cloned().unwrap_or(Val::None)).collect();
        d.set_item(name, crate::numpy_io::out_to_numpy(py, &crate::numpy_io::vals_to_out(col))?)?;
    }
    Ok(d.into_any().unbind())
}

/// `expr_from_spec(raw)` → Python expression.
#[pyfunction]
fn expr_from_spec(py: Python<'_>, raw: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if is_expr(raw)? {
        return Ok(raw.clone().unbind());
    }
    expr_to_py(py, &spec_to_expr(raw)?)
}

/// Shortcut for `NHyDB.query`/`explain_query` (fresh engine without runtime providers).
pub fn run_core<T>(db: &nhydb_core::db::Db, f: impl FnOnce(&QueryEngine<'_>) -> nhydb_core::Result<T>) -> PyResult<T> {
    let providers = Providers::default();
    let eng = QueryEngine::new(db, &providers);
    f(&eng).py()
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyQueryEngine>()?;
    m.add_function(wrap_pyfunction!(expr_from_spec, m)?)?;
    m.add_function(wrap_pyfunction!(parse_sql, m)?)?;
    m.add_function(wrap_pyfunction!(_sql_scan, m)?)?;
    m.add("SQL_LANGUAGE_VERSION", sql::SQL_LANGUAGE_VERSION)?;
    m.add("QUERY_VERSION", q::QUERY_VERSION)?;
    Ok(())
}
