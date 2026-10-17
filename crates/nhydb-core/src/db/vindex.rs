//! Gezielter Zugriff auf Sparse-Zellen einer u-Gruppe ohne Vollmerge.
//!
//! v0.14 (`_sparse_updates_for_u`) dekodiert für jeden Zellzugriff alle
//! Sparse-Records aller Segmente, die `u` enthalten, und führt sie zu einer
//! Map zusammen. Bei COLUMN (u = Feld, v = Objekt) und PACKED (alle Objekte
//! auf einem u) ist das der komplette Datenbestand des Layouts.
//!
//! Hier wird je `(realm, lane, u)` einmalig ein v-Index im Speicher
//! aufgebaut (Dateiformat unverändert):
//! * pro Sparse-Record dessen v-Bereich `[v_first, v_last]` in einem
//!   Intervallindex,
//! * pro Record Sprungmarken alle `CK_EVERY` Zellen (Byteposition, v und
//!   Vorgänger-v, weil v delta-kodiert ist).
//!
//! Ein Punktzugriff prüft nur Records, deren v-Bereich `v` enthält, vom
//! neuesten zum ältesten Block (der neueste Zustand gewinnt, wie beim Merge),
//! springt per Sprungmarke in die Nähe von `v` und dekodiert nur die
//! Trefferzelle. Bereichszugriffe arbeiten analog.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::address::{validate_local_v, Lane};
use crate::codec::{decode_value, skip_raw, FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::nat::{get_vu, get_vu_u64, Nat};

use super::interval::IntervalIndex;
use super::layout::FieldResolver;
use super::{Db, SVal, SparseCellMeta, SparseSegment};

/// Abstand der Sprungmarken in Zellen.
const CK_EVERY: u64 = 128;
/// Obergrenze aller gehaltenen Sprungmarken (≈ 50 B je Marke).
pub(crate) const CK_BUDGET: usize = 4 << 20;
/// Obergrenze gehaltener v-Indizes.
const UINDEX_MAX_ENTRIES: usize = 4096;
/// Höchstzahl gemeinsam indexierter u-Gruppen eines Blocks.
const MAX_SIBLINGS: usize = 64;

/// Sprungmarke: Zelle Nr. `idx` beginnt bei `pos` (Schlüsselbyte), hat den
/// Wert `v`, Vorgänger ist `prev`.
#[derive(Clone, Debug)]
struct Ck {
    v: Nat,
    prev: Nat,
    pos: u32,
    idx: u64,
}

/// Ein Sparse-Record (eine u-Gruppe in einem Block) im v-Index.
#[derive(Clone, Debug)]
pub(crate) struct RecSpan {
    block_offset: u64,
    rel: usize,
    len: usize,
    count: u64,
    v_first: Nat,
    v_last: Nat,
    ck: Vec<Ck>,
}

/// v-Index einer u-Gruppe über alle Segmente.
#[derive(Clone, Debug, Default)]
pub(crate) struct UIndex {
    recs: Vec<RecSpan>,
    iv: IntervalIndex,
    /// Anzahl der berücksichtigten Segmente (zur Konsistenzprüfung)
    segs_seen: usize,
}

impl UIndex {
    pub(crate) fn ck_count(&self) -> usize {
        self.recs.iter().map(|r| r.ck.len()).sum()
    }

    fn push(&mut self, r: RecSpan) {
        let id = self.recs.len();
        self.iv.insert(r.v_first.clone(), r.v_last.clone(), id);
        self.recs.push(r);
    }
}

/// Kopf einer Sparse-Zelle; `p` steht danach am Wertanfang.
pub(crate) struct CellHead {
    pub v: Nat,
    pub tomb: bool,
    pub null: bool,
    pub selftyped: bool,
    pub typ: Option<TypeCode>,
}

/// Liest den Zellkopf (`vu (v_delta<<3 | flags) [typecode]`).
pub(crate) fn read_head(raw: &[u8], p: &mut usize, prev: &Nat, realm: &Nat) -> Result<CellHead> {
    let key = get_vu(raw, p)?;
    let (flags, delta) = match &key {
        Nat::S(k) => (k & 7, Nat::S(k >> 3)),
        Nat::B(b) => ((&**b % 8u32).try_into().unwrap_or(0u64), Nat::from(&**b >> 3usize)),
    };
    if delta.is_zero() {
        return Err(Error::Corruption("SPARSE v-Delta muss >0 sein".into()));
    }
    let v = prev.add(&delta);
    validate_local_v(&v, Some(realm))?;
    let tomb = flags & 1 != 0;
    let selftyped = flags & 2 != 0;
    let null = flags & 4 != 0;
    let mut typ = None;
    if selftyped && !tomb {
        let Some(&code) = raw.get(*p) else {
            return Err(Error::Corruption("SPARSE Typcode fehlt".into()));
        };
        *p += 1;
        typ = Some(TypeCode::from_code(code).ok_or_else(|| Error::Corruption(format!("SPARSE Typcode {code} unbekannt")))?);
    }
    Ok(CellHead { v, tomb, null, selftyped, typ })
}

/// Lazily gebaute Typauflösung einer u-Gruppe.
pub(crate) struct LazyResolver<'a> {
    db: &'a Db,
    realm: &'a Nat,
    u: &'a Nat,
    r: Option<FieldResolver<'a>>,
    fast: Option<FastTyp>,
}

