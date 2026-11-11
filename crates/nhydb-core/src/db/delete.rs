//! Delete operations of the public API (0.28.2/delete-api-f1):
//! `delete_cell`, `delete_area` and – in `columns.rs` – `delete_object`
//! (`delete_record`).
//!
//! All of them work on the logical cell `(R, u, v, lane)`: BASE or
//! extension, DENSE or SPARSE, named or not makes no difference. What
//! exists is decided by one rule, the one of the cell cursor and SQL
//! ([`Db::live_cell_value`], [`Db::live_cells_of_u`]): a cell lives when its
//! newest state is a value – a stored `None` included. A tombstoned cell, a
//! cell never written and a field the stored shape of a record lacks do not
//! exist. Every operation returns what it deleted: `delete_cell` whether the
//! cell lived, `delete_area`/`delete_record` the number of distinct live
//! cells (a cell with DENSE history and SPARSE overlay counts once).
//!
//! The physical form stays as before, no new block type:
//! * a single cell gets a SPARSE tombstone;
//! * a whole CLASSIC record (every live cell of a u) gets the DELETE block
//!   of its DENSE record plus tombstones for its SPARSE values (also the
//!   ones the record hides, F1, which the DELETE alone would let show
//!   again);
//! * an area is cut into those two cases per u.
//!
//! Atomicity: each call runs in one `write_scope`; a failure (I/O, size
//! limit, RAM limiter cancel) cuts the file back to its length before the
//! call, nothing of it stays. Memory: `delete_area` streams the candidates
//! (DENSE u in order, SPARSE cells through the bounded `SparseMerge`) and
//! writes the tombstones in chunks, so its memory is bounded by one chunk
//! and the merge windows, independent of the size of the area.

use crate::address::{require_positive, validate_local_v, Lane};
use crate::error::{Error, Result};
use crate::nat::Nat;

use super::merge::{MergedCell, SparseMerge, Verify};
use super::sparse::CellInput;
use super::{Db, SVal};

/// Tombstones written per `put_points` of `delete_area`.
/// (bounded memory: a chunk is one `put_points`; its transient cost grows
/// with its size, measured 2026-11-08: 5 000 cells ≈ 60 MB below 50 000 for
/// 1.6 M deleted cells)
pub(crate) const AREA_CHUNK_CELLS: usize = 8_192;

/// E5 (R9D-05): progress reported to the RAM limiter per visited u of a
/// delete (directory entry, merge state; an estimate). With the meter step
/// of 1 MiB a long delete reaches a checkpoint every ≈ 16 000 u, so it is
/// registered, can be held, cancelled (`cancel_job`) and rolled back as a
/// whole (`write_scope`), also when it writes only DELETE blocks.
pub(crate) const DELETE_U_COST: u64 = 64;

impl Db {
    /// Write checks of the delete operations (open, writable, realm, no
    /// exclusive MATH realm: its cells are written by its layout only).
    pub fn check_delete_target(&self, realm: &Nat) -> Result<()> {
        self.check_open()?;
        if self.readonly {
            return Err(Error::NHyDB("database is opened read-only (readonly=True); writing is not possible".into()));
        }
        self.require_realm(realm)?;
        if let Some(owner) = self.math_owner_of_realm(realm) {
            if !self.internal_math_writes {
                return Err(Error::Schema(format!("Realm {realm} belongs exclusively to MATH layout {owner}; direct deletes blocked")));
            }
        }
        Ok(())
    }

    /// Deletes one cell (`delete_cell`). `true`: the cell lived (a stored
    /// `None` included) and is now tombstoned; `false`: there was no live
    /// cell, nothing is written. `commit=True` commits in both cases.
    pub fn delete_cell(&mut self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane, commit: bool) -> Result<bool> {
        self.check_delete_target(realm)?;
        require_positive("u", u)?;
        validate_local_v(v, Some(realm))?;
        let live = self.live_cell_value(realm, u, v, lane)?.is_some();
        if live {
            self.delete_point(realm, u, v, lane, false)?;
        }
        if commit {
            self.commit()?;
        }
        Ok(live)
    }

