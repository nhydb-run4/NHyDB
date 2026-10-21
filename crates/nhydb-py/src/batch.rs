//! `NHyDB.batch()` (P011): opt-in collector for single cell writes.
//!
//! ```python
//! with db.batch() as b:          # commit=False by default
//!     b.put_cell(1, u, v, x, type="f64")
//!     b.delete_cell(1, u, w)
//! ```
//!
//! Every call is validated immediately (same rules and errors as
//! `put_cell`/`delete_cell`); the cells are written on leaving the block with
//! one `put_points` per realm and lane (far fewer, larger SPARSE blocks).
//! Inside the block there is no read-your-writes: reads see the database
//! without the collected cells. The final state of every cell is the one
//! the same single calls would leave (a `delete_cell` on a null or missing
//! cell is a no-op, after a non-null `put_cell` it deletes). If the block
//! is left by an exception, nothing is written; if writing fails (e.g. size
//! limit), nothing of the batch stays written either (G3-04). `put_cell`/`delete_cell` of
//! the batch return `None` (no block metadata).

use std::collections::BTreeMap;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use nhydb_core::address::{self, Lane};
use nhydb_core::{CellInput, Nat, Val};

use crate::conv::*;
use crate::exceptions::SchemaError;
use crate::{type_code, NHyDB};

/// Collected state of one cell: the net effect of all calls on it so far,
/// resolved exactly as the single calls would act in sequence (G3-02).
enum Op {
    /// last call was a `put_cell` (value may be `None`: null cell)
    Put(CellInput),
    /// only `delete_cell` calls so far: a tombstone is written at the flush
    /// only if the database then has a non-null value (like `delete_cell`)
    Delete,
    /// `delete_cell` after a non-null `put_cell` of this batch: the single
    /// calls would write the value and then a tombstone; the tombstone
    /// alone gives the same final cell state
    Tomb,
}

impl Op {
    /// State after a further `delete_cell` on this cell. A delete is a no-op
    /// on a null cell (`delete_cell` returns `False`), so a collected null
    /// put stays; after a non-null put the delete always takes effect.
    fn then_delete(prev: Option<Op>) -> Op {
        match prev {
            None | Some(Op::Delete) => Op::Delete,
            Some(Op::Tomb) => Op::Tomb,
            Some(Op::Put(c)) if matches!(c.value, Val::None) => Op::Put(c),
            Some(Op::Put(_)) => Op::Tomb,
        }
    }
}

#[pyclass(module = "nhydb", name = "WriteBatch")]
pub struct WriteBatch {
    db: Py<NHyDB>,
    pending: BTreeMap<(Nat, Lane), BTreeMap<(Nat, Nat), Op>>,
    commit: bool,
    u_window: u64,
    done: bool,
}

impl WriteBatch {
    pub fn new(db: Py<NHyDB>, commit: bool, u_window: u64) -> Self {
        WriteBatch { db, pending: BTreeMap::new(), commit, u_window: u_window.max(1), done: false }
    }

    fn check_active(&self) -> PyResult<()> {
        if self.done {
            return Err(PyRuntimeError::new_err("this batch has already been written or discarded"));
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.pending.values().map(|m| m.len()).sum()
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
        self.db.borrow(py).db.check_point(&realm, &cell).py()?;
        self.pending.entry((realm, lane)).or_default().insert((cell.u.clone(), cell.v.clone()), Op::Put(cell));
        Ok(())
    }

    /// Collects a deletion; at the end of the block a tombstone is written
    /// only if the cell then has a value (like `delete_cell`).
    #[pyo3(signature = (realm_id, u, v, *, lane=None))]
    fn delete_cell(&mut self, py: Python<'_>, realm_id: &Bound<'_, PyAny>, u: &Bound<'_, PyAny>, v: &Bound<'_, PyAny>, lane: Option<&Bound<'_, PyAny>>) -> PyResult<()> {
        self.check_active()?;
        let lane = to_lane(lane)?;
        let (realm, u, v) = (to_nat(realm_id)?, to_nat(u)?, to_nat(v)?);
        {
            let h = self.db.borrow(py);
            if !h.db.has_realm(&realm) {
                return Err(SchemaError::new_err("realm missing"));
            }
        }
        address::require_positive("u", &u).py()?;
        address::validate_local_v(&v, Some(&realm)).py()?;
        let cells = self.pending.entry((realm, lane)).or_default();
        let prev = cells.remove(&(u.clone(), v.clone()));
        cells.insert((u, v), Op::then_delete(prev));
        Ok(())
    }

    /// Writes the collected cells now (also done on leaving the block);
    /// returns `{"cells", "tombstones", "blocks", "bytes"}`.
    fn flush(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.check_active()?;
        let pending = std::mem::take(&mut self.pending);
        let mut h = self.db.borrow_mut(py);
        let db = &mut h.db;
        let (mut cells_n, mut tombs_n, mut blocks, mut bytes) = (0i64, 0i64, 0i64, 0i64);
        let commit = self.commit;
        let u_window = self.u_window;
        py.detach(|| -> nhydb_core::Result<()> {
            // reads first (tombstone decisions), then all groups as one unit:
            // a failing group leaves nothing of the batch behind (G3-04)
            let mut groups = Vec::with_capacity(pending.len());
            for ((realm, lane), ops) in pending {
                let mut cells = Vec::with_capacity(ops.len());
                for ((u, v), op) in ops {
                    match op {
                        Op::Put(c) => cells.push(c),
                        Op::Delete => {
                            if !matches!(db.get_cell_value(&realm, &u, &v, lane)?, None | Some(Val::None)) {
                                tombs_n += 1;
                                cells.push(CellInput::tombstone(u, v));
                            }
                        }
                        Op::Tomb => {
                            tombs_n += 1;
                            cells.push(CellInput::tombstone(u, v));
                        }
                    }
                }
                if !cells.is_empty() {
                    groups.push((realm, lane, cells));
                }
            }
            for r in db.put_point_groups(groups, u_window)? {
                cells_n += r.get("cells").and_then(|x| x.as_i64()).unwrap_or(0);
                blocks += r.get("blocks").and_then(|x| x.as_i64()).unwrap_or(0);
                bytes += r.get("bytes").and_then(|x| x.as_i64()).unwrap_or(0);
            }
            if commit {
                db.commit()?;
            }
            Ok(())
        })
        .py()?;
        let d = PyDict::new(py);
        d.set_item("cells", cells_n - tombs_n)?;
        d.set_item("tombstones", tombs_n)?;
        d.set_item("blocks", blocks)?;
        d.set_item("bytes", bytes)?;
        Ok(d.into_any().unbind())
    }

    /// Drops the collected cells without writing.
    fn discard(&mut self) {
        self.pending.clear();
    }

    fn __len__(&self) -> usize {
        self.len()
    }

    fn __enter__(slf: Bound<'_, Self>) -> PyResult<Bound<'_, Self>> {
        slf.borrow().check_active()?;
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
        self.pending.clear();
        self.done = true;
        res.map(|_| false)
    }
}
