//! Memory-efficient cursors for large result sets.
//!
//! `range`, `select`, `find` and `iter_materialized_sparse_points` are
//! exposed as generators. The cursors here hold
//! no borrow on the database, only their position; each call
//! of `next_batch` returns at most `max` elements, so memory use
//! stays independent of the result size. Writes between two calls are
//! allowed; after the handle discarded data (`rollback`, dropped append
//! buffer) `next_batch` fails with [`super::CURSOR_INVALIDATED`] (IT1).
//!
//! * `RangeCursor` (`range`/`select`): skips u ranges without a
//!   dense segment instead of iterating every u individually.
//! * `FindCursor`: u list from the postings (materialized),
//!   records are read only when fetched.
//! * `SparsePointCursor`: memory-bounded merge over the sparse blocks per
//!   (realm, lane) (`merge.rs`); output sorted by (realm, lane, u, v),
//!   memory independent of the number of cells.

use crate::address::Lane;
use crate::error::Result;
use crate::nat::Nat;
use crate::val::Val;

use super::merge::{SparseMerge, Verify};
use super::sparse::MaterializedPoint;
use super::{Db, SVal};

/// What a `RangeCursor` returns per u.
#[derive(Clone, Debug)]
pub enum RangeMode {
    /// `range(...)`: record as returned by `get`.
    Get { dyn_mode: Option<String>, dyn_v: Option<Nat>, include_extensions: bool },
    /// `select(...)`: column projection.
    Select { columns: Vec<Val> },
}

#[derive(Clone, Debug)]
pub struct RangeCursor {
    realm: Nat,
    lane: Lane,
    next: Nat,
    to: Nat,
    mode: RangeMode,
    done: bool,
    /// IT1: epoch of the handle at the first batch.
    epoch: Option<u64>,
}

impl RangeCursor {
    pub fn new(realm: Nat, lane: Lane, from: Nat, to: Nat, mode: RangeMode) -> RangeCursor {
        let from = if from.is_zero() { Nat::ONE } else { from };
        RangeCursor { realm, lane, next: from, to, mode, done: false, epoch: None }
    }

    /// Next u ≥ `self.next` with a dense segment (or None); a skipped gap
    /// is checked like a point access (F021 rest, G3-03).
    fn next_candidate(&self, db: &Db) -> Result<Option<Nat>> {
        db.next_dense_u_checked(&self.realm, self.lane, &self.next, &self.to)
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<Val>> {

        db.check_open()?;
        db.check_cursor_epoch(&mut self.epoch)?;
        let mut out = Vec::new();
        let mut resolved: Option<Vec<(String, Nat)>> = None;
        while !self.done && out.len() < max {
            let Some(u) = self.next_candidate(db)? else {
                self.done = true;
                break;
            };
            if u > self.to {
                self.done = true;
                break;
            }
            self.next = u.add_u64(1);
            let row = match &self.mode {
                RangeMode::Get { dyn_mode, dyn_v, include_extensions } => {
                    db.get(&self.realm, &u, self.lane, dyn_mode.as_deref(), dyn_v.as_ref(), *include_extensions)?
                }
                RangeMode::Select { columns } => {
                    // P004: resolve the columns once per batch (lazily at the
                    // first candidate, so an empty range raises no error as before)
                    if resolved.is_none() {
                        resolved = Some(db.resolve_select_columns(&self.realm, columns)?);
                    }
                    db.select_row_resolved(&self.realm, &u, resolved.as_deref().unwrap(), self.lane)?
                }
            };
            if let Some(r) = row {
                out.push(r);
            }
        }
        Ok(out)
    }
}

#[derive(Clone, Debug)]
pub struct FindCursor {
    realm: Nat,
    lane: Lane,
    us: Vec<Nat>,
    pos: usize,
    dyn_mode: Option<String>,
    dyn_v: Option<Nat>,
    /// IT1: epoch of the handle at the first batch.
    epoch: Option<u64>,
}

impl FindCursor {
    pub fn new(realm: Nat, lane: Lane, us: Vec<Nat>, dyn_mode: Option<String>, dyn_v: Option<Nat>) -> FindCursor {
        FindCursor { realm, lane, us, pos: 0, dyn_mode, dyn_v, epoch: None }
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<Val>> {

        db.check_open()?;
        db.check_cursor_epoch(&mut self.epoch)?;
        let mut out = Vec::new();
        while self.pos < self.us.len() && out.len() < max {
            let u = &self.us[self.pos];
            self.pos += 1;
            if let Some(r) = db.get(&self.realm, u, self.lane, self.dyn_mode.as_deref(), self.dyn_v.as_ref(), true)? {
                out.push(r);
            }
        }
        Ok(out)
    }
}

/// (realm, lane, u, (shape id, block id) of the live DENSE record of u).
type LastDense = (Nat, Lane, Nat, Option<(u64, u64)>);

/// Newest live sparse points per (realm, lane), sorted by (realm, lane, u,
/// v): a memory-bounded merge over the sparse blocks of the current key
/// (`merge.rs`; segment set fixed when the key is started).
pub struct SparsePointCursor {
    keys: Vec<(Nat, Lane)>,
    ki: usize,
    merge: Option<SparseMerge>,
    /// IT1: epoch of the handle at creation; the merge holds block offsets
    /// that a rollback may hand to new blocks.
    epoch: Option<u64>,
    /// last u looked up: (realm, lane, u, (shape id, block id) of its live
    /// DENSE record)
    last: Option<LastDense>,
}

impl SparsePointCursor {
    pub fn new(db: &Db, realm: Option<&Nat>, lane: Option<Lane>) -> SparsePointCursor {
        let mut keys: Vec<(Nat, Lane)> = db
            .sparse
            .keys()
            .filter(|(r, l)| realm.is_none_or(|x| x == r) && lane.is_none_or(|x| x == *l))
            .cloned()
            .collect();
        keys.sort();
        SparsePointCursor { keys, ki: 0, merge: None, epoch: Some(db.cursor_epoch), last: None }
    }