/// Typ ohne `FieldDef`-Zugriff (nur zum Überspringen).
enum FastTyp {
    No,
    Fixed(TypeCode),
    /// span, Typ je Offset (dicht, Index = Offset)
    Packed(u64, Vec<Option<TypeCode>>),
}

impl<'a> LazyResolver<'a> {
    pub(crate) fn new(db: &'a Db, realm: &'a Nat, u: &'a Nat) -> Self {
        LazyResolver { db, realm, u, r: None, fast: None }
    }

    fn resolver(&mut self) -> &FieldResolver<'a> {
        let (db, realm, u) = (self.db, self.realm, self.u);
        self.r.get_or_insert_with(|| db.field_resolver(realm, u))
    }

    fn declared(&mut self, v: &Nat) -> Result<std::borrow::Cow<'_, FieldDef>> {
        let (db, realm, u) = (self.db, self.realm, self.u);
        let r = self.r.get_or_insert_with(|| db.field_resolver(realm, u));
        r.get(db, realm, u, v)?
            .ok_or_else(|| Error::Corruption(format!("R{realm},u{u},v{v}: Typ nicht ableitbar")))
    }

    #[inline]
    fn fast_typ(&mut self, v: &Nat) -> Option<TypeCode> {
        if self.fast.is_none() {
            let f = match self.resolver() {
                FieldResolver::Fixed(Some(f)) => FastTyp::Fixed(f.typ),
                FieldResolver::Packed { span: Nat::S(s), fields }
                    if fields.iter().all(|(o, _)| o.as_u64().map(|o| o < 4096).unwrap_or(false)) =>
                {
                    let mut tbl = Vec::new();
                    for (o, f) in fields {
                        let o = o.as_u64().unwrap() as usize;
                        if tbl.len() <= o {
                            tbl.resize(o + 1, None);
                        }
                        tbl[o] = Some(f.typ);
                    }
                    FastTyp::Packed(*s, tbl)
                }
                _ => FastTyp::No,
            };
            self.fast = Some(f);
        }
        match (self.fast.as_ref().unwrap(), v) {
            (FastTyp::Fixed(t), _) => Some(*t),
            (FastTyp::Packed(s, tbl), Nat::S(x)) => tbl.get(((x - 1) % *s) as usize).copied().flatten(),
            _ => None,
        }
    }

    /// Wert der Zelle dekodieren (Semantik `_decode_sparse_record`).
    pub(crate) fn decode(&mut self, h: &CellHead, raw: &[u8], p: &mut usize) -> Result<SVal> {
        if h.tomb {
            return Ok(SVal::Tomb);
        }
        if h.null {
            return Ok(SVal::V(crate::val::Val::None));
        }
        if h.selftyped {
            let f = FieldDef::new(h.v.clone(), format!("v{}", h.v), h.typ.unwrap());
            return Ok(SVal::V(decode_value(&f, raw, p)?));
        }
        let f = self.declared(&h.v)?;
        Ok(SVal::V(decode_value(&f, raw, p)?))
    }

    /// Wert der Zelle überspringen.
    pub(crate) fn skip(&mut self, h: &CellHead, raw: &[u8], p: &mut usize) -> Result<()> {
        if h.tomb || h.null {
            return Ok(());
        }
        if h.selftyped {
            return skip_raw(h.typ.unwrap(), raw, p, || format!("v{}", h.v));
        }
        if let Some(t) = self.fast_typ(&h.v) {
            let mut q = *p;
            if skip_raw(t, raw, &mut q, String::new).is_ok() {
                *p = q;
                return Ok(());
            }
        }
        let f = self.declared(&h.v)?;
        skip_raw(f.typ, raw, p, || f.name.clone())
    }
}

