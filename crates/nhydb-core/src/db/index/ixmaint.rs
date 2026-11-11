//! AP9 (0.28.2/index): indexes that stay usable and maintain themselves.
//!
//! * **S1 – overlay.** A SECONDARY tree is immutable and built for the data
//!   up to its source block `b0`. After later writes the index is *stale*
//!   (`index_is_fresh`). Instead of falling back to a scan, a query uses
//!   `base \ mask ∪ overlay`:
//!   - the **mask** is the set of u written since `b0` (DATA, SPARSE and
//!     DELETE entries of the block directory with a newer block id; pages
//!     whose largest block id is not newer are not read);
//!   - the **overlay** holds the current index keys of exactly these u,
//!     computed with the semantics of `build_maps` (newest live DATA
//!     segment, its stored shape, sparse states in block order, pure
//!     sparse records, extension cells, F010 routing) and the key
//!     generation of `append_index_document` (VALUE, TOKEN, TRIGRAM).
//!
//!   The overlay is derived state of one handle: never written, extended
//!   by newer blocks, dropped on rollbacks (block ids are handed out
//!   again) and bounded by `[index] overlay_max_bytes` (beyond it the
//!   index is unusable as before and the next commit renews it).
//!   A fresh index takes exactly the path of 0.28.2 (one comparison).
//! * **S2′ – merge rebuild.** A new base is written from the old tree and
//!   the overlay in key order with the writer of `persist_secondary`
//!   (`SecWriter`, same codecs); no data scan. `build_indexes` stays the
//!   fallback (FULLTEXT, no base, overlay not available).
//! * **Maintenance** after a successful data commit of a writer (`[index]
//!   maintenance = "auto"`): economical once `rebuild_ratio · N` u changed;
//!   deferred during bulk commits, while the RAM limiter is not green or
//!   other large jobs run; forced beyond `crisis_ratio · N` (never in the
//!   red zone). It runs in its own commit: a failure never touches the
//!   committed data.
//!
//! FULLTEXT stays on its former behaviour (scan while stale, rebuilt by
//! the maintenance with the full build).

use super::*;
use crate::db::directory::DelEntry;
use crate::db::segindex::{PageSource, SegEntry, SegIndex};
use crate::db::{DenseSegment, SparseSegment};
use crate::limiter::{JobKind, JobPath, Meter};
use std::collections::{BTreeMap, BTreeSet};

type RL = (Nat, Lane);
/// Overlays of a handle per `(realm, lane, index source)`.
pub(crate) type OverlayMap = HashMap<(Nat, Lane, u64), Arc<Overlay>>;
use std::sync::atomic::{AtomicU64, Ordering as AO};

/// Version of an overlay state (key of the merged postings in the pool).
static OV_VERSION: AtomicU64 = AtomicU64::new(1);

/// Estimated heap bytes of the overlay parts (budget of the overlay).
const OV_REF_COST: u64 = 24;
const OV_U_COST: u64 = 96;
const OV_KEY_COST: u64 = 96;

/// Encoded index key ordered like the tree (`cmp_keys`).
#[derive(Clone, Debug)]
pub(crate) struct OKey(pub(crate) Vec<u8>);

impl PartialEq for OKey {
    fn eq(&self, o: &Self) -> bool {
        secindex::cmp_keys(&self.0, &o.0) == Ordering::Equal
    }
}
impl Eq for OKey {}
impl PartialOrd for OKey {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for OKey {
    fn cmp(&self, o: &Self) -> Ordering {
        secindex::cmp_keys(&self.0, &o.0)
    }
}

/// Overlay of the indexes of one `(realm, lane)` built up to the same
/// source block.
#[derive(Clone, Default)]
pub(crate) struct Overlay {
    /// source block of the covered indexes
    pub(crate) source: u64,
    /// newest data block of the lane that is covered
    pub(crate) upto: u64,
    pub(crate) version: u64,
    /// changed u: sorted, merged ranges
    pub(crate) mask: Vec<(Nat, Nat)>,
    pub(crate) distinct: u64,
    /// per index: current key → sorted u
    pub(crate) keys: HashMap<u64, BTreeMap<OKey, Vec<Nat>>>,
    /// per FULLTEXT index: terms with positions, document lengths
    pub(crate) ft: HashMap<u64, FtOv>,
    /// keys of every changed u (to replace them when the u changes again)
    contrib: HashMap<Nat, Vec<(u64, OKey)>>,
    /// estimated heap bytes
    pub(crate) bytes: u64,
    /// beyond `overlay_max_bytes`: not usable (contents dropped)
    pub(crate) over_budget: bool,
}

/// FULLTEXT part of an overlay (one index).
#[derive(Clone, Default)]
pub(crate) struct FtOv {
    /// term → (u, positions), sorted by u
    pub(crate) terms: HashMap<String, Vec<FtPosting>>,
    /// document lengths of the overlay documents
    pub(crate) lens: BTreeMap<Nat, u64>,
    /// documents and tokens of the base that lie in the mask
    pub(crate) masked_docs: u64,
    pub(crate) masked_tokens: u64,
}

/// Maintenance state of a writer per `(realm, lane)`.
#[derive(Clone, Debug, Default)]
pub(crate) struct MaintState {
    /// records/cells/deletes written since the index source (upper bound of
    /// the changed u); valid once `seeded` (exact count after an open)
    pub(crate) written: u64,
    pub(crate) seeded: bool,
    /// `written` at the previous maintenance check (size of a commit)
    pub(crate) at_check: u64,
    /// next check once `written` reaches it (after a deferral or failure)
    pub(crate) retry_at: u64,
    /// exact number of changed u once it reached the threshold (then
    /// continued with the counter: it only grows until the rebuild)
    pub(crate) due_distinct: Option<u64>,
    pub(crate) last: String,
    pub(crate) rebuilds: u64,
    pub(crate) merge_rebuilds: u64,
    pub(crate) deferrals: u64,
    pub(crate) failures: u64,
    pub(crate) last_ms: f64,
    pub(crate) last_error: Option<String>,
}

/// Sorted ranges of `iv` merged (adjacent ranges join).
fn merge_ranges(mut iv: Vec<(Nat, Nat)>) -> Vec<(Nat, Nat)> {
    iv.sort();
    let mut out: Vec<(Nat, Nat)> = Vec::with_capacity(iv.len());
    for (lo, hi) in iv {
        if let Some(last) = out.last_mut() {
            if lo <= last.1.add_u64(1) {
                if hi > last.1 {
                    last.1 = hi;
                }
                continue;
            }
        }
        out.push((lo, hi));
    }
    out
}

fn ranges_len(r: &[(Nat, Nat)]) -> u64 {
    r.iter().map(|(l, h)| h.checked_sub(l).and_then(|d| d.as_u64()).map_or(u64::MAX, |d| d.saturating_add(1))).fold(0u64, |a, b| a.saturating_add(b))
}

/// Entries of `idx` with a block id above `b` (pages whose largest block
/// id is not above `b` are not loaded).
fn newer_entries<T: SegEntry>(src: &dyn PageSource<T>, idx: &SegIndex<T>, b: u64, out: &mut Vec<T>) -> Result<()> {
    out.extend(idx.tail.iter().filter(|e| e.bid() > b).cloned());
    for part in &idx.runs {
        for pr in &part.pages {
            if pr.max_bid > b {
                let p = src.load_page(pr)?;
                out.extend(p.entries.iter().filter(|e| e.bid() > b).cloned());
            }
        }
    }
    Ok(())
}

/// `base \ mask ∪ add` (`base`, `add` sorted; `add` holds only masked u).
fn apply_overlay(base: &[Nat], mask: &[(Nat, Nat)], add: &[Nat]) -> Vec<Nat> {
    let mut kept: Vec<Nat> = Vec::with_capacity(base.len() + add.len());
    let mut j = 0usize;
    for u in base {
        while j < mask.len() && &mask[j].1 < u {
            j += 1;
        }
        if j < mask.len() && &mask[j].0 <= u {
            continue;
        }
        kept.push(u.clone());
    }
    if add.is_empty() {
        return kept;
    }
    crate::setops::union_sorted(&kept, add)
}

fn lock_overlays(m: &std::sync::Mutex<OverlayMap>) -> std::sync::MutexGuard<'_, OverlayMap> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Db {
    // ------------------------------------------------------------------
    // bookkeeping