    /// Deletes every live cell of the rectangle `u_from..=u_to` ×
    /// `v_from..=v_to` of a realm (`delete_area`), independent of layout,
    /// BASE/extension and DENSE/SPARSE. Returns the number of live cells
    /// deleted. One unit: on any error nothing of the call stays.
    #[allow(clippy::too_many_arguments)]
    pub fn delete_area(&mut self, realm: &Nat, u_from: &Nat, u_to: &Nat, v_from: &Nat, v_to: &Nat, lane: Lane, commit: bool) -> Result<u64> {
        self.check_delete_target(realm)?;
        require_positive("u_from", u_from)?;
        require_positive("u_to", u_to)?;
        validate_local_v(v_from, Some(realm))?;
        validate_local_v(v_to, Some(realm))?;
        if u_from > u_to {
            return Err(Error::Value(format!("u_from ({u_from}) must not be greater than u_to ({u_to})")));
        }
        if v_from > v_to {
            return Err(Error::Value(format!("v_from ({v_from}) must not be greater than v_to ({v_to})")));
        }
        let n = self.write_scope(|db| {
            let mut area = AreaDelete::new(realm, lane);
            db.delete_area_into(realm, u_from, u_to, v_from, v_to, lane, &mut area)?;
            area.flush(db)?;
            Ok(area.deleted)
        })?;
        if commit {
            self.commit()?;
        }
        Ok(n)
    }

    /// Body of `delete_area` (inside a `write_scope`): one pass over the
    /// candidate u in ascending order – DENSE u from the segment index,
    /// SPARSE u from the merge – deciding per u from its live cells.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn delete_area_into(&mut self, realm: &Nat, u_from: &Nat, u_to: &Nat, v_from: &Nat, v_to: &Nat, lane: Lane, area: &mut AreaDelete) -> Result<()> {
        let key = (realm.clone(), lane);
        let mut merge = match self.sparse.contains_key(&key) {
            true => Some(SparseMerge::new_from(self, realm, lane, Verify::Upfront, u_from)?),
            false => None,
        };
        let mut look: Option<MergedCell> = None;
        let mut next_dense = match self.dense.contains_key(&key) {
            true => self.next_dense_u_checked(realm, lane, u_from, u_to)?.filter(|u| u <= u_to),
            false => None,
        };
        loop {
            // next sparse cell at or after u_from
            if look.is_none() {
                if let Some(m) = merge.as_mut() {
                    loop {
                        match m.next(self)? {
                            Some(c) if &c.u < u_from => continue,
                            other => {
                                look = other;
                                break;
                            }
                        }
                    }
                    if look.as_ref().is_some_and(|c| &c.u > u_to) {
                        look = None;
                        merge = None;
                    }
                }
            }
            let su = look.as_ref().map(|c| c.u.clone());
            let u = match (&next_dense, &su) {
                (Some(d), Some(s)) => d.min(s).clone(),
                (Some(d), None) => d.clone(),
                (None, Some(s)) => s.clone(),
                (None, None) => break,
            };
            // the sparse states of u (newest per v, with their block id)
            let mut states: Vec<(Nat, SVal, u64)> = Vec::new();
            while look.as_ref().is_some_and(|c| c.u == u) {
                let c = look.take().unwrap();
                states.push((c.v, c.meta.value, c.bid));
                if let Some(m) = merge.as_mut() {
                    look = m.next(self)?;
                    if look.as_ref().is_some_and(|c| &c.u > u_to) {
                        look = None;
                        merge = None;
                    }
                }
            }
            self.delete_area_u(realm, &u, v_from, v_to, lane, &states, area)?;
            area.progress(DELETE_U_COST)?;
            if next_dense.as_ref() == Some(&u) {
                let after = u.add_u64(1);
                next_dense = if &after > u_to { None } else { self.next_dense_u_checked(realm, lane, &after, u_to)?.filter(|x| x <= u_to) };
            }
            area.maybe_flush(self)?;
        }
        Ok(())
    }

    /// One u of `delete_area`: its live cells from the DENSE record (if
    /// live) and the SPARSE `states` (F1 rule); all of them in the v range
    /// → full record delete, else tombstones for those in the range.
    #[allow(clippy::too_many_arguments)]
    fn delete_area_u(&mut self, realm: &Nat, u: &Nat, v_from: &Nat, v_to: &Nat, lane: Lane, states: &[(Nat, SVal, u64)], area: &mut AreaDelete) -> Result<()> {
        let in_range = |v: &Nat| v >= v_from && v <= v_to;
        let seg = self.resolve_segment(realm, u, lane)?;
        let (live, in_area): (u64, Vec<Nat>) = match &seg {
            Some(seg) => {
                // the rule of `live_cells_of_u`, from the streamed states
                // (no record decode, no cache filled per u): every field of
                // the record's shape, unless a newer state tombstones it,
                // plus every visible value state
                let shape = self.shape_of(seg.shape_id)?;
                let mut cells: std::collections::BTreeSet<Nat> = shape.fields.iter().map(|f| f.v.clone()).collect();
                for (v, s, bid) in states {
                    if super::dense_hides(shape, seg.block_id, v, *bid) {
                        continue;
                    }
                    match s {
                        SVal::Tomb => {
                            cells.remove(v);
                        }
                        SVal::V(_) => {
                            cells.insert(v.clone());
                        }
                    }
                }
                let n = cells.len() as u64;
                (n, cells.into_iter().filter(|v| in_range(v)).collect())
            }
            None => {
                let vs: Vec<Nat> = states.iter().filter(|(_, s, _)| !s.is_tomb()).map(|(v, _, _)| v.clone()).collect();
                (vs.len() as u64, vs.into_iter().filter(|v| in_range(v)).collect())
            }
        };
        if in_area.is_empty() {
            return Ok(());
        }
        area.deleted += in_area.len() as u64;
        if seg.is_some() && in_area.len() as u64 == live {
            // every live cell of u: DELETE block + tombstones of the values
            self.delete_record_block(realm, u, lane)?;
            for (v, s, _) in states {
                if !s.is_tomb() {
                    area.push(CellInput::tombstone(u.clone(), v.clone()));
                }
            }
            return Ok(());
        }
        for v in in_area {
            area.push(CellInput::tombstone(u.clone(), v));
        }
        Ok(())
    }
}

