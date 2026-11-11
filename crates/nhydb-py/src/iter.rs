//! Python iterators over the memory-efficient cursors of the core.
//!
//! Like generators, they only start on the first `next()` (errors such as
//! `StaleIndexError` also surface there) and then read in chunks
//! (256 elements per step). Writes to the database between two steps are
//! allowed; the iterator then sees the current state. A `rollback()` after
//! the first step ends the iterator with `NHyDBError` (IT1), including the
//! elements still buffered from the discarded state.

use std::collections::VecDeque;

use pyo3::prelude::*;

use nhydb_core::db::cursor::{FindCursor, RangeCursor, RangeMode, SelectUsCursor, SparsePointCursor};
use nhydb_core::db::materialize::{MaterializedCellCursor, RecordNCursor};
use nhydb_core::{Cond, Lane, Nat, Val};

use crate::conv::{from_val, IntoPy};
use crate::NHyDB;

const BATCH: usize = 256;

pub(crate) enum Pending {
    Range { realm: Nat, lane: Lane, from: Nat, to: Nat, mode: RangeMode },
    Find { realm: Nat, lane: Lane, filters: Vec<(String, Cond)>, u_from: Option<Nat>, u_to: Option<Nat>, dyn_mode: Option<String>, dyn_v: Option<Nat> },
    Sparse { realm: Option<Nat>, lane: Option<Lane> },
    SelectUs { realm: Nat, lane: Lane, us: Vec<Nat>, columns: Vec<Val> },
    Cells { realm: Option<Nat>, lane: Option<Lane> },
    RecordNs { realm: Option<Nat>, lane: Option<Lane> },
    /// R4-B: rows of a SELECT (`sql_iter`)
    Sql { text: String, params: nhydb_core::sql::Params, lane: Lane },
}

enum State {
    Pending(Pending),
    Range(RangeCursor),
    Find(FindCursor),
    Sparse(Box<SparsePointCursor>),
    SelectUs(SelectUsCursor),
    Cells(Box<MaterializedCellCursor>),
    RecordNs(Box<RecordNCursor>),
    Sql(Box<nhydb_core::sql::SqlCursor>),
    Done,
}

#[pyclass(name = "ResultIterator", module = "nhydb")]
pub struct ResultIterator {
    db: Py<NHyDB>,
    state: State,
    buf: VecDeque<Val>,
    /// IT1: cursor epoch of the database object when the iteration started.
    epoch: Option<u64>,
}

impl ResultIterator {
    pub(crate) fn new(db: Py<NHyDB>, p: Pending) -> ResultIterator {
        ResultIterator { db, state: State::Pending(p), buf: VecDeque::new(), epoch: None }
    }

    fn fill(&mut self, py: Python<'_>) -> PyResult<()> {
        let dbref = self.db.try_borrow(py)?;
        let db = &dbref.db;
        if let State::Pending(_) = &self.state {
            let State::Pending(p) = std::mem::replace(&mut self.state, State::Done) else { unreachable!() };
            self.epoch = Some(db.cursor_epoch());
            self.state = match p {
                Pending::Range { realm, lane, from, to, mode } => State::Range(RangeCursor::new(realm, lane, from, to, mode)),
                Pending::Find { realm, lane, filters, u_from, u_to, dyn_mode, dyn_v } => {
                    let us = db.find_u(&realm, &filters, lane, u_from.as_ref(), u_to.as_ref()).py()?;
                    State::Find(FindCursor::new(realm, lane, us, dyn_mode, dyn_v))
                }
                Pending::Sparse { realm, lane } => State::Sparse(Box::new(SparsePointCursor::new(db, realm.as_ref(), lane))),
                Pending::SelectUs { realm, lane, us, columns } => State::SelectUs(SelectUsCursor::new(realm, lane, us, columns)),
                Pending::Cells { realm, lane } => State::Cells(Box::new(MaterializedCellCursor::new(db, realm.as_ref(), lane))),
                Pending::RecordNs { realm, lane } => State::RecordNs(Box::new(RecordNCursor::new(db, realm.as_ref(), lane))),
                // planning and candidate search without the GIL (P009)
                Pending::Sql { text, params, lane } => {
                    let c = crate::queryapi::run_core(py, db, |eng| nhydb_core::sql::open_sql(eng, &text, &params, lane, false))?;
                    State::Sql(Box::new(c))
                }
            };
        }
        let batch: Vec<Val> = match &mut self.state {
            State::Range(c) => c.next_batch(db, BATCH).py()?,
            State::Find(c) => c.next_batch(db, BATCH).py()?,
            State::SelectUs(c) => c.next_batch(db, BATCH).py()?,
            State::Sparse(c) => c.next_batch(db, BATCH).py()?.iter().map(|p| p.to_val()).collect(),
            State::Cells(c) => c.next_batch(db, BATCH).py()?,
            State::RecordNs(c) => c.next_batch(db, BATCH).py()?.into_iter().map(Val::Str).collect(),
            // a chunk keeps the GIL like the other iterators: releasing it
            // for every short chunk lets busy Python threads delay each
            // re-acquisition by a switch interval (GIL convoy)
            State::Sql(c) => {
                let providers = nhydb_core::query::Providers::default();
                let eng = nhydb_core::query::QueryEngine::new(db, &providers);
                c.next_batch(&eng, BATCH, None).py()?
            }
            _ => Vec::new(),
        };
        // RUN¹⁰ (R4): a WAIT signal met while this thread held the GIL is
        // sat out now, without it
        if let Some(w) = nhydb_core::limiter::job::take_owed_wait() {
            py.detach(|| w.wait()).py()?;
        }
        if batch.is_empty() {
            self.state = State::Done;
        }
        self.buf.extend(batch);
        Ok(())
    }
}

#[pymethods]
impl ResultIterator {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if !(self.buf.is_empty() && matches!(self.state, State::Done)) {
            // F028: a closed database ends every open iterator with the same
            // error, independent of what is still buffered. IT1: after a
            // rollback neither buffered elements nor further batches may be
            // returned (they can belong to the discarded state).
            let dbref = self.db.try_borrow(py)?;
            let err = if dbref.db.closed {
                Some(crate::exceptions::NHyDBError::new_err("database is closed"))
            } else if self.epoch.is_some() {
                dbref.db.check_cursor_epoch(&mut self.epoch).err().map(crate::conv::to_pyerr)
            } else {
                None
            };
            drop(dbref);
            if let Some(e) = err {
                self.buf.clear();
                self.state = State::Done;
                return Err(e);
            }
        }
        if self.buf.is_empty() && !matches!(self.state, State::Done) {
            self.fill(py)?;
        }
        match self.buf.pop_front() {
            Some(v) => Ok(Some(from_val(py, &v)?)),
            None => Ok(None),
        }
    }
}