    /// Drops overlays and invalidates the change counters (rollback, reset).
    pub(crate) fn ixm_reset(&mut self) {
        lock_overlays(&self.overlays).clear();
        for st in self.ixm.values_mut() {
            st.seeded = false;
            st.retry_at = 0;
            st.due_distinct = None;
        }
    }

    /// Frees overlays whose source no index of `realm` (lane) has any more.
    pub(crate) fn ixm_drop_unused_overlays(&self, realm: &Nat, lane: Lane) {
        let used: HashSet<u64> = self
            .index_defs
            .values()
            .filter(|i| &i.realm_id == realm)
            .filter_map(|i| self.secondary_source_block.get(&(i.index_id, lane)).copied())
            .collect();
        lock_overlays(&self.overlays).retain(|k, _| !(&k.0 == realm && k.1 == lane && !used.contains(&k.2)));
    }

    /// Counts written records/cells/deletes of a lane (write path, O(1)).
    #[inline]
    pub(crate) fn ixm_note_written(&mut self, key: &RL, n: u64) {
        if self.index_defs.is_empty() {
            return;
        }
        match self.ixm.get_mut(key) {
            Some(s) => s.written = s.written.saturating_add(n),
            None => {
                self.ixm.insert(key.clone(), MaintState { written: n, ..MaintState::default() });
            }
        }
    }

    fn lane_data_max(&self, realm: &Nat, lane: Lane) -> u64 {
        self.data_max_block_id_by_lane.get(&(realm.clone(), lane)).copied().unwrap_or(0)
    }

    /// Indexes of `realm` (lane) that an overlay with source `source` covers.
    fn ov_group(&self, realm: &Nat, lane: Lane, source: u64) -> Vec<IndexDef> {
        self.index_defs
            .values()
            .filter(|i| &i.realm_id == realm && self.secondary_source_block.get(&(i.index_id, lane)) == Some(&source))
            .cloned()
            .collect()
    }

    // ------------------------------------------------------------------
    // S1: change mask and overlay

    /// u written after block `b` as sorted, merged ranges.
    pub(crate) fn ixm_changed_since(&self, realm: &Nat, lane: Lane, b: u64) -> Result<Vec<(Nat, Nat)>> {
        let key: RL = (realm.clone(), lane);
        let mut iv: Vec<(Nat, Nat)> = Vec::new();
        if let Some(d) = self.dense.get(&key) {
            let mut v: Vec<DenseSegment> = Vec::new();
            newer_entries(self, &d.idx, b, &mut v)?;
            iv.extend(v.into_iter().map(|s| (s.u_start, s.u_end)));
        }
        if let Some(s) = self.sparse.get(&key) {
            let mut v: Vec<SparseSegment> = Vec::new();
            newer_entries(self, &s.idx, b, &mut v)?;
            for seg in &v {
                let bi = self.sparse_block_index(seg)?;
                iv.extend(bi.iter().map(|(u, _, _)| (u.clone(), u.clone())));
            }
        }
        if let Some(d) = self.deletes.get(&key) {
            let mut v: Vec<DelEntry> = Vec::new();
            newer_entries(self, &d.runs, b, &mut v)?;
            iv.extend(v.into_iter().map(|e| (e.u.clone(), e.u)));
            iv.extend(d.tail.iter().filter(|(_, x)| **x > b).map(|(u, _)| (u.clone(), u.clone())));
        }
        Ok(merge_ranges(iv))
    }