    /// Whether the live DENSE record of `u` hides a state of `v` written in
    /// block `bid` (one segment lookup per u group, cached).
    fn hidden_by_dense(&mut self, db: &Db, realm: &Nat, lane: Lane, u: &Nat, v: &Nat, bid: u64) -> Result<bool> {
        if !db.dense.contains_key(&(realm.clone(), lane)) {
            return Ok(false);
        }
        let fresh = self.last.as_ref().is_none_or(|(r, l, x, _)| r != realm || *l != lane || x != u);
        if fresh {
            let seg = db.resolve_segment(realm, u, lane)?.map(|s| (s.shape_id, s.block_id));
            self.last = Some((realm.clone(), lane, u.clone(), seg));
        }
        Ok(match self.last.as_ref().and_then(|(_, _, _, s)| *s) {
            Some((sid, dbid)) => super::dense_hides(db.shape_of(sid)?, dbid, v, bid),
            None => false,
        })
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<MaterializedPoint>> {
        db.check_open()?;
        db.check_cursor_epoch(&mut self.epoch)?;
        let mut out = Vec::new();
        while out.len() < max {
            if self.merge.is_none() {
                let Some((rid, ln)) = self.keys.get(self.ki) else { break };
                self.merge = Some(SparseMerge::new(db, rid, *ln, Verify::Upfront)?);
            }
            let (rid, ln) = self.keys[self.ki].clone();
            match self.merge.as_mut().unwrap().next(db)? {
                Some(c) => {
                    // F1: a state older than the live DENSE record of one of
                    // its shape fields is hidden by it – no live point
                    if self.hidden_by_dense(db, &rid, ln, &c.u, &c.v, c.bid)? {
                        continue;
                    }
                    if let SVal::V(value) = c.meta.value {
                        out.push(MaterializedPoint {
                            realm: rid,
                            lane: ln,
                            u: c.u,
                            v: c.v,
                            value,
                            typ: if c.meta.selftyped { c.meta.typ } else { None },
                        });
                    }
                }
                None => {
                    self.merge = None;
                    self.ki += 1;
                }
            }
        }
        Ok(out)
    }
}

/// `select_us(realm, us, columns)`: explicit, sorted candidates.
#[derive(Clone, Debug)]
pub struct SelectUsCursor {
    realm: Nat,
    lane: Lane,
    us: Vec<Nat>,
    pos: usize,
    columns: Vec<Val>,
    /// IT1: epoch of the handle at the first batch.
    epoch: Option<u64>,
}

impl SelectUsCursor {
    pub fn new(realm: Nat, lane: Lane, us: Vec<Nat>, columns: Vec<Val>) -> SelectUsCursor {
        let mut us: Vec<Nat> = us.into_iter().filter(|u| !u.is_zero()).collect();
        us.sort();
        us.dedup();
        SelectUsCursor { realm, lane, us, pos: 0, columns, epoch: None }
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<Val>> {

        db.check_open()?;
        db.check_cursor_epoch(&mut self.epoch)?;
        let end = (self.pos + max).min(self.us.len());
        let out = db.select_us(&self.realm, &self.us[self.pos..end], &self.columns, self.lane)?;
        self.pos = end;
        Ok(out)
    }
}

impl Db {
    /// `select_us`: rows for explicit u (only with a dense record,
    /// not deleted). Values include sparse overrides of the
    /// BASE fields (consistent with `get`).
    pub fn select_us(&self, realm: &Nat, us: &[Nat], columns: &[Val], lane: Lane) -> Result<Vec<Val>> {
        self.check_open()?;
        self.select_us_ext(realm, us, columns, lane, false)
    }

