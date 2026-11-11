//! 0.28.2/scan stage 1: exact read view of a `(realm, lane)` whose DENSE
//! segments overlap or that has DELETE entries ("winner view").
//!
//! `overlay_keys` stays what it was: the persisted, conservative mark that
//! invariant I of the metadata fast paths ("every u of a segment is a live
//! record exactly there") may not hold. Without the mark those paths run as
//! before. With it, and only without SPARSE cells (stage 1), they may use
//! this view instead of falling back to the cell cursor. The view applies
//! the rules of `resolve_segment` to the whole lane at once:
//!
//! * the segment with the highest block id that covers u serves u
//!   (`stab_newest`);
//! * u is dead when a DELETE of u has a higher block id than that segment
//!   (`delete_block_id`; a later write revives u).
//!
//! The u axis is cut into disjoint *runs*, each served by exactly one
//! segment. A run is 12 bytes (segment index, offset, length); a single
//! overwritten record is a run of length 1, so the "point list" of the
//! analysis is part of the same compact list. The view is derived, built per
//! call from the block directory and held only in memory: nothing is
//! persisted, nothing changes the format, and rollback / `undo_to_mark` need
//! no invalidation (block ids reused after an undo can never meet a stale
//! view).
//!
//! Memory: the size is estimated from the entry counts before anything is
//! collected; above `[query] overlay_view_max_bytes` (or when a run does not
//! fit the compact form) no view is built and the caller takes the general
//! path. What is built is reported to the RAM limiter (rigid job, path
//! `Candidates`); a cancel of the limiter ends the query as on every path.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use crate::error::{Error, Result};
use crate::limiter::Meter;
use crate::nat::Nat;
use crate::val::Val;

use super::dense::DenseReadState;
use super::{Db, DenseSegment, SVal, RL};

/// One run of the view: the records `off .. off + len` of `segs[seg]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WinRun {
    pub seg: u32,
    pub off: u32,
    pub len: u32,
}

/// Predicate of [`Db::scan_view_fields`] over the values of one record.
pub(crate) type FieldTest<'f> = dyn FnMut(&Nat, &[Option<Val>]) -> Result<bool> + 'f;

/// Exact u → segment assignment of one `(realm, lane)`.
pub(crate) struct WinnerView {
    /// all DENSE segments of the lane in start order (including fully
    /// overwritten ones: their shapes and CRCs are checked like the others)
    pub segs: Vec<DenseSegment>,
    /// disjoint, sorted by u
    pub runs: Vec<WinRun>,
    /// newest DELETE block per u, sorted by u
    pub dels: Vec<(Nat, u64)>,
}

/// Stage 2: effect of the SPARSE cells on the view (`Db::sparse_effect`);
/// all lists sorted by u.
#[derive(Default)]
pub(crate) struct SparseEffect {
    /// u with a live DENSE record whose every cell is tombstoned
    pub gone: Vec<Nat>,
    /// u without a live DENSE record but with a live SPARSE cell
    pub pure: Vec<Nat>,
    /// newest state of the asked v at the u of a record (DENSE-live or pure)
    pub v_states: Vec<(Nat, SVal)>,
}

/// Estimated bytes per SPARSE u group held by a [`SparseEffect`].
const EST_SPARSE_U: u64 = std::mem::size_of::<(Nat, SVal)>() as u64 + 32;

/// Estimated bytes per segment held by the view (entry + heap of the
/// sweep), per run and per DELETE entry; used before collecting.
const EST_SEG: u64 = std::mem::size_of::<DenseSegment>() as u64 + 64;
const EST_RUN: u64 = std::mem::size_of::<WinRun>() as u64;
const EST_DEL: u64 = std::mem::size_of::<(Nat, u64)>() as u64 + 16;

impl WinnerView {
    pub fn lo(&self, r: &WinRun) -> Nat {
        self.segs[r.seg as usize].u_start.add_u64(r.off as u64)
    }

    pub fn hi(&self, r: &WinRun) -> Nat {
        self.segs[r.seg as usize].u_start.add_u64(r.off as u64 + r.len as u64 - 1)
    }

