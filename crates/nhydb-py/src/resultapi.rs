//! Result files and background result jobs (0.28.2 round 4, stage C):
//! `NHyDB.sql_to_file`, `NHyDB.submit_sql`, `ResultFile`, `ResultJob`,
//! `cleanup_results`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use nhydb_core::container::ContainerReader;
use nhydb_core::results::{self, JobState, Progress, ResultTarget};
use nhydb_core::Val;

use crate::conv::{from_val, IntoPy};

/// An open result file: the rows of a query in a result container. Managed
/// files (written without an explicit path) are deleted on `close()` or
/// when the object is released, unless pinned.
#[pyclass(name = "ResultFile", module = "nhydb")]
pub struct PyResultFile {
    inner: Mutex<Option<results::ResultFile>>,
    /// U4 (RUN8): `[query] max_result_bytes` of the writing handle (or of
    /// the configuration found for an opened file) for `fetchall`/`to_numpy`
    limit: u64,
}

impl PyResultFile {
    pub(crate) fn wrap(rf: results::ResultFile, limit: u64) -> PyResultFile {
        PyResultFile { inner: Mutex::new(Some(rf)), limit }
    }

    fn with<T>(&self, f: impl FnOnce(&mut results::ResultFile) -> PyResult<T>) -> PyResult<T> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_mut() {
            Some(rf) => f(rf),
            None => Err(crate::exceptions::NHyDBError::new_err("result file is closed")),
        }
    }
}

#[pymethods]
impl PyResultFile {
    /// Opens an existing result file (header and footer checksums are
    /// checked; every chunk when it is read). Never deleted automatically.
    #[new]
    fn new(path: PathBuf) -> PyResult<Self> {
        // the configuration that would apply to a database next to the file
        // (NHYDB_CONFIG, nhydb_config.toml, built-in default); a broken one
        // must not make the file unreadable
        let limit = nhydb_core::Config::load(None, &path)
            .map(|c| c.query_max_result_bytes)
            .unwrap_or(nhydb_core::config::DEFAULT_MAX_RESULT_BYTES);
        Ok(PyResultFile::wrap(results::ResultFile::open(&path).py()?, limit))
    }

    #[getter]
    fn path(&self) -> PyResult<PathBuf> {
        self.with(|r| Ok(r.path().to_path_buf()))
    }
    #[getter]
    fn columns(&self) -> PyResult<Vec<String>> {
        self.with(|r| Ok(r.columns()))
    }
    /// Number of rows.
    #[getter]
    fn rows(&self) -> PyResult<u64> {
        self.with(|r| Ok(r.rows()))
    }
    /// File size in bytes.
    #[getter]
    fn size(&self) -> PyResult<u64> {
        self.with(|r| Ok(r.info.file_bytes))
    }
    #[getter]
    fn managed(&self) -> PyResult<bool> {
        self.with(|r| Ok(r.managed()))
    }
    #[getter]
    fn pinned(&self) -> PyResult<bool> {
        self.with(|r| Ok(r.pinned()))
    }
    /// Header metadata (`columns`, `query`, `created`).
    #[getter]
    fn meta(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with(|r| from_val(py, &r.info.header_meta))
    }

    /// Iterator over the rows (dicts, read chunk by chunk).
    fn __iter__(&self) -> PyResult<ResultRows> {
        let reader = self.with(|r| r.reader().py())?;
        Ok(ResultRows { reader: Some(reader), buf: std::collections::VecDeque::new() })
    }

    fn __len__(&self) -> PyResult<usize> {
        self.with(|r| Ok(r.rows() as usize))
    }

    /// All rows as a list of dicts (bounded by `[query] max_result_bytes`
    /// like `sql()`; iterate the file for larger results).
    fn fetchall(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let mut reader = self.with(|r| r.reader().py())?;
        let mut guard = FetchGuard::new(self.limit);
        let l = PyList::empty(py);
        loop {
            let batch = reader.next_values(4096).py()?;
            if batch.is_empty() {
                break;
            }
            for v in &batch {
                guard.add_gil(py, v)?;
                l.append(from_val(py, v)?)?;
            }
        }
        Ok(l.into_any().unbind())
    }

