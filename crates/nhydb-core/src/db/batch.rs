//! Opt-in write grouping (P011) and explicit rollback (P024).
//!
//! `check_point` validates a single cell exactly like `put_points` without
//! writing anything, so that a write batch of the binding layer reports
//! errors at the call that caused them and writes all collected cells with
//! one `put_points` per realm/lane later. `rollback` discards everything
//! written since the last commit of this handle.

use std::collections::{BTreeMap, HashSet};

use crate::address::{require_positive, validate_local_v, Lane};
use crate::error::{Error, Result};
use crate::nat::Nat;
use crate::val::Val;

use super::sparse::CellInput;
use super::{system, Db};

/// Tombstones buffered across consecutive record deletes of a batch.
const BATCH_TOMB_CHUNK: usize = super::delete::AREA_CHUNK_CELLS;

/// Single cell operations of a phase written per `put_points` (bounded
/// memory; within a phase every cell occurs once, so the order of the
/// chunks does not matter).
const BATCH_CELL_CHUNK: usize = 65_536;

/// Collected state of one cell in a batch phase: the net effect of the
/// calls on it, resolved as the single calls would act in sequence.
///
/// R9A-02: `pre` records a `delete_cell` before the first `put_cell` of
/// the phase (it counts 1 in `deleted` if the cell lived at the start of
/// the phase), `n` the `delete_cell` calls that followed a `put_cell` (each
/// deleted a live cell). `deleted` thus equals the sum of the single calls.
#[derive(Clone, Debug)]
pub enum BatchCell {
    /// last call was a `put_cell` (the value may be `None`: a null cell);
    /// boxed, so that the (many) delete entries stay small
    Put { cell: Box<CellInput>, pre: bool, n: u32 },
    /// only `delete_cell` calls: a tombstone is written at the flush if the
    /// cell then lives (a stored `None` included), like `delete_cell`
    Delete,
    /// `delete_cell` after a `put_cell` of this phase: the cell lived, so
    /// the tombstone alone gives the final state
    Tomb { pre: bool, n: u32 },
}

impl BatchCell {
    /// State after a further `delete_cell` on this cell.
    pub fn then_delete(prev: Option<BatchCell>) -> BatchCell {
        match prev {
            None | Some(BatchCell::Delete) => BatchCell::Delete,
            Some(BatchCell::Tomb { pre, n }) => BatchCell::Tomb { pre, n },
            Some(BatchCell::Put { pre, n, .. }) => BatchCell::Tomb { pre, n: n.saturating_add(1) },
        }
    }

    /// State after a further `put_cell` on this cell.
    pub fn then_put(prev: Option<BatchCell>, cell: CellInput) -> BatchCell {
        let (pre, n) = match prev {
            None => (false, 0),
            Some(BatchCell::Delete) => (true, 0),
            Some(BatchCell::Put { pre, n, .. }) | Some(BatchCell::Tomb { pre, n }) => (pre, n),
        };
        BatchCell::Put { cell: Box::new(cell), pre, n }
    }
}

/// A record or area delete of a batch.
#[derive(Clone, Debug)]
pub enum BatchWide {
    Record { layout: super::layout::LayoutRef, obj: Nat, lane: Lane },
    Area { realm: Nat, lane: Lane, u_from: Nat, u_to: Nat, v_from: Nat, v_to: Nat },
}

/// One phase of a batch: single cell operations, then wide deletes.
#[derive(Clone, Debug, Default)]
pub struct BatchPhase {
    pub cells: BTreeMap<(Nat, Lane), BTreeMap<(Nat, Nat), BatchCell>>,
    pub wide: Vec<BatchWide>,
}

/// Result of [`Db::apply_batch`].
#[derive(Clone, Debug, Default)]
pub struct BatchStats {
    /// value cells written (a stored `None` included)
    pub cells: u64,
    /// tombstones written by `delete_cell` operations
    pub tombstones: u64,
    /// live cells deleted by all delete operations (`delete_cell` counts 1
    /// when the cell lived, `delete_record`/`delete_area` their counts)
    pub deleted: u64,
    /// blocks and bytes the batch appended to the file
    pub blocks: u64,
    pub bytes: u64,
}

/// File and block state at the start of a scoped write (`Db::write_scope`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct WriteMark {
    len: u64,
    next_block_id: u64,
    last_valid_end: u64,
    layout_dirty: bool,
}

impl Db {
    /// Validates one cell like `put_points` (open/writable handle, realm,
    /// exclusive MATH realm, u/v range, data type and encoding) without
    /// writing anything (P011).
    pub fn check_point(&self, realm: &Nat, cell: &CellInput) -> Result<()> {
        self.check_open()?;
        if self.readonly {
            return Err(Error::NHyDB("database is opened read-only (readonly=True); writing is not possible".into()));
        }
        self.require_realm(realm)?;
        if let Some(owner) = self.math_owner_of_realm(realm) {
            if !self.internal_math_writes {
                return Err(Error::Schema(format!(
                    "Realm {realm} belongs exclusively to MATH layout {owner}; direct put_points blocked"
                )));
            }
        }
        require_positive("u", &cell.u)?;
        validate_local_v(&cell.v, Some(realm))?;
        let mut m = BTreeMap::new();
        m.insert(cell.u.clone(), vec![cell.clone()]);
        self.encode_sparse_segment(realm, &mut m)?;
        Ok(())
    }