    pub fn block_id(&self, r: &WinRun) -> u64 {
        self.segs[r.seg as usize].block_id
    }

    /// The segment serving u (newest covering segment), unless a newer
    /// DELETE hides u.
    pub fn serving(&self, u: &Nat) -> Option<&DenseSegment> {
        let i = self.runs.partition_point(|r| &self.hi(r) < u);
        let r = self.runs.get(i)?;
        if &self.lo(r) > u {
            return None;
        }
        let bid = self.block_id(r);
        let dead = self.dels.binary_search_by(|(d, _)| d.cmp(u)).is_ok_and(|j| self.dels[j].1 > bid);
        (!dead).then(|| &self.segs[r.seg as usize])
    }

    /// Stage 2: [`WinnerView::live_ranges`] minus `gone`, plus `pure`.
    pub fn record_ranges(&self, sp: &SparseEffect, meter: &mut Meter) -> Result<Vec<(Nat, Nat)>> {
        let dense = self.live_ranges(meter)?;
        let mut out: Vec<(Nat, Nat)> = Vec::new();
        let mut push = |a: Nat, b: Nat, out: &mut Vec<(Nat, Nat)>| -> Result<()> {
            if let Some(last) = out.last_mut() {
                if last.1.add_u64(1) == a {
                    last.1 = b;
                    return Ok(());
                }
            }
            meter.grow(std::mem::size_of::<(Nat, Nat)>() as u64)?;
            out.push((a, b));
            Ok(())
        };
        // ranges and pure u are disjoint (a pure u has no live DENSE
        // record); merge in u order, cutting the gone u out of the ranges
        let (mut g, mut p) = (0usize, 0usize);
        for (a, b) in dense {
            while p < sp.pure.len() && sp.pure[p] < a {
                push(sp.pure[p].clone(), sp.pure[p].clone(), &mut out)?;
                p += 1;
            }
            while g < sp.gone.len() && sp.gone[g] < a {
                g += 1;
            }
            let mut cur = a;
            while g < sp.gone.len() && sp.gone[g] <= b {
                let x = &sp.gone[g];
                if &cur < x {
                    push(cur, x.sub_u64(1).unwrap(), &mut out)?;
                }
                cur = x.add_u64(1);
                g += 1;
            }
            if cur <= b {
                push(cur, b, &mut out)?;
            }
        }
        while p < sp.pure.len() {
            push(sp.pure[p].clone(), sp.pure[p].clone(), &mut out)?;
            p += 1;
        }
        Ok(out)
    }

    /// The live u of the view as sorted, disjoint ranges (adjacent ranges
    /// joined): every u of a run unless a newer DELETE hides it.
    pub fn live_ranges(&self, meter: &mut Meter) -> Result<Vec<(Nat, Nat)>> {
        let mut out: Vec<(Nat, Nat)> = Vec::new();
        let push = |a: Nat, b: Nat, out: &mut Vec<(Nat, Nat)>, meter: &mut Meter| -> Result<()> {
            if let Some(last) = out.last_mut() {
                if last.1.add_u64(1) == a {
                    last.1 = b;
                    return Ok(());
                }
            }
            meter.grow(std::mem::size_of::<(Nat, Nat)>() as u64)?;
            out.push((a, b));
            Ok(())
        };
        let mut d = 0usize;
        for r in &self.runs {
            let (lo, hi, bid) = (self.lo(r), self.hi(r), self.block_id(r));
            while d < self.dels.len() && self.dels[d].0 < lo {
                d += 1;
            }
            let mut cur = lo;
            while d < self.dels.len() && self.dels[d].0 <= hi {
                let (u, dbid) = &self.dels[d];
                if *dbid > bid {
                    if &cur < u {
                        push(cur, u.sub_u64(1).unwrap(), &mut out, meter)?;
                    }
                    cur = u.add_u64(1);
                }
                d += 1;
            }
            if cur <= hi {
                push(cur, hi, &mut out, meter)?;
            }
        }
        Ok(out)
    }
}