    /// `select_us` with optional inclusion of pure sparse records (u without
    /// a live dense record, but with live sparse cells). The
    /// query layer uses this logical view so that candidates
    /// of an index do not silently disappear from rows/aggregates.
    pub fn select_us_ext(&self, realm: &Nat, us: &[Nat], columns: &[Val], lane: Lane, sparse_only: bool) -> Result<Vec<Val>> {
        self.check_open()?;
        let mut resolved = Vec::with_capacity(columns.len());
        for col in columns {
            let v = self.resolve_v(realm, col)?;
            let mut label = match col {
                Val::Str(s) => s.clone(),
                _ => self.extension_label(realm, &v),
            };
            if let Some(f) = self.current_base_shape(realm).and_then(|b| b.by_v(&v)) {
                label = f.name.clone();
            }
            resolved.push((label, v));
        }
        let mut out = Vec::with_capacity(us.len());
        let vs: Vec<Nat> = resolved.iter().map(|(_, v)| v.clone()).collect();
        let mut seg_cache = super::dense::DenseReadState::default();
        for u in us {
            if u.is_zero() {
                continue;
            }
            // R4-B: only the selected fields are decoded
            if let Some(vals) = self.get_fields(realm, u, lane, &vs, &mut seg_cache)? {
                let mut row: Vec<(String, Val)> = Vec::with_capacity(3 + resolved.len());
                row.push(("_u".into(), Val::nat(u)));
                row.push(("_lane".into(), Val::Int(lane as i64)));
                row.push(("_lane_name".into(), Val::str(crate::address::lane_name(lane))));
                for ((label, _), val) in resolved.iter().zip(vals) {
                    super::read::set_or_push(&mut row, label, val.unwrap_or(Val::None));
                }
                out.push(Val::Map(row));
                continue;
            }
            // no dense record: sparse-only u
            let by_v = if sparse_only && self.sparse.contains_key(&(realm.clone(), lane)) {
                let upd = self.sparse_updates_for_u(realm, u, lane)?;
                let live: std::collections::BTreeMap<Nat, Val> =
                    upd.iter().filter_map(|(v, (st, _))| st.val().map(|x| (v.clone(), x.clone()))).collect();
                if live.is_empty() {
                    None
                } else {
                    Some(live)
                }
            } else {
                None
            };
            if let Some(by_v) = by_v {
                let mut row: Vec<(String, Val)> = vec![
                    ("_u".into(), Val::nat(u)),
                    ("_lane".into(), Val::Int(lane as i64)),
                    ("_lane_name".into(), Val::str(crate::address::lane_name(lane))),
                ];
                for (label, v) in &resolved {
                    super::read::set_or_push(&mut row, label, by_v.get(v).cloned().unwrap_or(Val::None));
                }
                out.push(Val::Map(row));
            }
        }
        Ok(out)
    }

    /// A row object of `select` (only u with a dense record).
    pub fn select_row(&self, realm: &Nat, u: &Nat, columns: &[Val], lane: Lane) -> Result<Option<Val>> {
        self.check_open()?;
        Ok(self.select(realm, u, u, columns, lane)?.into_iter().next())
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "cursor_tests.rs"]
mod tests;
