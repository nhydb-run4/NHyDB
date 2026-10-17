//! Python API of the index semantics (`nhydb.index`): stateless
//! functions plus `FullTextPosting`, `FullTextCorpus`, `PostingRef`.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyTuple};

use nhydb_core::textindex::{self as ti, FtCorpus, FtPosting, IndexKind};
use nhydb_core::{Nat, Val};

use crate::conv::{from_val, nat_py, to_nat, to_val, IntoPy};

fn opt_val(o: Option<&Bound<'_, PyAny>>) -> PyResult<Val> {
    match o {
        Some(x) if !x.is_none() => to_val(x),
        _ => Ok(Val::None),
    }
}

fn kind_of(kind: &Bound<'_, PyAny>) -> PyResult<IndexKind> {
    ti::normalize_kind(&to_val(kind)?).py()
}

fn str_tuple<'py>(py: Python<'py>, v: Vec<String>) -> PyResult<Bound<'py, PyTuple>> {
    PyTuple::new(py, v)
}

/// Entry of a position-based FULLTEXT posting list.
#[pyclass(name = "FullTextPosting", module = "nhydb", frozen, eq, skip_from_py_object)]
#[derive(Clone, PartialEq)]
pub struct PyFtPosting {
    pub(crate) inner: FtPosting,
}

#[pymethods]
impl PyFtPosting {
    #[new]
    fn new(u: &Bound<'_, PyAny>, positions: &Bound<'_, PyAny>) -> PyResult<Self> {
        let pos: Vec<u64> = positions.try_iter()?.map(|x| x?.extract()).collect::<PyResult<_>>()?;
        Ok(PyFtPosting { inner: FtPosting { u: to_nat(u)?, positions: pos } })
    }
    #[getter]
    fn u(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        nat_py(py, &self.inner.u)
    }
    #[getter]
    fn positions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.inner.positions)
    }
    #[getter]
    fn tf(&self) -> usize {
        self.inner.positions.len()
    }
    fn __repr__(&self) -> String {
        let p: Vec<String> = self.inner.positions.iter().map(|x| x.to_string()).collect();
        let tail = if p.len() == 1 { "," } else { "" };
        format!("FullTextPosting(u={}, positions=({}{}))", self.inner.u, p.join(", "), tail)
    }
}

/// Document lengths and corpus statistics of a FULLTEXT index.
#[pyclass(name = "FullTextCorpus", module = "nhydb", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyFtCorpus {
    pub(crate) inner: FtCorpus,
}

#[pymethods]
impl PyFtCorpus {
    #[getter]
    fn lengths<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (u, l) in &self.inner.lengths {
            d.set_item(nat_py(py, u)?, *l)?;
        }
        Ok(d)
    }
    #[getter]
    fn total_docs(&self) -> u64 {
        self.inner.total_docs
    }
    #[getter]
    fn total_tokens(&self) -> u64 {
        self.inner.total_tokens
    }
    #[getter]
    fn avg_doc_len(&self) -> f64 {
        self.inner.avg_doc_len()
    }
    fn __repr__(&self) -> String {
        format!("FullTextCorpus(total_docs={}, total_tokens={})", self.inner.total_docs, self.inner.total_tokens)
    }
}

/// Directory entry of a SECONDARY block.
#[pyclass(name = "PostingRef", module = "nhydb", frozen)]
pub struct PyPostingRef {
    #[pyo3(get)]
    pub index_id: u64,
    pub value: Val,
    #[pyo3(get)]
    pub abs_offset: u64,
    #[pyo3(get)]
    pub compressed_len: u64,
    #[pyo3(get)]
    pub raw_count: u64,
    #[pyo3(get)]
    pub width: u8,
    #[pyo3(get)]
    pub source_block_id: u64,
}

#[pymethods]
impl PyPostingRef {
    #[getter]
    fn value(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        from_val(py, &self.value)
    }
}