impl Db {
    /// The winner view of `key`, or `None` when it is not built: disabled
    /// (`[query] overlay_view = false`), no DENSE segments, larger than
    /// `[query] overlay_view_max_bytes`, a run beyond the compact form, or
    /// segment metadata that contradicts itself (`u_end` ≠ `u_start + count
    /// − 1`). `None` always means "take the general path"; it is never an
    /// answer of its own.
    pub(crate) fn winner_view(&self, key: &RL, meter: &mut Meter) -> Result<Option<WinnerView>> {
        if !self.config.query_overlay_view {
            return Ok(None);
        }
        let view = self.build_winner_view(key, meter)?;
        let mut c = self.cache();
        match view {
            Some(_) => c.overlay_view_built += 1,
            None => c.overlay_view_declined += 1,
        }
        drop(c);
        Ok(view)
    }

    fn build_winner_view(&self, key: &RL, meter: &mut Meter) -> Result<Option<WinnerView>> {
        let Some(idx) = self.dense.get(key) else { return Ok(None) };
        let n_segs = idx.idx.len();
        let n_dels = self.deletes.get(key).map(|d| d.tail.len() as u64 + d.runs.len()).unwrap_or(0);
        // runs: at most 2 per segment (each start and each end + 1 cut once)
        let est = n_segs
            .checked_mul(EST_SEG + 2 * EST_RUN)
            .and_then(|a| n_dels.checked_mul(EST_DEL).and_then(|b| a.checked_add(b)));
        if est.is_none_or(|e| e > self.config.query_overlay_view_max_bytes) || n_segs > u32::MAX as u64 {
            return Ok(None);
        }
        meter.grow(n_segs * EST_SEG)?;
        let segs = idx.idx.collect_by_start(self)?;
        if segs.len() as u64 > u32::MAX as u64 {
            return Ok(None);
        }
        for s in &segs {
            if s.count > 0 && s.u_end != s.u_start.add_u64(s.count - 1) {
                return Ok(None);
            }
        }
        if segs.windows(2).any(|w| w[1].u_start < w[0].u_start) {
            return Err(Error::Corruption("DATA segments of the block directory out of start order".into()));
        }
        let Some(runs) = sweep_runs(&segs, meter)? else { return Ok(None) };
        let mut dels: Vec<(Nat, u64)> = Vec::new();
        if let Some(dl) = self.deletes.get(key) {
            meter.grow(n_dels * EST_DEL)?;
            reserve(&mut dels, n_dels as usize)?;
            for (u, b) in &dl.tail {
                dels.push((u.clone(), *b));
            }
            for e in dl.runs.collect_by_start(self)? {
                dels.push((e.u, e.block_id));
            }
            // newest DELETE per u (as `delete_block_id`: max over tail and runs)
            dels.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
            dels.dedup_by(|later, kept| later.0 == kept.0);
        }
        Ok(Some(WinnerView { segs, runs, dels }))
    }

    /// Stage 2: what the SPARSE cells of `key` change in the view, from the
    /// newest state per (u, v) as the cell cursor sees it (`SparseMerge`,
    /// every SPARSE block CRC-checked up front, memory-bounded):
    ///
    /// * a u with a live DENSE record keeps it unless every cell is
    ///   tombstoned (no `V`, every field of the serving shape and every
    ///   extension of the record tombstoned) → `gone`;
    /// * a u without a live DENSE record is a record when it has a `V`
    ///   (pure sparse record; a DELETE hides the DENSE record only) → `pure`;
    /// * with `v`: the newest state of v at every u of the two kinds.
    ///
    /// `None`: larger than `[query] overlay_view_max_bytes` (general path).
    pub(crate) fn sparse_effect(&self, key: &RL, view: &WinnerView, v: Option<&Nat>, meter: &mut Meter) -> Result<Option<SparseEffect>> {
        let mut eff = SparseEffect::default();
        let Some(sx) = self.sparse.get(key) else { return Ok(Some(eff)) };
        let est = sx.groups.checked_mul(EST_SPARSE_U);
        if est.is_none_or(|e| e > self.config.query_overlay_view_max_bytes) {
            return Ok(None);
        }
        let mut merge = super::merge::SparseMerge::new(self, &key.0, key.1, super::merge::Verify::Upfront)?;
        let mut group: Vec<(Nat, SVal, u64)> = Vec::new();
        let mut cur: Option<Nat> = None;
        loop {
            let c = merge.next(self)?;
            let next_u = c.as_ref().map(|c| &c.u);
            if cur.is_some() && next_u != cur.as_ref() {
                let u = cur.take().unwrap();
                self.sparse_group(view, &u, &group, v, &mut eff, meter)?;
                group.clear();
            }
            let Some(c) = c else { break };
            if cur.is_none() {
                cur = Some(c.u.clone());
            }
            group.push((c.v, c.meta.value, c.bid));
        }
        Ok(Some(eff))
    }