    /// `{column: numpy.ndarray}` like `NHyDB.sql_numpy`.
    fn to_numpy(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        use crate::numpy_io::ColBuilder;
        let (names, mut reader) = self.with(|r| Ok((r.columns(), r.reader().py()?)))?;
        let limit = self.limit;
        let cols = py
            .detach(|| -> nhydb_core::Result<Vec<ColBuilder>> {
                // the guard (limiter job) first: it ends after the columns (U2)
                let mut guard = FetchGuard::new(limit);
                let last: Vec<bool> = (0..names.len()).map(|j| !names[j + 1..].contains(&names[j])).collect();
                let mut cols: Vec<ColBuilder> = names.iter().map(|_| ColBuilder::default()).collect();
                while let Some(mut row) = reader.next_value()? {
                    guard.add(&row)?;
                    for (j, name) in names.iter().enumerate() {
                        let v = match &mut row {
                            Val::Map(items) => match items.iter_mut().find(|(k, _)| k == name) {
                                Some((_, v)) if last[j] => std::mem::take(v),
                                Some((_, v)) => v.clone(),
                                None => Val::None,
                            },
                            _ => Val::None,
                        };
                        cols[j].push(v);
                    }
                }
                Ok(cols)
            })
            .py()?;
        let d = PyDict::new(py);
        for (name, col) in names.iter().zip(cols) {
            d.set_item(name, crate::numpy_io::out_to_numpy(py, col.finish())?)?;
        }
        Ok(d.into_any().unbind())
    }

    /// Keeps a managed file after `close()` (renamed to `….pinned.nhyc`).
    fn pin(&self) -> PyResult<()> {
        self.with(|r| r.pin().py())
    }

    /// A pinned managed file becomes temporary again.
    fn unpin(&self) -> PyResult<()> {
        self.with(|r| r.unpin().py())
    }

    /// Deletes the file (also pinned and caller-owned files) and closes.
    fn delete(&self) -> PyResult<()> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut rf) = g.take() {
            rf.delete().py()?;
        }
        Ok(())
    }

    /// Closes the handle; an unpinned managed file is deleted.
    fn close(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut rf) = g.take() {
            rf.close();
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, _args: &Bound<'_, pyo3::types::PyTuple>) {
        self.close();
    }

    fn __repr__(&self) -> String {
        match self.inner.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(r) => format!("ResultFile({:?}, rows={}, pinned={})", r.path().display().to_string(), r.rows(), r.pinned()),
            None => "ResultFile(closed)".into(),
        }
    }
}

/// Rows of a result file.
#[pyclass(name = "ResultRows", module = "nhydb")]
pub struct ResultRows {
    reader: Option<ContainerReader>,
    buf: std::collections::VecDeque<Val>,
}

#[pymethods]
impl ResultRows {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if self.buf.is_empty() {
            if let Some(r) = self.reader.as_mut() {
                // short chunk: keeps the GIL (see `iter.rs`)
                let batch = r.next_values(256).py()?;
                if batch.is_empty() {
                    self.reader = None;
                }
                self.buf.extend(batch);
            }
        }
        match self.buf.pop_front() {
            Some(v) => Ok(Some(from_val(py, &v)?)),
            None => Ok(None),
        }
    }
}

/// A query running in the background on a read-only snapshot.
#[pyclass(name = "ResultJob", module = "nhydb")]
pub struct PyResultJob {
    st: Arc<JobState>,
    taken: Mutex<Option<Py<PyResultFile>>>,
    limit: u64,
}

#[pymethods]
impl PyResultJob {
    /// Whether the job has finished (successfully or with an error).
    fn done(&self) -> bool {
        self.st.is_done()
    }

    /// Rows written so far.
    #[getter]
    fn rows_written(&self) -> u64 {
        self.st.progress.rows.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Asks the job to stop; `wait()` then raises `QueryError`.
    fn cancel(&self) {
        self.st.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Waits for the job (`timeout` in seconds; `None` = without limit) and
    /// returns its `ResultFile`; raises the error of the job. `TimeoutError`
    /// when the time runs out.
    #[pyo3(signature = (timeout=None))]
    fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyResultFile>> {
        if let Some(rf) = self.taken.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            return Ok(rf.clone_ref(py));
        }
        let st = self.st.clone();
        // RD4-04: NaN is an error; inf and values beyond `Duration` wait
        // without limit; negative values do not wait
        let t = match timeout {
            Some(s) if s.is_nan() => return Err(pyo3::exceptions::PyValueError::new_err("timeout must be a number of seconds, not NaN")),
            Some(s) => std::time::Duration::try_from_secs_f64(s.max(0.0)).ok(),
            None => None,
        };
        if py.detach(move || st.wait(t)).is_none() {
            return Err(pyo3::exceptions::PyTimeoutError::new_err("result job still running"));
        }
        let mut g = self.taken.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rf) = g.as_ref() {
            return Ok(rf.clone_ref(py));
        }
        match self.st.take() {
            Some(Ok(rf)) => {
                let obj = Py::new(py, PyResultFile::wrap(rf, self.limit))?;
                *g = Some(obj.clone_ref(py));
                Ok(obj)
            }
            Some(Err(e)) => Err(crate::conv::to_pyerr(e)),
            None => Err(crate::exceptions::NHyDBError::new_err("result job result was already taken")),
        }
    }
}

