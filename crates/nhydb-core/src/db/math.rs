//! MATH_MAP layouts in the core (Python API `NHyDB.create_math_layout`,
//! `put_math_batch`, `get_math_cell`, …).
//!
//! A MATH layout owns exclusive physical realms. Its state
//! (definition, fingerprint, frozen flag, RealmMap) lives in the external
//! layout plan (`math_layouts`), the payload data as self-typed
//! sparse cells in the ordinary point store.

use std::collections::{BTreeMap, HashMap, HashSet};

use num_bigint::BigInt;
use num_traits::{One, Signed, ToPrimitive, Zero};

use crate::codec::{int_like, TypeCode};
use crate::error::{Error, Result};
use crate::mathexpr::{MathMap, MathMapLayoutSpec, MathMapTarget};
use crate::mathlayout::{
    coalesce_math_extents, plan_math_realms, resolve_math_target, validate_map_against_snapshot, validate_plan, CoreLayoutSnapshot, MathRealmMap,
    OwnedRealmExtent, ResolvedMathCell,
};
use crate::nat::Nat;
use crate::val::Val;
use crate::vmap;

use super::sparse::CellInput;
use super::Db;

pub const STATE_CONTRACT: &str = "NHYDB_MATH_LAYOUT_STATE_V015";
/// u window of the sparse blocks for MATH payloads (`MATH_SPARSE_U_WINDOW`).
pub const MATH_SPARSE_U_WINDOW: u64 = 100_000_000;

/// Persisted state of a MATH layout (`MathLayoutState`).
#[derive(Clone, Debug, PartialEq)]
pub struct MathLayoutState {
    pub layout_id: u64,
    pub spec: MathMapLayoutSpec,
    pub realm_map: MathRealmMap,
    pub payload_type: TypeCode,
    pub frozen: bool,
    pub created_block_id: u64,
    pub first_write_block_id: Option<u64>,
    /// `math_max_intermediate_bits` frozen when the layout is created (F030):
    /// writing and reading always evaluate the mapping with the same limit,
    /// independent of the configuration of a later session.
    pub max_intermediate_bits: u64,
}

fn schema(m: impl Into<String>) -> Error {
    Error::Schema(m.into())
}

impl MathLayoutState {
    pub fn new(
        layout_id: u64,
        spec: MathMapLayoutSpec,
        realm_map: MathRealmMap,
        payload_type: &str,
        frozen: bool,
        created_block_id: u64,
        first_write_block_id: Option<u64>,
    ) -> Result<Self> {
        if layout_id == 0 {
            return Err(Error::Value("layout_id must be > 0".into()));
        }
        crate::schema::check_id("layout_id", layout_id)?;
        let pt = TypeCode::from_name(payload_type)
            .ok_or_else(|| schema(format!("unknown MATH payload type {}", Val::str(payload_type).py_repr())))?;
        if realm_map.layout_id != BigInt::from(layout_id) {
            return Err(schema("MATH RealmMap layout_id does not match"));
        }
        if realm_map.mapping_fingerprint != spec.fingerprint() {
            return Err(schema("MATH RealmMap fingerprint does not match the mapping definition"));
        }
        if realm_map.realm_auto_limit != spec.realm_auto_limit {
            return Err(schema("MATH RealmMap limit does not match the mapping definition"));
        }
        if spec.mapping.realm_span != crate::address::realm_span().to_bigint() {
            return Err(schema("MATH realm_span must equal the NHyDB REALM_SPAN"));
        }
        if !realm_map.mapped_count().is_zero() && !frozen {
            return Err(schema("MATH with a persisted RealmMap must be frozen"));
        }
        if first_write_block_id.is_some() && !frozen {
            return Err(schema("first_write_block_id requires frozen"));
        }
        Ok(MathLayoutState {
            layout_id,
            spec,
            realm_map,
            payload_type: pt,
            frozen,
            created_block_id,
            first_write_block_id,
            max_intermediate_bits: crate::config::DEFAULT_MATH_MAX_INTERMEDIATE_BITS,
        })
    }