pub(crate) fn postings_from_py(postings: &Bound<'_, PyAny>) -> PyResult<Vec<(String, Vec<FtPosting>)>> {
    let mut out = Vec::new();
    let items = postings.call_method0("items")?;
    for it in items.try_iter()? {
        let (k, v): (String, Bound<'_, PyAny>) = it?.extract()?;
        let mut list = Vec::new();
        for p in v.try_iter()? {
            list.push(posting_from_py(&p?)?);
        }
        out.push((k, list));
    }
    Ok(out)
}

fn posting_from_py(p: &Bound<'_, PyAny>) -> PyResult<FtPosting> {
    if let Ok(x) = p.cast::<PyFtPosting>() {
        return Ok(x.get().inner.clone());
    }
    let u = to_nat(&p.get_item(0)?)?;
    let positions: Vec<u64> = p.get_item(1)?.try_iter()?.map(|x| x?.extract()).collect::<PyResult<_>>()?;
    Ok(FtPosting { u, positions })
}

pub(crate) fn postings_tuple<'py>(py: Python<'py>, v: &[FtPosting]) -> PyResult<Bound<'py, PyTuple>> {
    let items: Vec<Py<PyFtPosting>> = v.iter().map(|p| Py::new(py, PyFtPosting { inner: p.clone() })).collect::<PyResult<_>>()?;
    PyTuple::new(py, items)
}

fn nat_tuple<'py>(py: Python<'py>, v: &[Nat]) -> PyResult<Bound<'py, PyTuple>> {
    let items: Vec<Py<PyAny>> = v.iter().map(|n| nat_py(py, n)).collect::<PyResult<_>>()?;
    PyTuple::new(py, items)
}

#[pyfunction]
fn normalize_index_kind(kind: Option<&Bound<'_, PyAny>>) -> PyResult<&'static str> {
    Ok(ti::normalize_kind(&opt_val(kind)?).py()?.name())
}

#[pyfunction]
#[pyo3(signature = (kind, options=None))]
fn normalize_index_options(py: Python<'_>, kind: Option<&Bound<'_, PyAny>>, options: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
    let k = ti::normalize_kind(&opt_val(kind)?).py()?;
    from_val(py, &ti::normalize_options(k, &opt_val(options)?).py()?)
}

#[pyfunction]
#[pyo3(signature = (value, *, casefold))]
fn normalize_text(value: &Bound<'_, PyAny>, casefold: bool) -> PyResult<String> {
    Ok(ti::normalize_text(&to_val(value)?, casefold))
}

#[pyfunction]
#[pyo3(signature = (value, *, casefold=true))]
fn simple_tokens<'py>(py: Python<'py>, value: &Bound<'py, PyAny>, casefold: bool) -> PyResult<Bound<'py, PyTuple>> {
    str_tuple(py, ti::simple_tokens(&to_val(value)?, casefold))
}

#[pyfunction]
#[pyo3(signature = (value, *, n=3, casefold=false))]
fn ngrams<'py>(py: Python<'py>, value: &Bound<'py, PyAny>, n: usize, casefold: bool) -> PyResult<Bound<'py, PyTuple>> {
    str_tuple(py, ti::ngrams(&to_val(value)?, n, casefold))
}

#[pyfunction]
#[pyo3(signature = (value, options=None))]
fn fulltext_tokens<'py>(py: Python<'py>, value: &Bound<'py, PyAny>, options: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyTuple>> {
    str_tuple(py, ti::fulltext_tokens(&to_val(value)?, &opt_val(options)?).py()?)
}

#[pyfunction]
#[pyo3(signature = (value, options=None))]
fn fulltext_document<'py>(py: Python<'py>, value: &Bound<'py, PyAny>, options: Option<&Bound<'py, PyAny>>) -> PyResult<(u64, Bound<'py, PyDict>)> {
    let (dl, pos) = ti::fulltext_document(&to_val(value)?, &opt_val(options)?).py()?;
    let d = PyDict::new(py);
    for (t, p) in pos {
        d.set_item(t, PyTuple::new(py, p)?)?;
    }
    Ok((dl, d))
}

#[pyfunction]
#[pyo3(signature = (value, options=None))]
fn query_fulltext_sequence<'py>(py: Python<'py>, value: &Bound<'py, PyAny>, options: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyTuple>> {
    str_tuple(py, ti::query_fulltext_sequence(&to_val(value)?, &opt_val(options)?).py()?)
}

#[pyfunction]
#[pyo3(signature = (value, options=None))]
fn query_fulltext_terms<'py>(py: Python<'py>, value: &Bound<'py, PyAny>, options: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyTuple>> {
    str_tuple(py, ti::query_fulltext_terms(&to_val(value)?, &opt_val(options)?).py()?)
}