    /// Writes several `(realm, lane)` groups of cells (one `put_points` per
    /// group, without commit) as one unit (G3-04): if a group fails (size
    /// limit, I/O error, realm gone), the file is cut back to its length
    /// before the first group and the in-memory state (segments, overlays,
    /// deletes, index freshness, caches and verified registry, schema) is
    /// rebuilt as it was – the same append rollback as for a failed MATH
    /// batch. Earlier uncommitted writes of the handle are kept. If the
    /// rollback itself fails, the handle is poisoned (reopen required).
    /// Returns the `put_points` results per group.
    pub fn put_point_groups(&mut self, groups: Vec<(Nat, Lane, Vec<CellInput>)>, u_window: u64) -> Result<Vec<Val>> {
        self.write_scope(|db| {
            let mut out = Vec::with_capacity(groups.len());
            for (realm, lane, cells) in groups {
                out.push(db.put_points(&realm, cells, lane, false, u_window)?);
            }
            Ok(out)
        })
    }

    /// Applies a write batch (`NHyDB.batch()`) as one unit: the phases in
    /// call order, each first its single cell operations (net state per
    /// cell), then its record and area deletes in order. Every operation
    /// sees the state the operations before it left, exactly as the single
    /// calls would; on any error nothing of the batch stays (`write_scope`).
    /// Consecutive record deletes share one tombstone buffer (written per
    /// realm and lane in chunks), so millions of them do not produce
    /// millions of small blocks.
    pub fn apply_batch(&mut self, phases: Vec<BatchPhase>, u_window: u64) -> Result<BatchStats> {
        let (blocks0, len0) = (self.next_block_id, self.store.len());
        let mut st = self.write_scope(|db| {
            let mut st = BatchStats::default();
            let mut buf: BTreeMap<(Nat, Lane), Vec<CellInput>> = BTreeMap::new();
            let mut buffered = 0usize;
            // records deleted since the buffer took tombstones without a
            // cell operation in between: a repeat finds nothing to delete
            let mut seen: HashSet<(Nat, Lane, Nat)> = HashSet::new();
            // E5 (R9D-05): the batch takes part in the RAM limiter: progress
            // per operation, checkpoints per meter step (a hold waits, a
            // cancel rolls the whole batch back – this write_scope)
            let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Write);
            // after a flush the file holds every delete so far: a repeat
            // finds nothing by itself, so `seen` is cleared with it
            let flush = |db: &mut Db, buf: &mut BTreeMap<(Nat, Lane), Vec<CellInput>>, buffered: &mut usize, seen: &mut HashSet<(Nat, Lane, Nat)>| -> Result<()> {
                for ((realm, lane), cells) in std::mem::take(buf) {
                    if !cells.is_empty() {
                        db.put_points(&realm, cells, lane, false, u64::MAX)?;
                    }
                }
                *buffered = 0;
                seen.clear();
                Ok(())
            };
            for phase in phases {
                if !phase.cells.is_empty() {
                    flush(db, &mut buf, &mut buffered, &mut seen)?;
                    seen.clear();
                    for ((realm, lane), ops) in phase.cells {
                        let mut cells = Vec::with_capacity(ops.len().min(BATCH_CELL_CHUNK));
                        for ((u, v), op) in ops {
                            meter.grow(super::delete::DELETE_U_COST)?;
                            if cells.len() >= BATCH_CELL_CHUNK {
                                db.put_points(&realm, std::mem::take(&mut cells), lane, false, u_window)?;
                            }
                            match op {
                                BatchCell::Put { cell, pre, n } => {
                                    if pre && db.live_cell_value(&realm, &u, &v, lane)?.is_some() {
                                        st.deleted += 1;
                                    }
                                    st.deleted += n as u64;
                                    st.cells += 1;
                                    cells.push(*cell);
                                }
                                BatchCell::Delete => {
                                    if db.live_cell_value(&realm, &u, &v, lane)?.is_some() {
                                        st.tombstones += 1;
                                        st.deleted += 1;
                                        cells.push(CellInput::tombstone(u, v));
                                    }
                                }
                                BatchCell::Tomb { pre, n } => {
                                    if pre && db.live_cell_value(&realm, &u, &v, lane)?.is_some() {
                                        st.deleted += 1;
                                    }
                                    st.tombstones += 1;
                                    st.deleted += n as u64;
                                    cells.push(CellInput::tombstone(u, v));
                                }
                            }
                        }
                        if !cells.is_empty() {
                            db.put_points(&realm, cells, lane, false, u_window)?;
                        }
                    }
                }
                for w in phase.wide {
                    match w {
                        BatchWide::Record { layout, obj, lane } => {
                            let lay = db.layout_by_ref(&layout)?.clone();
                            if !seen.insert((lay.realm_id.clone(), lane, obj.clone())) {
                                continue;
                            }
                            let tombs = buf.entry((lay.realm_id.clone(), lane)).or_default();
                            let before = tombs.len();
                            st.deleted += db.collect_object_delete(&lay, &layout, &obj, lane, tombs)?;
                            buffered += tombs.len() - before;
                            meter.grow(super::delete::DELETE_U_COST + ((tombs.len() - before) * std::mem::size_of::<CellInput>()) as u64)?;
                            if buffered >= BATCH_TOMB_CHUNK || seen.len() >= BATCH_TOMB_CHUNK {
                                flush(db, &mut buf, &mut buffered, &mut seen)?;
                            }
                        }
                        BatchWide::Area { realm, lane, u_from, u_to, v_from, v_to } => {
                            flush(db, &mut buf, &mut buffered, &mut seen)?;
                            seen.clear();
                            let mut area = super::delete::AreaDelete::new(&realm, lane);
                            db.delete_area_into(&realm, &u_from, &u_to, &v_from, &v_to, lane, &mut area)?;
                            area.flush(db)?;
                            st.deleted += area.deleted;
                        }
                    }
                }
            }
            flush(db, &mut buf, &mut buffered, &mut seen)?;
            Ok(st)
        })?;
        st.blocks = self.next_block_id - blocks0;
        st.bytes = self.store.len() - len0;
        Ok(st)
    }

    /// Start state of a write that may append several blocks (RUN8, N2).
    pub(crate) fn write_mark(&self) -> WriteMark {
        WriteMark {
            len: self.store.len(),
            next_block_id: self.next_block_id,
            last_valid_end: self.last_valid_end,
            layout_dirty: self.sys.layout_dirty,
        }
    }

    /// Runs a write that may append several blocks as one unit (RUN8, N2,
    /// generalised from G3-04): if `f` fails after appending anything, the
    /// file is cut back to its length at the start and the in-memory state
    /// is rebuilt as it was, so a later commit cannot take over the blocks
    /// written before the error. Earlier uncommitted writes of the handle
    /// are kept. `f` must not commit and must not change the schema. If the
    /// rollback itself fails, the handle is poisoned (reopen required).
    pub(crate) fn write_scope<T>(&mut self, f: impl FnOnce(&mut Db) -> Result<T>) -> Result<T> {
        let mark = self.write_mark();
        match f(self) {
            Ok(v) => Ok(v),
            Err(e) => {
                self.undo_to_mark(mark);
                Err(e)
            }
        }
    }

    /// Discards everything appended since `mark` (see [`Db::write_scope`]).
    pub(crate) fn undo_to_mark(&mut self, mark: WriteMark) {
        if self.store.len() == mark.len && self.next_block_id == mark.next_block_id {
            return;
        }
        // AP9: block ids are handed out again after the rollback; overlays
        // and change counters of the discarded blocks must not survive
        self.ixm_reset();
        // the scoped writes never change the schema, so the current one is
        // the one at the mark
        let restored = self
            .export_schema()
            .and_then(|schema| self.rollback_math_tail(mark.len, mark.next_block_id, mark.last_valid_end, &schema, mark.layout_dirty));
        if restored.is_err() {
            self.sys.poisoned = true;
        }
    }

    /// `rollback()` (P024, opt-in): discards every block and schema change
    /// of this handle since its last commit and restores the committed state
    /// (data, R0,u1, schema from the active layout plan entry, MATH states,
    /// size limit). Returns the commit boundary. Session settings (plugins,
    /// durability, dyn mode) stay. Nothing beyond the commit boundary of
    /// this handle is touched: with `lock=False` a concurrent writer is
    /// detected first (as for `commit()`).
    pub fn rollback(&mut self) -> Result<u64> {
        self.check_open()?;
        if self.readonly {
            return Err(Error::NHyDB("database is opened read-only (readonly=True); rollback is not possible".into()));
        }
        if self.sys.poisoned {
            return Err(Error::NHyDB(
                "security/durability state unclear after a failed commit (fsync or schema publication); reopen the database".into(),
            ));
        }
        let committed = self.sup.last_valid_end;
        if !self.has_pending() && !self.sys.layout_dirty {
            return Ok(committed);
        }
        self.check_no_concurrent_writer()?;
        // IT1: open cursors may hold offsets of the blocks discarded here.
        self.cursor_epoch += 1;
        self.store.rollback_to(committed)?;
        self.reset_in_memory_state();
        self.next_block_id = self.sup.next_block_id;
        self.checkpoint_offset = self.sup.checkpoint_offset;
        self.last_valid_end = committed;
        self.max_db_size_bytes = crate::config::DEFAULT_DB_MAX_SIZE_BYTES;
        self.last_checkpoint_obj = Val::map();
        self.sys.math_constants_stored = false;
        let record = self.load_storage_state(committed)?;
        self.sys.record = record;
        let snapshot = system::open_entry(&self.sys.db_id(), &self.sys.active_entry, &self.sys.key)?;
        self.import_schema(&snapshot)?;
        self.sys.layout_dirty = false;
        self.validate_math_layout_state()?;
        Ok(committed)
    }
}