    fn sparse_group(&self, view: &WinnerView, u: &Nat, group: &[(Nat, SVal, u64)], v: Option<&Nat>, eff: &mut SparseEffect, meter: &mut Meter) -> Result<()> {
        let serving = view.serving(u);
        // F1: states older than the serving DENSE record of one of its
        // fields are hidden by it (the cell cursor's rule)
        let shape = match serving {
            Some(seg) => Some(self.shape_of(seg.shape_id)?),
            None => None,
        };
        let states: Vec<(Nat, SVal)> = group
            .iter()
            .filter(|(w, _, bid)| match (serving, shape) {
                (Some(seg), Some(sh)) => !super::dense_hides(sh, seg.block_id, w, *bid),
                _ => true,
            })
            .map(|(w, s, _)| (w.clone(), s.clone()))
            .collect();
        let has_v = states.iter().any(|(_, s)| matches!(s, SVal::V(_)));
        let v_state = v.and_then(|v| states.iter().find(|(w, _)| w == v)).map(|(_, s)| s.clone());
        match serving {
            Some(seg) => {
                if !has_v {
                    // every cell tombstoned? (the record's cells: its shape
                    // fields and the extensions of its EXT area)
                    let shape = shape.unwrap();
                    let tombed = |w: &Nat| states.iter().any(|(x, s)| x == w && matches!(s, SVal::Tomb));
                    if shape.fields.iter().all(|f| tombed(&f.v)) {
                        let rec = self.with_record_bytes(seg, u, |raw| self.decode_dense_record(seg, raw))?;
                        if rec.by_v().keys().all(tombed) {
                            meter.grow(EST_SPARSE_U)?;
                            eff.gone.push(u.clone());
                        }
                    }
                }
            }
            None => {
                if !has_v {
                    return Ok(());
                }
                meter.grow(EST_SPARSE_U)?;
                eff.pure.push(u.clone());
            }
        }
        if let Some(s) = v_state {
            meter.grow(EST_SPARSE_U)?;
            eff.v_states.push((u.clone(), s));
        }
        Ok(())
    }