    pub fn name(&self) -> &str {
        &self.spec.layout_name
    }

    pub fn to_val(&self) -> Val {
        vmap! {
            "contract" => STATE_CONTRACT, "layout_id" => Val::int(self.layout_id),
            "spec" => self.spec.to_val(), "realm_map" => self.realm_map.to_val(),
            "payload_type" => self.payload_type.name(), "frozen" => self.frozen,
            "created_block_id" => Val::int(self.created_block_id),
            "first_write_block_id" => self.first_write_block_id.map(Val::int).unwrap_or(Val::None),
            "max_intermediate_bits" => Val::int(self.max_intermediate_bits),
        }
    }

    pub fn from_val(d: &Val) -> Result<Self> {
        if d.get("contract").map(|v| v.py_str()).as_deref() != Some(STATE_CONTRACT) {
            return Err(schema("unknown MATH layout state contract"));
        }
        // F004: persisted IDs/block IDs are never clamped; out of range = corruption.
        let id_of = |k: &str, v: &Val| -> Result<u64> {
            match int_like(v).and_then(|b| b.to_u64()) {
                Some(x) => Ok(x),
                None => Err(Error::Corruption(format!("MATH state {k}: {} outside 0..2^64-1", v.py_repr()))),
            }
        };
        let lid = id_of("layout_id", d.get("layout_id").ok_or_else(|| Error::Key("'layout_id'".into()))?)?;
        if lid > crate::schema::MAX_ID {
            return Err(Error::Corruption(format!("MATH state layout_id {lid} outside 0..2^63-1")));
        }
        let spec = MathMapLayoutSpec::from_val(d.get("spec").ok_or_else(|| Error::Key("'spec'".into()))?)?;
        let rm = MathRealmMap::from_val(d.get("realm_map").ok_or_else(|| Error::Key("'realm_map'".into()))?)?;
        let pt = d.get("payload_type").map(|v| v.py_str()).unwrap_or_else(|| "bytes".into());
        let frozen = d.get("frozen").map(|v| v.is_truthy()).unwrap_or(false);
        let created = match d.get("created_block_id") {
            None | Some(Val::None) => 0,
            Some(v) => id_of("created_block_id", v)?,
        };
        let first = match d.get("first_write_block_id") {
            None | Some(Val::None) => None,
            Some(v) => Some(id_of("first_write_block_id", v)?),
        };
        let bits = match d.get("max_intermediate_bits") {
            None | Some(Val::None) => crate::config::DEFAULT_MATH_MAX_INTERMEDIATE_BITS,
            Some(v) => match id_of("max_intermediate_bits", v)? {
                0 => return Err(Error::Corruption("MATH state max_intermediate_bits must be > 0".into())),
                b => b,
            },
        };
        let mut st = MathLayoutState::new(lid, spec, rm, &pt, frozen, created, first)?;
        st.max_intermediate_bits = bits;
        Ok(st)
    }
}

/// Physical extents `(start, end)` of a MATH state.
pub(crate) fn physical_extents(rec: &MathLayoutState) -> Vec<(Nat, Nat)> {
    rec.realm_map
        .extents
        .iter()
        .filter_map(|e| Some((Nat::from_bigint(&e.physical_start)?, Nat::from_bigint(&e.physical_end)?)))
        .collect()
}

