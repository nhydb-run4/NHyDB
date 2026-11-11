//! Layouts (CLASSIC/COLUMN/PACKED/CUSTOM), mixed-realm routing,
//! space-ownership protection and safe layout evolution.

use std::collections::BTreeSet;
use std::sync::Arc;

use num_bigint::{BigInt, Sign};

use crate::address::{self, lane_name, realm_span, CellAddress, Lane};
use crate::codec::{int_like, FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::Nat;
use crate::plugin::{LayoutPlugin, PluginMethod};
use crate::schema::{LayoutDef, LayoutKind};

/// Entries of the field type cache before its older half is evicted. Kept
/// at 3/8 of the 2^17 buckets the map reaches: the tombstones the eviction
/// leaves then always allow an in-place rehash. Near the old bound of 65 536
/// (more than half of the bucket capacity) the map sometimes doubled its
/// table on a rehash and briefly held both (RUN8 QA: +38 MB in
/// `memory_bounds`).
const FIELD_META_MAX: usize = 49_152;
use crate::val::Val;
use crate::vmap;

use super::Db;

/// Input for `create_layout`.
#[derive(Clone, Debug)]
pub struct LayoutSpec {
    pub layout_id: u64,
    pub realm_id: Nat,
    pub name: String,
    pub kind: String,
    pub u_start: Nat,
    pub u_end: Option<Nat>,
    pub priority: i64,
    pub shape_id: Option<u64>,
    pub base_reserve_to: Nat,
    pub params: Val,
    pub spacemap: Option<Val>,
    pub plugin: String,
    pub description: String,
    pub replace: bool,
}

impl LayoutSpec {
    pub fn new(layout_id: u64, realm_id: Nat, name: &str, kind: &str) -> Self {
        LayoutSpec {
            layout_id,
            realm_id,
            name: name.to_string(),
            kind: kind.to_string(),
            u_start: Nat::ONE,
            u_end: None,
            priority: 0,
            shape_id: None,
            base_reserve_to: Nat::ZERO,
            params: Val::map(),
            spacemap: None,
            plugin: String::new(),
            description: String::new(),
            replace: false,
        }
    }
}

/// Layout reference by ID or unique name.
#[derive(Clone, Debug)]
pub enum LayoutRef {
    Id(u64),
    Name(String),
}

impl From<u64> for LayoutRef {
    fn from(v: u64) -> Self {
        LayoutRef::Id(v)
    }
}

/// Changes for a layout evolution (`None` = unchanged).
#[derive(Clone, Debug, Default)]
pub struct EvolveSpec {
    pub name: Option<String>,
    pub description: Option<String>,
    pub u_start: Option<Nat>,
    /// `None` = unchanged; `Some(None)` = unbounded.
    pub u_end: Option<Option<Nat>>,
    pub priority: Option<i64>,
    pub add_fields: Vec<Val>,
    pub spacemap_update: Option<Val>,
}

#[derive(Clone, Debug)]
pub struct RoutingChange {
    pub from: Nat,
    pub to: Option<Nat>,
    pub old: Option<(u64, String, LayoutKind)>,
    pub new: Option<(u64, String, LayoutKind)>,
}

impl RoutingChange {
    fn same_target(&self, o: &RoutingChange) -> bool {
        self.old == o.old && self.new == o.new
    }

    pub fn to_val(&self) -> Val {
        let f = |x: &Option<(u64, String, LayoutKind)>| -> (Val, Val, Val) {
            match x {
                Some((id, n, k)) => (Val::Int(*id as i64), Val::str(n), Val::str(k.name())),
                None => (Val::None, Val::None, Val::None),
            }
        };
        let (oi, on, ok) = f(&self.old);
        let (ni, nn, nk) = f(&self.new);
        vmap! {
            "from" => &self.from, "to" => Val::opt_nat(self.to.as_ref()),
            "old_layout_id" => oi, "old_layout_name" => on, "old_kind" => ok,
            "new_layout_id" => ni, "new_layout_name" => nn, "new_kind" => nk,
        }
    }

    fn contains(&self, u: &Nat) -> bool {
        u >= &self.from && self.to.as_ref().map(|t| u <= t).unwrap_or(true)
    }
}

pub(crate) fn lane_from_val(v: &Val) -> Result<Lane> {
    match v {
        Val::None => Ok(0),
        Val::Str(s) => address::normalize_lane_str(s),
        other => match other.as_i64() {
            Some(i) => address::normalize_lane_int(i),
            None => Err(Error::Value("lane must be 0 or 1".into())),
        },
    }
}

/// Python `int(x)` for positive coordinates (negative → 0 → positivity error).
pub(crate) fn coord(v: &Val) -> Result<Nat> {
    match int_like(v) {
        Some(b) if b.sign() == Sign::Minus => Ok(Nat::ZERO),
        Some(b) => Ok(Nat::from(b.magnitude().clone())),
        None => Err(Error::Type(format!("int() argument must be a string or a number, not {}", v.py_repr()))),
    }
}

fn big_to_nat_checked(b: &BigInt) -> Option<Nat> {
    if b.sign() == Sign::Minus { None } else { Some(Nat::from(b.magnitude().clone())) }
}

impl Db {
    // ------------------------------------------------------------------
    // SpaceMaps
    // ------------------------------------------------------------------

    pub fn default_spacemap(kind: LayoutKind, params: &Val) -> Val {
        match kind {
            LayoutKind::Classic => vmap! {"record" => "u", "field" => "v", "anchor" => "v=1"},
            LayoutKind::Column => vmap! {"record" => "v", "field" => "u", "anchor" => "u=anchor_u"},
            LayoutKind::Packed => {
                let u = params.get("u").cloned().map(|v| int_like(&v).map(Val::from_bigint).unwrap_or(v)).unwrap_or(Val::Int(1));
                let span = params
                    .get("span")
                    .cloned()
                    .map(|v| int_like(&v).map(Val::from_bigint).unwrap_or(v))
                    .unwrap_or(Val::Int(100_000_000));
                vmap! {
                    "u" => u, "span" => span, "anchor" => "v=(record-1)*span+1",
                    "record" => "floor((v-1)/span)+1", "offset" => "(v-1)%span",
                }
            }
            LayoutKind::Custom => vmap! {
                "mapping" => "runtime plugin",
                "plugin" => params.get("plugin").cloned().unwrap_or(Val::str("")),
            },
        }
    }

    pub(crate) fn plugin_for(&self, lay: &LayoutDef) -> Option<Arc<dyn LayoutPlugin>> {
        self.plugins.get(&lay.plugin).cloned()
    }

    pub fn effective_spacemap(&self, lay: &LayoutDef) -> Result<Val> {
        if lay.kind.is_builtin() {
            return Ok(Self::default_spacemap(lay.kind, &lay.params));
        }
        if let Some(p) = self.plugin_for(lay) {
            if p.has(PluginMethod::Spacemap) {
                let sm = p.spacemap(lay)?;
                if !matches!(sm, Val::Map(_)) {
                    return Err(Error::Schema(format!("CUSTOM plugin '{}': spacemap() must return a dict", lay.plugin)));
                }
                return Ok(sm);
            }
        }
        if lay.spacemap.is_truthy() {
            Ok(lay.spacemap.clone())
        } else {
            Ok(vmap! {"mapping" => "runtime plugin", "plugin" => lay.plugin.as_str()})
        }
    }

    // ------------------------------------------------------------------
    // Routing
    // ------------------------------------------------------------------

    fn winner_from<'a>(layouts: &[&'a LayoutDef], realm: &Nat, u: &Nat) -> Option<&'a LayoutDef> {
        layouts
            .iter()
            .copied()
            .filter(|l| &l.realm_id == realm && l.contains_u(u))
            .max_by(|a, b| LayoutDef::sort_cmp(a, b))
    }

    pub(crate) fn realm_layouts(&self, realm: &Nat) -> Vec<&LayoutDef> {
        self.layouts_by_realm
            .get(realm)
            .map(|ids| ids.iter().filter_map(|i| self.layouts.get(i)).collect())
            .unwrap_or_default()
    }

    /// Candidates in winner order (`layout_candidates`).
    pub fn layout_candidates(&self, realm: &Nat, u: &Nat) -> Vec<&LayoutDef> {
        let mut c: Vec<&LayoutDef> = self.realm_layouts(realm).into_iter().filter(|l| l.contains_u(u)).collect();
        c.sort_by(|a, b| LayoutDef::sort_cmp(b, a));
        c
    }

    /// Winning layout at `(realm, u)`.
    pub fn layout_at(&self, realm: &Nat, u: &Nat) -> Option<&LayoutDef> {
        let ids = self.layouts_by_realm.get(realm)?;
        let mut best: Option<&LayoutDef> = None;
        for i in ids {
            if let Some(l) = self.layouts.get(i) {
                if l.contains_u(u) && best.map(|b| LayoutDef::sort_cmp(l, b).is_gt()).unwrap_or(true) {
                    best = Some(l);
                }
            }
        }
        best
    }

    pub fn route_address(&self, realm: &Nat, u: &Nat, v: Option<&Nat>, lane: Lane) -> Result<Val> {
        address::require_positive("u", u)?;
        self.require_realm(realm)?;
        if let Some(v) = v {
            address::validate_local_v(v, Some(realm))?;
        }
        let cands = self.layout_candidates(realm, u);
        let w = cands.first();
        Ok(vmap! {
            "realm" => realm, "u" => u, "v" => Val::opt_nat(v), "lane" => lane as i64,
            "lane_name" => lane_name(lane),
            "layout_id" => w.map(|l| l.layout_id as i64),
            "layout_name" => w.map(|l| l.name.clone()),
            "kind" => w.map(|l| l.kind.name()),
            "priority" => w.map(|l| l.priority),
            "candidate_count" => cands.len() as i64,
            "candidates" => Val::List(cands.iter().map(|x| vmap! {
                "layout_id" => x.layout_id as i64, "name" => x.name.as_str(), "kind" => x.kind.name(),
                "priority" => x.priority, "u_start" => &x.u_start, "u_end" => Val::opt_nat(x.u_end.as_ref()),
            }).collect()),
        })
    }

    pub fn routing_change_intervals(&self, candidate: &LayoutDef, replace_layout_id: Option<u64>) -> Vec<RoutingChange> {
        let rid = &candidate.realm_id;
        let old: Vec<&LayoutDef> = self.realm_layouts(rid);
        let mut new: Vec<&LayoutDef> =
            old.iter().copied().filter(|x| replace_layout_id.map(|r| x.layout_id != r).unwrap_or(true)).collect();
        new.push(candidate);
        let mut points: BTreeSet<Nat> = BTreeSet::new();
        points.insert(Nat::ONE);
        for lay in old.iter().chain(new.iter()) {
            points.insert(lay.u_start.clone());
            if let Some(e) = &lay.u_end {
                points.insert(e.add_u64(1));
            }
        }
        let ordered: Vec<Nat> = points.into_iter().filter(|x| *x >= 1).collect();
        let mut changes: Vec<RoutingChange> = Vec::new();
        for (i, lo) in ordered.iter().enumerate() {
            let hi = ordered.get(i + 1).map(|n| n.sub_u64(1).unwrap());
            let ow = Self::winner_from(&old, rid, lo);
            let nw = Self::winner_from(&new, rid, lo);
            let oid = ow.map(|l| l.layout_id);
            let nid = nw.map(|l| l.layout_id);
            if oid == nid {
                continue;
            }
            let row = RoutingChange {
                from: lo.clone(),
                to: hi,
                old: ow.map(|l| (l.layout_id, l.name.clone(), l.kind)),
                new: nw.map(|l| (l.layout_id, l.name.clone(), l.kind)),
            };
            if let Some(last) = changes.last_mut() {
                if let Some(t) = &last.to {
                    if &t.add_u64(1) == lo && last.same_target(&row) {
                        last.to = row.to;
                        continue;
                    }
                }
            }
            changes.push(row);
        }
        changes
    }

    pub fn occupied_routing_impact(&self, realm: &Nat, changes: &[RoutingChange]) -> Result<Val> {
        let risky: Vec<&RoutingChange> =
            changes.iter().filter(|x| x.old.is_some() && x.old.as_ref().map(|o| o.0) != x.new.as_ref().map(|n| n.0)).collect();
        if risky.is_empty() {
            return Ok(vmap! {
                "unsafe" => false, "dense_records_at_risk_upper_bound" => 0i64,
                "sparse_u_groups_at_risk" => 0i64, "sparse_cells_at_risk" => 0i64,
                "sample_u" => Val::List(vec![]), "routing_intervals_at_risk" => 0i64,
            });
        }
        let mut samples: BTreeSet<Nat> = BTreeSet::new();
        let add_sample = |s: &mut BTreeSet<Nat>, u: &Nat| {
            if s.len() < 12 {
                s.insert(u.clone());
            }
        };
        let mut dense_count = Nat::ZERO;
        for lane in [0u8, 1] {
            let mut ranges: Vec<(Nat, Nat)> = Vec::new();
            if let Some(idx) = self.dense.get(&(realm.clone(), lane)) {
                for seg in &idx.idx.collect_by_start(self)? {
                    for ch in &risky {
                        let lo = seg.u_start.clone().max(ch.from.clone());
                        let hi = match &ch.to {
                            None => seg.u_end.clone(),
                            Some(t) => seg.u_end.clone().min(t.clone()),
                        };
                        if lo <= hi {
                            add_sample(&mut samples, &lo);
                            ranges.push((lo, hi));
                        }
                    }
                }
            }
            if ranges.is_empty() {
                continue;
            }
            ranges.sort();
            let (mut cs, mut ce) = ranges[0].clone();
            for (a, b) in ranges.into_iter().skip(1) {
                if a <= ce.add_u64(1) {
                    if b > ce {
                        ce = b;
                    }
                } else {
                    dense_count = dense_count.add(&ce.checked_sub(&cs).unwrap().add_u64(1));
                    cs = a;
                    ce = b;
                }
            }
            dense_count = dense_count.add(&ce.checked_sub(&cs).unwrap().add_u64(1));
        }
        // P021: membership via a hash set (was `Vec::contains`, quadratic in
        // the number of sparse u groups); the counts do not depend on order.
        let mut sparse_u: Vec<(Lane, Nat)> = Vec::new();
        let mut sparse_seen: std::collections::HashSet<(Lane, Nat)> = std::collections::HashSet::new();
        for lane in [0u8, 1] {
            if let Some(idx) = self.sparse.get(&(realm.clone(), lane)) {
                for seg in &idx.idx.collect_block_order(self)? {
                    let touched = risky.iter().any(|ch| {
                        seg.u_end >= ch.from && ch.to.as_ref().map(|t| &seg.u_start <= t).unwrap_or(true)
                    });
                    if !touched {
                        continue;
                    }
                    let bi = self.sparse_block_index(seg)?;
                    for (u, _, _) in bi.iter() {
                        if risky.iter().any(|ch| ch.contains(u)) {
                            if sparse_seen.insert((lane, u.clone())) {
                                sparse_u.push((lane, u.clone()));
                            }
                            add_sample(&mut samples, u);
                        }
                    }
                }
            }
        }
        let mut sparse_cells: u64 = 0;
        let mut groups: u64 = 0;
        for (lane, u) in &sparse_u {
            let m = self.sparse_updates_for_u(realm, u, *lane)?;
            let live = m.values().filter(|(s, _)| !s.is_tomb()).count() as u64;
            sparse_cells += live;
            if live > 0 {
                groups += 1;
            }
        }
        Ok(vmap! {
            "unsafe" => !dense_count.is_zero() || sparse_cells > 0,
            "dense_records_at_risk_upper_bound" => dense_count,
            "sparse_u_groups_at_risk" => groups as i64,
            "sparse_cells_at_risk" => sparse_cells as i64,
            "sample_u" => Val::List(samples.iter().take(12).map(Val::nat).collect()),
            "routing_intervals_at_risk" => risky.len() as i64,
        })
    }

    pub fn plan_layout_candidate(&self, candidate: &LayoutDef, replace_layout_id: Option<u64>, action: &str) -> Result<Val> {
        let changes = self.routing_change_intervals(candidate, replace_layout_id);
        let impact = self.occupied_routing_impact(&candidate.realm_id, &changes)?;
        let unsafe_ = impact.get("unsafe").map(|v| v.is_truthy()).unwrap_or(false);
        Ok(vmap! {
            "action" => action, "layout_id" => candidate.layout_id as i64,
            "layout_name" => candidate.name.as_str(), "kind" => candidate.kind.name(),
            "realm" => &candidate.realm_id,
            "proposed" => vmap! {
                "u_start" => &candidate.u_start, "u_end" => Val::opt_nat(candidate.u_end.as_ref()),
                "priority" => candidate.priority, "spacemap" => self.effective_spacemap(candidate)?,
            },
            "routing_changes" => Val::List(changes.iter().map(|c| c.to_val()).collect()),
            "occupied_impact" => impact,
            "status" => if unsafe_ { "unsafe" } else { "safe" },
            "safe" => !unsafe_,
        })
    }

    // ------------------------------------------------------------------
    // Creation
    // ------------------------------------------------------------------

    pub fn create_layout(&mut self, spec: LayoutSpec) -> Result<LayoutDef> {
        let realm_id = spec.realm_id.clone();
        self.require_realm(&realm_id)?;
        if spec.layout_id == 0 {
            return Err(Error::Value("layout_id must be > 0".into()));
        }
        if self.math_layouts.contains_key(&spec.layout_id) {
            return Err(Error::Schema(format!("layout ID {} already belongs to a MATH layout", spec.layout_id)));
        }
        self.deny_math_realm(&realm_id)?;
        let old_layout = self.layouts.get(&spec.layout_id).cloned();
        if let Some(old) = &old_layout {
            if !spec.replace {
                return Err(Error::Schema(format!(
                    "layout {} already exists; use replace=True for evolution",
                    spec.layout_id
                )));
            }
            if old.realm_id != realm_id {
                return Err(Error::Schema("an existing layout must not change its realm".into()));
            }
        }
        let mut params = if let Val::Map(_) = spec.params { spec.params.clone() } else { Val::map() };
        let kind_u = spec.kind.trim().to_uppercase();
        if let Some(sid) = spec.shape_id {
            match self.shapes.get(&sid) {
                Some(sh) if sh.realm_id == realm_id => {}
                _ => return Err(Error::Schema("shape missing or does not match the realm".into())),
            }
        }
        if kind_u == "CLASSIC" && spec.shape_id.is_none() {
            return Err(Error::Schema("CLASSIC requires shape_id".into()));
        }
        if kind_u == "PACKED" {
            let span = match params.get("span") {
                None => Nat::S(100_000_000),
                Some(v) => coord(v)?,
            };
            address::require_positive("span", &span)?;
            let pu = match params.get("u") {
                None => spec.u_start.clone(),
                Some(v) => coord(v)?,
            };
            address::require_positive("packed u", &pu)?;
            params.set("span", Val::nat(&span));
            params.set("u", Val::nat(&pu));
            if &span > realm_span() {
                return Err(Error::Schema("PACKED span must not exceed REALM_SPAN".into()));
            }
            if pu < spec.u_start || spec.u_end.as_ref().map(|e| &pu > e).unwrap_or(false) {
                return Err(Error::Schema("PACKED u must lie within the layout u range".into()));
            }
        }
        if matches!(kind_u.as_str(), "COLUMN" | "PACKED" | "CUSTOM") {
            let fields: Vec<Val> = params.get("fields").and_then(|f| f.as_list()).cloned().unwrap_or_default();
            let mut seen_n = std::collections::HashSet::new();
            let mut seen_pos = std::collections::HashSet::new();
            for f in &fields {
                let lf = crate::schema::LayoutField { raw: f };
                let name_f = lf.name();
                let typ = lf.type_name();
                if TypeCode::from_name(&typ).is_none() {
                    return Err(Error::Schema(format!("unknown data type '{typ}'")));
                }
                if !seen_n.insert(name_f.clone()) {
                    return Err(Error::Schema(format!("layout field name '{name_f}' is duplicated")));
                }
                if kind_u == "COLUMN" {
                    let pos = lf.u()?;
                    address::require_positive("column u", &pos)?;
                    if !seen_pos.insert(Val::nat(&pos).py_repr()) {
                        return Err(Error::Schema("COLUMN fields must have unique u".into()));
                    }
                    if pos < spec.u_start || spec.u_end.as_ref().map(|e| &pos > e).unwrap_or(false) {
                        return Err(Error::Schema(format!("COLUMN field u{pos} lies outside the layout range")));
                    }
                } else if kind_u == "PACKED" {
                    let pos = lf.offset()?;
                    let span = params.get("span").and_then(|s| s.as_bigint()).unwrap_or_default();
                    if pos.sign() == Sign::Minus || pos >= span {
                        return Err(Error::Schema("PACKED field offset outside span".into()));
                    }
                    if !seen_pos.insert(pos.to_string()) {
                        return Err(Error::Schema("PACKED fields must have unique offsets".into()));
                    }
                }
            }
            if kind_u == "COLUMN" && !fields.is_empty() && !params.contains_key("anchor_u") {
                let mut m: Option<BigInt> = None;
                for f in &fields {
                    let u = crate::schema::LayoutField { raw: f }.u_raw()?;
                    if m.as_ref().map(|x| &u < x).unwrap_or(true) {
                        m = Some(u);
                    }
                }
                params.set("anchor_u", Val::from_bigint(m.unwrap()));
            }
        }
        if kind_u == "CUSTOM" && spec.plugin.trim().is_empty() {
            return Err(Error::Schema("CUSTOM requires a persisted plugin ID".into()));
        }
        let kind = LayoutKind::parse(&kind_u)?;
        let sm = if kind.is_builtin() {
            let canonical = Self::default_spacemap(kind, &params);
            if let Some(s) = &spec.spacemap {
                if s != &canonical {
                    return Err(Error::Schema(format!(
                        "{} SpaceMap is read-only and derived from the actual mapping logic",
                        kind.name()
                    )));
                }
            }
            canonical
        } else {
            match &spec.spacemap {
                Some(s) if s.is_truthy() => s.clone(),
                _ => vmap! {"mapping" => "runtime plugin", "plugin" => spec.plugin.as_str()},
            }
        };
        let lay = LayoutDef::new(
            spec.layout_id,
            realm_id.clone(),
            spec.name.clone(),
            kind.name(),
            spec.u_start.clone(),
            spec.u_end.clone(),
            spec.priority,
            spec.shape_id,
            spec.base_reserve_to.clone(),
            params,
            sm,
            spec.plugin.clone(),
            spec.description.clone(),
        )?;
        let plan = self.plan_layout_candidate(
            &lay,
            old_layout.as_ref().map(|o| o.layout_id),
            if old_layout.is_some() { "replace" } else { "create" },
        )?;
        let imp = plan.get("occupied_impact").cloned().unwrap_or_default();
        if imp.get("unsafe").map(|v| v.is_truthy()).unwrap_or(false) {
            return Err(Error::Schema(format!(
                "layout change would re-route already materialized space (dense<={}, sparse_cells={}, sample_u={}). Check the routing/migration plan first.",
                imp.get("dense_records_at_risk_upper_bound").map(|v| v.py_repr()).unwrap_or_default(),
                imp.get("sparse_cells_at_risk").map(|v| v.py_repr()).unwrap_or_default(),
                imp.get("sample_u").map(|v| v.py_repr()).unwrap_or_default(),
            )));
        }
        self.append_block(BT_LAYOUT_DEF, lay.to_val().to_json()?.as_bytes(), 0)?;
        self.layouts.insert(lay.layout_id, lay.clone());
        let ids = self.layouts_by_realm.entry(realm_id.clone()).or_default();
        if !ids.contains(&lay.layout_id) {
            ids.push(lay.layout_id);
        }
        self.clear_decode_caches();
        if kind == LayoutKind::Classic {
            let old = self.realms[&realm_id].clone();
            if old.default_shape_id.is_none() || (spec.u_start == 1 && spec.u_end.is_none()) {
                let brt = old.base_reserve_to.clone().max(spec.base_reserve_to.clone());
                self.store_realm_def(&realm_id, &old.name, &old.description, spec.shape_id, &brt)?;
            }
        }
        Ok(lay)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_classic_layout(
        &mut self,
        layout_id: u64,
        realm_id: &Nat,
        name: &str,
        shape_id: u64,
        base_reserve_to: &Nat,
        u_start: &Nat,
        u_end: Option<&Nat>,
        priority: i64,
        description: &str,
    ) -> Result<LayoutDef> {
        let mut s = LayoutSpec::new(layout_id, realm_id.clone(), name, "CLASSIC");
        s.u_start = u_start.clone();
        s.u_end = u_end.cloned();
        s.priority = priority;
        s.shape_id = Some(shape_id);
        s.base_reserve_to = base_reserve_to.clone();
        s.description = description.to_string();
        self.create_layout(s)
    }

    #[allow(clippy::too_many_arguments)] // mirrors the Python signature (keyword arguments)
    pub fn create_column_layout(
        &mut self,
        layout_id: u64,
        realm_id: &Nat,
        name: &str,
        fields: Vec<Val>,
        anchor_u: Option<&Nat>,
        priority: i64,
        description: &str,
    ) -> Result<LayoutDef> {
        let mut us = Vec::new();
        for f in &fields {
            us.push(crate::schema::LayoutField { raw: f }.u()?);
        }
        let mut params = vmap! {"fields" => Val::List(fields)};
        if let Some(a) = anchor_u {
            params.set("anchor_u", Val::nat(a));
        }
        let mut s = LayoutSpec::new(layout_id, realm_id.clone(), name, "COLUMN");
        s.u_start = us.iter().min().cloned().unwrap_or(Nat::ONE);
        s.u_end = us.iter().max().cloned();
        s.priority = priority;
        s.params = params;
        s.description = description.to_string();
        self.create_layout(s)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_packed_layout(
        &mut self,
        layout_id: u64,
        realm_id: &Nat,
        name: &str,
        u: &Nat,
        span: &Nat,
        fields: Vec<Val>,
        priority: i64,
        description: &str,
    ) -> Result<LayoutDef> {
        let mut s = LayoutSpec::new(layout_id, realm_id.clone(), name, "PACKED");
        s.u_start = u.clone();
        s.u_end = Some(u.clone());
        s.priority = priority;
        s.params = vmap! {"u" => u, "span" => span, "fields" => Val::List(fields)};
        s.description = description.to_string();
        self.create_layout(s)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_custom_layout(
        &mut self,
        layout_id: u64,
        realm_id: &Nat,
        name: &str,
        plugin: &str,
        u_start: &Nat,
        u_end: Option<&Nat>,
        params: Val,
        spacemap: Option<Val>,
        priority: i64,
        description: &str,
    ) -> Result<LayoutDef> {
        let mut s = LayoutSpec::new(layout_id, realm_id.clone(), name, "CUSTOM");
        s.u_start = u_start.clone();
        s.u_end = u_end.cloned();
        s.priority = priority;
        s.params = params;
        s.spacemap = spacemap;
        s.plugin = plugin.to_string();
        s.description = description.to_string();
        self.create_layout(s)
    }

    pub fn register_layout_plugin(&mut self, name: &str, plugin: Arc<dyn LayoutPlugin>) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Value("plugin name missing".into()));
        }
        self.plugins.insert(name.to_string(), plugin);
        self.clear_decode_caches();
        Ok(())
    }

    pub fn has_plugin(&self, name: &str) -> bool {
        self.plugins.contains_key(name)
    }

    // ------------------------------------------------------------------
    // Diagnostics / queries
    // ------------------------------------------------------------------

    pub fn layout_diagnostics(&self, realm: Option<&Nat>) -> Result<Val> {
        let realm_ids: Vec<Nat> = match realm {
            Some(r) => vec![r.clone()],
            None => self.realms.keys().cloned().collect(),
        };
        let big = address::unbounded_width();
        let mut realms_out = Vec::new();
        let (mut t_over, mut t_conf, mut t_gaps) = (0i64, 0i64, 0i64);
        for rid in realm_ids {
            if !self.realms.contains_key(&rid) {
                return Err(Error::Schema(format!("realm {rid} missing")));
            }
            let mut lays = self.realm_layouts(&rid);
            let end_of = |l: &LayoutDef| l.u_end.clone().unwrap_or_else(|| big.clone());
            lays.sort_by(|a, b| {
                a.u_start.cmp(&b.u_start).then_with(|| end_of(a).cmp(&end_of(b))).then_with(|| a.layout_id.cmp(&b.layout_id))
            });
            let mut overlaps = Vec::new();
            for (i, a) in lays.iter().enumerate() {
                let a_end = end_of(a);
                for b in &lays[i + 1..] {
                    let b_end = end_of(b);
                    let lo = a.u_start.clone().max(b.u_start.clone());
                    let hi = a_end.clone().min(b_end.clone());
                    if lo > hi {
                        if b.u_start > a_end {
                            break;
                        }
                        continue;
                    }
                    let winner = if LayoutDef::sort_cmp(b, a).is_gt() { b } else { a };
                    let same_rank = a.priority == b.priority
                        && a.u_end.is_none() == b.u_end.is_none()
                        && (a.u_end.is_none() || a.width() == b.width());
                    overlaps.push(vmap! {
                        "from" => &lo, "to" => if &hi >= big { Val::None } else { Val::nat(&hi) },
                        "layouts" => Val::List(vec![Val::Int(a.layout_id as i64), Val::Int(b.layout_id as i64)]),
                        "winner" => winner.layout_id as i64,
                        "resolution" => if same_rank { "layout_id_tiebreak" } else { "priority_or_specificity" },
                        "conflict" => same_rank,
                    });
                }
            }
            let mut gaps = Vec::new();
            if !lays.is_empty() {
                let mut intervals: Vec<(Nat, Nat)> = lays.iter().map(|x| (x.u_start.clone(), end_of(x))).collect();
                intervals.sort();
                let (cur_s, mut cur_e) = intervals[0].clone();
                if cur_s > 1 {
                    gaps.push(vmap! {"from" => 1i64, "to" => cur_s.sub_u64(1).unwrap()});
                }
                for (s, e) in intervals.into_iter().skip(1) {
                    if s > cur_e.add_u64(1) {
                        gaps.push(vmap! {"from" => cur_e.add_u64(1), "to" => s.sub_u64(1).unwrap()});
                    }
                    if e > cur_e {
                        cur_e = e;
                    }
                }
            }
            let mut plugin_missing = Vec::new();
            for x in &lays {
                if x.kind == LayoutKind::Custom && !self.plugins.contains_key(&x.plugin) {
                    plugin_missing.push(vmap! {"layout_id" => x.layout_id as i64, "plugin" => x.plugin.as_str()});
                }
            }
            let mut classic_shapes: Vec<u64> =
                lays.iter().filter(|x| x.kind == LayoutKind::Classic).filter_map(|x| x.shape_id).collect();
            classic_shapes.sort();
            classic_shapes.dedup();
            let mut risks = Vec::new();
            if classic_shapes.len() > 1 {
                risks.push(Val::str(
                    "multiple CLASSIC shapes in the same realm; realm-wide extension names remain shared",
                ));
            }
            t_over += overlaps.len() as i64;
            t_gaps += gaps.len() as i64;
            t_conf += overlaps.iter().filter(|x| x.get("conflict").map(|v| v.is_truthy()).unwrap_or(false)).count() as i64;
            realms_out.push(vmap! {
                "realm" => &rid, "layout_count" => lays.len() as i64, "gaps" => Val::List(gaps),
                "overlaps" => Val::List(overlaps), "missing_runtime_plugins" => Val::List(plugin_missing),
                "risks" => Val::List(risks),
            });
        }
        let n = realms_out.len() as i64;
        Ok(vmap! {
            "realms" => Val::List(realms_out),
            "summary" => vmap! {"realms" => n, "overlaps" => t_over, "conflicts" => t_conf, "gaps" => t_gaps},
        })
    }

    /// `_layout_by_id`
    pub fn layout_by_ref(&self, r: &LayoutRef) -> Result<&LayoutDef> {
        match r {
            LayoutRef::Id(id) => self.layouts.get(id).ok_or_else(|| Error::Schema(format!("layout {id} missing"))),
            LayoutRef::Name(name) => {
                if let Ok(id) = name.parse::<u64>() {
                    return self.layouts.get(&id).ok_or_else(|| Error::Schema(format!("layout {name} missing")));
                }
                let m: Vec<&LayoutDef> = self.layouts.values().filter(|x| &x.name == name).collect();
                match m.len() {
                    0 => Err(Error::Schema(format!("layout '{name}' missing"))),
                    1 => Ok(m[0]),
                    _ => Err(Error::Schema(format!("layout name '{name}' is not unique; use layout_id"))),
                }
            }
        }
    }

    pub fn layout_info(&self, lay: &LayoutDef) -> Result<Val> {
        let mut d = lay.to_val();
        d.set("spacemap", self.effective_spacemap(lay)?);
        d.set("u_range", vmap! {"from" => &lay.u_start, "to" => Val::opt_nat(lay.u_end.as_ref())});
        d.set("lane_semantics", vmap! {"0" => "NL", "1" => "NR"});
        if lay.kind == LayoutKind::Classic {
            let base = lay
                .shape_id
                .and_then(|s| self.shapes.get(&s))
                .map(|sh| sh.fields.iter().map(crate::schema::field_to_val).collect())
                .unwrap_or_default();
            d.set("base", Val::List(base));
            let brt = !lay.base_reserve_to.is_zero();
            d.set(
                "base_reserve",
                vmap! {
                    "from" => if brt { Val::Int(1) } else { Val::None },
                    "to" => if brt { Val::nat(&lay.base_reserve_to) } else { Val::None },
                },
            );
            d.set("extensions", self.extension_catalog(&lay.realm_id)?);
        }
        Ok(d)
    }

    /// `get_layout(realm_id, u, layout_id)`: dict, None or list.
    pub fn get_layout(&self, realm: Option<&Nat>, u: Option<&Nat>, layout: Option<&LayoutRef>) -> Result<Val> {
        let single = layout.is_some() || (realm.is_some() && u.is_some());
        let mut lays: Vec<&LayoutDef> = if let Some(r) = layout {
            vec![self.layout_by_ref(r)?]
        } else if let (Some(r), Some(u)) = (realm, u) {
            self.layout_at(r, u).into_iter().collect()
        } else if let Some(r) = realm {
            self.realm_layouts(r)
        } else {
            self.layouts.values().collect()
        };
        lays.sort_by_key(|l| l.layout_id);
        let out: Vec<Val> = lays.iter().map(|l| self.layout_info(l)).collect::<Result<_>>()?;
        if single {
            return Ok(out.into_iter().next().unwrap_or(Val::None));
        }
        Ok(Val::List(out))
    }

    // ------------------------------------------------------------------
    // Evolution
    // ------------------------------------------------------------------

    fn evolved_layout_candidate(&self, r: &LayoutRef, e: &EvolveSpec) -> Result<LayoutDef> {
        let old = self.layout_by_ref(r)?;
        let mut ns = match &e.u_start {
            None => old.u_start.clone(),
            Some(x) => {
                address::require_positive("u_start", x)?;
                x.clone()
            }
        };
        let mut ne = match &e.u_end {
            None => old.u_end.clone(),
            Some(None) => None,
            Some(Some(x)) => {
                address::require_positive("u_end", x)?;
                Some(x.clone())
            }
        };
        if ns > old.u_start {
            return Err(Error::Schema("layout evolution must not move u_start to the right".into()));
        }
        if old.u_end.is_none() && ne.is_some() {
            return Err(Error::Schema("an unbounded layout range must not be shrunk".into()));
        }
        if let (Some(o), Some(n)) = (&old.u_end, &ne) {
            if n < o {
                return Err(Error::Schema("layout evolution must not decrease u_end".into()));
            }
        }
        let mut params = old.params.clone();
        if !e.add_fields.is_empty() {
            if old.kind == LayoutKind::Classic {
                return Err(Error::Schema("extend CLASSIC fields via the shape/extension API".into()));
            }
            let mut fields: Vec<Val> = params.get("fields").and_then(|f| f.as_list()).cloned().unwrap_or_default();
            let mut names: Vec<String> = fields.iter().map(|f| crate::schema::LayoutField { raw: f }.name()).collect();
            match old.kind {
                LayoutKind::Column => {
                    let mut used: Vec<Nat> = fields.iter().map(|f| crate::schema::LayoutField { raw: f }.u()).collect::<Result<_>>()?;
                    for f in &e.add_fields {
                        let lf = crate::schema::LayoutField { raw: f };
                        let nm = lf.name();
                        let pos = lf.u()?;
                        address::require_positive("column u", &pos)?;
                        let typ = lf.type_name();
                        if names.contains(&nm) || used.contains(&pos) {
                            return Err(Error::Schema("COLUMN evolution: name/u already exists".into()));
                        }
                        if TypeCode::from_name(&typ).is_none() {
                            return Err(Error::Schema(format!("unknown data type '{typ}'")));
                        }
                        fields.push(f.clone());
                        names.push(nm);
                        used.push(pos.clone());
                        if pos < ns {
                            ns = pos.clone();
                        }
                        ne = Some(match ne {
                            None => pos,
                            Some(x) => x.max(pos),
                        });
                    }
                }
                LayoutKind::Packed => {
                    let mut used: Vec<BigInt> =
                        fields.iter().map(|f| crate::schema::LayoutField { raw: f }.offset()).collect::<Result<_>>()?;
                    let span = old.packed_span()?.to_bigint();
                    for f in &e.add_fields {
                        let lf = crate::schema::LayoutField { raw: f };
                        let nm = lf.name();
                        let off = lf.offset()?;
                        let typ = lf.type_name();
                        if names.contains(&nm) || used.contains(&off) {
                            return Err(Error::Schema("PACKED evolution: name/offset already exists".into()));
                        }
                        if off.sign() == Sign::Minus || off >= span {
                            return Err(Error::Schema("PACKED offset outside span".into()));
                        }
                        if TypeCode::from_name(&typ).is_none() {
                            return Err(Error::Schema(format!("unknown data type '{typ}'")));
                        }
                        fields.push(f.clone());
                        names.push(nm);
                        used.push(off);
                    }
                }
                _ => {
                    for f in &e.add_fields {
                        let lf = crate::schema::LayoutField { raw: f };
                        let nm = lf.name();
                        let typ = lf.type_name();
                        if names.contains(&nm) {
                            return Err(Error::Schema("CUSTOM evolution: field name already exists".into()));
                        }
                        if TypeCode::from_name(&typ).is_none() {
                            return Err(Error::Schema(format!("unknown data type '{typ}'")));
                        }
                        fields.push(f.clone());
                        names.push(nm);
                    }
                }
            }
            params.set("fields", Val::List(fields));
        }
        let sm = if old.kind.is_builtin() {
            if e.spacemap_update.as_ref().map(|s| s.is_truthy()).unwrap_or(false) {
                return Err(Error::Schema(format!(
                    "{} SpaceMap is read-only; it is derived from the actual mapping logic",
                    old.kind.name()
                )));
            }
            Self::default_spacemap(old.kind, &params)
        } else {
            let mut sm = old.spacemap.clone();
            if let Some(Val::Map(upd)) = &e.spacemap_update {
                for (k, v) in upd {
                    sm.set(k.clone(), v.clone());
                }
            }
            sm
        };
        LayoutDef::new(
            old.layout_id,
            old.realm_id.clone(),
            e.name.clone().unwrap_or_else(|| old.name.clone()),
            old.kind.name(),
            ns,
            ne,
            e.priority.unwrap_or(old.priority),
            old.shape_id,
            old.base_reserve_to.clone(),
            params,
            sm,
            old.plugin.clone(),
            e.description.clone().unwrap_or_else(|| old.description.clone()),
        )
    }

    pub fn plan_layout_evolution(&self, r: &LayoutRef, e: &EvolveSpec) -> Result<Val> {
        let old = self.layout_by_ref(r)?;
        let cand = self.evolved_layout_candidate(r, e)?;
        let mut plan = self.plan_layout_candidate(&cand, Some(old.layout_id), "evolve")?;
        plan.set(
            "current",
            vmap! {
                "u_start" => &old.u_start, "u_end" => Val::opt_nat(old.u_end.as_ref()),
                "priority" => old.priority, "spacemap" => self.effective_spacemap(old)?,
            },
        );
        Ok(plan)
    }

    pub fn evolve_layout(&mut self, r: &LayoutRef, e: &EvolveSpec, commit: bool) -> Result<LayoutDef> {
        let plan = self.plan_layout_evolution(r, e)?;
        if !plan.get("safe").map(|v| v.is_truthy()).unwrap_or(false) {
            let imp = plan.get("occupied_impact").cloned().unwrap_or_default();
            return Err(Error::Schema(format!(
                "UNSAFE layout evolution: already materialized space would be re-routed (dense<={}, sparse_cells={}, sample_u={})",
                imp.get("dense_records_at_risk_upper_bound").map(|v| v.py_repr()).unwrap_or_default(),
                imp.get("sparse_cells_at_risk").map(|v| v.py_repr()).unwrap_or_default(),
                imp.get("sample_u").map(|v| v.py_repr()).unwrap_or_default(),
            )));
        }
        let c = self.evolved_layout_candidate(r, e)?;
        let spec = LayoutSpec {
            layout_id: c.layout_id,
            realm_id: c.realm_id.clone(),
            name: c.name.clone(),
            kind: c.kind.name().to_string(),
            u_start: c.u_start.clone(),
            u_end: c.u_end.clone(),
            priority: c.priority,
            shape_id: c.shape_id,
            base_reserve_to: c.base_reserve_to.clone(),
            params: c.params.clone(),
            spacemap: Some(c.spacemap.clone()),
            plugin: c.plugin.clone(),
            description: c.description.clone(),
            replace: true,
        };
        let lay = self.create_layout(spec)?;
        if commit {
            self.commit()?;
        }
        Ok(lay)
    }

    // ------------------------------------------------------------------
    // Logical addresses
    // ------------------------------------------------------------------

    fn assert_layout_address(&self, lay: &LayoutDef, addr: CellAddress) -> Result<CellAddress> {
        if addr.realm() != &lay.realm_id {
            return Err(Error::Schema(format!("layout {} must not leave realm {}", lay.layout_id, lay.realm_id)));
        }
        if !lay.contains_u(addr.u()) {
            return Err(Error::Schema(format!(
                "layout {} produces u={} outside {}..{}",
                lay.layout_id,
                addr.u(),
                lay.u_start,
                lay.u_end.as_ref().map(|e| e.to_string()).unwrap_or_else(|| "unlimited".into())
            )));
        }
        if let Some(w) = self.layout_at(&lay.realm_id, addr.u()) {
            if w.layout_id != lay.layout_id {
                return Err(Error::Schema(format!(
                    "address is routed by layout {} ({}); layout {} is shadowed there",
                    w.layout_id, w.name, lay.layout_id
                )));
            }
        }
        Ok(addr)
    }

    /// Logical object/field → cell address (`logical_address`).
    /// `field`: None, int or str.
    pub fn logical_address(&self, r: &LayoutRef, object_id: &Nat, field: &Val, lane: Lane) -> Result<CellAddress> {
        let lay = self.layout_by_ref(r)?;
        address::require_positive("object_id", object_id)?;
        self.logical_address_in(lay, object_id, field, lane)
    }

    pub(crate) fn logical_address_in(&self, lay: &LayoutDef, obj: &Nat, field: &Val, lane: Lane) -> Result<CellAddress> {
        match lay.kind {
            LayoutKind::Classic => {
                let v = if field.is_none() { Nat::ONE } else { self.resolve_v(&lay.realm_id, field)? };
                let a = self.cell_address(&lay.realm_id, obj, &v, lane)?;
                self.assert_layout_address(lay, a)
            }
            LayoutKind::Column => {
                let fields = lay.fields();
                let u = match field {
                    Val::None => match lay.params.get("anchor_u") {
                        Some(a) => coord(a)?,
                        None => fields
                            .iter()
                            .map(|f| f.u())
                            .collect::<Result<Vec<_>>>()?
                            .into_iter()
                            .min()
                            .ok_or_else(|| Error::Value("min() arg is an empty sequence".into()))?,
                    },
                    Val::Int(_) | Val::Big(_) | Val::Bool(_) => coord(field)?,
                    other => {
                        let key = other.py_str();
                        let mut found = None;
                        for f in &fields {
                            let u = f.u()?;
                            let nm = f.name_or(|| format!("u{u}"));
                            if nm == key {
                                found = Some(u);
                            }
                        }
                        found.ok_or_else(|| Error::Schema(format!("COLUMN field {} unknown", other.py_repr())))?
                    }
                };
                let a = self.cell_address(&lay.realm_id, &u, obj, lane)?;
                self.assert_layout_address(lay, a)
            }
            LayoutKind::Packed => {
                let u = lay.packed_u()?;
                let span = lay.packed_span()?;
                let anchor = obj.sub_u64(1).unwrap().mul(&span).add_u64(1);
                if &anchor > realm_span() {
                    return Err(Error::RealmLimit("PACKED record lies outside this realm".into()));
                }
                let off: BigInt = match field {
                    Val::None => BigInt::from(0),
                    Val::Int(_) | Val::Big(_) | Val::Bool(_) => field.as_bigint().unwrap(),
                    other => {
                        let key = other.py_str();
                        let mut found = None;
                        for f in lay.fields() {
                            let off = f.offset()?;
                            let nm = f.name_or(|| format!("offset_{off}"));
                            if nm == key {
                                found = Some(off);
                            }
                        }
                        found.ok_or_else(|| Error::Schema(format!("PACKED field {} unknown", other.py_repr())))?
                    }
                };
                let off = match big_to_nat_checked(&off) {
                    Some(o) if o < span => o,
                    _ => return Err(Error::Schema("PACKED offset outside span".into())),
                };
                let v = anchor.add(&off);
                if &v > realm_span() {
                    return Err(Error::RealmLimit("PACKED field lies outside this realm".into()));
                }
                let a = self.cell_address(&lay.realm_id, &u, &v, lane)?;
                self.assert_layout_address(lay, a)
            }
            LayoutKind::Custom => {
                let plugin = self
                    .plugin_for(lay)
                    .filter(|p| p.has(PluginMethod::LogicalAddress))
                    .ok_or_else(|| Error::Schema(format!("CUSTOM layout requires registered plugin '{}'", lay.plugin)))?;
                let result = plugin.logical_address(lay, obj, field, lane)?;
                let a = match &result {
                    Val::Map(_) => {
                        let realm = match result.get("realm") {
                            Some(r) => coord(r)?,
                            None => lay.realm_id.clone(),
                        };
                        let u = coord(result.get("u").ok_or_else(|| Error::Key("'u'".into()))?)?;
                        let v = coord(result.get("v").ok_or_else(|| Error::Key("'v'".into()))?)?;
                        let l = match result.get("lane") {
                            Some(x) => lane_from_val(x)?,
                            None => lane,
                        };
                        self.cell_address(&realm, &u, &v, l)?
                    }
                    Val::List(items) if items.len() == 2 => {
                        self.cell_address(&lay.realm_id, &coord(&items[0])?, &coord(&items[1])?, lane)?
                    }
                    Val::List(items) if items.len() == 4 => self.cell_address(
                        &coord(&items[0])?,
                        &coord(&items[1])?,
                        &coord(&items[2])?,
                        lane_from_val(&items[3])?,
                    )?,
                    _ => return Err(Error::Value("CUSTOM logical_address: invalid result".into())),
                };
                self.assert_layout_address(lay, a)
            }
        }
    }

    pub fn record_anchor(&self, r: &LayoutRef, object_id: &Nat, lane: Lane) -> Result<CellAddress> {
        self.logical_address(r, object_id, &Val::None, lane)
    }

    // ------------------------------------------------------------------
    // Field types / interpretation
    // ------------------------------------------------------------------

    /// Declared field type of an address (`_field_meta_for_address`).
    pub fn field_meta_for_address(&self, realm: &Nat, u: &Nat, v: &Nat) -> Result<Option<FieldDef>> {
        let key = (realm.clone(), u.clone(), v.clone());
        {
            let mut c = self.cache();
            c.field_meta_tick += 1;
            let tick = c.field_meta_tick;
            if let Some((f, used)) = c.field_meta.get_mut(&key) {
                *used = tick;
                return Ok(f.clone());
            }
        }
        let result = self.compute_field_meta(realm, u, v)?;
        let mut c = self.cache();
        if c.field_meta.len() > FIELD_META_MAX {
            // RUN8: the least recently used half goes (no full reset that
            // makes a working set above the bound start from scratch)
            let mut used: Vec<u64> = c.field_meta.values().map(|(_, t)| *t).collect();
            let mid = used.len() / 2;
            let cutoff = *used.select_nth_unstable(mid).1;
            c.field_meta.retain(|_, (_, t)| *t > cutoff);
        }
        c.field_meta_tick += 1;
        let tick = c.field_meta_tick;
        c.field_meta.insert(key, (result.clone(), tick));
        Ok(result)
    }

    fn compute_field_meta(&self, realm: &Nat, u: &Nat, v: &Nat) -> Result<Option<FieldDef>> {
        let Some(lay) = self.layout_at(realm, u) else {
            return Ok(None);
        };
        Ok(match lay.kind {
            LayoutKind::Classic => {
                let from_shape = lay.shape_id.and_then(|s| self.shapes.get(&s)).and_then(|sh| sh.by_v(v)).cloned();
                from_shape.or_else(|| self.extension_defs.get(&(realm.clone(), v.clone())).map(|e| e.as_field()))
            }
            LayoutKind::Column => {
                let mut r = None;
                for f in lay.fields() {
                    if &f.u()? == u {
                        r = Some(f.to_field(v.clone(), f.name_or(|| format!("u{u}")))?);
                        break;
                    }
                }
                r
            }
            LayoutKind::Packed => {
                let span = lay.packed_span()?;
                let off = v.sub_u64(1).unwrap_or_default().div_rem(&span).1.to_bigint();
                let mut r = None;
                for f in lay.fields() {
                    if f.offset()? == off {
                        r = Some(f.to_field(v.clone(), f.name_or(|| format!("offset_{off}")))?);
                        break;
                    }
                }
                r
            }
            LayoutKind::Custom => {
                let mut r = None;
                if let Some(p) = self.plugin_for(lay) {
                    if p.has(PluginMethod::TypeOf) {
                        let t = p.type_of(lay, &vmap! {"realm" => realm, "u" => u, "v" => v})?;
                        if t.is_truthy() {
                            let tn = t.py_str();
                            let typ = TypeCode::from_name(&tn)
                                .ok_or_else(|| Error::Schema(format!("unknown data type '{tn}'")))?;
                            r = Some(FieldDef::new(v.clone(), format!("v{v}"), typ));
                        }
                    }
                }
                r
            }
        })
    }

    /// Type resolution for all cells of a u group (same semantics as
    /// `compute_field_meta`, but without per-cell work). Any error while
    /// building it falls back to the exact per-cell path.
    pub(crate) fn field_resolver(&self, realm: &Nat, u: &Nat) -> FieldResolver<'_> {
        let Some(lay) = self.layout_at(realm, u) else {
            return FieldResolver::Absent;
        };
        match lay.kind {
            LayoutKind::Classic => FieldResolver::Classic {
                realm: realm.clone(),
                shape: lay.shape_id.and_then(|s| self.shapes.get(&s)),
            },
            LayoutKind::Column => {
                for f in lay.fields() {
                    match f.u() {
                        Ok(fu) if &fu == u => {
                            return match f.to_field(Nat::ONE, f.name_or(|| format!("u{u}"))) {
                                Ok(fd) => FieldResolver::Fixed(Some(fd)),
                                Err(_) => FieldResolver::Dynamic,
                            };
                        }
                        Ok(_) => {}
                        Err(_) => return FieldResolver::Dynamic,
                    }
                }
                FieldResolver::Fixed(None)
            }
            LayoutKind::Packed => {
                let Ok(span) = lay.packed_span() else {
                    return FieldResolver::Dynamic;
                };
                let mut fields: Vec<(Nat, FieldDef)> = Vec::new();
                for f in lay.fields() {
                    let Ok(off) = f.offset() else {
                        return FieldResolver::Dynamic;
                    };
                    // negative offsets never match; later duplicates are unreachable
                    let Some(off) = off.to_biguint().map(Nat::from) else {
                        continue;
                    };
                    if fields.iter().any(|(o, _)| *o == off) {
                        continue;
                    }
                    match f.to_field(Nat::ONE, f.name_or(|| format!("offset_{off}"))) {
                        Ok(fd) => fields.push((off, fd)),
                        Err(_) => return FieldResolver::Dynamic,
                    }
                }
                FieldResolver::Packed { span, fields }
            }
            LayoutKind::Custom => FieldResolver::Dynamic,
        }
    }

    /// `_interpret_layout_address`
    pub fn interpret_layout_address(&self, lay: &LayoutDef, addr: &CellAddress) -> Result<Val> {
        let rid = addr.realm();
        let u = addr.u();
        let v = addr.v_local();
        match lay.kind {
            LayoutKind::Classic => {
                let meta = self.field_meta_for_address(rid, u, v)?;
                Ok(vmap! {
                    "kind" => "record_cell", "record" => u, "field_v" => v,
                    "field" => meta.as_ref().map(|m| m.name.clone()),
                    "type" => meta.as_ref().map(|m| m.typ.name()),
                    "anchor" => *v == 1,
                })
            }
            LayoutKind::Column => {
                let mut f = None;
                for x in lay.fields() {
                    if &x.u()? == u {
                        f = Some(x);
                    }
                }
                let anchor_u = match lay.params.get("anchor_u") {
                    Some(a) => coord(a)?,
                    None => u.clone(),
                };
                Ok(vmap! {
                    "kind" => "column_cell", "record" => v, "column_u" => u,
                    "field" => f.as_ref().and_then(|x| x.raw.get("name").cloned()).unwrap_or(Val::None),
                    "type" => f.as_ref().and_then(|x| x.raw.get("type").cloned()).unwrap_or(Val::None),
                    "anchor" => u == &anchor_u,
                })
            }
            LayoutKind::Packed => {
                let span = lay.packed_span()?;
                let (q, off) = v.sub_u64(1).unwrap().div_rem(&span);
                let offb = off.to_bigint();
                let mut f = None;
                for x in lay.fields() {
                    if x.offset()? == offb {
                        f = Some(x);
                    }
                }
                Ok(vmap! {
                    "kind" => "packed_cell", "record" => q.add_u64(1), "offset" => &off, "span" => &span,
                    "field" => f.as_ref().and_then(|x| x.raw.get("name").cloned()).unwrap_or(Val::None),
                    "type" => f.as_ref().and_then(|x| x.raw.get("type").cloned()).unwrap_or(Val::None),
                    "anchor" => off.is_zero(),
                })
            }
            LayoutKind::Custom => {
                if let Some(p) = self.plugin_for(lay) {
                    if p.has(PluginMethod::Describe) {
                        let mut out = p.describe(lay, &addr.to_val())?;
                        if !matches!(out, Val::Map(_)) {
                            out = if out.is_truthy() {
                                return Err(Error::Type("CUSTOM describe() must return a dict".into()));
                            } else {
                                Val::map()
                            };
                        }
                        if !out.contains_key("kind") {
                            out.set("kind", Val::str("custom"));
                        }
                        if !out.contains_key("plugin") {
                            out.set("plugin", Val::str(&lay.plugin));
                        }
                        return Ok(out);
                    }
                }
                Ok(vmap! {"kind" => "custom", "plugin" => lay.plugin.as_str(), "record" => Val::None, "anchor" => false})
            }
        }
    }

    /// `_fallback_cell_label`
    pub fn fallback_cell_label(lay: &LayoutDef, interp: &Val, u: &Nat, v: &Nat) -> String {
        if let Some(n) = interp.get("field") {
            if n.is_truthy() {
                return n.py_str();
            }
        }
        if lay.kind == LayoutKind::Packed {
            if let Some(off) = interp.get("offset") {
                return format!("offset_{}", off.py_str());
            }
        }
        match lay.kind {
            LayoutKind::Column => format!("u{u}"),
            LayoutKind::Classic => format!("v{v}"),
            _ => format!("u{u}.v{v}"),
        }
    }

    /// Record ID from an interpretation (`interp.get("record")`).
    pub(crate) fn interp_record(interp: &Val) -> Option<Nat> {
        match interp.get("record") {
            None | Some(Val::None) => None,
            Some(v) => int_like(v).and_then(|b| b.to_biguint()).map(Nat::from),
        }
    }
}

