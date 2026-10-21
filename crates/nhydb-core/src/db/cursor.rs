//! Memory-efficient cursors for large result sets.
//!
//! `range`, `select`, `find` and `iter_materialized_sparse_points` are
//! exposed as generators. The cursors here hold
//! no borrow on the database, only their position; each call
//! of `next_batch` returns at most `max` elements, so memory use
//! stays independent of the result size.
//!
//! * `RangeCursor` (`range`/`select`): skips u ranges without a
//!   dense segment instead of iterating every u individually.
//! * `FindCursor`: u list from the postings (materialized),
//!   records are read only when fetched.
//! * `SparsePointCursor`: k-way merge over the sparse blocks per
//!   (realm, lane); output sorted by (realm, lane, u, v),
//!   memory O(number of blocks + one u group).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::sync::Arc;

use crate::address::Lane;
use crate::error::Result;
use crate::nat::Nat;
use crate::val::Val;

use super::sparse::MaterializedPoint;
use super::{Db, SVal, SparseCellMeta, SparseSegment};

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
}

impl RangeCursor {
    pub fn new(realm: Nat, lane: Lane, from: Nat, to: Nat, mode: RangeMode) -> RangeCursor {
        let from = if from.is_zero() { Nat::ONE } else { from };
        RangeCursor { realm, lane, next: from, to, mode, done: false }
    }