/// Python `int(x)` semantics for MATH inputs.
fn py_int(v: &Val) -> Result<Val> {
    Ok(match v {
        Val::Bool(b) => Val::Int(*b as i64),
        Val::Int(_) | Val::Big(_) => v.clone(),
        // F031: exact truncation (no saturation at i128), NaN/inf → ValueError
        Val::Float(f) if f.is_nan() => return Err(Error::Value("cannot convert float NaN to integer".into())),
        Val::Float(f) if f.is_infinite() => return Err(Error::Value("cannot convert float infinity to integer".into())),
        Val::Float(f) => {
            use num_traits::FromPrimitive;
            Val::from_bigint(BigInt::from_f64(f.trunc()).unwrap_or_default())
        }
        Val::Str(s) => Val::from_bigint(
            crate::codec::parse_py_int(s)
                .ok_or_else(|| Error::Value(format!("invalid literal for int() with base 10: {}", Val::str(s.as_str()).py_repr())))?,
        ),
        Val::Bytes(b) => {
            let parsed = std::str::from_utf8(b).ok().filter(|s| s.is_ascii()).and_then(crate::codec::parse_py_int);
            Val::from_bigint(parsed.ok_or_else(|| Error::Value(format!("invalid literal for int() with base 10: {}", v.py_repr())))?)
        }
        other => return Err(Error::Type(format!("int() argument must be a string, a bytes-like object or a real number, not {}", other.py_repr()))),
    })
}

fn tuple_repr(items: &[String]) -> String {
    format!("({})", items.join(", "))
}

impl Db {
    pub(crate) fn math_record(&self, layout_id: u64) -> Result<&MathLayoutState> {
        self.math_layouts
            .get(&layout_id)
            .ok_or_else(|| Error::MathLayoutNotFound(format!("MATH layout {layout_id} does not exist")))
    }

    /// Physical owner of a realm through a MATH layout.
    pub fn math_owner_of_realm(&self, realm: &Nat) -> Option<u64> {
        for (lid, rec) in &self.math_layouts {
            for (a, b) in physical_extents(rec) {
                if &a <= realm && realm <= &b {
                    return Some(*lid);
                }
            }
        }
        None
    }

    pub(crate) fn deny_math_realm(&self, realm: &Nat) -> Result<()> {
        if let Some(owner) = self.math_owner_of_realm(realm) {
            return Err(Error::Schema(format!("Realm {realm} belongs exclusively to MATH layout {owner}")));
        }
        Ok(())
    }

    /// `_math_layout_snapshot`: revision = next_block_id; every non-MATH
    /// realm counts as occupied by `("CORE", 1)`.
    pub fn math_layout_snapshot(&self) -> Result<CoreLayoutSnapshot> {
        let mut ownership: BTreeMap<Nat, (String, u64)> = BTreeMap::new();
        for (lid, rec) in &self.math_layouts {
            for (a, b) in physical_extents(rec) {
                let mut r = a.clone();
                while r <= b {
                    if !self.realms.contains_key(&r) {
                        return Err(Error::LayoutState(format!("MATH {lid}: physical realm {r} missing in the core")));
                    }
                    if let Some(old) = ownership.get(&r) {
                        if old != &("MATH".to_string(), *lid) {
                            return Err(Error::LayoutState(format!("Realm {r} occupied multiple times: ('{}', {}) / MATH:{lid}", old.0, old.1)));
                        }
                    }
                    ownership.insert(r.clone(), ("MATH".into(), *lid));
                    r = r.add_u64(1);
                }
            }
        }
        for rid in self.realms.keys() {
            ownership.entry(rid.clone()).or_insert(("CORE".into(), 1));
        }
        // merge contiguous runs of the same owner
        let mut occ: Vec<OwnedRealmExtent> = Vec::new();
        for (rid, (kind, owner)) in ownership {
            let r = rid.to_bigint();
            if let Some(last) = occ.last_mut() {
                if last.end.clone() + 1 == r && last.owner_kind == kind && last.owner_id == BigInt::from(owner) {
                    last.end = r;
                    continue;
                }
            }
            occ.push(OwnedRealmExtent::new(r.clone(), r, &kind, BigInt::from(owner))?);
        }
        CoreLayoutSnapshot::new(BigInt::from(self.next_block_id), occ)
    }