impl Db {
    /// Dekodiert eine u-Gruppe (`_decode_sparse_record`), Typauflösung
    /// einmal pro Gruppe statt pro Zelle.
    /// `_decode_sparse_selected` der v0.27 (Indexaufbau): Typ zuerst aus der
    /// Extension-Definition, dann aus dem Layout; Abbruch nach der ersten
    /// Zelle mit `v > max_v` (wie Python, ohne Restbytes-Prüfung).
    pub(crate) fn decode_sparse_selected(&self, realm: &Nat, u: &Nat, raw: &[u8], max_v: &Nat) -> Result<Vec<(Nat, SparseCellMeta)>> {
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let mut res = LazyResolver::new(self, realm, u);
        let mut p = 0usize;
        let count = get_vu_u64(raw, &mut p, "SPARSE count")?;
        let mut prev = Nat::ZERO;
        let mut out: Vec<(Nat, SparseCellMeta)> = Vec::new();
        for _ in 0..count {
            let h = read_head(raw, &mut p, &prev, realm)?;
            let value = if h.tomb {
                SVal::Tomb
            } else if h.null {
                SVal::V(crate::val::Val::None)
            } else if h.selftyped {
                let f = FieldDef::new(h.v.clone(), format!("v{}", h.v), h.typ.unwrap());
                SVal::V(decode_value(&f, raw, &mut p)?)
            } else if let Some(ext) = self.extension_defs.get(&(realm.clone(), h.v.clone())) {
                SVal::V(decode_value(&ext.field, raw, &mut p)?)
            } else {
                res.decode(&h, raw, &mut p)?
            };
            let meta = SparseCellMeta { value, typ: h.typ, selftyped: h.selftyped };
            let stop = &h.v > max_v;
            match out.last_mut() {
                Some((lv, slot)) if *lv == h.v => *slot = meta,
                _ => out.push((h.v.clone(), meta)),
            }
            prev = h.v;
            if stop {
                break;
            }
        }
        Ok(out)
    }

    pub(crate) fn decode_sparse_record(&self, realm: &Nat, u: &Nat, raw: &[u8]) -> Result<Vec<(Nat, SparseCellMeta)>> {
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let mut res = LazyResolver::new(self, realm, u);
        let mut p = 0usize;
        let count = get_vu_u64(raw, &mut p, "SPARSE count")?;
        let mut prev = Nat::ZERO;
        let mut out: Vec<(Nat, SparseCellMeta)> = Vec::with_capacity(count.min(raw.len() as u64) as usize);
        for _ in 0..count {
            let h = read_head(raw, &mut p, &prev, realm)?;
            let value = res.decode(&h, raw, &mut p)?;
            let meta = SparseCellMeta { value, typ: h.typ, selftyped: h.selftyped };
            match out.last_mut() {
                Some((lv, slot)) if *lv == h.v => *slot = meta,
                _ => out.push((h.v.clone(), meta)),
            }
            prev = h.v;
        }
        if p != raw.len() {
            return Err(Error::Corruption("SPARSE Record Restbytes".into()));
        }
        Ok(out)
    }

    /// Vollständiger Durchlauf eines Records ohne Dekodierung: v-Bereich und
    /// Sprungmarken. `None` für einen leeren Record.
    fn scan_record(&self, realm: &Nat, u: &Nat, raw: &[u8], block_offset: u64, rel: usize) -> Result<Option<RecSpan>> {
        if raw.is_empty() {
            return Ok(None);
        }
        let mut res = LazyResolver::new(self, realm, u);
        let mut p = 0usize;
        let count = get_vu_u64(raw, &mut p, "SPARSE count")?;
        if count == 0 {
            if p != raw.len() {
                return Err(Error::Corruption("SPARSE Record Restbytes".into()));
            }
            return Ok(None);
        }
        let mut prev = Nat::ZERO;
        let mut ck = Vec::with_capacity((count / CK_EVERY + 1).min(raw.len() as u64) as usize);
        let mut v_first = Nat::ZERO;
        for i in 0..count {
            let pos = p;
            let h = read_head(raw, &mut p, &prev, realm)?;
            if i % CK_EVERY == 0 {
                let pos = u32::try_from(pos).map_err(|_| Error::Corruption("SPARSE Record zu gross".into()))?;
                ck.push(Ck { v: h.v.clone(), prev: prev.clone(), pos, idx: i });
            }
            if i == 0 {
                v_first = h.v.clone();
            }
            res.skip(&h, raw, &mut p)?;
            prev = h.v;
        }
        if p != raw.len() {
            return Err(Error::Corruption("SPARSE Record Restbytes".into()));
        }
        Ok(Some(RecSpan { block_offset, rel, len: raw.len(), count, v_first, v_last: prev, ck }))
    }