#[pyfunction]
#[pyo3(signature = (kind, value, options=None))]
fn index_keys<'py>(py: Python<'py>, kind: &Bound<'py, PyAny>, value: &Bound<'py, PyAny>, options: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyTuple>> {
    let k = kind_of(kind)?;
    if k == IndexKind::Value {
        ti::normalize_options(k, &opt_val(options)?).py()?;
        return PyTuple::new(py, [value.clone()]);
    }
    let keys: Vec<Py<PyAny>> = ti::index_keys(k, &to_val(value)?, &opt_val(options)?).py()?.iter().map(|v| from_val(py, v)).collect::<PyResult<_>>()?;
    PyTuple::new(py, keys)
}

#[pyfunction]
#[pyo3(signature = (value, options=None))]
fn query_token_keys<'py>(py: Python<'py>, value: &Bound<'py, PyAny>, options: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyTuple>> {
    str_tuple(py, ti::query_token_keys(&to_val(value)?, &opt_val(options)?).py()?)
}

#[pyfunction]
#[pyo3(signature = (value, options=None))]
fn query_trigram_keys<'py>(py: Python<'py>, value: &Bound<'py, PyAny>, options: Option<&Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyTuple>> {
    str_tuple(py, ti::query_trigram_keys(&to_val(value)?, &opt_val(options)?).py()?)
}

#[pyfunction]
#[pyo3(signature = (kind, op, options=None, predicate_options=None, value=None))]
fn index_supports_predicate(
    kind: &Bound<'_, PyAny>,
    op: &str,
    options: Option<&Bound<'_, PyAny>>,
    predicate_options: Option<&Bound<'_, PyAny>>,
    value: Option<&Bound<'_, PyAny>>,
) -> PyResult<bool> {
    ti::index_supports_predicate(kind_of(kind)?, op, &opt_val(options)?, &opt_val(predicate_options)?, &opt_val(value)?).py()
}

#[pyfunction]
#[pyo3(signature = (actual, op, value=None, value2=None, options=None))]
fn predicate_matches(
    actual: &Bound<'_, PyAny>,
    op: &str,
    value: Option<&Bound<'_, PyAny>>,
    value2: Option<&Bound<'_, PyAny>>,
    options: Option<&Bound<'_, PyAny>>,
) -> PyResult<bool> {
    let _ = value2;
    ti::predicate_matches(&to_val(actual)?, op, &opt_val(value)?, &opt_val(options)?).py()
}

#[pyfunction]
fn encode_fulltext_posting<'py>(py: Python<'py>, entries: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyBytes>> {
    let e: Vec<(Nat, Vec<u64>)> = entries
        .try_iter()?
        .map(|p| posting_from_py(&p?).map(|x| (x.u, x.positions)))
        .collect::<PyResult<_>>()?;
    Ok(PyBytes::new(py, &ti::encode_fulltext_posting(&e).py()?))
}

#[pyfunction]
#[pyo3(signature = (raw, expected_count=None))]
fn decode_fulltext_posting<'py>(py: Python<'py>, raw: &[u8], expected_count: Option<u64>) -> PyResult<Bound<'py, PyTuple>> {
    postings_tuple(py, &ti::decode_fulltext_posting(raw, expected_count).py()?)
}

#[pyfunction]
fn encode_fulltext_doc_lengths<'py>(py: Python<'py>, entries: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyBytes>> {
    let mut e = Vec::new();
    for it in entries.try_iter()? {
        let it = it?;
        let dl: i64 = it.get_item(1)?.extract()?;
        if dl < 0 {
            return Err(crate::exceptions::IndexConfigError::new_err("FULLTEXT document length must not be negative"));
        }
        e.push((to_nat(&it.get_item(0)?)?, dl as u64));
    }
    Ok(PyBytes::new(py, &ti::encode_fulltext_doc_lengths(&e).py()?))
}

#[pyfunction]
#[pyo3(signature = (raw, expected_count=None))]
fn decode_fulltext_doc_lengths(raw: &[u8], expected_count: Option<u64>) -> PyResult<PyFtCorpus> {
    Ok(PyFtCorpus { inner: ti::decode_fulltext_doc_lengths(raw, expected_count).py()? })
}