    /// `_validate_math_layout_state`
    pub fn validate_math_layout_state(&self) -> Result<()> {
        let dup: Vec<u64> = self.layouts.keys().filter(|k| self.math_layouts.contains_key(k)).cloned().collect();
        if !dup.is_empty() {
            let shown: Vec<String> = dup.iter().take(8).map(|x| x.to_string()).collect();
            return Err(Error::Corruption(format!("Layout IDs used by both classic and MATH layouts: [{}]", shown.join(", "))));
        }
        let snapshot = self.math_layout_snapshot().map_err(|e| Error::Corruption(e.to_string()))?;
        let mut math_realms: HashSet<Nat> = HashSet::new();
        for (lid, rec) in &self.math_layouts {
            if rec.spec.mapping.realm_span != crate::address::realm_span().to_bigint() {
                return Err(Error::Corruption(format!("MATH {lid}: realm_span does not match the core")));
            }
            validate_map_against_snapshot(&snapshot, &rec.realm_map).map_err(|e| Error::Corruption(format!("MATH {lid}: {e}")))?;
            for (a, b) in physical_extents(rec) {
                let mut r = a;
                while r <= b {
                    math_realms.insert(r.clone());
                    r = r.add_u64(1);
                }
            }
        }
        for (lid, lay) in &self.layouts {
            if math_realms.contains(&lay.realm_id) {
                return Err(Error::Corruption(format!("Layout {lid} lies in exclusive MATH realm {}", lay.realm_id)));
            }
        }
        for (sid, sh) in &self.shapes {
            if math_realms.contains(&sh.realm_id) {
                return Err(Error::Corruption(format!("Shape {sid} lies in exclusive MATH realm {}", sh.realm_id)));
            }
        }
        for (iid, idx) in &self.index_defs {
            if math_realms.contains(&idx.realm_id) {
                return Err(Error::Corruption(format!("Index {iid} lies in exclusive MATH realm {}", idx.realm_id)));
            }
        }
        for (rid, _v) in self.extension_defs.keys() {
            if math_realms.contains(rid) {
                return Err(Error::Corruption(format!("Extension lies in exclusive MATH realm {rid}")));
            }
        }
        Ok(())
    }

    fn next_free_layout_id(&self) -> u64 {
        let mut lid = 1;
        while self.layouts.contains_key(&lid) || self.math_layouts.contains_key(&lid) {
            lid += 1;
        }
        lid
    }

    /// `create_math_layout(spec, layout_id=None, payload_type="bytes", commit=False)`
    pub fn create_math_layout(&mut self, spec: MathMapLayoutSpec, layout_id: Option<u64>, payload_type: &str, commit: bool) -> Result<MathLayoutState> {
        if spec.mapping.realm_span != crate::address::realm_span().to_bigint() {
            return Err(schema("MATH realm_span must equal REALM_SPAN"));
        }
        if self.math_layouts.values().any(|x| x.name() == spec.layout_name) {
            return Err(schema(format!("MATH layout name {} already exists", Val::str(spec.layout_name.as_str()).py_repr())));
        }
        let lid = match layout_id {
            None => self.next_free_layout_id(),
            Some(0) => return Err(Error::Value("layout_id must be > 0".into())),
            Some(l) => l,
        };
        if self.layouts.contains_key(&lid) || self.math_layouts.contains_key(&lid) {
            return Err(schema(format!("Layout ID {lid} already exists")));
        }
        if TypeCode::from_name(payload_type).is_none() {
            return Err(schema(format!("unknown payload type {}", Val::str(payload_type).py_repr())));
        }
        let rm = MathRealmMap::new(BigInt::from(lid), &spec.fingerprint(), Vec::new(), spec.realm_auto_limit.clone())?;
        let mut rec = MathLayoutState::new(lid, spec, rm, payload_type, false, self.next_block_id, None)?;
        rec.max_intermediate_bits = self.config.math_max_intermediate_bits;
        self.append_block(crate::format::BT_MATH_LAYOUT_STATE, rec.to_val().to_json()?.as_bytes(), 0)?;
        self.math_layouts.insert(lid, rec.clone());
        if commit {
            self.commit()?;
        }
        Ok(rec)
    }