pub(crate) fn job(st: Arc<JobState>, limit: u64) -> PyResultJob {
    PyResultJob { st, taken: Mutex::new(None), limit }
}

/// U4 (RUN8): the bytes `fetchall`/`to_numpy` hold, booked like the rows of
/// a collected `sql()` result (0 = no limit).
struct FetchGuard {
    limit: u64,
    used: u64,
    rows: u64,
    /// RUN¹⁰: collecting job of the RAM limiter (checkpoint per MiB)
    meter: nhydb_core::limiter::Meter,
}

impl FetchGuard {
    fn new(limit: u64) -> FetchGuard {
        FetchGuard {
            limit,
            used: 0,
            rows: 0,
            meter: nhydb_core::limiter::Meter::new(nhydb_core::limiter::JobKind::Rigid, nhydb_core::limiter::JobPath::Collect),
        }
    }

    /// Books a row while the GIL is held (waits only after releasing it).
    fn add_gil(&mut self, py: Python<'_>, row: &Val) -> PyResult<()> {
        let n = self.book(row).map_err(crate::exceptions::QueryLimitError::new_err)?;
        if self.meter.due(n) {
            crate::conv::limiter_checkpoint(py, &mut self.meter)?;
        }
        Ok(())
    }

    /// Books a row without the GIL (may wait for the limiter). The limit
    /// error keeps its former text (`QueryLimitError: …`).
    fn add(&mut self, row: &Val) -> nhydb_core::Result<()> {
        let n = self.book(row).map_err(|m| nhydb_core::Error::QueryLimit(format!("QueryLimitError: {m}")))?;
        self.meter.grow(n).map(|_| ())
    }

    fn book(&mut self, row: &Val) -> Result<u64, String> {
        self.rows += 1;
        let n = (std::mem::size_of::<Val>() + nhydb_core::db::val_heap(row)) as u64;
        self.used += n;
        if self.limit == 0 || self.used <= self.limit {
            return Ok(n);
        }
        Err(format!(
            "result file too large to load at once: about {} bytes after {} rows exceed [query] max_result_bytes = {}; \
             iterate the ResultFile (for row in rf) instead of fetchall()/to_numpy(), or raise max_result_bytes (0 = no limit)",
            self.used, self.rows, self.limit
        ))
    }
}

pub(crate) fn target(path: Option<PathBuf>, compress: Option<bool>) -> ResultTarget {
    ResultTarget { path, compress }
}

pub(crate) fn write_now(py: Python<'_>, db: &nhydb_core::db::Db, text: &str, params: &nhydb_core::sql::Params, lane: nhydb_core::Lane, t: &ResultTarget) -> PyResult<PyResultFile> {
    let rf = crate::queryapi::run_core(py, db, |eng| results::write_sql(eng, text, params, lane, t, &Progress::default()))?;
    Ok(PyResultFile::wrap(rf, db.config.query_max_result_bytes))
}

/// `cleanup_results(directory=None)`: removes `.part` files of interrupted
/// writers and unpinned result files of ended processes from the result
/// directory (default: the one of the built-in configuration or
/// `NHYDB_CONFIG`). Pinned files stay. Returns the number of removed files.
#[pyfunction]
#[pyo3(signature = (directory=None))]
fn cleanup_results(py: Python<'_>, directory: Option<PathBuf>) -> PyResult<u64> {
    let dir = match directory {
        Some(d) => d,
        None => {
            let cfg = match std::env::var_os(nhydb_core::config::CONFIG_ENV).filter(|p| !p.is_empty()) {
                Some(p) => nhydb_core::Config::from_file(std::path::Path::new(&p)).py()?,
                None => nhydb_core::Config::default(),
            };
            results::result_dir(&cfg)
        }
    };
    py.detach(|| results::cleanup_results(&dir)).py()
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyResultFile>()?;
    m.add_class::<ResultRows>()?;
    m.add_class::<PyResultJob>()?;
    m.add_function(wrap_pyfunction!(cleanup_results, m)?)?;
    Ok(())
}
