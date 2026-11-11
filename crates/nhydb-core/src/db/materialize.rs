//! Layout-independent view of materialized cells
//! (`iter_materialized_cells`, `iter_materialized_record_ns`,
//! `record_n_for_address`, `record_n_from_n`).
//!
//! The iterator is a materialization scan (not an address-space scan) and
//! returns cells in a fixed order: first dense records per
//! `(realm, lane)`, sorted, with overlaid sparse cells, then the
//! pure sparse points. Instead of keeping a set of all dense keys,
//! membership is checked via `resolve_segment` – same result,
//! constant memory.

use std::collections::{HashSet, VecDeque};

use crate::address::{lane_name, n_to_address, Lane};
use crate::error::{Error, Result};
use crate::nat::Nat;
use crate::schema::LayoutKind;
use crate::val::Val;
use crate::vmap;

use super::cursor::SparsePointCursor;
use super::{Db, SVal, RL};

/// Raw cell of the materialization scan.
#[derive(Clone, Debug)]
pub struct RawCell {
    pub realm: Nat,
    pub lane: Lane,
    pub u: Nat,
    pub v: Nat,
    pub value: Val,
}

enum Phase {
    Dense,
    Sparse(Box<SparsePointCursor>),
    Done,
}

/// Lazy iterator over all materialized cells (raw form).
pub struct RawCellCursor {
    realm: Option<Nat>,
    lane: Option<Lane>,
    keys: Vec<RL>,
    ki: usize,
    intervals: Vec<(Nat, Nat)>,
    ii: usize,
    next_u: Option<Nat>,
    pending: VecDeque<RawCell>,
    phase: Phase,
    /// IT1: epoch of the handle at creation (keys are taken from it).
    epoch: Option<u64>,
}

impl RawCellCursor {
    pub fn new(db: &Db, realm: Option<&Nat>, lane: Option<Lane>) -> RawCellCursor {
        let mut keys: Vec<RL> = db
            .dense
            .keys()
            .filter(|(r, l)| realm.is_none_or(|x| x == r) && lane.is_none_or(|x| x == *l))
            .cloned()
            .collect();
        keys.sort();
        RawCellCursor {
            realm: realm.cloned(),
            lane,
            keys,
            ki: 0,
            intervals: Vec::new(),
            ii: 0,
            next_u: None,
            pending: VecDeque::new(),
            phase: Phase::Dense,
            epoch: Some(db.cursor_epoch),
        }
    }