    /// Values the full build would index for `u`, per position in `idxs`
    /// (semantics of `build_maps`).
    fn ov_values_of_u(&self, realm: &Nat, lane: Lane, u: &Nat, idxs: &[IndexDef], base_vs: &HashSet<Nat>, mixed: bool) -> Result<Vec<(usize, Val)>> {
        let mut out = Vec::new();
        // F010: u routed to a non-CLASSIC layout are never indexed
        if mixed && self.layout_at(realm, u).is_some_and(|l| l.kind != LayoutKind::Classic) {
            return Ok(out);
        }
        let key: RL = (realm.clone(), lane);
        let sp = if self.sparse.contains_key(&key) { Some(self.sparse_updates_for_u(realm, u, lane)?) } else { None };
        let sstate = |v: &Nat| sp.as_ref().and_then(|m| m.get(v));
        let seg = self.resolve_segment(realm, u, lane)?;
        let mut decoded: Option<HashMap<Nat, Val>> = None;
        for (i, idef) in idxs.iter().enumerate() {
            let v = idef.field_v();
            if !base_vs.contains(v) {
                // extension field: newest sparse state (F026: also of a deleted record)
                if let Some((SVal::V(x), _)) = sstate(v) {
                    out.push((i, x.clone()));
                }
                continue;
            }
            match &seg {
                Some(seg) => {
                    let shape = self.shape_of(seg.shape_id)?;
                    if shape.by_v(v).is_none() {
                        // BASE field the stored shape lacks: only a cell
                        // written alone (newest SPARSE state) can hold it
                        if let Some((SVal::V(x), _)) = sstate(v) {
                            out.push((i, x.clone()));
                        }
                        continue;
                    }
                    // F1: a SPARSE state older than the record is hidden
                    let st = sstate(v).filter(|(_, bid)| !crate::db::dense_hides(shape, seg.block_id, v, *bid));
                    match st {
                        Some((SVal::Tomb, _)) => {}
                        Some((SVal::V(x), _)) => out.push((i, x.clone())),
                        None => {
                            if decoded.is_none() {
                                let wanted: HashSet<&Nat> = idxs.iter().map(|d| d.field_v()).filter(|w| base_vs.contains(*w)).collect();
                                decoded = Some(self.with_record_bytes(seg, u, |raw| {
                                    let mut m = HashMap::new();
                                    let mut p = 0usize;
                                    for f in &shape.fields {
                                        if wanted.contains(&f.v) {
                                            m.insert(f.v.clone(), decode_value(f, raw, &mut p)?);
                                        } else {
                                            skip_value(f, raw, &mut p)?;
                                        }
                                    }
                                    Ok(m)
                                })?);
                            }
                            if let Some(val) = decoded.as_ref().unwrap().get(v) {
                                out.push((i, val.clone()));
                            }
                        }
                    }
                }
                None => {
                    // pure sparse record (no live dense record at u)
                    if let Some((SVal::V(x), _)) = sstate(v) {
                        out.push((i, x.clone()));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Computes (or extends `prev`) the overlay of the indexes of `realm`
    /// (lane) with source `source`.
    fn ov_compute(&self, realm: &Nat, lane: Lane, source: u64, prev: Option<&Overlay>) -> Result<Overlay> {
        let data_max = self.lane_data_max(realm, lane);
        let idxs = self.ov_group(realm, lane, source);
        let mut ov = prev.cloned().unwrap_or_else(|| Overlay { source, upto: source, ..Overlay::default() });
        for i in &idxs {
            if i.kind == IndexKind::Fulltext {
                ov.ft.entry(i.index_id).or_default();
            } else {
                ov.keys.entry(i.index_id).or_default();
            }
        }
        let new = self.ixm_changed_since(realm, lane, ov.upto)?;
        let budget = self.config.index_maint.overlay_max_bytes;
        let mut meter = Meter::new(JobKind::Rigid, JobPath::Postings);
        // 1. u written again: their former keys leave the overlay
        if !ov.contrib.is_empty() {
            let mut removed: BTreeMap<(u64, OKey), HashSet<Nat>> = BTreeMap::new();
            for (lo, hi) in &new {
                let n = hi.checked_sub(lo).and_then(|d| d.as_u64()).unwrap_or(u64::MAX);
                if n as usize > ov.contrib.len() {
                    let gone: Vec<Nat> = ov.contrib.keys().filter(|u| *u >= lo && *u <= hi).cloned().collect();
                    for u in gone {
                        for (iid, k) in ov.contrib.remove(&u).unwrap_or_default() {
                            removed.entry((iid, k)).or_default().insert(u.clone());
                        }
                    }
                } else {
                    let mut u = lo.clone();
                    while &u <= hi {
                        if let Some(ks) = ov.contrib.remove(&u) {
                            for (iid, k) in ks {
                                removed.entry((iid, k)).or_default().insert(u.clone());
                            }
                        }
                        u = u.add_u64(1);
                    }
                }
            }
            for ((iid, k), us) in removed {
                if let Some(f) = ov.ft.get_mut(&iid) {
                    if let Ok(Val::Str(term)) = secindex::decode_key(&k.0) {
                        if let Some(list) = f.terms.get_mut(&term) {
                            let before = list.len();
                            list.retain(|p| !us.contains(&p.u));
                            ov.bytes = ov.bytes.saturating_sub((before - list.len()) as u64 * OV_REF_COST);
                            if list.is_empty() {
                                f.terms.remove(&term);
                            }
                        }
                    }
                    continue;
                }
                if let Some(m) = ov.keys.get_mut(&iid) {
                    if let Some(list) = m.get_mut(&k) {
                        let before = list.len();
                        list.retain(|u| !us.contains(u));
                        ov.bytes = ov.bytes.saturating_sub((before - list.len()) as u64 * OV_REF_COST);
                        if list.is_empty() {
                            m.remove(&k);
                        }
                    }
                }
            }
        }
        // 2. current keys of the changed u
        let base_vs: HashSet<Nat> = self.current_base_shape(realm).map(|b| b.fields.iter().map(|f| f.v.clone()).collect()).unwrap_or_default();
        let mixed = self.realm_layouts(realm).iter().any(|l| l.kind != LayoutKind::Classic);
        let kgs: Vec<KeyGen> = idxs.iter().map(KeyGen::of).collect::<Result<_>>()?;
        let mut touched: BTreeSet<(u64, OKey)> = BTreeSet::new();
        let mut ft_touched: BTreeSet<(u64, String)> = BTreeSet::new();
        let has_ft = !ov.ft.is_empty();
        let old_mask = if has_ft { ov.mask.clone() } else { Vec::new() };
        let in_old = |u: &Nat| {
            let j = old_mask.partition_point(|m| &m.1 < u);
            j < old_mask.len() && &old_mask[j].0 <= u
        };
        let mut newly_masked: Vec<Nat> = Vec::new();
        'outer: for (lo, hi) in &new {
            let mut u = lo.clone();
            while &u <= hi {
                if has_ft {
                    for f in ov.ft.values_mut() {
                        f.lens.remove(&u);
                    }
                    if !in_old(&u) {
                        newly_masked.push(u.clone());
                    }
                }
                let vals = self.ov_values_of_u(realm, lane, &u, &idxs, &base_vs, mixed)?;
                let mut mine: Vec<(u64, OKey)> = Vec::new();
                let mut grown = OV_U_COST;
                for (i, val) in vals {
                    let idef = &idxs[i];
                    let iid = idef.index_id;
                    if idef.kind == IndexKind::Fulltext {
                        // terms with positions and the document length, as the build makes them
                        let mut vm = ValueMap::default();
                        Self::append_index_document(&mut vm, idef, &kgs[i], &u, &val)?;
                        let f = ov.ft.get_mut(&iid).unwrap();
                        for (k, b) in vm.items {
                            match b {
                                Bucket::Doc(d) => {
                                    for (du, len) in d {
                                        f.lens.insert(du, len);
                                    }
                                }
                                Bucket::Ft(list) => {
                                    let Val::Str(term) = k else { continue };
                                    for (pu, pos) in list {
                                        grown += OV_REF_COST + 8 * pos.len() as u64;
                                        f.terms.entry(term.clone()).or_default().push(FtPosting { u: pu, positions: pos });
                                    }
                                    mine.push((iid, OKey(secindex::encode_key_vec(&Val::str(term.as_str()))?)));
                                    ft_touched.insert((iid, term));
                                }
                                Bucket::Us(_) => {}
                            }
                        }
                        continue;
                    }
                    let keys: Vec<Val> = if idef.kind == IndexKind::Value {
                        vec![val]
                    } else {
                        let mut vm = ValueMap::default();
                        Self::append_index_document(&mut vm, idef, &kgs[i], &u, &val)?;
                        vm.items.into_iter().map(|(k, _)| k).collect()
                    };
                    let m = ov.keys.get_mut(&iid).unwrap();
                    for k in keys {
                        let ok = OKey(secindex::encode_key_vec(&k)?);
                        match m.get_mut(&ok) {
                            Some(list) => list.push(u.clone()),
                            None => {
                                grown += OV_KEY_COST + ok.0.len() as u64;
                                m.insert(ok.clone(), vec![u.clone()]);
                            }
                        }
                        grown += OV_REF_COST;
                        touched.insert((iid, ok.clone()));
                        mine.push((iid, ok));
                    }
                }
                ov.contrib.insert(u.clone(), mine);
                ov.bytes += grown;
                meter.grow(grown)?;
                if ov.bytes > budget {
                    ov.over_budget = true;
                    break 'outer;
                }
                u = u.add_u64(1);
            }
        }
        if ov.over_budget {
            ov.keys.clear();
            ov.ft.clear();
            ov.contrib.clear();
            ov.mask.clear();
            ov.bytes = 0;
        } else {
            for (iid, term) in ft_touched {
                if let Some(list) = ov.ft.get_mut(&iid).and_then(|f| f.terms.get_mut(&term)) {
                    list.sort_by(|a, b| a.u.cmp(&b.u));
                    list.dedup_by(|a, b| a.u == b.u);
                }
            }
            // base documents that the mask hides now (corpus statistics)
            if !newly_masked.is_empty() {
                for (iid, f) in ov.ft.iter_mut() {
                    for chunk in newly_masked.chunks(65_536) {
                        let c = self.base_fulltext_doc_lengths(*iid, lane, chunk)?;
                        f.masked_docs += c.lengths.len() as u64;
                        f.masked_tokens += c.lengths.values().sum::<u64>();
                    }
                }
            }
            // postings sorted by u (the ranges come in order, but a key may
            // hold u of earlier states and of several ranges)
            for (iid, k) in touched {
                if let Some(list) = ov.keys.get_mut(&iid).and_then(|m| m.get_mut(&k)) {
                    list.sort();
                    list.dedup();
                }
            }
            let mut all = std::mem::take(&mut ov.mask);
            all.extend(new);
            ov.mask = merge_ranges(all);
            ov.distinct = ranges_len(&ov.mask);
        }
        ov.upto = data_max;
        ov.version = OV_VERSION.fetch_add(1, AO::Relaxed);
        Ok(ov)
    }

    /// Overlay of the stale index `index_id` (lane): `base \ mask ∪
    /// overlay` is the current state. `None`: not available (no base,
    /// FULLTEXT, beyond `[index] overlay_max_bytes`).
    pub(crate) fn ixm_overlay(&self, index_id: u64, lane: Lane) -> Result<Option<Arc<Overlay>>> {
        let Some(idx) = self.index_defs.get(&index_id) else { return Ok(None) };
        let Some(&source) = self.secondary_source_block.get(&(index_id, lane)) else { return Ok(None) };
        let data_max = self.lane_data_max(&idx.realm_id, lane);
        let key = (idx.realm_id.clone(), lane, source);
        let prev = {
            let g = lock_overlays(&self.overlays);
            g.get(&key).cloned()
        };
        if let Some(o) = &prev {
            if o.over_budget {
                return Ok(None);
            }
            if o.upto == data_max {
                return Ok(Some(o.clone()));
            }
        }
        // R9D-03: computed without the overlay mutex – the computation may
        // wait in the limiter (memory checkpoints); a reader on the same
        // handle must not block on the mutex meanwhile (with the GIL held)
        // and two readers must not wait for each other. Two threads may
        // compute the same overlay; the one covering more data is kept.
        let ov = Arc::new(self.ov_compute(&idx.realm_id, lane, source, prev.as_deref())?);
        let mut g = lock_overlays(&self.overlays);
        match g.get(&key) {
            Some(cur) if !cur.over_budget && cur.upto >= ov.upto && cur.upto == data_max => {
                let cur = cur.clone();
                return Ok(Some(cur));
            }
            _ => {
                g.insert(key, ov.clone());
            }
        }
        Ok(if ov.over_budget { None } else { Some(ov) })
    }

    /// The planner may use the index: fresh, or stale with an overlay.
    pub fn index_usable(&self, index_id: u64, lane: Lane) -> bool {
        self.index_is_fresh(index_id, lane) || matches!(self.ixm_overlay(index_id, lane), Ok(Some(_)))
    }

    /// Overlay of the index when it is stale; `Err(StaleIndex)` when it is
    /// stale and has none; `None` when it is fresh.
    pub(crate) fn ixm_for_query(&self, index_id: u64, lane: Lane, what: &str) -> Result<Option<Arc<Overlay>>> {
        if self.index_is_fresh(index_id, lane) {
            return Ok(None);
        }
        match self.ixm_overlay(index_id, lane)? {
            Some(o) => Ok(Some(o)),
            None => {
                let name = self.index_defs.get(&index_id).map(|i| i.name.clone()).unwrap_or_default();
                Err(Error::StaleIndex(format!("Index {}/{} {what}", Val::str(name.as_str()).py_repr(), lane_name(lane))))
            }
        }
    }

    /// Sorted u of the overlay keys of `index_id` that satisfy `cond`
    /// (same key semantics as `eq_entries`/`range_entries`).
    fn ov_matching(&self, ov: &Overlay, index_id: u64, cond: &Cond) -> Result<Vec<Nat>> {
        let Some(m) = ov.keys.get(&index_id) else { return Ok(Vec::new()) };
        let mut lists: Vec<&Vec<Nat>> = Vec::new();
        match cond {
            Cond::Eq(v) => {
                if contains_nan(v) {
                    return Ok(Vec::new());
                }
                match v {
                    Val::List(_) | Val::Map(_) => {
                        for (k, l) in m {
                            if secindex::is_container_key(&k.0) && &secindex::decode_key(&k.0)? == v {
                                lists.push(l);
                            }
                        }
                    }
                    _ => {
                        if let Some(l) = m.get(&OKey(secindex::encode_key_vec(v)?)) {
                            lists.push(l);
                        }
                    }
                }
            }
            Cond::Range(lo, hi) => {
                let c = range_class(lo);
                if c >= 3 || range_class(hi) != c {
                    for (k, l) in m {
                        if crate::query::match_raw(&secindex::decode_key(&k.0)?, "between", lo, hi)? {
                            lists.push(l);
                        }
                    }
                } else {
                    let (elo, ehi) = (secindex::encode_key_vec(lo)?, secindex::encode_key_vec(hi)?);
                    if secindex::cmp_keys(&elo, &ehi) != Ordering::Greater {
                        for (k, l) in m.range(OKey(elo)..) {
                            if secindex::range_class_of(&k.0) != c || secindex::cmp_keys(&k.0, &ehi) == Ordering::Greater {
                                break;
                            }
                            lists.push(l);
                        }
                    }
                }
            }
        }
        Ok(match lists.len() {
            0 => Vec::new(),
            1 => lists[0].clone(),
            _ => {
                let mut all: Vec<Nat> = lists.into_iter().flatten().cloned().collect();
                all.sort();
                all.dedup();
                all
            }
        })
    }

    /// `posting_for_condition` of a stale index: base \ mask ∪ overlay
    /// (point lookups cached per overlay version).
    pub(crate) fn ov_posting_for_condition(&self, ov: &Overlay, index_id: u64, cond: &Cond, lane: Lane) -> Result<Arc<Vec<Nat>>> {
        let point = match cond {
            Cond::Eq(v) if !matches!(v, Val::List(_) | Val::Map(_)) && !contains_nan(v) => Some(v),
            _ => None,
        };
        if let Some(v) = point {
            if let Some(PostVal::Us(c)) = self.cache().postings.get(&PostKey::Ov(index_id, lane, ValKey(v.clone()), ov.version)) {
                return Ok(c);
            }
        }
        let base = self.posting_for_condition(index_id, cond, lane)?;
        let add = self.ov_matching(ov, index_id, cond)?;
        let out = Arc::new(apply_overlay(&base, &ov.mask, &add));
        if let Some(v) = point {
            self.posting_cache_put(PostKey::Ov(index_id, lane, ValKey(v.clone()), ov.version), PostVal::Us(out.clone()));
        }
        Ok(out)
    }

    /// Posting of one key, with the overlay when the index is stale.
    pub(crate) fn ov_load_posting(&self, ov: Option<&Overlay>, index_id: u64, value: &Val, lane: Lane) -> Result<Arc<Vec<Nat>>> {
        match ov {
            None => self.load_posting(index_id, value, lane),
            Some(o) => self.ov_posting_for_condition(o, index_id, &Cond::Eq(value.clone()), lane),
        }
    }

    /// `posting_for_condition`, with the overlay when the index is stale.
    pub(crate) fn ov_posting(&self, ov: Option<&Overlay>, index_id: u64, cond: &Cond, lane: Lane) -> Result<Arc<Vec<Nat>>> {
        match ov {
            None => self.posting_for_condition(index_id, cond, lane),
            Some(o) => self.ov_posting_for_condition(o, index_id, cond, lane),
        }
    }

}

/// Order of the posting widths (4 < 8 < 0 = arbitrary u).
fn width_rank(w: u8) -> u8 {
    match w {
        4 => 1,
        8 => 2,
        _ => 3,
    }
}

/// Result of a maintenance step of one lane.
#[derive(Default, Debug)]
pub(crate) struct RebuildReport {
    pub(crate) merged: Vec<u64>,
    pub(crate) full: Vec<u64>,
}

impl Db {
    // ------------------------------------------------------------------
    // S2′: merge rebuild

    /// Renews the stale index `index_id` (lane) from its key tree and the
    /// overlay, without a data scan: every key of the old tree minus the
    /// changed u, merged in key order with the overlay keys, written with
    /// the writer and codecs of `persist_secondary` (one write scope).
    /// `Ok(false)`: not applicable (FULLTEXT, no base, no overlay) – the
    /// caller uses `build_indexes`.
    pub(crate) fn ixm_merge_rebuild(&mut self, index_id: u64, lane: Lane) -> Result<bool> {
        let Some(idef) = self.index_defs.get(&index_id).cloned() else { return Ok(false) };
        if !self.secondary_blocks.contains_key(&(index_id, lane)) {
            return Ok(false);
        }
        if idef.kind == IndexKind::Fulltext {
            return self.ixm_merge_rebuild_fulltext(index_id, lane);
        }
        let Some(ov) = self.ixm_overlay(index_id, lane)? else { return Ok(false) };
        let rk: RL = (idef.realm_id.clone(), lane);
        let source = self.lane_data_max(&idef.realm_id, lane);
        if ov.upto != source || self.secondary_source_block.get(&(index_id, lane)) != Some(&ov.source) {
            return Ok(false);
        }
        let mut base: Vec<SecEntry> = Vec::new();
        self.sec_scan(index_id, lane, None, &mut |e| {
            base.push(e.clone());
            Ok(true)
        })?;
        let empty = BTreeMap::new();
        let ovkeys = ov.keys.get(&index_id).unwrap_or(&empty);
        // posting width: the lane maximum, the overlay u and every width
        // the old tree already uses (F009: sparse u beyond the dense maximum)
        let mut max_u = self.max_u.get(&rk).cloned().unwrap_or_default();
        for us in ovkeys.values() {
            if let Some(l) = us.last() {
                if l > &max_u {
                    max_u = l.clone();
                }
            }
        }
        let mut width = width_for(&max_u);
        for e in &base {
            if width_rank(e.width) > width_rank(width) {
                width = e.width;
            }
        }
        let mask = ov.mask.clone();
        let ovk: Vec<(Vec<u8>, Vec<Nat>)> = ovkeys.iter().map(|(k, v)| (k.0.clone(), v.clone())).collect();
        drop(ov);
        self.write_scope(|db| {
            let mut meter = Meter::new(JobKind::Rigid, JobPath::IndexBuild);
            let mut w = secindex::SecWriter::new(index_id, lane);
            let (mut bi, mut oi) = (0usize, 0usize);
            loop {
                let ord = match (base.get(bi), ovk.get(oi)) {
                    (None, None) => break,
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (Some(b), Some(o)) => secindex::cmp_keys(&b.key, &o.0),
                };
                let (key, from_base, add): (Vec<u8>, Option<&SecEntry>, &[Nat]) = match ord {
                    Ordering::Less => {
                        bi += 1;
                        (base[bi - 1].key.clone(), Some(&base[bi - 1]), &[])
                    }
                    Ordering::Greater => {
                        oi += 1;
                        (ovk[oi - 1].0.clone(), None, &ovk[oi - 1].1[..])
                    }
                    Ordering::Equal => {
                        bi += 1;
                        oi += 1;
                        (base[bi - 1].key.clone(), Some(&base[bi - 1]), &ovk[oi - 1].1[..])
                    }
                };
                // fast path: fixed width of the old posting = target width,
                // filtered and merged on the raw bytes; an untouched key
                // keeps its bytes
                if let Some(e) = from_base.filter(|e| (e.width == 4 || e.width == 8) && e.width == width) {
                    let raw = db.sec_posting_raw(index_id, lane, e)?;
                    let wd = e.width as usize;
                    let n = raw.len() / wd;
                    if n as u64 != e.raw_count || raw.len() != n * wd {
                        return Err(Error::Corruption("Posting count mismatch".into()));
                    }
                    let at = |i: usize| -> u64 {
                        if wd == 4 {
                            u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap()) as u64
                        } else {
                            u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap())
                        }
                    };
                    // R9A-01: a bound >= 2^64 does not fit u64 (None). A lower
                    // bound beyond u64 lies above every posting u (not touched);
                    // an upper bound beyond u64 lies above every posting u too.
                    let hi_of = |m: &(Nat, Nat)| m.1.as_u64();
                    let lo_of = |m: &(Nat, Nat)| m.0.as_u64();
                    let touched = n > 0 && {
                        let (first, last) = (at(0), at(n - 1));
                        let j = mask.partition_point(|m| hi_of(m).is_some_and(|h| h < first));
                        j < mask.len() && lo_of(&mask[j]).is_some_and(|l| l <= last)
                    };
                    if !touched && add.is_empty() {
                        meter.grow(raw.len() as u64)?;
                        w.add(db, key, n as u64, e.width, &raw)?;
                        continue;
                    }
                    let addv: Vec<u64> = add.iter().map(|u| u.as_u64().ok_or_else(|| Error::Corruption("overlay u wider than the posting".into()))).collect::<Result<_>>()?;
                    let mut out: Vec<u8> = Vec::with_capacity(raw.len() + addv.len() * wd);
                    let emit = |v: u64, out: &mut Vec<u8>| {
                        if wd == 4 {
                            out.extend_from_slice(&(v as u32).to_le_bytes())
                        } else {
                            out.extend_from_slice(&v.to_le_bytes())
                        }
                    };
                    let (mut j, mut a, mut cnt) = (0usize, 0usize, 0u64);
                    for i in 0..n {
                        let u = at(i);
                        while j < mask.len() && hi_of(&mask[j]).is_some_and(|h| h < u) {
                            j += 1;
                        }
                        if j < mask.len() && lo_of(&mask[j]).is_some_and(|l| l <= u) {
                            continue;
                        }
                        while a < addv.len() && addv[a] < u {
                            emit(addv[a], &mut out);
                            a += 1;
                            cnt += 1;
                        }
                        emit(u, &mut out);
                        cnt += 1;
                    }
                    while a < addv.len() {
                        emit(addv[a], &mut out);
                        a += 1;
                        cnt += 1;
                    }
                    if cnt == 0 {
                        continue;
                    }
                    meter.grow(out.len() as u64)?;
                    w.add(db, key, cnt, e.width, &out)?;
                    continue;
                }
                // general path: through Nat (other widths, overlay-only keys)
                let kept: Vec<Nat> = match from_base {
                    Some(e) => apply_overlay(&db.sec_posting_us(index_id, lane, e)?, &mask, &[]),
                    None => Vec::new(),
                };
                let all: Vec<Nat> = if add.is_empty() { kept } else { crate::setops::union_sorted(&kept, add) };
                if all.is_empty() {
                    continue;
                }
                let mut ul = UList::default();
                for u in &all {
                    ul.push(u)?;
                }
                let (code, raw) = encode_bucket(&Bucket::Us(ul), width)?;
                meter.grow(raw.len() as u64)?;
                w.add(db, key, all.len() as u64, code, &raw)?;
            }
            let (root, _pages, page_blocks) = w.finish(db, source)?;
            db.dir.unlisted_blocks += page_blocks;
            let (off, _) = db.append_block(BT_SECONDARY, &root, lane_flags(lane))?;
            let info = db.read_header_at(off)?;
            db.parse_secondary_directory(&info)?;
            Ok(())
        })?;
        self.store.flush()?;
        Ok(true)
    }

    // ------------------------------------------------------------------
    // maintenance after a commit

    /// Records of a lane for the shares of `[index]` (directory counts;
    /// overwritten records count again until a compaction).
    fn ixm_records(&self, key: &RL) -> u64 {
        let d = self.dense.get(key).map(|d| d.records).unwrap_or(0);
        let s = self.sparse.get(key).map(|s| s.groups).unwrap_or(0);
        d.max(s)
    }

    /// Indexes of `(realm, lane)` the maintenance renews: stale ones with a
    /// base, and never built ones of a lane with data (not those defined
    /// since the last commit – `build_indexes` builds them).
    fn ixm_due_indexes(&self, realm: &Nat, lane: Lane) -> Vec<u64> {
        let has_data = self.lane_data_max(realm, lane) > 0;
        self.index_defs
            .values()
            .filter(|i| &i.realm_id == realm && !self.ixm_new_defs.contains(&i.index_id))
            .filter(|i| match self.secondary_source_block.get(&(i.index_id, lane)) {
                Some(_) => !self.index_is_fresh(i.index_id, lane),
                None => has_data,
            })
            .map(|i| i.index_id)
            .collect()
    }

    /// AP9: maintenance after a successful data commit of a writer.
    pub(crate) fn ixm_after_commit(&mut self) {
        if self.readonly || self.closed || self.sys.poisoned || self.index_defs.is_empty() {
            return;
        }
        let mut lanes: Vec<RL> = Vec::new();
        for i in self.index_defs.values() {
            for lane in [0u8, 1] {
                let k = (i.realm_id.clone(), lane);
                if !lanes.contains(&k) && self.lane_data_max(&k.0, lane) > 0 {
                    lanes.push(k);
                }
            }
        }
        for (realm, lane) in lanes {
            self.ixm_maintain_lane(&realm, lane);
            if self.sys.poisoned {
                return;
            }
        }
    }

    /// Decision and maintenance step of one lane (never fails the commit).
    fn ixm_maintain_lane(&mut self, realm: &Nat, lane: Lane) {
        let key: RL = (realm.clone(), lane);
        let due = self.ixm_due_indexes(realm, lane);
        if due.is_empty() {
            // fresh again (or nothing to do): the counter starts at the source
            if let Some(st) = self.ixm.get_mut(&key) {
                st.written = 0;
                st.seeded = true;
                st.at_check = 0;
                st.retry_at = 0;
                st.due_distinct = None;
            }
            return;
        }
        let cfg = self.config.index_maint.clone();
        let n = self.ixm_records(&key).max(1);
        let thr = ((cfg.rebuild_ratio * n as f64) as u64).max(cfg.rebuild_min_changes);
        let crisis_n = ((cfg.crisis_ratio * n as f64) as u64).max(thr);
        let bulk_n = ((cfg.bulk_commit_ratio * n as f64) as u64).max(1);
        // oldest source of the due indexes (never built: everything changed)
        let sources: Vec<Option<u64>> = due.iter().map(|i| self.secondary_source_block.get(&(*i, lane)).copied()).collect();
        let never_built = sources.iter().any(|s| s.is_none());
        let b0 = sources.iter().flatten().copied().min().unwrap_or(0);
        let mut st = self.ixm.remove(&key).unwrap_or_default();
        // size of this commit from the write counter (before it is seeded)
        let commit_size = st.written.saturating_sub(st.at_check);
        if !st.seeded {
            // first check of the handle: exact count once, then counted while writing
            st.written = if never_built { n } else { self.ixm_changed_since(realm, lane, b0).map(|r| ranges_len(&r)).unwrap_or(u64::MAX) };
            st.seeded = true;
        }
        st.at_check = st.written;
        let over_budget = lock_overlays(&self.overlays).iter().any(|(k, o)| k.0 == *realm && k.1 == lane && o.over_budget);
        // E4 (R9A-03): an index without a base in this lane (never built,
        // or its base lost by a compaction of an emptied lane) has no
        // overlay – its direct queries fail until it is built. It is built
        // at the next commit, without waiting for the rebuild threshold
        // (which a small lane would never reach); a failed build is retried
        // after `retry_at` as usual.
        let below = if never_built { st.written < st.retry_at } else { st.written < thr.max(st.retry_at) };
        if below && !over_budget {
            self.ixm.insert(key, st);
            return;
        }
        // exact number of changed u (duplicates of the counter removed);
        // known once above the threshold and continued with the counter
        let distinct = match st.due_distinct {
            Some(d) => d.saturating_add(commit_size),
            None if never_built => n,
            None => self.ixm_changed_since(realm, lane, b0).map(|r| ranges_len(&r)).unwrap_or(u64::MAX),
        };
        if distinct < thr && !over_budget && !never_built {
            st.written = distinct;
            st.at_check = distinct;
            st.retry_at = 0;
            st.last = format!("{distinct} changed u below the rebuild threshold {thr}");
            self.ixm.insert(key, st);
            return;
        }
        st.due_distinct = Some(distinct);
        // crisis: the share bound, or the overlay would no longer fit its
        // budget (queries would scan) – estimated per changed u from the
        // current overlay of the lane, else from the indexes of the lane
        let per_u = {
            let g = lock_overlays(&self.overlays);
            g.iter()
                .filter(|(k, o)| k.0 == *realm && k.1 == lane && o.distinct > 0 && !o.over_budget)
                .map(|(_, o)| o.bytes / o.distinct)
                .max()
                .unwrap_or(OV_U_COST + OV_REF_COST * due.len() as u64)
        };
        let fills_budget = distinct.saturating_mul(per_u) >= cfg.overlay_max_bytes / 10 * 9;
        // E4: an index without a base cannot answer at all (no overlay) –
        // built like in a crisis, not deferred by a bulk commit (with a
        // small lane every commit counts as one); only red defers it
        let crisis = distinct >= crisis_n || over_budget || fills_budget || never_built;
        let lim = crate::limiter::global();
        // zone measured now (the monitor thread parks while no job runs);
        // a disabled limiter never steers – neither does it defer here
        let zone = if !lim.config().enabled {
            crate::limiter::Zone::Green
        } else {
            match crate::limiter::footprint::footprint() {
                Some(level) => lim.bounds().next_zone(lim.zone(), level),
                None => lim.zone(),
            }
        };
        let others = lim.jobs().len();
        let defer = if zone == crate::limiter::Zone::Red {
            Some("RAM limiter red".to_string())
        } else if crisis {
            None
        } else if commit_size >= bulk_n {
            Some(format!("bulk commit of {commit_size} records"))
        } else if zone != crate::limiter::Zone::Green {
            Some(format!("RAM limiter {}", zone.name()))
        } else if others > 0 {
            Some(format!("{others} other large job(s) running"))
        } else {
            None
        };
        if let Some(why) = defer {
            st.deferrals += 1;
            st.last = format!("deferred: {why} ({distinct} changed u)");
            // re-evaluated at the next commit (a bulk import resumes as soon
            // as a smaller commit follows)
            st.retry_at = 0;
            self.ixm.insert(key, st);
            return;
        }
        let t = Instant::now();
        self.ixm_busy = true;
        let r = self.ixm_rebuild_lane(realm, lane, &due);
        self.ixm_busy = false;
        st.last_ms = t.elapsed().as_secs_f64() * 1e3;
        match r {
            Ok(rep) => {
                st.rebuilds += 1;
                st.merge_rebuilds += !rep.merged.is_empty() as u64;
                st.due_distinct = None;
                st.written = 0;
                st.at_check = 0;
                st.retry_at = 0;
                st.seeded = true;
                st.last_error = None;
                st.last = format!(
                    "rebuilt after {distinct} changed u{}: merge {:?}, full {:?} ({:.0} ms)",
                    if over_budget || fills_budget {
                        " (overlay budget)"
                    } else if crisis {
                        " (crisis bound)"
                    } else {
                        ""
                    },
                    rep.merged,
                    rep.full,
                    st.last_ms
                );
            }
            Err(e) => {
                // the data commit stands; the overlay keeps the answers right
                st.failures += 1;
                st.last_error = Some(e.to_string());
                st.last = format!("rebuild failed after {distinct} changed u: {e}");
                st.retry_at = st.written.saturating_add((thr / 2).max(1));
                // the rollback of the write scope reset the counters
                st.seeded = false;
                st.due_distinct = None;
            }
        }
        self.ixm.insert(key, st);
    }

    /// Renews the due indexes of a lane: merge rebuild where possible,
    /// `build_indexes` for the rest, then one commit.
    fn ixm_rebuild_lane(&mut self, _realm: &Nat, lane: Lane, due: &[u64]) -> Result<RebuildReport> {
        let mut rep = RebuildReport::default();
        for &iid in due {
            if self.ixm_merge_rebuild(iid, lane)? {
                rep.merged.push(iid);
            } else {
                rep.full.push(iid);
            }
        }
        if !rep.full.is_empty() {
            self.build_indexes(Some(&rep.full), false)?;
        }
        self.cache().postings.clear();
        self.commit_core()?;
        Ok(rep)
    }

    /// Maintenance state of the indexes (`health()`).
    pub(crate) fn ixm_status(&self) -> Val {
        let mut rows = Vec::new();
        for ((realm, lane), st) in &self.ixm {
            rows.push(vmap! {
                "realm" => realm, "lane" => *lane as i64, "changed_since_build" => st.written as i64,
                "rebuilds" => st.rebuilds as i64, "merge_rebuilds" => st.merge_rebuilds as i64,
                "deferrals" => st.deferrals as i64, "failures" => st.failures as i64,
                "last" => st.last.as_str(), "last_ms" => st.last_ms,
                "last_error" => st.last_error.clone().map(Val::Str).unwrap_or(Val::None),
            });
        }
        vmap! {"maintenance" => if self.config.index_maint.auto { "auto" } else { "manual" }, "lanes" => Val::List(rows)}
    }
}

impl Db {
    // ------------------------------------------------------------------
    // FULLTEXT through the overlay

    /// Overlay of a stale FULLTEXT index (`None`: fresh or not available).
    pub(crate) fn ixm_ft_overlay(&self, index_id: u64, lane: Lane) -> Result<Option<Arc<Overlay>>> {
        if self.index_is_fresh(index_id, lane) {
            return Ok(None);
        }
        Ok(self.ixm_overlay(index_id, lane)?.filter(|o| o.ft.contains_key(&index_id)))
    }

    /// Term posting of a stale FULLTEXT index: base \ mask ∪ overlay.
    pub(crate) fn ov_fulltext_term_posting(&self, ov: &Overlay, index_id: u64, term: &str, lane: Lane) -> Result<Arc<Vec<FtPosting>>> {
        let base = self.base_fulltext_term_posting(index_id, term, lane)?;
        let add: &[FtPosting] = ov.ft.get(&index_id).and_then(|f| f.terms.get(term)).map(|v| &v[..]).unwrap_or(&[]);
        let m = &ov.mask;
        let mut out: Vec<FtPosting> = Vec::with_capacity(base.len() + add.len());
        let (mut j, mut a) = (0usize, 0usize);
        for p in base.iter() {
            while j < m.len() && m[j].1 < p.u {
                j += 1;
            }
            if j < m.len() && m[j].0 <= p.u {
                continue;
            }
            while a < add.len() && add[a].u < p.u {
                out.push(add[a].clone());
                a += 1;
            }
            out.push(p.clone());
        }
        out.extend(add[a..].iter().cloned());
        Ok(Arc::new(out))
    }

    /// Document lengths and corpus totals of a stale FULLTEXT index.
    pub(crate) fn ov_fulltext_doc_lengths(&self, ov: &Overlay, index_id: u64, lane: Lane, us: &[Nat]) -> Result<FtCorpus> {
        let f = ov.ft.get(&index_id).cloned().unwrap_or_default();
        let m = &ov.mask;
        let masked = |u: &Nat| {
            let j = m.partition_point(|r| &r.1 < u);
            j < m.len() && &m[j].0 <= u
        };
        let base_us: Vec<Nat> = us.iter().filter(|u| !masked(u)).cloned().collect();
        let base = self.base_fulltext_doc_lengths(index_id, lane, &base_us)?;
        let mut lengths = base.lengths;
        for u in us.iter().filter(|u| masked(u)) {
            if let Some(l) = f.lens.get(u) {
                lengths.insert(u.clone(), *l);
            }
        }
        Ok(FtCorpus {
            lengths,
            total_docs: base.total_docs - f.masked_docs.min(base.total_docs) + f.lens.len() as u64,
            total_tokens: base.total_tokens - f.masked_tokens.min(base.total_tokens) + f.lens.values().sum::<u64>(),
        })
    }

    /// Whole corpus of a stale FULLTEXT index.
    pub(crate) fn ov_fulltext_corpus(&self, ov: &Overlay, index_id: u64, lane: Lane) -> Result<FtCorpus> {
        let f = ov.ft.get(&index_id).cloned().unwrap_or_default();
        let base = self.base_fulltext_corpus(index_id, lane)?;
        let m = &ov.mask;
        let mut lengths: BTreeMap<Nat, u64> = base
            .lengths
            .into_iter()
            .filter(|(u, _)| {
                let j = m.partition_point(|r| &r.1 < u);
                !(j < m.len() && &m[j].0 <= u)
            })
            .collect();
        lengths.extend(f.lens.iter().map(|(u, l)| (u.clone(), *l)));
        let total_tokens = lengths.values().sum();
        Ok(FtCorpus { total_docs: lengths.len() as u64, total_tokens, lengths })
    }
}

impl Db {
    /// S2′ for FULLTEXT: terms (positions) of the old tree minus the
    /// changed documents, merged with the overlay terms; document lengths
    /// re-segmented from the old segments and the overlay lengths (streamed,
    /// one segment at a time). `Ok(false)`: old document length format
    /// (0.28.0/0.28.1) or no overlay – the caller uses `build_indexes`.
    fn ixm_merge_rebuild_fulltext(&mut self, index_id: u64, lane: Lane) -> Result<bool> {
        let Some(ov) = self.ixm_overlay(index_id, lane)? else { return Ok(false) };
        let realm = self.index_defs[&index_id].realm_id.clone();
        let source = self.lane_data_max(&realm, lane);
        if ov.upto != source || self.secondary_source_block.get(&(index_id, lane)) != Some(&ov.source) {
            return Ok(false);
        }
        let Some(f) = ov.ft.get(&index_id) else { return Ok(false) };
        let mut base: Vec<SecEntry> = Vec::new();
        self.sec_scan(index_id, lane, None, &mut |e| {
            base.push(e.clone());
            Ok(true)
        })?;
        let doc_key = secindex::encode_key_vec(&Val::str(FULLTEXT_DOCLEN_KEY))?;
        let Some(summary) = base.iter().find(|e| e.key == doc_key) else { return Ok(false) };
        if summary.width != PAYLOAD_FULLTEXT_DOCLEN_SUMMARY {
            return Ok(false);
        }
        let mask = ov.mask.clone();
        let masked = |u: &Nat| {
            let j = mask.partition_point(|r| &r.1 < u);
            j < mask.len() && &mask[j].0 <= u
        };
        // overlay terms in key order
        let mut ovt: Vec<(Vec<u8>, Vec<FtPosting>)> =
            f.terms.iter().map(|(t, l)| Ok((secindex::encode_key_vec(&Val::str(t.as_str()))?, l.clone()))).collect::<Result<_>>()?;
        ovt.sort_by(|a, b| secindex::cmp_keys(&a.0, &b.0));
        // document lengths: old segments \ mask ∪ overlay, re-segmented
        let mut segs_out: Vec<(Vec<u8>, u64, Vec<u8>)> = Vec::new();
        let (mut docs, mut tokens) = (0u64, 0u64);
        {
            let target = secindex::page_bytes().min(textindex::DOCLEN_SEGMENT_BYTES);
            let mut batch: Vec<(Nat, u64)> = Vec::new();
            let mut ovl = f.lens.iter().peekable();
            let flush = |batch: &mut Vec<(Nat, u64)>, out: &mut Vec<(Vec<u8>, u64, Vec<u8>)>, docs: &mut u64, tokens: &mut u64| -> Result<()> {
                *docs += batch.len() as u64;
                *tokens += batch.iter().map(|x| x.1).sum::<u64>();
                for seg in textindex::doclen_segments(batch, target) {
                    let last = &seg.last().expect("segments are not empty").0;
                    let k = secindex::encode_key_vec(&Val::Str(textindex::doclen_segment_key(last)))?;
                    out.push((k, seg.len() as u64, textindex::encode_fulltext_doc_lengths(seg)?));
                }
                batch.clear();
                Ok(())
            };
            for e in base.iter().filter(|e| e.width == PAYLOAD_FULLTEXT_DOCLEN_SEGMENT) {
                let raw = self.sec_posting_raw(index_id, lane, e)?;
                let c = textindex::decode_fulltext_doc_lengths(&raw, Some(e.raw_count))?;
                for (u, l) in c.lengths {
                    while let Some((ou, ol)) = ovl.peek() {
                        if *ou >= &u {
                            break;
                        }
                        batch.push(((*ou).clone(), **ol));
                        ovl.next();
                    }
                    if masked(&u) {
                        continue;
                    }
                    batch.push((u, l));
                }
                if batch.len() >= 1 << 20 {
                    flush(&mut batch, &mut segs_out, &mut docs, &mut tokens)?;
                }
            }
            for (ou, ol) in ovl {
                batch.push((ou.clone(), *ol));
            }
            flush(&mut batch, &mut segs_out, &mut docs, &mut tokens)?;
        }
        drop(ov);
        self.write_scope(|db| {
            let mut meter = Meter::new(JobKind::Rigid, JobPath::IndexBuild);
            let mut w = secindex::SecWriter::new(index_id, lane);
            let (mut bi, mut oi) = (0usize, 0usize);
            loop {
                // old document length entries are replaced as a whole
                while base.get(bi).is_some_and(|e| e.width == PAYLOAD_FULLTEXT_DOCLEN_SEGMENT) {
                    bi += 1;
                }
                let ord = match (base.get(bi), ovt.get(oi)) {
                    (None, None) => break,
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (Some(b), Some(o)) => secindex::cmp_keys(&b.key, &o.0),
                };
                if ord != Ordering::Greater && base[bi].width == PAYLOAD_FULLTEXT_DOCLEN_SUMMARY {
                    // the new summary and all segments at the place of the old ones
                    let sum = textindex::encode_doclen_summary(docs, tokens, segs_out.len() as u64);
                    w.add(db, doc_key.clone(), docs, PAYLOAD_FULLTEXT_DOCLEN_SUMMARY, &sum)?;
                    for (k, n, raw) in &segs_out {
                        meter.grow(raw.len() as u64)?;
                        w.add(db, k.clone(), *n, PAYLOAD_FULLTEXT_DOCLEN_SEGMENT, raw)?;
                    }
                    bi += 1;
                    continue;
                }
                let (key, from_base, add): (Vec<u8>, Option<&SecEntry>, &[FtPosting]) = match ord {
                    Ordering::Less => {
                        bi += 1;
                        (base[bi - 1].key.clone(), Some(&base[bi - 1]), &[])
                    }
                    Ordering::Greater => {
                        oi += 1;
                        (ovt[oi - 1].0.clone(), None, &ovt[oi - 1].1[..])
                    }
                    Ordering::Equal => {
                        bi += 1;
                        oi += 1;
                        (base[bi - 1].key.clone(), Some(&base[bi - 1]), &ovt[oi - 1].1[..])
                    }
                };
                let mut entries: Vec<(Nat, Vec<u64>)> = Vec::new();
                if let Some(e) = from_base {
                    if e.width != PAYLOAD_FULLTEXT_POSITIONS {
                        return Err(Error::Corruption("FULLTEXT term has wrong payload codec".into()));
                    }
                    let raw = db.sec_posting_raw(index_id, lane, e)?;
                    let old = textindex::decode_fulltext_posting(&raw, Some(e.raw_count))
                        .map_err(|x| Error::Corruption(format!("FULLTEXT term payload corrupt: {x}")))?;
                    if add.is_empty() && !old.iter().any(|p| masked(&p.u)) {
                        // untouched term: its bytes stay
                        meter.grow(raw.len() as u64)?;
                        w.add(db, key, e.raw_count, PAYLOAD_FULLTEXT_POSITIONS, &raw)?;
                        continue;
                    }
                    let mut a = 0usize;
                    for p in old {
                        if masked(&p.u) {
                            continue;
                        }
                        while a < add.len() && add[a].u < p.u {
                            entries.push((add[a].u.clone(), add[a].positions.clone()));
                            a += 1;
                        }
                        entries.push((p.u, p.positions));
                    }
                    entries.extend(add[a..].iter().map(|p| (p.u.clone(), p.positions.clone())));
                } else {
                    entries.extend(add.iter().map(|p| (p.u.clone(), p.positions.clone())));
                }
                if entries.is_empty() {
                    continue;
                }
                let raw = textindex::encode_fulltext_posting(&entries)?;
                meter.grow(raw.len() as u64)?;
                w.add(db, key, entries.len() as u64, PAYLOAD_FULLTEXT_POSITIONS, &raw)?;
            }
            let (root, _pages, page_blocks) = w.finish(db, source)?;
            db.dir.unlisted_blocks += page_blocks;
            let (off, _) = db.append_block(BT_SECONDARY, &root, lane_flags(lane))?;
            let info = db.read_header_at(off)?;
            db.parse_secondary_directory(&info)?;
            Ok(())
        })?;
        self.store.flush()?;
        Ok(true)
    }
}