    /// The values of `vs` for every live u of `view` within `[lo, hi]`
    /// (`hi = None`: no upper bound), tested by `test`; returns the matching
    /// u in u order. Same reads and checks as `get_fields` per u, but each
    /// segment is visited once, in file order, with one read state
    /// (`scan_run_fields`). Dead u (newer DELETE) are cut out before reading,
    /// so – as on the general path – their bytes are never read.
    /// Stage 2 (`sp`, exactly one v): the newest SPARSE state of v replaces
    /// the DENSE value (`Tomb` = no cell), pure sparse records are tested
    /// with their state of v.
    /// Errors: the one of the smallest u, as a scan in u order reports it
    /// (a later segment in file order may hold smaller u); limiter and
    /// allocation errors end the scan at once.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scan_view_fields(
        &self,
        view: &WinnerView,
        sp: Option<&SparseEffect>,
        lo: &Nat,
        hi: Option<&Nat>,
        vs: &[Nat],
        meter: &mut Meter,
        test: &mut FieldTest<'_>,
    ) -> Result<Vec<Nat>> {
        if sp.is_some() && vs.len() != 1 {
            return Err(Error::NHyDB("internal: sparse scan of the winner view needs exactly one v".into()));
        }
        let in_bounds = |u: &Nat| u >= lo && hi.is_none_or(|h| u <= h);
        // live pieces (run, off, len): runs clipped to [lo, hi], dead u cut out
        let mut pieces: Vec<(u32, u32, u32)> = Vec::new();
        meter.grow(view.runs.len() as u64 * 12)?;
        reserve(&mut pieces, view.runs.len())?;
        let mut d = 0usize;
        for (ri, r) in view.runs.iter().enumerate() {
            let (rlo, rhi) = (view.lo(r), view.hi(r));
            if hi.is_some_and(|h| &rlo > h) || &rhi < lo {
                continue;
            }
            let base = &view.segs[r.seg as usize].u_start;
            let first = if &rlo < lo { lo.clone() } else { rlo };
            let last = match hi {
                Some(h) if &rhi > h => h.clone(),
                _ => rhi,
            };
            let bid = view.block_id(r);
            let rel = |u: &Nat| u.checked_sub(base).and_then(|x| x.as_u64()).unwrap() as u32;
            while d < view.dels.len() && view.dels[d].0 < first {
                d += 1;
            }
            let mut cur = first;
            while d < view.dels.len() && view.dels[d].0 <= last {
                let (u, dbid) = &view.dels[d];
                if *dbid > bid {
                    if &cur < u {
                        meter.grow(12)?;
                        pieces.push((ri as u32, rel(&cur), rel(u) - rel(&cur)));
                    }
                    cur = u.add_u64(1);
                }
                d += 1;
            }
            if cur <= last {
                meter.grow(12)?;
                pieces.push((ri as u32, rel(&cur), rel(&last) - rel(&cur) + 1));
            }
        }
        pieces.sort_by_key(|&(ri, off, _)| (view.segs[view.runs[ri as usize].seg as usize].block_offset, off));
        let empty: Vec<(Nat, SVal)> = Vec::new();
        let states = sp.map(|s| &s.v_states).unwrap_or(&empty);
        let mut hits: Vec<(u32, u32)> = Vec::new();
        // error of the smallest u so far
        let mut first_err: Option<(Nat, Error)> = None;
        let mut st = DenseReadState::default();
        for &(ri, off, len) in &pieces {
            let seg = &view.segs[view.runs[ri as usize].seg as usize];
            let start_u = seg.u_start.add_u64(off as u64);
            if first_err.as_ref().is_some_and(|(eu, _)| &start_u > eu) {
                continue;
            }
            // sparse states of v within the piece (ascending)
            let mut si = states.partition_point(|(x, _)| x < &start_u);
            // next record the reader hands over (a read error is at it)
            let mut next = off as u64;
            let mut test_err: Option<(Nat, Error)> = None;
            let res = self.scan_run_fields(seg, off as u64, len as u64, vs, &mut st, &mut |idx, mut vals| {
                next = idx + 1;
                let u = seg.u_start.add_u64(idx);
                while si < states.len() && states[si].0 < u {
                    si += 1;
                }
                if si < states.len() && states[si].0 == u {
                    vals[0] = match &states[si].1 {
                        SVal::Tomb => None,
                        SVal::V(x) => Some(x.clone()),
                    };
                }
                match test(&u, &vals) {
                    Ok(true) => {
                        meter.grow(8)?;
                        hits.push((ri, idx as u32));
                        Ok(true)
                    }
                    Ok(false) => Ok(true),
                    Err(e) if is_resource(&e) => Err(e),
                    Err(e) => {
                        test_err = Some((u, e));
                        Ok(false)
                    }
                }
            });
            let found = match res {
                Err(e) if is_resource(&e) => return Err(e),
                Err(e) => Some((seg.u_start.add_u64(next), e)),
                Ok(()) => test_err,
            };
            if let Some((u, e)) = found {
                if first_err.as_ref().is_none_or(|(eu, _)| &u < eu) {
                    first_err = Some((u, e));
                }
            }
        }
        // pure sparse records (no DENSE read), in u order
        let mut pure_hits: Vec<Nat> = Vec::new();
        if let Some(sp) = sp {
            for u in sp.pure.iter().filter(|u| in_bounds(u)) {
                if first_err.as_ref().is_some_and(|(eu, _)| u > eu) {
                    break;
                }
                let val = match states.binary_search_by(|(x, _)| x.cmp(u)) {
                    Ok(i) => match &states[i].1 {
                        SVal::V(x) => Some(x.clone()),
                        SVal::Tomb => None,
                    },
                    Err(_) => None,
                };
                match test(u, &[val]) {
                    Ok(true) => {
                        meter.grow(16)?;
                        pure_hits.push(u.clone());
                    }
                    Ok(false) => {}
                    Err(e) if is_resource(&e) => return Err(e),
                    Err(e) => {
                        if first_err.as_ref().is_none_or(|(eu, _)| u < eu) {
                            first_err = Some((u.clone(), e));
                        }
                        break;
                    }
                }
            }
        }
        if let Some((_, e)) = first_err {
            return Err(e);
        }
        // run order is u order; within a run the record index is
        hits.sort_unstable();
        let mut dense: Vec<Nat> = Vec::new();
        reserve(&mut dense, hits.len())?;
        for (ri, idx) in hits {
            dense.push(view.segs[view.runs[ri as usize].seg as usize].u_start.add_u64(idx as u64));
        }
        if pure_hits.is_empty() {
            return Ok(dense);
        }
        Ok(crate::setops::union_sorted(&dense, &pure_hits))
    }
}