/// Tombstones of a running area delete, written in chunks.
pub(crate) struct AreaDelete {
    realm: Nat,
    lane: Lane,
    tombs: Vec<CellInput>,
    /// live cells deleted so far
    pub deleted: u64,
    meter: crate::limiter::Meter,
}

impl AreaDelete {
    pub(crate) fn new(realm: &Nat, lane: Lane) -> AreaDelete {
        AreaDelete {
            realm: realm.clone(),
            lane,
            tombs: Vec::new(),
            deleted: 0,
            meter: crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Write),
        }
    }

    fn push(&mut self, c: CellInput) {
        self.tombs.push(c);
    }

    /// E5: progress of one visited u (checkpoint of the RAM limiter when
    /// a meter step is due; a cancel rolls the whole call back).
    pub(crate) fn progress(&mut self, bytes: u64) -> Result<()> {
        self.meter.grow(bytes)?;
        Ok(())
    }

    fn maybe_flush(&mut self, db: &mut Db) -> Result<()> {
        if self.tombs.len() >= AREA_CHUNK_CELLS {
            self.flush(db)?;
        }
        Ok(())
    }

    pub(crate) fn flush(&mut self, db: &mut Db) -> Result<()> {
        if self.tombs.is_empty() {
            return Ok(());
        }
        let cells = std::mem::take(&mut self.tombs);
        // RUN¹⁰: checkpoint of the RAM limiter per chunk (a cancel rolls the
        // whole call back, `write_scope`)
        self.meter.grow((cells.len() * std::mem::size_of::<CellInput>()) as u64)?;
        // sorted by (u, v): one block per chunk. The group directory of a
        // tombstone block is not put into the sparse cache (mass deletes
        // would fill it with directories nobody asked for; a read loads it
        // on demand, as after a reopen)
        let keep = std::mem::replace(&mut db.write_cache, false);
        let r = db.put_points(&self.realm, cells, self.lane, false, u64::MAX);
        db.write_cache = keep;
        r?;
        Ok(())
    }
}