#[pyfunction]
fn posting_doc_ids<'py>(py: Python<'py>, posting: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyTuple>> {
    let us: Vec<Nat> = posting.try_iter()?.map(|p| posting_from_py(&p?).map(|x| x.u)).collect::<PyResult<_>>()?;
    nat_tuple(py, &us)
}

#[pyfunction]
#[pyo3(signature = (postings, *, mode="all"))]
fn fulltext_candidate_docs<'py>(py: Python<'py>, postings: &Bound<'py, PyAny>, mode: &str) -> PyResult<Bound<'py, PyTuple>> {
    let owned = postings_from_py(postings)?;
    let view: Vec<(String, &[FtPosting])> = owned.iter().map(|(t, p)| (t.clone(), &p[..])).collect();
    nat_tuple(py, &ti::fulltext_candidate_docs(&view, mode).py()?)
}

#[pyfunction]
fn phrase_candidate_docs<'py>(py: Python<'py>, query_sequence: &Bound<'py, PyAny>, postings: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyTuple>> {
    let seq: Vec<String> = query_sequence.try_iter()?.map(|x| x?.str().map(|s| s.to_string())).collect::<PyResult<_>>()?;
    let owned = postings_from_py(postings)?;
    let view: Vec<(String, &[FtPosting])> = owned.iter().map(|(t, p)| (t.clone(), &p[..])).collect();
    // missing terms count as an empty posting list
    nat_tuple(py, &ti::phrase_candidate_docs(&seq, &view))
}

#[pyfunction]
#[pyo3(signature = (postings, corpus, candidates=None, *, k1=1.2, b=0.75))]
fn bm25_scores<'py>(
    py: Python<'py>,
    postings: &Bound<'py, PyAny>,
    corpus: &Bound<'py, PyAny>,
    candidates: Option<&Bound<'py, PyAny>>,
    k1: f64,
    b: f64,
) -> PyResult<Bound<'py, PyDict>> {
    let corpus = corpus.cast::<PyFtCorpus>().map_err(|_| PyValueError::new_err("corpus must be a FullTextCorpus"))?.get().inner.clone();
    let cands: Option<Vec<Nat>> = match candidates {
        Some(c) if !c.is_none() => Some(c.try_iter()?.map(|x| to_nat(&x?)).collect::<PyResult<_>>()?),
        _ => None,
    };
    let owned = postings_from_py(postings)?;
    let view: Vec<(String, &[FtPosting])> = owned.iter().map(|(t, p)| (t.clone(), &p[..])).collect();
    let scores = ti::bm25_scores(&view, &corpus, cands.as_deref(), k1, b).py()?;
    let d = PyDict::new(py);
    for (u, s) in scores {
        d.set_item(nat_py(py, &u)?, s)?;
    }
    Ok(d)
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyFtPosting>()?;
    m.add_class::<PyFtCorpus>()?;
    m.add_class::<PyPostingRef>()?;
    for f in [
        wrap_pyfunction!(normalize_index_kind, m)?,
        wrap_pyfunction!(normalize_index_options, m)?,
        wrap_pyfunction!(normalize_text, m)?,
        wrap_pyfunction!(simple_tokens, m)?,
        wrap_pyfunction!(ngrams, m)?,
        wrap_pyfunction!(fulltext_tokens, m)?,
        wrap_pyfunction!(fulltext_document, m)?,
        wrap_pyfunction!(query_fulltext_sequence, m)?,
        wrap_pyfunction!(query_fulltext_terms, m)?,
        wrap_pyfunction!(index_keys, m)?,
        wrap_pyfunction!(query_token_keys, m)?,
        wrap_pyfunction!(query_trigram_keys, m)?,
        wrap_pyfunction!(index_supports_predicate, m)?,
        wrap_pyfunction!(predicate_matches, m)?,
        wrap_pyfunction!(encode_fulltext_posting, m)?,
        wrap_pyfunction!(decode_fulltext_posting, m)?,
        wrap_pyfunction!(encode_fulltext_doc_lengths, m)?,
        wrap_pyfunction!(decode_fulltext_doc_lengths, m)?,
        wrap_pyfunction!(posting_doc_ids, m)?,
        wrap_pyfunction!(fulltext_candidate_docs, m)?,
        wrap_pyfunction!(phrase_candidate_docs, m)?,
        wrap_pyfunction!(bm25_scores, m)?,
    ] {
        m.add_function(f)?;
    }
    Ok(())
}
