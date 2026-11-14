//! MATH realm planner: exclusive physical realms for MATH layouts.
//! No file I/O, no database state.
//!
//! * existing logical→physical mappings are immutable,
//! * new mappings are append-only and only for logical realms that are
//!   actually needed,
//! * each missing contiguous logical extent goes into the first free
//!   physical run that holds it completely,
//! * budget `realm_auto_limit` per MATH layout.

use num_bigint::BigInt;
use num_traits::{One, Signed, ToPrimitive};

use crate::error::{Error, Result};
use crate::val::Val;
use crate::vmap;

fn state_err(m: impl Into<String>) -> Error {
    Error::LayoutState(m.into())
}

fn pos(v: &BigInt, name: &str) -> Result<()> {
    if !v.is_positive() {
        return Err(state_err(format!("{name} must be a positive integer")));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnedRealmExtent {
    pub start: BigInt,
    pub end: BigInt,
    pub owner_kind: String,
    pub owner_id: BigInt,
}

impl OwnedRealmExtent {
    pub fn new(start: BigInt, end: BigInt, owner_kind: &str, owner_id: BigInt) -> Result<Self> {
        pos(&start, "start")?;
        pos(&end, "end")?;
        if end < start {
            return Err(state_err("OwnedRealmExtent: end < start"));
        }
        if crate::unicode::strip(owner_kind).is_empty() {
            return Err(state_err("owner_kind must not be empty"));
        }
        pos(&owner_id, "owner_id")?;
        Ok(OwnedRealmExtent { start, end, owner_kind: owner_kind.into(), owner_id })
    }
    pub fn count(&self) -> BigInt {
        &self.end - &self.start + 1
    }
    pub fn overlaps(&self, start: &BigInt, end: &BigInt) -> bool {
        !(end < &self.start || start > &self.end)
    }
    pub fn to_val(&self) -> Val {
        vmap! {
            "start" => Val::from_bigint(self.start.clone()), "end" => Val::from_bigint(self.end.clone()),
            "owner_kind" => self.owner_kind.as_str(), "owner_id" => Val::from_bigint(self.owner_id.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MathRealmExtent {
    pub logical_start: BigInt,
    pub logical_end: BigInt,
    pub physical_start: BigInt,
    pub physical_end: BigInt,
}

impl MathRealmExtent {
    pub fn new(ls: BigInt, le: BigInt, ps: BigInt, pe: BigInt) -> Result<Self> {
        pos(&ls, "logical_start")?;
        pos(&le, "logical_end")?;
        pos(&ps, "physical_start")?;
        pos(&pe, "physical_end")?;
        if le < ls {
            return Err(state_err("MathRealmExtent: logical_end < logical_start"));
        }
        if pe < ps {
            return Err(state_err("MathRealmExtent: physical_end < physical_start"));
        }
        if &le - &ls != &pe - &ps {
            return Err(state_err("MathRealmExtent: logical and physical length must be identical"));
        }
        Ok(MathRealmExtent { logical_start: ls, logical_end: le, physical_start: ps, physical_end: pe })
    }
    pub fn logical_count(&self) -> BigInt {
        &self.logical_end - &self.logical_start + 1
    }
    pub fn contains_logical(&self, r: &BigInt) -> bool {
        &self.logical_start <= r && r <= &self.logical_end
    }
    pub fn resolve(&self, r: &BigInt) -> Option<BigInt> {
        if !self.contains_logical(r) {
            return None;
        }
        Some(&self.physical_start + (r - &self.logical_start))
    }
    pub fn to_val(&self) -> Val {
        vmap! {
            "logical_start" => Val::from_bigint(self.logical_start.clone()), "logical_end" => Val::from_bigint(self.logical_end.clone()),
            "physical_start" => Val::from_bigint(self.physical_start.clone()), "physical_end" => Val::from_bigint(self.physical_end.clone()),
        }
    }
    pub fn from_val(raw: &Val) -> Result<Self> {
        let g = |k: &str| -> Result<BigInt> {
            raw.get(k).and_then(|v| v.as_bigint()).ok_or_else(|| Error::Key(Val::str(k).py_repr()))
        };
        MathRealmExtent::new(g("logical_start")?, g("logical_end")?, g("physical_start")?, g("physical_end")?)
    }
}

fn validate_math_extents(mut xs: Vec<MathRealmExtent>) -> Result<Vec<MathRealmExtent>> {
    xs.sort_by(|a, b| (&a.logical_start, &a.logical_end).cmp(&(&b.logical_start, &b.logical_end)));
    let mut last = BigInt::from(0);
    let mut phys: Vec<(BigInt, BigInt)> = Vec::new();
    for e in &xs {
        if e.logical_start <= last {
            return Err(state_err("Math RealmMap has overlapping logical extents"));
        }
        last = e.logical_end.clone();
        phys.push((e.physical_start.clone(), e.physical_end.clone()));
    }
    phys.sort();
    let mut prev = BigInt::from(0);
    for (a, b) in phys {
        if a <= prev {
            return Err(state_err("Math RealmMap has overlapping physical extents"));
        }
        prev = b;
    }
    Ok(xs)
}

pub fn coalesce_math_extents(extents: Vec<MathRealmExtent>) -> Result<Vec<MathRealmExtent>> {
    let xs = validate_math_extents(extents)?;
    let mut out: Vec<MathRealmExtent> = Vec::new();
    for e in xs {
        match out.last_mut() {
            Some(p) if &p.logical_end + 1 == e.logical_start && &p.physical_end + 1 == e.physical_start => {
                *p = MathRealmExtent::new(p.logical_start.clone(), e.logical_end, p.physical_start.clone(), e.physical_end)?;
            }
            _ => out.push(e),
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MathRealmMap {
    pub layout_id: BigInt,
    pub mapping_fingerprint: String,
    pub extents: Vec<MathRealmExtent>,
    pub realm_auto_limit: BigInt,
}

pub const REALMMAP_CONTRACT: &str = "NHYDB_MATH_REALMMAP_V015";

impl MathRealmMap {
    pub fn new(layout_id: BigInt, fingerprint: &str, extents: Vec<MathRealmExtent>, realm_auto_limit: BigInt) -> Result<Self> {
        pos(&layout_id, "layout_id")?;
        if crate::unicode::strip(fingerprint).is_empty() {
            return Err(state_err("mapping_fingerprint must not be empty"));
        }
        if !realm_auto_limit.is_positive() {
            return Err(state_err("realm_auto_limit must be > 0"));
        }
        Ok(MathRealmMap { layout_id, mapping_fingerprint: fingerprint.into(), extents: coalesce_math_extents(extents)?, realm_auto_limit })
    }
    pub fn mapped_count(&self) -> BigInt {
        self.extents.iter().map(|e| e.logical_count()).sum()
    }
    pub fn resolve(&self, logical: &BigInt) -> Result<Option<BigInt>> {
        pos(logical, "logical_realm")?;
        for e in &self.extents {
            if logical < &e.logical_start {
                return Ok(None);
            }
            if let Some(p) = e.resolve(logical) {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }
    pub fn to_val(&self) -> Val {
        vmap! {
            "contract" => REALMMAP_CONTRACT, "layout_id" => Val::from_bigint(self.layout_id.clone()),
            "mapping_fingerprint" => self.mapping_fingerprint.as_str(),
            "realm_auto_limit" => Val::from_bigint(self.realm_auto_limit.clone()),
            "extents" => Val::List(self.extents.iter().map(|e| e.to_val()).collect()),
        }
    }
    /// SHA-256 over `json.dumps(to_dict, sort_keys, separators)` (ASCII).
    pub fn state_fingerprint(&self) -> String {
        crate::crypto::sha256_hex(self.to_val().to_json_ascii().expect("JSON").as_bytes())
    }
    pub fn from_val(raw: &Val) -> Result<Self> {
        if raw.get("contract").map(|v| v.py_str()).as_deref() != Some(REALMMAP_CONTRACT) {
            return Err(state_err("unknown Math RealmMap contract"));
        }
        let lid = raw.get("layout_id").and_then(|v| v.as_bigint()).ok_or_else(|| Error::Key("'layout_id'".into()))?;
        let fp = raw.get("mapping_fingerprint").ok_or_else(|| Error::Key("'mapping_fingerprint'".into()))?.py_str();
        let lim = match raw.get("realm_auto_limit") {
            None => BigInt::from(crate::config::DEFAULT_REALM_AUTO_LIMIT),
            Some(v) => v.as_bigint().ok_or_else(|| Error::Value("invalid realm_auto_limit".into()))?,
        };
        let mut ex = Vec::new();
        for e in raw.get("extents").and_then(|v| v.as_list()).into_iter().flatten() {
            ex.push(MathRealmExtent::from_val(e)?);
        }
        MathRealmMap::new(lid, &fp, ex, lim)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreLayoutSnapshot {
    pub revision: BigInt,
    pub occupied: Vec<OwnedRealmExtent>,
}

impl CoreLayoutSnapshot {
    pub fn new(revision: BigInt, mut occupied: Vec<OwnedRealmExtent>) -> Result<Self> {
        if revision.is_negative() {
            return Err(state_err("revision must be >= 0"));
        }
        occupied.sort_by(|a, b| (&a.start, &a.end).cmp(&(&b.start, &b.end)));
        let mut prev = BigInt::from(0);
        for e in &occupied {
            if e.start <= prev {
                return Err(state_err("core snapshot has overlapping realm allocations"));
            }
            prev = e.end.clone();
        }
        Ok(CoreLayoutSnapshot { revision, occupied })
    }
    pub fn owner_at(&self, realm: &BigInt) -> Result<Option<&OwnedRealmExtent>> {
        pos(realm, "realm_id")?;
        for e in &self.occupied {
            if realm < &e.start {
                return Ok(None);
            }
            if &e.start <= realm && realm <= &e.end {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }
    pub fn is_free(&self, start: &BigInt, end: &BigInt) -> Result<bool> {
        pos(start, "start")?;
        pos(end, "end")?;
        if end < start {
            return Err(state_err("end < start"));
        }
        Ok(!self.occupied.iter().any(|e| e.overlaps(start, end)))
    }
    pub fn to_val(&self) -> Val {
        vmap! {"revision" => Val::from_bigint(self.revision.clone()), "occupied" => Val::List(self.occupied.iter().map(|e| e.to_val()).collect())}
    }
}

fn coalesce_owned(mut xs: Vec<OwnedRealmExtent>) -> Result<Vec<OwnedRealmExtent>> {
    xs.sort_by(|a, b| (&a.start, &a.end).cmp(&(&b.start, &b.end)));
    let mut out: Vec<OwnedRealmExtent> = Vec::new();
    for e in xs {
        if let Some(p) = out.last_mut() {
            if e.start <= p.end {
                return Err(state_err("overlapping realm allocation on commit"));
            }
            if e.start == &p.end + 1 && e.owner_kind == p.owner_kind && e.owner_id == p.owner_id {
                *p = OwnedRealmExtent::new(p.start.clone(), e.end, &p.owner_kind.clone(), p.owner_id.clone())?;
                continue;
            }
        }
        out.push(e);
    }
    Ok(out)
}

/// `validate_map_against_snapshot`
pub fn validate_map_against_snapshot(snapshot: &CoreLayoutSnapshot, map: &MathRealmMap) -> Result<()> {
    for m in &map.extents {
        let mut cursor = m.physical_start.clone();
        while cursor <= m.physical_end {
            let owner = snapshot
                .owner_at(&cursor)?
                .ok_or_else(|| state_err(format!("persisted MATH realm {cursor} is missing from the core allocation")))?;
            if crate::unicode::upper(&owner.owner_kind) != "MATH" || owner.owner_id != map.layout_id {
                return Err(state_err(format!(
                    "persisted MATH realm {cursor} belongs to {}:{}, not MATH:{}",
                    owner.owner_kind, owner.owner_id, map.layout_id
                )));
            }
            cursor = std::cmp::min(m.physical_end.clone(), owner.end.clone()) + 1;
        }
    }
    Ok(())
}

fn first_free_run(occupied: &[OwnedRealmExtent], length: &BigInt) -> BigInt {
    let mut cand = BigInt::one();
    for e in occupied {
        if e.end < cand {
            continue;
        }
        if e.start > cand && &e.start - &cand >= *length {
            return cand;
        }
        if e.end.clone() + 1 > cand {
            cand = &e.end + 1;
        }
    }
    cand
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MathAllocationPlan {
    pub allowed: bool,
    pub reason: String,
    pub base_revision: BigInt,
    pub base_map_fingerprint: String,
    pub layout_id: BigInt,
    pub mapping_fingerprint: String,
    pub requested_realms: Vec<BigInt>,
    pub missing_realms: Vec<BigInt>,
    pub missing_logical_extents: Vec<(BigInt, BigInt)>,
    pub proposed_extents: Vec<MathRealmExtent>,
    pub resulting_extents: Vec<MathRealmExtent>,
    pub existing_count: BigInt,
    pub new_count: BigInt,
    pub resulting_count: BigInt,
    pub realm_auto_limit: BigInt,
}

impl MathAllocationPlan {
    pub fn require_allowed(&self) -> Result<()> {
        if !self.allowed {
            return Err(Error::RealmBudget(self.reason.clone()));
        }
        Ok(())
    }
}

/// `plan_math_realms(snapshot, realm_map, needed_logical_realms)`
pub fn plan_math_realms(snapshot: &CoreLayoutSnapshot, map: &MathRealmMap, needed: &[BigInt]) -> Result<MathAllocationPlan> {
    validate_map_against_snapshot(snapshot, map)?;
    for x in needed {
        pos(x, "logical_realm")?;
    }
    let mut needed: Vec<BigInt> = needed.to_vec();
    needed.sort();
    needed.dedup();
    let mut missing = Vec::new();
    for r in &needed {
        if map.resolve(r)?.is_none() {
            missing.push(r.clone());
        }
    }
    let missing_extents = crate::mathexpr::coalesce(&missing);
    let existing = map.mapped_count();
    let new_count = BigInt::from(missing.len());
    let resulting = &existing + &new_count;
    if resulting > map.realm_auto_limit {
        let reason = format!(
            "MATH realm auto limit exceeded: existing={existing}, new={new_count}, resulting={resulting}, limit={}",
            map.realm_auto_limit
        );
        return Ok(MathAllocationPlan {
            allowed: false,
            reason,
            base_revision: snapshot.revision.clone(),
            base_map_fingerprint: map.state_fingerprint(),
            layout_id: map.layout_id.clone(),
            mapping_fingerprint: map.mapping_fingerprint.clone(),
            requested_realms: needed,
            missing_realms: missing,
            missing_logical_extents: missing_extents,
            proposed_extents: Vec::new(),
            resulting_extents: map.extents.clone(),
            existing_count: existing,
            new_count,
            resulting_count: resulting,
            realm_auto_limit: map.realm_auto_limit.clone(),
        });
    }
    let mut temp = snapshot.occupied.clone();
    let mut proposed = Vec::new();
    for (ls, le) in &missing_extents {
        let length = le - ls + 1;
        let ps = first_free_run(&temp, &length);
        let pe: BigInt = &ps + &length - BigInt::one();
        proposed.push(MathRealmExtent::new(ls.clone(), le.clone(), ps.clone(), pe.clone())?);
        temp.push(OwnedRealmExtent::new(ps, pe, "MATH", map.layout_id.clone())?);
        temp = coalesce_owned(temp)?;
    }
    let mut all = map.extents.clone();
    all.extend(proposed.iter().cloned());
    let resulting_extents = coalesce_math_extents(all)?;
    Ok(MathAllocationPlan {
        allowed: true,
        reason: "OK".into(),
        base_revision: snapshot.revision.clone(),
        base_map_fingerprint: map.state_fingerprint(),
        layout_id: map.layout_id.clone(),
        mapping_fingerprint: map.mapping_fingerprint.clone(),
        requested_realms: needed,
        missing_realms: missing,
        missing_logical_extents: missing_extents,
        proposed_extents: proposed,
        resulting_extents,
        existing_count: existing,
        new_count,
        resulting_count: resulting,
        realm_auto_limit: map.realm_auto_limit.clone(),
    })
}

/// `validate_plan`: final core check, no silent re-planning.
pub fn validate_plan(snapshot: &CoreLayoutSnapshot, map: &MathRealmMap, plan: &MathAllocationPlan) -> Result<()> {
    if !plan.allowed {
        return Err(Error::RealmBudget(plan.reason.clone()));
    }
    if snapshot.revision != plan.base_revision {
        return Err(Error::StalePlan(format!(
            "core revision changed: plan={}, current={}",
            plan.base_revision, snapshot.revision
        )));
    }
    if map.state_fingerprint() != plan.base_map_fingerprint {
        return Err(Error::StalePlan("persisted Math RealmMap has changed since planning".into()));
    }
    if map.layout_id != plan.layout_id {
        return Err(Error::StalePlan("layout ID does not match the plan".into()));
    }
    if map.mapping_fingerprint != plan.mapping_fingerprint {
        return Err(Error::StalePlan("mapping fingerprint does not match the plan".into()));
    }
    validate_map_against_snapshot(snapshot, map)?;
    if plan.resulting_count > map.realm_auto_limit {
        return Err(Error::RealmBudget(plan.reason.clone()));
    }
    for e in &plan.proposed_extents {
        if !snapshot.is_free(&e.physical_start, &e.physical_end)? {
            return Err(Error::AllocationConflict(format!(
                "physical realms {}..{} are no longer free",
                e.physical_start, e.physical_end
            )));
        }
        let mut l = e.logical_start.clone();
        while l <= e.logical_end {
            if map.resolve(&l)?.is_some() {
                return Err(Error::AllocationConflict(format!("logical realm {l} has been mapped in the meantime")));
            }
            l += 1;
        }
    }
    let mut all = map.extents.clone();
    all.extend(plan.proposed_extents.iter().cloned());
    if coalesce_math_extents(all)? != plan.resulting_extents {
        return Err(state_err("plan resulting_extents do not match the current state"));
    }
    Ok(())
}

/// `commit_plan_simulated` (test helper).
pub fn commit_plan_simulated(snapshot: &CoreLayoutSnapshot, map: &MathRealmMap, plan: &MathAllocationPlan) -> Result<(CoreLayoutSnapshot, MathRealmMap)> {
    validate_plan(snapshot, map, plan)?;
    let mut occ = snapshot.occupied.clone();
    for e in &plan.proposed_extents {
        occ.push(OwnedRealmExtent::new(e.physical_start.clone(), e.physical_end.clone(), "MATH", map.layout_id.clone())?);
    }
    let new_snapshot = CoreLayoutSnapshot::new(&snapshot.revision + 1, coalesce_owned(occ)?)?;
    let new_map = MathRealmMap::new(map.layout_id.clone(), &map.mapping_fingerprint, plan.resulting_extents.clone(), map.realm_auto_limit.clone())?;
    validate_map_against_snapshot(&new_snapshot, &new_map)?;
    Ok((new_snapshot, new_map))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResolvedMathCell {
    pub physical_realm: BigInt,
    pub u: BigInt,
    pub v_local: BigInt,
    pub side: i64,
}

/// `resolve_math_target`: reads only the persisted RealmMap.
pub fn resolve_math_target(map: &MathRealmMap, t: &crate::mathexpr::MathMapTarget) -> Result<ResolvedMathCell> {
    pos(&t.logical_realm, "logical_realm")?;
    let physical = map
        .resolve(&t.logical_realm)?
        .ok_or_else(|| state_err(format!("logical realm {} is not mapped for MATH layout {}", t.logical_realm, map.layout_id)))?;
    pos(&t.u, "u")?;
    pos(&t.v_local, "v_local")?;
    Ok(ResolvedMathCell { physical_realm: physical, u: t.u.clone(), v_local: t.v_local.clone(), side: t.side })
}

pub fn to_u64(b: &BigInt) -> Option<u64> {
    b.to_u64()
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "mathlayout_tests.rs"]
mod tests;