    fn rec_span_for(&self, realm: &Nat, u: &Nat, seg: &SparseSegment) -> Result<Option<RecSpan>> {
        let Some((payload, rel, len)) = self.read_sparse_record_bytes(seg, u)? else {
            return Ok(None);
        };
        self.scan_record(realm, u, &payload[rel..rel + len], seg.block_offset, rel)
    }

    /// v-Index der u-Gruppe (gecacht).
    pub(crate) fn sparse_uindex(&self, realm: &Nat, u: &Nat, lane: Lane) -> Result<Option<Arc<UIndex>>> {
        let Some(idx) = self.sparse.get(&(realm.clone(), lane)) else {
            return Ok(None);
        };
        let key = (realm.clone(), lane, u.clone());
        if let Some(x) = self.cache().sparse_uindex.get(&key) {
            if x.segs_seen == idx.segs.len() {
                return Ok(Some(x.clone()));
            }
        }
        let mut hits = Vec::new();
        idx.iv.stab(u, &mut hits);
        hits.sort_unstable();
        // Geschwister-u-Gruppen mit derselben Segmentmenge (COLUMN: alle
        // Felder liegen in denselben Blöcken) im selben Durchlauf indexieren,
        // damit jeder Block nur einmal gelesen wird.
        let mut us: Vec<Nat> = vec![u.clone()];
        if let Some(&first) = hits.first() {
            let bi = self.sparse_block_index(&idx.segs[first])?;
            if bi.len() <= MAX_SIBLINGS {
                let mut h2 = Vec::new();
                for (su, _, _) in bi.iter() {
                    if su == u {
                        continue;
                    }
                    let fresh = self
                        .cache()
                        .sparse_uindex
                        .get(&(realm.clone(), lane, su.clone()))
                        .map(|x| x.segs_seen == idx.segs.len())
                        .unwrap_or(false);
                    if fresh {
                        continue;
                    }
                    h2.clear();
                    idx.iv.stab(su, &mut h2);
                    h2.sort_unstable();
                    if h2 == hits {
                        us.push(su.clone());
                    }
                }
            }
        }
        let mut uis: Vec<UIndex> = us.iter().map(|_| UIndex { segs_seen: idx.segs.len(), ..Default::default() }).collect();
        for &i in &hits {
            let seg = &idx.segs[i];
            let bi = self.sparse_block_index(seg)?;
            let payload = self.block_payload(seg.block_offset)?;
            for (k, uu) in us.iter().enumerate() {
                let Ok(j) = bi.binary_search_by(|(x, _, _)| x.cmp(uu)) else {
                    continue;
                };
                let (_, abs, len) = &bi[j];
                let rel = (abs - seg.payload_offset) as usize;
                if let Some(r) = self.scan_record(realm, uu, &payload[rel..rel + *len as usize], seg.block_offset, rel)? {
                    uis[k].push(r);
                }
            }
        }
        let mut result = None;
        // angefragte u-Gruppe zuletzt speichern (überlebt eine Budgetleerung)
        for (k, ui) in uis.into_iter().enumerate().rev() {
            let arc = Arc::new(ui);
            self.store_uindex((realm.clone(), lane, us[k].clone()), arc.clone());
            if k == 0 {
                result = Some(arc);
            }
        }
        Ok(result)
    }

    fn store_uindex(&self, key: (Nat, Lane, Nat), ui: Arc<UIndex>) {
        let n = ui.ck_count();
        let mut c = self.cache();
        if let Some(old) = c.sparse_uindex.remove(&key) {
            c.uindex_ck = c.uindex_ck.saturating_sub(old.ck_count());
        }
        if n > CK_BUDGET {
            return;
        }
        if c.uindex_ck + n > CK_BUDGET || c.sparse_uindex.len() >= UINDEX_MAX_ENTRIES {
            c.sparse_uindex.clear();
            c.uindex_ck = 0;
        }
        c.uindex_ck += n;
        c.sparse_uindex.insert(key, ui);
    }

