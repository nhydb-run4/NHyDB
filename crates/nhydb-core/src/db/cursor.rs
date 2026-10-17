//! Speicherschonende Cursor für große Ergebnismengen.
//!
//! Python-v0.14/v0.27 liefern `range`, `select`, `find` und
//! `iter_materialized_sparse_points` als Generatoren. Die Cursor hier halten
//! keinen Borrow auf die Datenbank, sondern nur ihre Position; jeder Aufruf
//! von `next_batch` liefert höchstens `max` Elemente. Damit bleibt der
//! Speicherbedarf unabhängig von der Ergebnisgröße.
//!
//! * `RangeCursor` (`range`/`select`): überspringt u-Bereiche ohne
//!   Dense-Segment (Python iteriert jedes u einzeln).
//! * `FindCursor`: u-Liste aus den Postings (wie Python materialisiert),
//!   Datensätze werden erst beim Abruf gelesen.
//! * `SparsePointCursor`: k-Wege-Merge über die Sparse-Blöcke je
//!   (Realm, Lane); Ausgabe sortiert nach (Realm, Lane, u, v) wie Python,
//!   Speicher O(Anzahl Blöcke + eine u-Gruppe).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::sync::Arc;

use crate::address::Lane;
use crate::error::Result;
use crate::nat::Nat;
use crate::val::Val;

use super::sparse::MaterializedPoint;
use super::{Db, SVal, SparseCellMeta, SparseSegment};

/// Was ein `RangeCursor` je u liefert.
#[derive(Clone, Debug)]
pub enum RangeMode {
    /// `range(...)`: Datensatz wie `get`.
    Get { dyn_mode: Option<String>, dyn_v: Option<Nat>, include_extensions: bool },
    /// `select(...)`: Spaltenprojektion.
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

    /// Nächstes u ≥ `self.next` mit einem Dense-Segment (oder None).
    fn next_candidate(&self, db: &Db) -> Option<Nat> {
        let idx = db.dense.get(&(self.realm.clone(), self.lane))?;
        let mut best: Option<Nat> = None;
        for seg in &idx.segs {
            if seg.u_end < self.next || seg.u_start > self.to {
                continue;
            }
            let cand = if seg.u_start > self.next { seg.u_start.clone() } else { self.next.clone() };
            if best.as_ref().is_none_or(|b| &cand < b) {
                best = Some(cand);
                if best.as_ref() == Some(&self.next) {
                    break;
                }
            }
        }
        best
    }

    pub fn next_batch(&mut self, db: &Db, max: usize) -> Result<Vec<Val>> {
        let mut out = Vec::new();
        while !self.done && out.len() < max {
            let Some(u) = self.next_candidate(db) else {
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
                RangeMode::Select { columns } => db.select_row(&self.realm, &u, columns, self.lane)?,
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

/// k-Wege-Merge über die Sparse-Blöcke.
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
            members.sort(); // aufsteigende Block-ID: spätere überschreiben frühere
            let (rid, ln) = self.keys[self.ki].clone();
            let mut latest: BTreeMap<Nat, SparseCellMeta> = BTreeMap::new();
            for si in members {
                let (seg, bi) = &self.segs[si];
                let (_, abs, len) = &bi[self.pos[si]];
                let payload = db.block_payload(seg.block_offset)?;
                let rel = (abs - seg.payload_offset) as usize;
                for (v, meta) in db.decode_sparse_record(&rid, &u, &payload[rel..rel + *len as usize])? {
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

/// `select_us(realm, us, columns)`: explizite, sortierte Kandidaten.
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
        let end = (self.pos + max).min(self.us.len());
        let out = db.select_us(&self.realm, &self.us[self.pos..end], &self.columns, self.lane)?;
        self.pos = end;
        Ok(out)
    }
}

impl Db {
    /// `select_us` (v0.27): Zeilen für explizite u (nur mit Dense-Datensatz,
    /// nicht gelöscht). Werte inklusive Sparse-Überschreibungen der
    /// BASE-Felder (Korrektur S7, konsistent zu `get`).
    pub fn select_us(&self, realm: &Nat, us: &[Nat], columns: &[Val], lane: Lane) -> Result<Vec<Val>> {
        self.select_us_ext(realm, us, columns, lane, false)
    }

    /// `select_us` mit optionaler Aufnahme reiner Sparse-Records (u ohne
    /// lebenden Dense-Record, aber mit lebenden Sparse-Zellen). Die
    /// Abfrageschicht nutzt diese logische Sicht (Fix S9), damit Kandidaten
    /// eines Index nicht still aus Zeilen/Aggregaten verschwinden.
    pub fn select_us_ext(&self, realm: &Nat, us: &[Nat], columns: &[Val], lane: Lane, sparse_only: bool) -> Result<Vec<Val>> {
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

    /// Ein Zeilenobjekt von `select` (nur u mit Dense-Datensatz).
    pub fn select_row(&self, realm: &Nat, u: &Nat, columns: &[Val], lane: Lane) -> Result<Option<Val>> {
        Ok(self.select(realm, u, u, columns, lane)?.into_iter().next())
    }
}