    /// Load the next dense u group into `pending`; `false` = dense phase done.
    fn advance_dense(&mut self, db: &Db) -> Result<bool> {
        loop {
            if self.next_u.is_none() {
                if self.ii >= self.intervals.len() {
                    if self.ki >= self.keys.len() {
                        return Ok(false);
                    }
                    let key = &self.keys[self.ki];
                    self.ki += 1;
                    self.intervals = db.dense_merged_ranges(key)?;
                    self.ii = 0;
                    continue;
                }
                self.next_u = Some(self.intervals[self.ii].0.clone());
            }
            let u = self.next_u.take().unwrap();
            let (rid, ln) = self.keys[self.ki - 1].clone();
            if u < self.intervals[self.ii].1 {
                self.next_u = Some(u.add_u64(1));
            } else {
                self.ii += 1;
            }
            let Some(seg) = db.resolve_segment(&rid, &u, ln)? else {
                continue;
            };
            let rec = db.with_record_bytes(&seg, &u, |raw| db.decode_dense_record(&seg, raw))?;
            let mut by_v = rec.by_v();
            if db.sparse.contains_key(&(rid.clone(), ln)) {
                let shape = db.shape_of(seg.shape_id)?;
                for (v, (st, bid)) in db.sparse_updates_for_u(&rid, &u, ln)?.iter() {
                    // F1: the record is newer than this state of its field
                    if super::dense_hides(shape, seg.block_id, v, *bid) {
                        continue;
                    }
                    match st {
                        SVal::Tomb => {
                            by_v.remove(v);
                        }
                        SVal::V(x) => {
                            by_v.insert(v.clone(), x.clone());
                        }
                    }
                }
            }
            for (v, value) in by_v {
                self.pending.push_back(RawCell { realm: rid.clone(), lane: ln, u: u.clone(), v, value });
            }
            if !self.pending.is_empty() {
                return Ok(true);
            }
        }
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<RawCell>> {

        db.check_open()?;
        db.check_cursor_epoch(&mut self.epoch)?;
        let mut out = Vec::new();
        while out.len() < max {
            if let Some(c) = self.pending.pop_front() {
                out.push(c);
                continue;
            }
            match &mut self.phase {
                Phase::Dense => {
                    if !self.advance_dense(db)? {
                        self.phase = Phase::Sparse(Box::new(SparsePointCursor::new(db, self.realm.as_ref(), self.lane)));
                    }
                }
                Phase::Sparse(cur) => {
                    let pts = cur.next_batch(db, max - out.len())?;
                    if pts.is_empty() {
                        self.phase = Phase::Done;
                        continue;
                    }
                    for p in pts {
                        if db.resolve_segment(&p.realm, &p.u, p.lane)?.is_some() {
                            continue;
                        }
                        out.push(RawCell { realm: p.realm, lane: p.lane, u: p.u, v: p.v, value: p.value });
                    }
                }
                Phase::Done => break,
            }
        }
        Ok(out)
    }

    /// Callback variant; `f` returns `false` to stop early.
    pub fn for_each(mut self, db: &Db, mut f: impl FnMut(RawCell) -> Result<bool>) -> Result<()> {
        db.check_open()?;
        loop {
            let batch = self.next_batch(db, 1024)?;
            if batch.is_empty() {
                return Ok(());
            }
            for c in batch {
                if !f(c)? {
                    return Ok(());
                }
            }
        }
    }
}

/// Lazy iterator with layout-aware envelopes (`iter_materialized_cells`).
pub struct MaterializedCellCursor {
    raw: RawCellCursor,
}

impl MaterializedCellCursor {
    pub fn new(db: &Db, realm: Option<&Nat>, lane: Option<Lane>) -> Self {
        MaterializedCellCursor { raw: RawCellCursor::new(db, realm, lane) }
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<Val>> {

        db.check_open()?;
        self.raw
            .next_batch(db, max)?
            .into_iter()
            .map(|c| db.materialized_cell_envelope(&c.realm, &c.u, &c.v, c.value, c.lane))
            .collect()
    }
}

/// Lazy iterator over unique record_N references (`iter_materialized_record_ns`).
pub struct RecordNCursor {
    raw: RawCellCursor,
    seen: HashSet<Nat>,
    memo: RecordNMemo,
}

impl RecordNCursor {
    pub fn new(db: &Db, realm: Option<&Nat>, lane: Option<Lane>) -> Self {
        RecordNCursor { raw: RawCellCursor::new(db, realm, lane), seen: HashSet::new(), memo: RecordNMemo::default() }
    }

    /// Next unique record_N (as a number), in order of first occurrence.
    pub fn next_batch_nat(&mut self, db: &Db, max: usize) -> Result<Vec<Nat>> {
        db.check_open()?;
        let mut out = Vec::new();
        while out.len() < max {
            let batch = self.raw.next_batch(db, max)?;
            if batch.is_empty() {
                break;
            }
            for c in batch {
                // CLASSIC: record_N is unique per u group (each u group appears
                // exactly once in the scan) → no set needs to be kept.
                match self.memo.lookup(db, &c)? {
                    (Some(r), true, fresh) => {
                        if fresh {
                            out.push(r);
                        }
                    }
                    (Some(r), false, _) => {
                        if self.seen.insert(r.clone()) {
                            out.push(r);
                        }
                    }
                    (None, _, _) => {}
                }
            }
        }
        Ok(out)
    }

    /// Like `next_batch_nat`, as `"N…"` references.
    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<String>> {
        db.check_open()?;
        Ok(self.next_batch_nat(db, max)?.into_iter().map(|n| format!("N{n}")).collect())
    }
}

/// record_N of a raw cell, computed only once per u group for CLASSIC
/// (with CLASSIC the record of every cell is its u).
#[derive(Default)]
pub struct RecordNMemo {
    last: Option<(Nat, Lane, Nat, Option<Nat>)>,
}

impl RecordNMemo {
    pub fn record_n(&mut self, db: &Db, c: &RawCell) -> Result<Option<Nat>> {
        Ok(self.lookup(db, c)?.0)
    }