    /// Nach dem Schreiben (und Registrieren) eines Sparse-Blocks: gecachte
    /// v-Indizes desselben `(realm, lane)` fortschreiben; betroffene
    /// u-Gruppen erhalten den neuen Record.
    pub(crate) fn uindex_note_written(&mut self, realm: &Nat, lane: Lane, us: &[Nat]) {
        let Some(idx) = self.sparse.get(&(realm.clone(), lane)) else {
            return;
        };
        let seg_idx = idx.segs.len() - 1;
        let keys: Vec<(Nat, Lane, Nat)> = self
            .cache()
            .sparse_uindex
            .keys()
            .filter(|(r, l, _)| r == realm && *l == lane)
            .cloned()
            .collect();
        for key in keys {
            let Some(mut ui) = self.cache().sparse_uindex.remove(&key) else {
                continue;
            };
            {
                let mut c = self.cache();
                c.uindex_ck = c.uindex_ck.saturating_sub(ui.ck_count());
            }
            // nur fortschreiben, wenn der Index bis zum Vorgängersegment aktuell war
            if ui.segs_seen != seg_idx {
                continue;
            }
            let r = if us.contains(&key.2) { self.rec_span_for(realm, &key.2, &idx.segs[seg_idx]) } else { Ok(None) };
            if let Ok(r) = r {
                let m = Arc::make_mut(&mut ui);
                if let Some(r) = r {
                    m.push(r);
                }
                m.segs_seen = seg_idx + 1;
                self.store_uindex(key, ui);
            }
        }
    }

    /// Neuester Zustand einer Sparse-Zelle oder `None` (nie geschrieben).
    pub(crate) fn sparse_point(&self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<Option<SVal>> {
        let Some(ui) = self.sparse_uindex(realm, u, lane)? else {
            return Ok(None);
        };
        let mut hits = Vec::new();
        ui.iv.stab(v, &mut hits);
        hits.sort_unstable_by(|a, b| b.cmp(a));
        let mut res = LazyResolver::new(self, realm, u);
        for i in hits {
            let r = &ui.recs[i];
            let payload = self.block_payload(r.block_offset)?;
            let raw = &payload[r.rel..r.rel + r.len];
            if let Some(s) = find_in_record(r, raw, v, realm, &mut res)? {
                return Ok(Some(s));
            }
        }
        Ok(None)
    }

    /// Neueste Zustände aller Zellen mit `lo <= v <= hi` (inkl. Tombstones).
    pub(crate) fn sparse_range(&self, realm: &Nat, u: &Nat, lo: &Nat, hi: &Nat, lane: Lane) -> Result<BTreeMap<Nat, SVal>> {
        let mut out = BTreeMap::new();
        if hi < lo {
            return Ok(out);
        }
        let Some(ui) = self.sparse_uindex(realm, u, lane)? else {
            return Ok(out);
        };
        let mut hits = Vec::new();
        ui.iv.overlapping(lo, hi, &mut hits);
        hits.sort_unstable();
        let mut res = LazyResolver::new(self, realm, u);
        for i in hits {
            let r = &ui.recs[i];
            let payload = self.block_payload(r.block_offset)?;
            let raw = &payload[r.rel..r.rel + r.len];
            range_in_record(r, raw, lo, hi, realm, &mut res, &mut out)?;
        }
        Ok(out)
    }
}

/// Sprungmarke mit größtem `v <= x` (oder die erste).
fn start_ck<'r>(r: &'r RecSpan, x: &Nat) -> &'r Ck {
    let i = r.ck.partition_point(|c| &c.v <= x);
    &r.ck[i.saturating_sub(1)]
}

fn find_in_record(r: &RecSpan, raw: &[u8], v: &Nat, realm: &Nat, res: &mut LazyResolver<'_>) -> Result<Option<SVal>> {
    if v < &r.v_first || v > &r.v_last {
        return Ok(None);
    }
    let c = start_ck(r, v);
    let mut p = c.pos as usize;
    let mut prev = c.prev.clone();
    for _ in c.idx..r.count {
        let h = read_head(raw, &mut p, &prev, realm)?;
        if &h.v == v {
            return res.decode(&h, raw, &mut p).map(Some);
        }
        if &h.v > v {
            return Ok(None);
        }
        res.skip(&h, raw, &mut p)?;
        prev = h.v;
    }
    Ok(None)
}

fn range_in_record(
    r: &RecSpan,
    raw: &[u8],
    lo: &Nat,
    hi: &Nat,
    realm: &Nat,
    res: &mut LazyResolver<'_>,
    out: &mut BTreeMap<Nat, SVal>,
) -> Result<()> {
    let c = start_ck(r, lo);
    let mut p = c.pos as usize;
    let mut prev = c.prev.clone();
    for _ in c.idx..r.count {
        let h = read_head(raw, &mut p, &prev, realm)?;
        if &h.v > hi {
            break;
        }
        if &h.v >= lo {
            let s = res.decode(&h, raw, &mut p)?;
            out.insert(h.v.clone(), s);
        } else {
            res.skip(&h, raw, &mut p)?;
        }
        prev = h.v;
    }
    Ok(())
}
