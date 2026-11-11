//! `NHyDB.batch()` (P011): opt-in collector for cell writes and deletes.
//!
//! ```python
//! with db.batch() as b:          # commit=False by default
//!     b.put_cell(1, u, v, x, type="f64")
//!     b.delete_cell(1, u, w)
//!     b.delete_record(layout_id, object_id)
//!     b.delete_area(1, u_from, u_to, v_from, v_to)
//! ```
//!
//! Every call is validated immediately (same rules and errors as the single
//! calls); everything is written on leaving the block (or by `flush()`) in
//! one unit: if the block is left by an exception, nothing is written; if
//! writing fails (e.g. size limit), nothing of the batch stays written
//! either (G3-04). Inside the block there is no read-your-writes: reads see
//! the database without the collected operations.
//!
//! The final state is the one the same single calls would leave in the same
//! order. Single cell operations between two record/area deletes are
//! collected per cell (one `put_points` per realm and lane, far fewer and
//! larger SPARSE blocks); a record or area delete acts on the state the
//! calls before it left, so a cell written after it survives and a cell
//! written before it is deleted. The calls of the batch return `None`;
//! `flush()` returns the counters.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use nhydb_core::address;
use nhydb_core::{BatchCell, BatchPhase, BatchWide, CellInput, Error as CoreError};

use crate::conv::*;
use crate::exceptions::SchemaError;
use crate::{layout_ref, type_code, NHyDB};

#[pyclass(module = "nhydb", name = "WriteBatch")]
pub struct WriteBatch {
    db: Py<NHyDB>,
    /// phases in call order; the last one takes new cell operations as
    /// long as it has no wide delete yet
    phases: Vec<BatchPhase>,
    commit: bool,
    u_window: u64,
    done: bool,
    /// collected calls since the last flush (`len()`, R9E-05)
    ops: usize,
    /// consumed by a failed flush (E2)
    failed: bool,
}

impl WriteBatch {
    pub fn new(db: Py<NHyDB>, commit: bool, u_window: u64) -> Self {
        WriteBatch { db, phases: Vec::new(), commit, u_window: u_window.max(1), done: false, ops: 0, failed: false }
    }

    fn check_active(&self) -> PyResult<()> {
        if self.failed {
            return Err(PyRuntimeError::new_err(
                "this batch is consumed: an earlier flush failed and its collected operations were discarded (nothing of them was written)",
            ));
        }
        if self.done {
            return Err(PyRuntimeError::new_err("this batch has already been written or discarded"));
        }
        Ok(())
    }


    /// The phase taking a cell operation: a new one after a wide delete.
    fn cell_phase(&mut self) -> &mut BatchPhase {
        if self.phases.last().is_none_or(|p| !p.wide.is_empty()) {
            self.phases.push(BatchPhase::default());
        }
        self.phases.last_mut().unwrap()
    }

    /// Realm exists, handle writable, no exclusive MATH realm (the checks
    /// of the single delete calls, made at the call).
    fn check_delete_target(&self, py: Python<'_>, realm: &nhydb_core::Nat) -> PyResult<()> {
        let h = self.db.try_borrow(py)?;
        if !h.db.has_realm(realm) {
            return Err(SchemaError::new_err("realm missing"));
        }
        h.db.check_delete_target(realm).py()
    }

    fn flush_inner(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let db_obj = self.db.clone_ref(py);
        let mut h = db_obj.try_borrow_mut(py)?;
        let phases = std::mem::take(&mut self.phases);
        self.ops = 0;
        let db = &mut h.db;
        let commit = self.commit;
        let u_window = self.u_window;
        let st = py
            .detach(|| -> nhydb_core::Result<nhydb_core::BatchStats> {
                let st = db.apply_batch(phases, u_window)?;
                if commit {
                    db.commit()?;
                }
                Ok(st)
            })
            .py()?;
        let d = PyDict::new(py);
        d.set_item("cells", st.cells)?;
        d.set_item("tombstones", st.tombstones)?;
        d.set_item("deleted", st.deleted)?;
        d.set_item("blocks", st.blocks)?;
        d.set_item("bytes", st.bytes)?;
        Ok(d.into_any().unbind())
    }

    fn wide(&mut self, w: BatchWide) {
        if self.phases.is_empty() {
            self.phases.push(BatchPhase::default());
        }
        self.phases.last_mut().unwrap().wide.push(w);
    }
}