/// Pre-resolved field types of a u group (see `Db::field_resolver`).
pub enum FieldResolver<'a> {
    /// no layout at this position
    Absent,
    Classic { realm: Nat, shape: Option<&'a crate::schema::ShapeDef> },
    /// COLUMN: one type for the whole u group
    Fixed(Option<FieldDef>),
    /// PACKED: offset → field
    Packed { span: Nat, fields: Vec<(Nat, FieldDef)> },
    /// CUSTOM or error case: `field_meta_for_address` per cell
    Dynamic,
}

impl FieldResolver<'_> {
    pub(crate) fn get<'s>(&'s self, db: &'s Db, realm: &Nat, u: &Nat, v: &Nat) -> Result<Option<std::borrow::Cow<'s, FieldDef>>> {
        use std::borrow::Cow;
        Ok(match self {
            FieldResolver::Absent => None,
            FieldResolver::Classic { realm, shape } => shape
                .and_then(|sh| sh.by_v(v))
                .or_else(|| db.extension_defs.get(&(realm.clone(), v.clone())).map(|e| &e.field))
                .map(Cow::Borrowed),
            FieldResolver::Fixed(f) => f.as_ref().map(Cow::Borrowed),
            FieldResolver::Packed { span, fields } => {
                let off = match (v, span) {
                    (Nat::S(x), Nat::S(s)) => Nat::S((x - 1) % s),
                    _ => v.sub_u64(1).unwrap_or_default().div_rem(span).1,
                };
                fields.iter().find(|(o, _)| *o == off).map(|(_, f)| Cow::Borrowed(f))
            }
            FieldResolver::Dynamic => db.field_meta_for_address(realm, u, v)?.map(Cow::Owned),
        })
    }
}