    /// `(record_N, is_CLASSIC, first_cell_of_the_u_group)`
    pub fn lookup(&mut self, db: &Db, c: &RawCell) -> Result<(Option<Nat>, bool, bool)> {
        if let Some((r, l, u, out)) = &self.last {
            if r == &c.realm && *l == c.lane && u == &c.u {
                return Ok((out.clone(), true, false));
            }
        }
        let lay = db.layout_at(&c.realm, &c.u);
        let out = db.cell_record_n(&c.realm, &c.u, &c.v, c.lane)?;
        let classic = lay.is_some_and(|l| l.kind == LayoutKind::Classic);
        self.last = if classic { Some((c.realm.clone(), c.lane, c.u.clone(), out.clone())) } else { None };
        Ok((out, classic, true))
    }
}

impl Db {
    /// record_N of a materialized cell as in the envelope
    /// (`_materialized_cell_envelope`: error in the anchor computation ⇒ None).
    pub(crate) fn cell_record_n(&self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<Option<Nat>> {
        let addr = self.cell_address(realm, u, v, lane)?;
        let Some(lay) = self.layout_at(realm, u) else {
            return Ok(None);
        };
        let interp = self.interpret_layout_address(lay, &addr)?;
        let Some(obj) = Db::interp_record(&interp) else {
            return Ok(None);
        };
        match self.logical_address_in(lay, &obj, &Val::None, lane) {
            Ok(a) => Ok(Some(a.n)),
            Err(Error::Schema(_) | Error::RealmLimit(_) | Error::Key(_) | Error::Value(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `_materialized_cell_envelope`
    pub fn materialized_cell_envelope(&self, realm: &Nat, u: &Nat, v: &Nat, value: Val, lane: Lane) -> Result<Val> {
        self.check_open()?;
        let addr = self.cell_address(realm, u, v, lane)?;
        let lay = self.layout_at(realm, u);
        let mut record_n = Val::None;
        let mut field: Option<String> = None;
        let mut object = Val::None;
        let (mut lid, mut lname, mut lkind) = (Val::None, Val::None, Val::None);
        if let Some(l) = lay {
            lid = Val::Int(l.layout_id as i64);
            lname = Val::str(l.name.as_str());
            lkind = Val::str(l.kind.name());
            let interp = self.interpret_layout_address(l, &addr)?;
            let obj = Db::interp_record(&interp);
            field = Some(Db::fallback_cell_label(l, &interp, u, v));
            if let Some(o) = obj {
                object = Val::nat(&o);
                match self.logical_address_in(l, &o, &Val::None, lane) {
                    Ok(a) => record_n = Val::Str(a.n_ref()),
                    Err(Error::Schema(_) | Error::RealmLimit(_) | Error::Key(_) | Error::Value(_)) => {}
                    Err(e) => return Err(e),
                }
            } else if let Some(raw) = interp.get("record") {
                object = raw.clone();
            }
        } else if let Some(owner) = self.math_owner_of_realm(realm) {
            let m = &self.math_layouts[&owner];
            lid = Val::Int(owner as i64);
            lname = Val::str(m.name());
            lkind = Val::str("MATH");
            field = Some("value".into());
        }
        Ok(vmap! {
            "N" => addr.n_ref(), "realm" => realm, "u" => u, "v" => v,
            "lane" => lane as i64, "lane_name" => lane_name(lane), "value" => value,
            "field" => field.unwrap_or_else(|| format!("v{v}")),
            "layout_id" => lid, "layout_name" => lname, "layout_kind" => lkind,
            "object" => object, "record_N" => record_n,
        })
    }

    /// `record_n_for_address`: geometric, without reading payload data.
    pub fn record_n_for_address(&self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<Option<String>> {
        self.check_open()?;
        Ok(self.record_n_nat(realm, u, v, lane)?.map(|n| format!("N{n}")))
    }

    /// `record_n_for_address` as a number.
    pub fn record_n_nat(&self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<Option<Nat>> {
        if !self.realms.contains_key(realm) {
            return Ok(None);
        }
        let Some(lay) = self.layout_at(realm, u) else {
            return Ok(None);
        };
        let addr = self.cell_address(realm, u, v, lane)?;
        let interp = self.interpret_layout_address(lay, &addr)?;
        let Some(obj) = Db::interp_record(&interp) else {
            return Ok(None);
        };
        Ok(Some(self.logical_address_in(lay, &obj, &Val::None, lane)?.n))
    }

    /// `record_n_from_n`
    pub fn record_n_from_n(&self, n: &Nat) -> Result<Option<String>> {
        self.check_open()?;
        Ok(self.record_n_from_n_nat(n)?.map(|r| format!("N{r}")))
    }

    pub fn record_n_from_n_nat(&self, n: &Nat) -> Result<Option<Nat>> {
        let a = n_to_address(n)?;
        self.record_n_nat(a.realm(), a.u(), a.v_local(), a.lane)
    }
}