    /// `create_math_map_layout(name, parameters, u=, V=, side=, …)`;
    /// `realm_auto_limit=None` ⇒ value from the configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn create_math_map_layout(
        &mut self,
        name: &str,
        parameters: &[String],
        u: &str,
        v: &str,
        side: &Val,
        layout_id: Option<u64>,
        payload_type: &str,
        realm_auto_limit: Option<BigInt>,
        description: &str,
        commit: bool,
    ) -> Result<MathLayoutState> {
        let mapping = MathMap::compile(name, parameters, u, v, side, &crate::address::realm_span().to_bigint())?;
        let limit = realm_auto_limit.unwrap_or_else(|| BigInt::from(self.config.realm_auto_limit));
        let spec = MathMapLayoutSpec::new(name, mapping, limit, description)?;
        self.create_math_layout(spec, layout_id, payload_type, commit)
    }

    /// `replace_math_spec(layout_id, spec, payload_type=None, commit=False)`
    pub fn replace_math_spec(&mut self, layout_id: u64, spec: MathMapLayoutSpec, payload_type: Option<&str>, commit: bool) -> Result<MathLayoutState> {
        let old = self.math_record(layout_id)?.clone();
        if old.frozen || old.first_write_block_id.is_some() || !old.realm_map.mapped_count().is_zero() {
            return Err(Error::MappingFrozen(format!("MATH {layout_id} is frozen after the first write")));
        }
        if spec.mapping.realm_span != crate::address::realm_span().to_bigint() {
            return Err(schema("MATH realm_span must equal REALM_SPAN"));
        }
        for (lid, other) in &self.math_layouts {
            if *lid != old.layout_id && other.name() == spec.layout_name {
                return Err(schema(format!("MATH layout name {} already exists", Val::str(spec.layout_name.as_str()).py_repr())));
            }
        }
        let typ = payload_type.map(|s| s.to_string()).unwrap_or_else(|| old.payload_type.name().to_string());
        if TypeCode::from_name(&typ).is_none() {
            return Err(schema(format!("unknown payload type {}", Val::str(typ.as_str()).py_repr())));
        }
        let rm = MathRealmMap::new(BigInt::from(old.layout_id), &spec.fingerprint(), Vec::new(), spec.realm_auto_limit.clone())?;
        let mut rec = MathLayoutState::new(old.layout_id, spec, rm, &typ, false, old.created_block_id, None)?;
        // nothing written yet: the replaced spec takes the current limit
        rec.max_intermediate_bits = self.config.math_max_intermediate_bits;
        self.append_block(crate::format::BT_MATH_LAYOUT_STATE, rec.to_val().to_json()?.as_bytes(), 0)?;
        self.math_layouts.insert(old.layout_id, rec.clone());
        if commit {
            self.commit()?;
        }
        Ok(rec)
    }

    /// `describe_math(layout_id)`
    pub fn describe_math(&self, layout_id: u64) -> Result<Val> {
        let rec = self.math_record(layout_id)?;
        let exts = physical_extents(rec);
        let mut cells = 0i64;
        for ((rid, _lane), idx) in &self.sparse {
            if exts.iter().any(|(a, b)| a <= rid && rid <= b) {
                cells += idx.segs.iter().map(|s| s.cell_count as i64).sum::<i64>();
            }
        }
        Ok(vmap! {
            "layout_id" => Val::int(rec.layout_id), "kind" => "MATH", "name" => rec.name(),
            "frozen" => rec.frozen, "payload_type" => rec.payload_type.name(),
            "mapping_fingerprint" => rec.spec.fingerprint(), "mapping" => rec.spec.mapping.to_val(),
            "mapped_realms" => Val::from_bigint(rec.realm_map.mapped_count()),
            "realm_extents" => Val::List(rec.realm_map.extents.iter().map(|e| e.to_val()).collect()),
            "realm_auto_limit" => Val::from_bigint(rec.realm_map.realm_auto_limit.clone()),
            "created_block_id" => Val::int(rec.created_block_id),
            "first_write_block_id" => rec.first_write_block_id.map(Val::int).unwrap_or(Val::None),
            "materialized_sparse_cells" => cells,
        })
    }

    fn math_target(&self, rec: &MathLayoutState, inputs: &[Val]) -> Result<MathMapTarget> {
        let ins: Vec<Val> = inputs.iter().map(py_int).collect::<Result<_>>()?;
        rec.spec.mapping.target(&ins, rec.max_intermediate_bits)
    }

    /// `put_math_batch(layout_id, rows, overwrite=False, commit=False)`
    pub fn put_math_batch(&mut self, layout_id: u64, rows: Vec<(Vec<Val>, Val)>, overwrite: bool, commit: bool) -> Result<Val> {
        self.put_math_rows(layout_id, rows, overwrite, commit).map(|(v, _)| v)
    }

    /// `put_math_batch` returning the physical target cells as well (P008:
    /// `put_math` evaluates the mapping only once).
    fn put_math_rows(&mut self, layout_id: u64, rows: Vec<(Vec<Val>, Val)>, overwrite: bool, commit: bool) -> Result<(Val, Vec<ResolvedMathCell>)> {
        let rec = self.math_record(layout_id)?.clone();
        let mut materialized: Vec<(MathMapTarget, Val)> = Vec::with_capacity(rows.len());
        let mut logical_seen = HashSet::new();
        for (inputs, payload) in rows {
            let t = self.math_target(&rec, &inputs)?;
            let key = t.logical_cell_key();
            if logical_seen.contains(&key) && !overwrite {
                return Err(Error::MathCellConflict(format!(
                    "Batch contains target cell more than once: {}",
                    tuple_repr(&[key.0.to_string(), key.1.to_string(), key.2.to_string(), key.3.to_string()])
                )));
            }
            logical_seen.insert(key);
            materialized.push((t, payload));
        }
        if materialized.is_empty() {
            if commit {
                self.commit()?;
            }
            return Ok((vmap! {"cells" => 0i64, "new_realms" => 0i64, "blocks" => 0i64}, Vec::new()));
        }
        let mut needed: Vec<BigInt> = materialized.iter().map(|(t, _)| t.logical_realm.clone()).collect();
        needed.sort();
        needed.dedup();
        // P008: when every logical realm is already mapped, no realm is
        // created and the RealmMap stays the same – the realm planner (a
        // snapshot over all realms) and the full state validation are only
        // needed when the RealmMap grows. The persisted state was validated
        // when it was opened and after every change of realms/layouts.
        let mut all_mapped = true;
        for r in &needed {
            if rec.realm_map.resolve(r)?.is_none() {
                all_mapped = false;
                break;
            }
        }
        let (plan_extents, new_realms) = if all_mapped {
            (Vec::new(), BigInt::zero())
        } else {
            let snapshot = self.math_layout_snapshot()?;
            let plan = plan_math_realms(&snapshot, &rec.realm_map, &needed)?;
            plan.require_allowed()?;
            validate_plan(&snapshot, &rec.realm_map, &plan)?;
            (plan.proposed_extents.clone(), plan.new_count.clone())
        };
        let mut resulting = rec.realm_map.extents.clone();
        resulting.extend(plan_extents.iter().cloned());
        let preview = MathRealmMap::new(rec.realm_map.layout_id.clone(), &rec.spec.fingerprint(), coalesce_math_extents(resulting)?, rec.realm_map.realm_auto_limit.clone())?;
        let resolved: Vec<_> = materialized.iter().map(|(t, _)| resolve_math_target(&preview, t)).collect::<Result<_>>()?;
        // Physical collisions within the batch imply logical ones (the RealmMap is
        // bijective) and are already rejected above; what remains to check is the
        // occupancy of existing realms ("target cell … is already occupied").
        if !overwrite {
            let mut realm_state: HashMap<&BigInt, Option<Nat>> = HashMap::new();
            for addr in &resolved {
                let pr = realm_state
                    .entry(&addr.physical_realm)
                    .or_insert_with(|| Nat::from_bigint(&addr.physical_realm).filter(|n| self.realms.contains_key(n)));
                if let Some(pr) = pr {
                    let u = Nat::from_bigint(&addr.u).unwrap();
                    let v = Nat::from_bigint(&addr.v_local).unwrap();
                    if self.get_cell_value(pr, &u, &v, addr.side as u8)?.is_some() {
                        let shown = tuple_repr(&[addr.physical_realm.to_string(), addr.u.to_string(), addr.v_local.to_string(), addr.side.to_string()]);
                        return Err(Error::MathCellConflict(format!("Target cell {shown} is already occupied")));
                    }
                }
            }
        }
        // rollback point (`_rollback_math_tail`); without new realms only the
        // state of this MATH layout changes, so the schema is exported only
        // if the batch fails (after restoring that state).
        let start_len = self.store.len();
        let start_next = self.next_block_id;
        let start_last = self.last_valid_end;
        let prior_schema = if all_mapped { None } else { Some(self.export_schema()?) };
        let prior_dirty = self.sys.layout_dirty;
        let result = (|| -> Result<Val> {
            for ext in &plan_extents {
                let mut r = Nat::from_bigint(&ext.physical_start).unwrap();
                let end = Nat::from_bigint(&ext.physical_end).unwrap();
                while r <= end {
                    if self.realms.contains_key(&r) {
                        return Err(Error::AllocationConflict(format!("Realm {r} is no longer free")));
                    }
                    self.create_realm(&r, &format!("Realm {r}"), "", None, &Nat::ZERO)?;
                    r = r.add_u64(1);
                }
            }
            let mut new_state = MathLayoutState::new(
                rec.layout_id,
                rec.spec.clone(),
                preview.clone(),
                rec.payload_type.name(),
                true,
                rec.created_block_id,
                Some(rec.first_write_block_id.unwrap_or(self.next_block_id)),
            )?;
            new_state.max_intermediate_bits = rec.max_intermediate_bits;
            self.math_layouts.insert(rec.layout_id, new_state.clone());
            let mut order: Vec<(Nat, u8)> = Vec::new();
            let mut grouped: HashMap<(Nat, u8), Vec<CellInput>> = HashMap::new();
            for ((_t, payload), addr) in materialized.iter().zip(resolved.iter()) {
                let key = (Nat::from_bigint(&addr.physical_realm).unwrap(), addr.side as u8);
                let cells = grouped.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    Vec::new()
                });
                cells.push(CellInput::typed(
                    Nat::from_bigint(&addr.u).unwrap(),
                    Nat::from_bigint(&addr.v_local).unwrap(),
                    payload.clone(),
                    Some(rec.payload_type),
                ));
            }
            let (mut cells_n, mut blocks_n, mut bytes_n) = (0i64, 0i64, 0i64);
            for key in order {
                let cells = grouped.remove(&key).unwrap();
                let out = self.put_points_as(&key.0, cells, key.1, false, MATH_SPARSE_U_WINDOW, Some(rec.layout_id))?;
                cells_n += out.get("cells").and_then(|v| v.as_i64()).unwrap_or(0);
                blocks_n += out.get("blocks").and_then(|v| v.as_i64()).unwrap_or(0);
                bytes_n += out.get("bytes").and_then(|v| v.as_i64()).unwrap_or(0);
            }
            self.append_block(crate::format::BT_MATH_LAYOUT_STATE, new_state.to_val().to_json()?.as_bytes(), 0)?;
            self.math_layouts.insert(rec.layout_id, new_state);
            if !all_mapped {
                self.validate_math_layout_state()?;
            }
            if commit {
                self.commit()?;
            }
            Ok(vmap! {
                "cells" => cells_n, "blocks" => blocks_n, "bytes" => bytes_n,
                "new_realms" => Val::from_bigint(new_realms.clone()),
                "mapped_realms" => Val::from_bigint(preview.mapped_count()),
                "realm_extents" => Val::List(preview.extents.iter().map(|e| e.to_val()).collect()),
            })
        })();
        match result {
            Ok(v) => Ok((v, resolved)),
            Err(e) => {
                let prior_schema = match prior_schema {
                    Some(s) => s,
                    None => {
                        self.math_layouts.insert(rec.layout_id, rec.clone());
                        self.export_schema()?
                    }
                };
                self.rollback_math_tail(start_len, start_next, start_last, &prior_schema, prior_dirty)?;
                Err(e)
            }
        }
    }

    /// `_rollback_math_tail`: truncate the file to its state before the batch and
    /// rebuild the state from the file + previous schema (also used by
    /// [`Db::put_point_groups`]).
    pub(super) fn rollback_math_tail(&mut self, file_len: u64, next_block_id: u64, last_valid_end: u64, prior_schema: &Val, prior_dirty: bool) -> Result<()> {
        self.store.rollback_to(file_len)?;
        self.invalidate_blocks_from(file_len);
        self.reset_in_memory_state();
        self.next_block_id = next_block_id;
        let record = self.open_scan(last_valid_end)?;
        self.sys.record = record;
        self.import_schema(prior_schema)?;
        self.sys.layout_dirty = prior_dirty;
        self.validate_math_layout_state()
    }

    /// `put_math(layout_id, inputs, payload, overwrite=False, commit=False)`
    pub fn put_math(&mut self, layout_id: u64, inputs: &[Val], payload: Val, overwrite: bool, commit: bool) -> Result<Val> {
        let (_, mut resolved) = self.put_math_rows(layout_id, vec![(inputs.to_vec(), payload)], overwrite, commit)?;
        let addr = resolved.pop().ok_or_else(|| Error::LayoutState("put_math: no target cell".into()))?;
        Ok(vmap! {
            "layout_id" => Val::int(layout_id), "physical_realm" => Val::from_bigint(addr.physical_realm),
            "u" => Val::from_bigint(addr.u), "v" => Val::from_bigint(addr.v_local),
            "lane" => addr.side, "lane_name" => if addr.side == 1 { "NR" } else { "NL" },
        })
    }

    /// `get_math_cell(layout_id, inputs, dyn_address=False)`: never allocates.
    pub fn get_math_cell(&self, layout_id: u64, inputs: &[Val], dyn_address: bool) -> Result<Option<Val>> {
        let rec = self.math_record(layout_id)?;
        let t = self.math_target(rec, inputs)?;
        let Some(physical) = rec.realm_map.resolve(&t.logical_realm)? else {
            return Ok(None);
        };
        let pr = Nat::from_bigint(&physical).unwrap();
        let cell = self.get_cell(&pr, &Nat::from_bigint(&t.u).unwrap(), &Val::from_bigint(t.v_local.clone()), t.side as u8, dyn_address)?;
        let Some(mut out) = cell else { return Ok(None) };
        let ins: Vec<Val> = inputs.iter().map(py_int).collect::<Result<_>>()?;
        out.set("_math_layout_id", Val::int(rec.layout_id));
        out.set("_math_layout_name", Val::str(rec.name()));
        out.set("_math_inputs", Val::List(ins));
        out.set("_logical_realm", Val::from_bigint(t.logical_realm.clone()));
        out.set("_V_global", Val::from_bigint(t.v_global.clone()));
        Ok(Some(out))
    }

    /// `get_math(layout_id, inputs)`
    pub fn get_math(&self, layout_id: u64, inputs: &[Val]) -> Result<Val> {
        let rec = self.math_record(layout_id)?;
        let t = self.math_target(rec, inputs)?;
        let Some(physical) = rec.realm_map.resolve(&t.logical_realm)? else {
            return Ok(Val::None);
        };
        let pr = Nat::from_bigint(&physical).unwrap();
        let (u, v) = (Nat::from_bigint(&t.u).unwrap(), Nat::from_bigint(&t.v_local).unwrap());
        Ok(self.get_cell_value(&pr, &u, &v, t.side as u8)?.unwrap_or(Val::None))
    }
}

pub fn bigint_u64(b: &BigInt) -> Option<u64> {
    if b.is_negative() { None } else { b.to_u64() }
}

pub fn one() -> BigInt {
    BigInt::one()
}