    /// Next u ≥ `self.next` with a dense segment (or None); a skipped gap
    /// is checked like a point access (F021 rest, G3-03).
    fn next_candidate(&self, db: &Db) -> Result<Option<Nat>> {
        db.next_dense_u_checked(&self.realm, self.lane, &self.next, &self.to)
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<Val>> {

        db.check_open()?;
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
}

impl FindCursor {
    pub fn new(realm: Nat, lane: Lane, us: Vec<Nat>, dyn_mode: Option<String>, dyn_v: Option<Nat>) -> FindCursor {
        FindCursor { realm, lane, us, pos: 0, dyn_mode, dyn_v }
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<Val>> {

        db.check_open()?;
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

type BlockIndex = Arc<Vec<(Nat, u64, u32)>>;

/// k-way merge over the sparse blocks.
pub struct SparsePointCursor {
    keys: Vec<(Nat, Lane)>,
    ki: usize,
    segs: Vec<(SparseSegment, BlockIndex)>,
    pos: Vec<usize>,
    heap: BinaryHeap<Reverse<(Nat, usize)>>,
    pending: VecDeque<MaterializedPoint>,
    loaded: bool,
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
        SparsePointCursor { keys, ki: 0, segs: Vec::new(), pos: Vec::new(), heap: BinaryHeap::new(), pending: VecDeque::new(), loaded: false }
    }

    fn load_key(&mut self, db: &Db) -> Result<()> {
        self.segs.clear();
        self.pos.clear();
        self.heap.clear();
        let key = &self.keys[self.ki];
        if let Some(idx) = db.sparse.get(key) {
            let mut segs: Vec<&SparseSegment> = idx.segs.iter().collect();
            segs.sort_by_key(|s| s.block_id);
            for seg in segs {
                let bi = db.sparse_block_index(seg)?;
                if let Some(first) = bi.first() {
                    self.heap.push(Reverse((first.0.clone(), self.segs.len())));
                }
                self.segs.push((seg.clone(), bi));
                self.pos.push(0);
            }
        }
        self.loaded = true;
        Ok(())
    }

    fn advance_group(&mut self, db: &Db) -> Result<bool> {
        loop {
            if !self.loaded {
                if self.ki >= self.keys.len() {
                    return Ok(false);
                }
                self.load_key(db)?;
            }
            let Some(Reverse((u, _))) = self.heap.peek().cloned() else {
                self.loaded = false;
                self.ki += 1;
                continue;
            };
            let mut members: Vec<usize> = Vec::new();
            while let Some(Reverse((uu, si))) = self.heap.peek().cloned() {
                if uu != u {
                    break;
                }
                self.heap.pop();
                members.push(si);
            }
            members.sort(); // ascending block ID: later ones overwrite earlier ones
            let (rid, ln) = self.keys[self.ki].clone();
            let mut latest: BTreeMap<Nat, SparseCellMeta> = BTreeMap::new();
            for si in members {
                let (seg, bi) = &self.segs[si];
                let (_, abs, len) = &bi[self.pos[si]];
                let rel = (abs - seg.payload_offset) as usize;
                let raw = db.block_range(seg.block_offset, rel, *len as usize)?;
                for (v, meta) in db.decode_sparse_record(&rid, &u, &raw)? {
                    latest.insert(v, meta);
                }
                self.pos[si] += 1;
                if let Some(nx) = bi.get(self.pos[si]) {
                    self.heap.push(Reverse((nx.0.clone(), si)));
                }
            }
            for (v, m) in latest {
                if let SVal::V(value) = m.value {
                    self.pending.push_back(MaterializedPoint {
                        realm: rid.clone(),
                        lane: ln,
                        u: u.clone(),
                        v,
                        value,
                        typ: if m.selftyped { m.typ } else { None },
                    });
                }
            }
            return Ok(true);
        }
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<MaterializedPoint>> {

        db.check_open()?;
        let mut out = Vec::new();
        while out.len() < max {
            if let Some(p) = self.pending.pop_front() {
                out.push(p);
                continue;
            }
            if !self.advance_group(db)? {
                break;
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
}

impl SelectUsCursor {
    pub fn new(realm: Nat, lane: Lane, us: Vec<Nat>, columns: Vec<Val>) -> SelectUsCursor {
        let mut us: Vec<Nat> = us.into_iter().filter(|u| !u.is_zero()).collect();
        us.sort();
        us.dedup();
        SelectUsCursor { realm, lane, us, pos: 0, columns }
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<Val>> {

        db.check_open()?;
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
        for u in us {
            if u.is_zero() {
                continue;
            }
            let by_v = match self.get_internal(realm, u, lane)? {
                Some(rec) => Some(rec.by_v()),
                None if sparse_only && self.sparse.contains_key(&(realm.clone(), lane)) => {
                    let upd = self.sparse_updates_for_u(realm, u, lane)?;
                    let live: std::collections::BTreeMap<Nat, Val> =
                        upd.iter().filter_map(|(v, st)| st.val().map(|x| (v.clone(), x.clone()))).collect();
                    if live.is_empty() {
                        None
                    } else {
                        Some(live)
                    }
                }
                None => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{NL, NR};
    use crate::codec::{FieldDef, TypeCode};
    use crate::db::OpenOptions;
    use crate::db::dense::DenseRows;

    fn make(name: &str) -> (std::path::PathBuf, Db) {
        let d = std::env::temp_dir().join(format!("nhydb_p004_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("r.nhydb");
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        let r = Nat::S(1);
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        db.create_shape(1, &r, "s", vec![FieldDef::new(Nat::S(1), "id", TypeCode::VarUInt)], "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
        (d, db)
    }

    fn put(db: &mut Db, u: &Nat, lane: Lane) {
        let rows = vec![vec![Val::Int(7)]];
        db.put_dense_batch(&Nat::S(1), u, &DenseRows::Tuples(&rows), Some(1), &[], lane, false).unwrap();
    }

    fn us(rows: &[Val]) -> Vec<Nat> {
        rows.iter().map(|r| r.get("_u").and_then(|x| x.as_nat()).unwrap()).collect()
    }

    /// P004: `Db::range`/`Db::select` and the cursor skip gaps (here 2^40 and
    /// 2^70 – the old u-by-u loop would never finish) and agree with each other.
    #[test]
    fn range_select_and_cursor_skip_gaps() {
        let (d, mut db) = make("gap");
        let big = Nat::S(1 << 40);
        let huge = Nat::from_bigint(&(num_bigint::BigInt::from(1u8) << 70)).unwrap();
        for u in [Nat::S(1), Nat::S(2), Nat::S(5), big.clone(), big.add_u64(1), huge.clone()] {
            put(&mut db, &u, NL);
        }
        put(&mut db, &Nat::S(3), NR);
        db.delete(&Nat::S(1), &Nat::S(2), NL, false).unwrap();
        db.commit().unwrap();
        let r = Nat::S(1);
        let to = huge.add_u64(10);
        let want = vec![Nat::S(1), Nat::S(5), big.clone(), big.add_u64(1), huge.clone()];
        assert_eq!(us(&db.range(&r, &Nat::ZERO, &to, NL, None, None, true).unwrap()), want);
        assert_eq!(us(&db.select(&r, &Nat::ZERO, &to, &[Val::str("id")], NL).unwrap()), want);
        assert_eq!(us(&db.range(&r, &Nat::S(3), &big, NL, None, None, true).unwrap()), vec![Nat::S(5), big.clone()]);
        assert_eq!(us(&db.range(&r, &Nat::S(1), &to, NR, None, None, true).unwrap()), vec![Nat::S(3)]);
        for mode in [
            RangeMode::Get { dyn_mode: None, dyn_v: None, include_extensions: true },
            RangeMode::Select { columns: vec![Val::str("id")] },
        ] {
            let mut cur = RangeCursor::new(r.clone(), NL, Nat::ZERO, to.clone(), mode);
            let mut got = Vec::new();
            loop {
                let b = cur.next_batch(&db, 2).unwrap();
                if b.is_empty() {
                    break;
                }
                got.extend(us(&b));
            }
            assert_eq!(got, want);
        }
        // empty range with an unknown column: no error (columns resolved lazily)
        let mut cur = RangeCursor::new(r.clone(), NL, Nat::S(10), Nat::S(20), RangeMode::Select { columns: vec![Val::str("nope")] });
        assert!(cur.next_batch(&db, 10).unwrap().is_empty());
        assert!(db.select(&r, &Nat::S(1), &Nat::S(1), &[Val::str("nope")], NL).is_err());
        db.close(false).unwrap();
        let _ = std::fs::remove_dir_all(d);
    }
}