fn reserve<T>(v: &mut Vec<T>, n: usize) -> Result<()> {
    v.try_reserve(n).map_err(|e| Error::Memory(format!("winner view: cannot allocate {n} entries: {e}")))
}

/// Limiter and allocation errors are not data errors of a record: they end
/// a scan at once instead of being ordered by u.
fn is_resource(e: &Error) -> bool {
    matches!(e, Error::ResourceLimit(_) | Error::Memory(_))
}

/// Cuts the u axis into runs: one sweep over the segments in start order
/// with the active segments ordered by block id (the newest wins) and their
/// ends in a min-heap. `None`: a run does not fit the compact form.
fn sweep_runs(segs: &[DenseSegment], meter: &mut Meter) -> Result<Option<Vec<WinRun>>> {
    let n = segs.len();
    let mut runs: Vec<WinRun> = Vec::new();
    let mut active: BTreeSet<(u64, u32)> = BTreeSet::new();
    let mut ends: BinaryHeap<Reverse<(Nat, u32)>> = BinaryHeap::new();
    let mut i = 0usize;
    let mut pos = Nat::ZERO;
    loop {
        if active.is_empty() {
            while i < n && segs[i].count == 0 {
                i += 1;
            }
            if i >= n {
                break;
            }
            pos = segs[i].u_start.clone();
        }
        while i < n && segs[i].u_start <= pos {
            if segs[i].count > 0 {
                active.insert((segs[i].block_id, i as u32));
                ends.push(Reverse((segs[i].u_end.clone(), i as u32)));
            }
            i += 1;
        }
        while let Some(Reverse((e, j))) = ends.peek() {
            if *e < pos {
                active.remove(&(segs[*j as usize].block_id, *j));
                ends.pop();
            } else {
                break;
            }
        }
        let Some(&(_, w)) = active.iter().next_back() else { continue };
        let min_end = &ends.peek().unwrap().0 .0;
        let mut stop = min_end.add_u64(1);
        if i < n && segs[i].u_start < stop {
            stop = segs[i].u_start.clone();
        }
        let seg = &segs[w as usize];
        let (Some(off), Some(len)) = (
            pos.checked_sub(&seg.u_start).and_then(|d| d.as_u64()).and_then(|d| u32::try_from(d).ok()),
            stop.checked_sub(&pos).and_then(|d| d.as_u64()).and_then(|d| u32::try_from(d).ok()),
        ) else {
            return Ok(None);
        };
        match runs.last_mut() {
            Some(last) if last.seg == w && last.off as u64 + last.len as u64 == off as u64 => {
                let Some(l) = last.len.checked_add(len) else { return Ok(None) };
                last.len = l;
            }
            _ => {
                meter.grow(EST_RUN)?;
                runs.push(WinRun { seg: w, off, len });
            }
        }
        pos = stop;
    }
    Ok(Some(runs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limiter::{JobKind, JobPath};

    fn seg(u0: u64, count: u64, bid: u64) -> DenseSegment {
        DenseSegment {
            realm_id: Nat::from(1u64),
            lane: 0,
            shape_id: 1,
            u_start: Nat::from(u0),
            u_end: Nat::from(u0 + count - 1),
            count,
            block_id: bid,
            block_offset: bid * 100,
            payload_offset: 0,
            offsets_offset: 0,
            records_offset: 0,
            payload_len: 0,
        }
    }

    /// reference: newest segment per u by brute force
    fn brute(segs: &[DenseSegment], max_u: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for u in 0..=max_u {
            let best = segs
                .iter()
                .filter(|s| { let n = Nat::from(u); s.count > 0 && n >= s.u_start && n <= s.u_end })
                .max_by_key(|s| s.block_id);
            if let Some(s) = best {
                out.push((u, s.block_id));
            }
        }
        out
    }

    fn expand(segs: &[DenseSegment], runs: &[WinRun]) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for r in runs {
            let s = &segs[r.seg as usize];
            for k in 0..r.len as u64 {
                out.push((s.u_start.as_u64().unwrap() + r.off as u64 + k, s.block_id));
            }
        }
        out
    }

    #[test]
    fn sweep_equals_newest_per_u() {
        // deterministic pseudo random segment sets
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        for case in 0..300 {
            let n = 1 + next(12) as usize;
            let mut segs: Vec<DenseSegment> = (0..n).map(|b| seg(next(60), 1 + next(if case % 3 == 0 { 3 } else { 25 }), b as u64 + 1)).collect();
            segs.sort_by(|a, b| a.u_start.cmp(&b.u_start));
            let mut m = Meter::new(JobKind::Rigid, JobPath::Candidates);
            let runs = sweep_runs(&segs, &mut m).unwrap().unwrap();
            assert_eq!(expand(&segs, &runs), brute(&segs, 100), "case {case}");
            // disjoint, sorted, maximal per segment
            for w in runs.windows(2) {
                let a = segs[w[0].seg as usize].u_start.as_u64().unwrap() + w[0].off as u64 + w[0].len as u64;
                let b = segs[w[1].seg as usize].u_start.as_u64().unwrap() + w[1].off as u64;
                assert!(a <= b);
                assert!(!(w[0].seg == w[1].seg && a == b && w[1].off == w[0].off + w[0].len));
            }
        }
    }

    #[test]
    fn sweep_big_u() {
        // u beyond 2^64: offsets stay relative to the segment start
        let base = Nat::from(u64::MAX).add_u64(1000);
        let mut a = seg(0, 10, 1);
        a.u_start = base.clone();
        a.u_end = base.add_u64(9);
        let mut b = seg(0, 2, 2);
        b.u_start = base.add_u64(4);
        b.u_end = base.add_u64(5);
        let segs = vec![a, b];
        let mut m = Meter::new(JobKind::Rigid, JobPath::Candidates);
        let runs = sweep_runs(&segs, &mut m).unwrap().unwrap();
        assert_eq!(runs, vec![WinRun { seg: 0, off: 0, len: 4 }, WinRun { seg: 1, off: 0, len: 2 }, WinRun { seg: 0, off: 6, len: 4 }]);
    }

    #[test]
    fn live_ranges_apply_newer_deletes_only() {
        let segs = vec![seg(1, 10, 5), seg(4, 2, 9)];
        let mut m = Meter::new(JobKind::Rigid, JobPath::Candidates);
        let runs = sweep_runs(&segs, &mut m).unwrap().unwrap();
        // u2 deleted after its segment (dead), u4 deleted before the
        // rewrite at block 9 (alive), u10 deleted after (dead)
        let dels = vec![(Nat::from(2u64), 6), (Nat::from(4u64), 7), (Nat::from(10u64), 6)];
        let v = WinnerView { segs, runs, dels };
        let r: Vec<(u64, u64)> = v.live_ranges(&mut m).unwrap().iter().map(|(a, b)| (a.as_u64().unwrap(), b.as_u64().unwrap())).collect();
        assert_eq!(r, vec![(1, 1), (3, 9)]);
    }
}