#[pymethods]
impl WriteBatch {
    /// Collects a cell (validated now, written at the end of the block).
    #[pyo3(signature = (realm_id, u, v, value, *, r#type=None, lane=None))]
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
    ) -> PyResult<()> {
        self.check_active()?;
        let lane = to_lane(lane)?;
        let typ = r#type.map(|t| type_code(&t)).transpose()?;
        let cell = CellInput::typed(to_nat(u)?, to_nat(v)?, to_val(value)?, typ);
        let realm = to_nat(realm_id)?;
        self.db.try_borrow(py)?.db.check_point(&realm, &cell).py()?;
        let key = (cell.u.clone(), cell.v.clone());
        let cells = self.cell_phase().cells.entry((realm, lane)).or_default();
        let prev = cells.remove(&key);
        cells.insert(key, BatchCell::then_put(prev, cell));
        self.ops += 1;
        Ok(())
    }

    /// Collects the deletion of one cell; at the end of the block a
    /// tombstone is written if the cell then lives (like `delete_cell`).
    #[pyo3(signature = (realm_id, u, v, *, lane=None))]
    fn delete_cell(&mut self, py: Python<'_>, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, v: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<()> {
        self.check_active()?;
        let lane = to_lane(lane)?;
        let (realm, u, v) = (to_nat(realm_id)?, to_nat(u)?, to_nat(v)?);
        self.check_delete_target(py, &realm)?;
        address::require_positive("u", &u).py()?;
        address::validate_local_v(&v, Some(&realm)).py()?;
        let cells = self.cell_phase().cells.entry((realm, lane)).or_default();
        let prev = cells.remove(&(u.clone(), v.clone()));
        cells.insert((u, v), BatchCell::then_delete(prev));
        self.ops += 1;
        Ok(())
    }

    /// Collects the deletion of a whole logical record (like
    /// `delete_record`: every live cell of the object).
    #[pyo3(signature = (layout_id, object_id, *, lane=None))]
    fn delete_record(&mut self, py: Python<'_>, layout_id: &Bound<'_, PyAny>, object_id: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<()> {
        self.check_active()?;
        let lane = to_lane(lane)?;
        let layout = layout_ref(layout_id)?;
        let obj = to_nat(object_id)?;
        {
            let h = self.db.try_borrow(py)?;
            let lay = h.db.delete_layout(&layout).py()?;
            let realm = lay.realm_id.clone();
            drop(h);
            self.check_delete_target(py, &realm)?;
        }
        address::require_positive("object_id", &obj).py()?;
        self.wide(BatchWide::Record { layout, obj, lane });
        self.ops += 1;
        Ok(())
    }

    /// Collects the deletion of every live cell of the rectangle
    /// `u_from..=u_to` × `v_from..=v_to` (like `delete_area`).
    #[pyo3(signature = (realm_id, u_from, u_to, v_from, v_to, *, lane=None))]
    #[allow(clippy::too_many_arguments)]
    fn delete_area(
        &mut self,
        py: Python<'_>,
        realm_id: &Bound<'_, PyAny>,
        u_from: &Bound<'_, PyAny>,
        u_to: &Bound<'_, PyAny>,
        v_from: &Bound<'_, PyAny>,
        v_to: &Bound<'_, PyAny>,
        lane: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        self.check_active()?;
        let lane = to_lane(lane)?;
        let realm = to_nat(realm_id)?;
        let (u_from, u_to, v_from, v_to) = (to_nat(u_from)?, to_nat(u_to)?, to_nat(v_from)?, to_nat(v_to)?);
        self.check_delete_target(py, &realm)?;
        address::require_positive("u_from", &u_from).py()?;
        address::require_positive("u_to", &u_to).py()?;
        address::validate_local_v(&v_from, Some(&realm)).py()?;
        address::validate_local_v(&v_to, Some(&realm)).py()?;
        if u_from > u_to {
            return Err(CoreError::Value(format!("u_from ({u_from}) must not be greater than u_to ({u_to})"))).py();
        }
        if v_from > v_to {
            return Err(CoreError::Value(format!("v_from ({v_from}) must not be greater than v_to ({v_to})"))).py();
        }
        self.wide(BatchWide::Area { realm, lane, u_from, u_to, v_from, v_to });
        self.ops += 1;
        Ok(())
    }

    /// Writes the collected operations now (also done on leaving the
    /// block); returns `{"cells", "tombstones", "deleted", "blocks",
    /// "bytes"}`: value cells written, tombstones of `delete_cell`, live
    /// cells deleted by all delete operations, blocks and bytes appended.
    ///
    /// E2 (R9D-02/R9B-01): a failed flush – the handle in use by another
    /// thread (`RuntimeError`), a write error (nothing of the batch stays
    /// written, earlier uncommitted writes of the handle are kept) or a
    /// failed commit – consumes the batch: the collected operations are
    /// discarded, every further call raises `RuntimeError` and leaving the
    /// block writes and commits nothing.
    fn flush(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.check_active()?;
        let res = self.flush_inner(py);
        if res.is_err() {
            self.phases.clear();
            self.ops = 0;
            self.done = true;
            self.failed = true;
        }
        res
    }

    /// Drops the collected operations without writing.
    fn discard(&mut self) {
        self.phases.clear();
        self.ops = 0;
    }

    /// Number of collected calls (`put_cell`, `delete_cell`,
    /// `delete_record`, `delete_area`) since the last flush.
    fn __len__(&self) -> usize {
        self.ops
    }

    fn __enter__(slf: Bound<'_, Self>) -> PyResult<Bound<'_, Self>> {
        slf.try_borrow()?.check_active()?;
        Ok(slf)
    }

    /// Writes on a normal exit, discards on an exception (never swallows it).
    #[pyo3(signature = (exc_type=None, _exc=None, _tb=None))]
    fn __exit__(
        &mut self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        _exc: Option<&Bound<'_, PyAny>>,
        _tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        if self.done {
            return Ok(false);
        }
        let ok = exc_type.is_none_or(|t| t.is_none());
        let res = if ok { self.flush(py).map(|_| ()) } else { Ok(()) };
        self.phases.clear();
        self.ops = 0;
        self.done = true;
        res.map(|_| false)
    }
}
