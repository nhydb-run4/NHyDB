//! Lese-APIs: `get`, `get_cell`, Objekt-APIs, geometrisches GET
//! (`cell`/`record`/`auto`), DESCRIBE, Belegungsabfragen, `range`/`select`.

use std::collections::{BTreeMap, BTreeSet};

use crate::address::{self, lane_name, n_to_address, CellAddress, Lane};
use crate::error::{Error, Result};
use crate::nat::Nat;
use crate::plugin::PluginMethod;
use crate::schema::LayoutKind;
use crate::val::Val;
use crate::vmap;

use super::layout::{coord, lane_from_val, LayoutRef};
use super::sparse::CellInput;
use super::{Db, DynMode, SVal};

/// GET-Modus für N-/Strukturadressen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GetMode {
    Auto,
    Cell,
    Record,
}

impl GetMode {
    pub fn parse(s: &str) -> Result<GetMode> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(GetMode::Auto),
            "cell" => Ok(GetMode::Cell),
            "record" => Ok(GetMode::Record),
            _ => Err(Error::Value("mode muss auto, cell oder record sein".into())),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            GetMode::Auto => "auto",
            GetMode::Cell => "cell",
            GetMode::Record => "record",
        }
    }
}

impl Db {
    /// Vollständiger CLASSIC-Record (`get`).
    pub fn get(
        &self,
        realm: &Nat,
        u: &Nat,
        lane: Lane,
        dyn_mode: Option<&str>,
        dyn_v: Option<&Nat>,
        include_extensions: bool,
    ) -> Result<Option<Val>> {
        let Some(rec) = self.get_internal(realm, u, lane)? else {
            return Ok(None);
        };
        let mut out = vmap! {
            "_realm" => realm, "_u" => u, "_lane" => lane as i64,
            "_lane_name" => lane_name(lane), "_shape" => rec.shape_id as i64,
        };
        let mut items = std::mem::take(&mut out).as_map().cloned().unwrap_or_default();
        for (k, v) in &rec.values {
            set_or_push(&mut items, k, v.clone());
        }
        if let Some(base) = self.current_base_shape(realm) {
            for f in &base.fields {
                if !items.iter().any(|(k, _)| *k == f.name) {
                    items.push((f.name.clone(), Val::None));
                }
            }
        }
        if include_extensions {
            for (v, val) in &rec.extensions_by_v {
                set_or_push(&mut items, &self.extension_label(realm, v), val.clone());
            }
        }
        out = Val::Map(items);
        let (mode, chosen) = self.resolve_dyn_output(dyn_mode, dyn_v)?;
        match mode {
            DynMode::Off => {}
            DynMode::Once => self.dyn_address(realm, u, chosen.as_ref().unwrap())?.flatten_into(&mut out),
            DynMode::Auto => {
                let mut dyns = Val::map();
                let shape = self.shape_of(rec.shape_id)?;
                for f in &shape.fields {
                    dyns.set(f.name.clone(), self.dyn_address(realm, u, &f.v)?.to_val());
                }
                for v in rec.extensions_by_v.keys() {
                    dyns.set(self.extension_label(realm, v), self.dyn_address(realm, u, v)?.to_val());
                }
                out.set("_dyn_addresses", dyns);
            }
        }
        Ok(Some(out))
    }

    /// Einzelne Zelle (`get_cell`). `v` ist int, `"v123"` oder ein Feldname.
    pub fn get_cell(&self, realm: &Nat, u: &Nat, v: &Val, lane: Lane, dyn_address: bool) -> Result<Option<Val>> {
        let mut u = u.clone();
        let vnum: Nat = match v {
            Val::Str(s) => {
                let vstyle = s.strip_prefix('v').map(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false);
                if vstyle {
                    Nat::parse_decimal(&s[1..]).unwrap()
                } else {
                    match self.resolve_v(realm, v) {
                        Ok(x) => x,
                        Err(Error::Schema(_)) => {
                            let Some(lay) = self.layout_at(realm, &u) else {
                                return Ok(None);
                            };
                            if lay.kind != LayoutKind::Column {
                                return Ok(None);
                            }
                            let mut hit = None;
                            for f in lay.fields() {
                                let fu = f.u()?;
                                if f.name_or(|| format!("u{fu}")) == *s {
                                    hit = Some(fu);
                                }
                            }
                            let Some(cu) = hit else {
                                return Ok(None);
                            };
                            u = cu;
                            let a = self.logical_address_in(lay, &Nat::ONE, v, 0)?;
                            a.v_local().clone()
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            other => coord(other)?,
        };
        let v = vnum;
        if v.is_zero() || u.is_zero() {
            return Ok(None);
        }
        let rec = self.get_internal(realm, &u, lane)?;
        let Some(rec) = rec else {
            let Some(SVal::V(value)) = self.sparse_point(realm, &u, &v, lane)? else {
                return Ok(None);
            };
            let meta = self.field_meta_for_address(realm, &u, &v)?;
            let mut out = vmap! {
                "_realm" => realm, "_u" => &u, "_lane" => lane as i64, "_lane_name" => lane_name(lane),
                "_v" => &v, "_shape" => Val::None,
                "field" => meta.map(|m| m.name).unwrap_or_else(|| format!("v{v}")),
                "present" => true, "value" => value,
            };
            if dyn_address {
                self.dyn_address(realm, &u, &v)?.flatten_into(&mut out);
            }
            return Ok(Some(out));
        };
        let stored = self.shape_of(rec.shape_id)?;
        let (name, value) = if let Some(f) = stored.by_v(&v) {
            (f.name.clone(), rec.base_by_v.get(&v).cloned().unwrap_or(Val::None))
        } else if let Some(cf) = self.current_base_shape(realm).and_then(|b| b.by_v(&v)) {
            (cf.name.clone(), Val::None)
        } else if let Some(x) = rec.extensions_by_v.get(&v) {
            (self.extension_label(realm, &v), x.clone())
        } else {
            return Ok(None);
        };
        let mut out = vmap! {
            "_realm" => realm, "_u" => &u, "_lane" => lane as i64, "_lane_name" => lane_name(lane),
            "_v" => &v, "_shape" => rec.shape_id as i64, "field" => name, "present" => true, "value" => value,
        };
        if dyn_address {
            self.dyn_address(realm, &u, &v)?.flatten_into(&mut out);
        }
        Ok(Some(out))
    }

    /// Nur der Zellwert (Rust-Schnellzugriff): None = nicht belegt.
    /// Gleiche Belegungs-/Wertregeln wie `get_cell`, ohne Umschlag und Feldnamen.
    pub fn get_cell_value(&self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<Option<Val>> {
        if v.is_zero() || u.is_zero() {
            return Ok(None);
        }
        let Some(rec) = self.get_internal(realm, u, lane)? else {
            return Ok(match self.sparse_point(realm, u, v, lane)? {
                Some(SVal::V(value)) => Some(value),
                _ => None,
            });
        };
        let stored = self.shape_of(rec.shape_id)?;
        Ok(if stored.by_v(v).is_some() {
            Some(rec.base_by_v.get(v).cloned().unwrap_or(Val::None))
        } else if self.current_base_shape(realm).and_then(|b| b.by_v(v)).is_some() {
            Some(Val::None)
        } else {
            rec.extensions_by_v.get(v).cloned()
        })
    }

    pub fn get_cell_n(&self, n: &Nat, dyn_address: bool) -> Result<Option<Val>> {
        let a = n_to_address(n)?;
        if !self.realms.contains_key(a.realm()) {
            return Ok(None);
        }
        self.get_cell(a.realm(), a.u(), &Val::nat(a.v_local()), a.lane, dyn_address)
    }

    // ------------------------------------------------------------------
    // Objekte
    // ------------------------------------------------------------------

    pub fn get_object(&self, r: &LayoutRef, obj: &Nat, lane: Lane, include_addresses: bool) -> Result<Option<Val>> {
        let lay = self.layout_by_ref(r)?;
        if lay.kind == LayoutKind::Classic {
            return self.get(&lay.realm_id, obj, lane, None, None, true);
        }
        let mut out = vmap! {
            "_layout" => lay.layout_id as i64, "_layout_name" => lay.name.as_str(), "_kind" => lay.kind.name(),
            "_object" => obj, "_lane" => lane as i64, "_lane_name" => lane_name(lane),
        };
        let mut found = false;
        let mut addresses = Val::map();
        address::require_positive("object_id", obj)?;
        for f in lay.fields() {
            let name = f.name();
            let addr = self.logical_address_in(lay, obj, &Val::str(&name), lane)?;
            let cell = self.get_cell(addr.realm(), addr.u(), &Val::nat(addr.v_local()), addr.lane, false)?;
            found |= cell.is_some();
            out.set(name.clone(), cell.and_then(|c| c.get("value").cloned()).unwrap_or(Val::None));
            if include_addresses {
                addresses.set(
                    name,
                    vmap! {"N" => addr.n_ref(), "realm" => addr.realm(), "u" => addr.u(), "v" => addr.v_local(), "lane" => addr.lane as i64},
                );
            }
        }
        if include_addresses {
            out.set("_addresses", addresses);
        }
        Ok(if found { Some(out) } else { None })
    }

    pub fn put_object(&mut self, r: &LayoutRef, obj: &Nat, values: &Val, lane: Lane, commit: bool) -> Result<Val> {
        let lay = self.layout_by_ref(r)?.clone();
        let items = values.as_map().ok_or_else(|| Error::Schema("values muss ein Dictionary sein".into()))?;
        if lay.kind == LayoutKind::Classic {
            let sid = lay.shape_id.ok_or_else(|| Error::Schema("CLASSIC Layout ohne Shape".into()))?;
            let shape = self.shapes.get(&sid).ok_or_else(|| Error::Key(sid.to_string()))?;
            let mut base = Vec::with_capacity(shape.fields.len());
            for f in &shape.fields {
                let v = values.get(&f.name).ok_or_else(|| Error::Schema(format!("Pflichtfeld '{}' fehlt", f.name)))?;
                base.push(v.clone());
            }
            let ext: Vec<(Val, Val)> = items
                .iter()
                .filter(|(k, _)| shape.by_name(k).is_none())
                .map(|(k, v)| (Val::str(k), v.clone()))
                .collect();
            let ext = if ext.is_empty() { None } else { Some(ext) };
            return self.put(&lay.realm_id, obj, Val::List(base), Some(sid), ext, lane, commit);
        }
        let declared: Vec<String> = lay.fields().iter().map(|f| f.name()).collect();
        let mut cells = Vec::with_capacity(items.len());
        for (name, value) in items {
            if !declared.contains(name) {
                return Err(Error::Schema(format!("Layout '{}': Feld '{}' unbekannt", lay.name, name)));
            }
            address::require_positive("object_id", obj)?;
            let addr = self.logical_address_in(&lay, obj, &Val::str(name), lane)?;
            cells.push(CellInput::new(addr.u().clone(), addr.v_local().clone(), value.clone()));
        }
        self.put_points(&lay.realm_id, cells, lane, commit, 10_000)
    }

    /// Mehrere Objekte über ein Layout (`put_objects_batch`).
    pub fn put_objects_batch(&mut self, r: &LayoutRef, objects: Vec<(Nat, Val)>, lane: Lane, commit: bool, u_window: u64) -> Result<Val> {
        let lay = self.layout_by_ref(r)?.clone();
        let mut items = Vec::with_capacity(objects.len());
        for (oid, vals) in objects {
            address::require_positive("object_id", &oid)?;
            if !matches!(vals, Val::Map(_)) {
                return Err(Error::Schema("values muss ein Dictionary sein".into()));
            }
            items.push((oid, vals));
        }
        if items.is_empty() {
            if commit {
                self.commit()?;
            }
            return Ok(vmap! {"objects" => 0i64, "cells" => 0i64, "blocks" => 0i64, "bytes" => 0i64});
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));
        if items.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(Error::Schema("object_id im Batch doppelt".into()));
        }
        let n_objects = items.len() as i64;
        if lay.kind == LayoutKind::Classic {
            let sid = lay.shape_id.ok_or_else(|| Error::Schema("CLASSIC Layout ohne Shape".into()))?;
            let shape = self.shapes.get(&sid).ok_or_else(|| Error::Key(sid.to_string()))?.clone();
            // Läufe zusammenhängender object_ids sammeln (vollständig validiert)
            struct Run {
                start: Nat,
                rows: Vec<Vec<Val>>,
                ext: Vec<(Nat, Vec<(Val, Val)>)>,
            }
            let mut runs: Vec<Run> = Vec::new();
            let mut prev: Option<Nat> = None;
            for (oid, vals) in &items {
                let consecutive = prev.as_ref().map(|p| p.add_u64(1) == *oid).unwrap_or(false);
                if !consecutive {
                    runs.push(Run { start: oid.clone(), rows: Vec::new(), ext: Vec::new() });
                }
                let run = runs.last_mut().unwrap();
                let mut row = Vec::with_capacity(shape.fields.len());
                for f in &shape.fields {
                    let v = vals.get(&f.name).ok_or_else(|| Error::Schema(format!("Pflichtfeld '{}' fehlt", f.name)))?;
                    row.push(v.clone());
                }
                let ext: Vec<(Val, Val)> = vals
                    .as_map()
                    .unwrap()
                    .iter()
                    .filter(|(k, _)| shape.by_name(k).is_none())
                    .map(|(k, v)| (Val::str(k), v.clone()))
                    .collect();
                if !ext.is_empty() {
                    run.ext.push((oid.clone(), ext));
                }
                run.rows.push(row);
                prev = Some(oid.clone());
            }
            let nf = shape.fields.len() as i64;
            let (mut total_cells, mut total_bytes, mut blocks) = (0i64, 0i64, 0i64);
            for run in runs {
                let r = self.put_dense_batch(
                    &lay.realm_id,
                    &run.start,
                    &super::dense::DenseRows::Tuples(&run.rows),
                    Some(sid),
                    &run.ext,
                    lane,
                    false,
                )?;
                total_cells += run.rows.len() as i64 * nf;
                total_bytes += r.get("bytes").and_then(|v| v.as_i64()).unwrap_or(0);
                blocks += 1;
                if let Some(s) = r.get("sparse").filter(|s| s.is_truthy()) {
                    total_cells += s.get("cells").and_then(|v| v.as_i64()).unwrap_or(0);
                    total_bytes += s.get("bytes").and_then(|v| v.as_i64()).unwrap_or(0);
                    blocks += 1;
                }
            }
            if commit {
                self.commit()?;
            }
            return Ok(vmap! {
                "objects" => n_objects, "cells" => total_cells, "blocks" => blocks,
                "bytes" => total_bytes, "kind" => lay.kind.name(),
            });
        }
        let fields = lay.fields();
        let mut cells = Vec::new();
        match lay.kind {
            LayoutKind::Column => {
                let mut by_name: Vec<(String, Nat)> = Vec::new();
                for f in &fields {
                    let nm = f.name();
                    let u = f.u()?;
                    by_name.retain(|(n, _)| *n != nm);
                    by_name.push((nm, u));
                }
                for (oid, vals) in &items {
                    for (name, value) in vals.as_map().unwrap() {
                        let u = by_name
                            .iter()
                            .find(|(n, _)| n == name)
                            .map(|(_, u)| u.clone())
                            .ok_or_else(|| Error::Schema(format!("Layout '{}': Feld '{}' unbekannt", lay.name, name)))?;
                        cells.push(CellInput::new(u, oid.clone(), value.clone()));
                    }
                }
            }
            LayoutKind::Packed => {
                let u = lay.packed_u()?;
                let span = lay.packed_span()?;
                let mut by_name: Vec<(String, num_bigint::BigInt)> = Vec::new();
                for f in &fields {
                    let nm = f.name();
                    by_name.retain(|(n, _)| *n != nm);
                    by_name.push((nm, f.offset()?));
                }
                for (oid, vals) in &items {
                    let anchor = oid.sub_u64(1).unwrap().mul(&span).add_u64(1);
                    if &anchor > address::realm_span() {
                        return Err(Error::RealmLimit("PACKED record liegt ausserhalb dieses Realms".into()));
                    }
                    for (name, value) in vals.as_map().unwrap() {
                        let off = by_name
                            .iter()
                            .find(|(n, _)| n == name)
                            .map(|(_, o)| o.clone())
                            .ok_or_else(|| Error::Schema(format!("Layout '{}': Feld '{}' unbekannt", lay.name, name)))?;
                        let off = match off.to_biguint().map(Nat::from) {
                            Some(o) if o < span => o,
                            _ => return Err(Error::Schema("PACKED offset ausserhalb span".into())),
                        };
                        cells.push(CellInput::new(u.clone(), anchor.add(&off), value.clone()));
                    }
                }
            }
            LayoutKind::Custom => {
                for (oid, vals) in &items {
                    for (name, value) in vals.as_map().unwrap() {
                        let a = self.logical_address_in(&lay, oid, &Val::str(name), lane)?;
                        cells.push(CellInput::new(a.u().clone(), a.v_local().clone(), value.clone()));
                    }
                }
            }
            LayoutKind::Classic => unreachable!(),
        }
        let mut r = self.put_points(&lay.realm_id, cells, lane, commit, u_window)?;
        r.set("objects", Val::Int(n_objects));
        r.set("kind", Val::str(lay.kind.name()));
        Ok(r)
    }

    // ------------------------------------------------------------------
    // Geometrisches GET
    // ------------------------------------------------------------------

    pub fn get_record_view(&self, r: &LayoutRef, obj: &Nat, lane: Lane, request_n: Option<&Nat>) -> Result<Val> {
        let lay = self.layout_by_ref(r)?;
        address::require_positive("object_id", obj)?;
        let anchor = self.logical_address_in(lay, obj, &Val::None, lane)?;
        let mut raw_cells: Vec<(CellAddress, Val)> = Vec::new();
        match lay.kind {
            LayoutKind::Classic => {
                let rec = self.get_internal(&lay.realm_id, obj, lane)?;
                let updates = self.sparse_updates_for_u(&lay.realm_id, obj, lane)?;
                if rec.is_some() || !updates.is_empty() {
                    let mut vs: BTreeSet<Nat> = rec.as_ref().map(|r| r.by_v().into_keys().collect()).unwrap_or_default();
                    for (v, s) in updates.iter() {
                        if s.is_tomb() {
                            vs.remove(v);
                        } else {
                            vs.insert(v.clone());
                        }
                    }
                    for v in vs {
                        let value = match updates.get(&v) {
                            Some(SVal::V(x)) => x.clone(),
                            _ => match rec.as_ref().and_then(|r| r.by_v_get(&v)) {
                                Some(x) => x.clone(),
                                None => continue,
                            },
                        };
                        raw_cells.push((self.cell_address(&lay.realm_id, obj, &v, lane)?, value));
                    }
                }
            }
            LayoutKind::Packed => {
                let span = lay.packed_span()?;
                let u = lay.packed_u()?;
                let lo = obj.sub_u64(1).unwrap().mul(&span).add_u64(1);
                let hi = lo.add(&span).sub_u64(1).unwrap();
                for (v, s) in self.sparse_updates_for_u_range(&lay.realm_id, &u, &lo, &hi, lane)? {
                    if let SVal::V(x) = s {
                        raw_cells.push((self.cell_address(&lay.realm_id, &u, &v, lane)?, x));
                    }
                }
            }
            _ => {
                for n in self.occupied_n(r, obj, lane)? {
                    let a = n_to_address(&n)?;
                    if let Some(cell) = self.get_cell(a.realm(), a.u(), &Val::nat(a.v_local()), a.lane, false)? {
                        let value = cell.get("value").cloned().unwrap_or(Val::None);
                        raw_cells.push((a, value));
                    }
                }
            }
        }
        let mut cells = Vec::new();
        let mut data: Vec<(String, Val)> = Vec::new();
        let anchor_ref = anchor.n_ref();
        let mut anchor_present = false;
        for (addr, value) in raw_cells {
            let interp = self.interpret_layout_address(lay, &addr)?;
            let label = Db::fallback_cell_label(lay, &interp, addr.u(), addr.v_local());
            let mut dict_label = label.clone();
            if data.iter().any(|(k, _)| *k == dict_label) {
                dict_label = format!("{label}@u{}v{}", addr.u(), addr.v_local());
            }
            set_or_push(&mut data, &dict_label, value.clone());
            let is_anchor = addr.n_ref() == anchor_ref;
            anchor_present |= is_anchor;
            cells.push(vmap! {
                "N" => addr.n_ref(), "realm" => addr.realm(), "u" => addr.u(), "v" => addr.v_local(),
                "lane" => addr.lane as i64, "lane_name" => lane_name(addr.lane),
                "field" => label, "value" => value, "anchor" => is_anchor,
            });
        }
        let n_cells = cells.len() as i64;
        let mut out = vmap! {
            "result" => "record", "record_N" => anchor_ref.clone(),
            "realm" => &lay.realm_id, "lane" => lane as i64, "lane_name" => lane_name(lane),
            "layout_id" => lay.layout_id as i64, "layout_name" => lay.name.as_str(), "layout_kind" => lay.kind.name(),
            "object" => obj, "present" => n_cells > 0, "cell_count" => n_cells,
            "anchor_present" => anchor_present, "data" => Val::Map(data), "cells" => Val::List(cells),
            "anchor_address" => vmap! {
                "realm" => anchor.realm(), "u" => anchor.u(), "v" => anchor.v_local(), "lane" => anchor.lane as i64,
            },
        };
        if let Some(rn) = request_n {
            let req = n_to_address(rn)?;
            out.set("request_N", Val::Str(req.n_ref()));
            out.set("request_is_anchor", Val::Bool(req.n_ref() == anchor_ref));
        }
        Ok(out)
    }

    pub fn get_resolved_address(&self, addr: &CellAddress, mode: GetMode) -> Result<Val> {
        let rid = addr.realm();
        let u = addr.u();
        let v = addr.v_local();
        let lane = addr.lane;
        if !self.realms.contains_key(rid) {
            return Ok(vmap! {
                "result" => "unresolved", "reason" => "realm_not_defined", "N" => addr.n_ref(),
                "realm" => rid, "u" => u, "v" => v, "lane" => lane as i64, "lane_name" => lane_name(lane),
                "requested_mode" => mode.name(), "effective_mode" => Val::None,
            });
        }
        let lay = self.layout_at(rid, u);
        let interp = match lay {
            Some(l) => Some(self.interpret_layout_address(l, addr)?),
            None => None,
        };
        let record_id = interp.as_ref().and_then(Db::interp_record);
        let is_anchor = interp.as_ref().and_then(|i| i.get("anchor")).map(|a| a.is_truthy()).unwrap_or(false);
        let chosen = match mode {
            GetMode::Auto => {
                if lay.is_some() && record_id.is_some() && is_anchor {
                    GetMode::Record
                } else {
                    GetMode::Cell
                }
            }
            m => m,
        };
        if chosen == GetMode::Record {
            let (Some(l), Some(rec)) = (lay, record_id.as_ref()) else {
                return Err(Error::Schema("Adresse gehoert zu keinem aufloesbaren logischen Datensatz".into()));
            };
            let mut out = self.get_record_view(&LayoutRef::Id(l.layout_id), rec, lane, Some(&addr.n))?;
            out.set("requested_mode", Val::str(mode.name()));
            out.set("effective_mode", Val::str("record"));
            if mode == GetMode::Auto {
                out.set("auto_policy", Val::str("anchor_record_else_cell"));
            }
            return Ok(out);
        }
        let cell = self.get_cell(rid, u, &Val::nat(v), lane, false)?;
        let mut record_n = Val::None;
        if let (Some(l), Some(rec)) = (lay, record_id.as_ref()) {
            match self.logical_address_in(l, rec, &Val::None, lane) {
                Ok(a) => record_n = Val::Str(a.n_ref()),
                Err(Error::Schema(_) | Error::RealmLimit(_) | Error::Key(_) | Error::Value(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let label = match (lay, &interp) {
            (Some(l), Some(i)) => Some(Db::fallback_cell_label(l, i, u, v)),
            _ => None,
        };
        let field = match label {
            Some(l) => Val::Str(l),
            None => cell.as_ref().and_then(|c| c.get("field").cloned()).unwrap_or(Val::None),
        };
        let mut out = vmap! {
            "result" => "cell", "N" => addr.n_ref(), "realm" => rid, "u" => u, "v" => v,
            "lane" => lane as i64, "lane_name" => lane_name(lane),
            "layout_id" => lay.map(|l| l.layout_id as i64), "layout_name" => lay.map(|l| l.name.clone()),
            "layout_kind" => lay.map(|l| l.kind.name()),
            "object" => record_id.as_ref().map(Val::nat).unwrap_or(Val::None), "record_N" => record_n,
            "field" => field, "anchor" => is_anchor, "present" => cell.is_some(),
            "value" => cell.as_ref().and_then(|c| c.get("value").cloned()).unwrap_or(Val::None),
            "requested_mode" => mode.name(), "effective_mode" => "cell",
        };
        if mode == GetMode::Auto {
            out.set("auto_policy", Val::str("anchor_record_else_cell"));
        }
        Ok(out)
    }

    /// GET über eine N-Referenz.
    pub fn get_n(&self, n: &Nat, mode: GetMode) -> Result<Val> {
        let a = n_to_address(n)?;
        self.get_resolved_address(&a, mode)
    }

    /// GET über die Strukturadresse (R, u, v, lane).
    pub fn get_address(&self, realm: &Nat, u: &Nat, v: &Nat, lane: Lane, mode: GetMode) -> Result<Val> {
        let a = address::cell_address(realm, u, v, lane)?;
        self.get_resolved_address(&a, mode)
    }

    pub fn describe_n(&self, n: &Nat) -> Result<Val> {
        let a = n_to_address(n)?;
        let mut out = a.to_n_val();
        let defined = self.realms.contains_key(a.realm());
        out.set("realm_defined", Val::Bool(defined));
        if !defined {
            out.set("layout", Val::None);
            out.set("interpretation", Val::None);
            out.set("occupied", Val::Bool(false));
            return Ok(out);
        }
        let lay = self.layout_at(a.realm(), a.u());
        match lay {
            Some(l) => {
                out.set("layout", self.layout_info(l)?);
                out.set("interpretation", self.interpret_layout_address(l, &a)?);
            }
            None => {
                out.set("layout", Val::None);
                out.set("interpretation", Val::None);
            }
        }
        let occ = self.get_cell(a.realm(), a.u(), &Val::nat(a.v_local()), a.lane, false)?.is_some();
        out.set("occupied", Val::Bool(occ));
        Ok(out)
    }

    /// Belegte Zellen eines logischen Objekts als N-Zahlen (`occupied_n`).
    pub fn occupied_n(&self, r: &LayoutRef, obj: &Nat, lane: Lane) -> Result<Vec<Nat>> {
        let lay = self.layout_by_ref(r)?;
        let rid = &lay.realm_id;
        let mut out = Vec::new();
        match lay.kind {
            LayoutKind::Classic => {
                let rec = self.get_internal(rid, obj, lane)?;
                let updates = self.sparse_updates_for_u(rid, obj, lane)?;
                if rec.is_none() && updates.is_empty() {
                    return Ok(out);
                }
                let mut vs: BTreeSet<Nat> = rec.map(|r| r.by_v().into_keys().collect()).unwrap_or_default();
                for (v, s) in updates.iter() {
                    if s.is_tomb() {
                        vs.remove(v);
                    } else {
                        vs.insert(v.clone());
                    }
                }
                for v in vs {
                    out.push(address::dynadron_n(rid, obj, &v, lane)?);
                }
            }
            LayoutKind::Column => {
                for f in lay.fields() {
                    let u = f.u()?;
                    if self.get_cell(rid, &u, &Val::nat(obj), lane, false)?.is_some() {
                        out.push(address::dynadron_n(rid, &u, obj, lane)?);
                    }
                }
            }
            LayoutKind::Packed => {
                let span = lay.packed_span()?;
                let u = lay.packed_u()?;
                let lo = obj.sub_u64(1).unwrap_or_default().mul(&span).add_u64(1);
                let hi = lo.add(&span).sub_u64(1).unwrap();
                for (v, s) in self.sparse_updates_for_u_range(rid, &u, &lo, &hi, lane)? {
                    if !s.is_tomb() {
                        out.push(address::dynadron_n(rid, &u, &v, lane)?);
                    }
                }
            }
            LayoutKind::Custom => {
                if let Some(p) = self.plugin_for(lay).filter(|p| p.has(PluginMethod::OccupiedAddresses)) {
                    for a in p.occupied_addresses(self, lay, obj, lane)? {
                        let (rr, u, v, ll) = match &a {
                            Val::Str(s) if s.starts_with('N') => {
                                out.push(address::parse_n_reference(s)?);
                                continue;
                            }
                            Val::Map(_) => (
                                match a.get("realm") {
                                    Some(x) => coord(x)?,
                                    None => rid.clone(),
                                },
                                coord(a.get("u").ok_or_else(|| Error::Key("'u'".into()))?)?,
                                coord(a.get("v").ok_or_else(|| Error::Key("'v'".into()))?)?,
                                match a.get("lane") {
                                    Some(x) => lane_from_val(x)?,
                                    None => lane,
                                },
                            ),
                            Val::List(l) if l.len() >= 2 => (rid.clone(), coord(&l[0])?, coord(&l[1])?, lane),
                            _ => return Err(Error::Value("occupied_addresses: ungueltiger Eintrag".into())),
                        };
                        if self.get_cell(&rr, &u, &Val::nat(&v), ll, false)?.is_some() {
                            out.push(address::dynadron_n(&rr, &u, &v, ll)?);
                        }
                    }
                    return Ok(out);
                }
                let fields = lay.fields();
                if fields.is_empty() {
                    return Err(Error::Schema(
                        "occupied_n fuer CUSTOM benoetigt deklarierte fields oder occupied_addresses()".into(),
                    ));
                }
                address::require_positive("object_id", obj)?;
                for f in fields {
                    let a = self.logical_address_in(lay, obj, &Val::str(f.name()), lane)?;
                    if self.get_cell(a.realm(), a.u(), &Val::nat(a.v_local()), a.lane, false)?.is_some() {
                        out.push(a.n.clone());
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn occupied_from_n(&self, n: &Nat, output: &str) -> Result<Val> {
        let desc = self.describe_n(n)?;
        let lay_obj = desc.get("layout").cloned().unwrap_or(Val::None);
        let interp = desc.get("interpretation").cloned().unwrap_or(Val::None);
        let record = Db::interp_record(&interp);
        if !lay_obj.is_truthy() || !interp.contains_key("record") || record.is_none() {
            return Err(Error::Schema("N-Referenz gehoert zu keinem aufloesbaren logischen Objekt".into()));
        }
        let lid = lay_obj.get("layout_id").and_then(|v| v.as_i64()).unwrap_or(0) as u64;
        let record = record.unwrap();
        let lane = desc.get("lane").and_then(|v| v.as_i64()).unwrap_or(0) as Lane;
        let refs = self.occupied_n(&LayoutRef::Id(lid), &record, lane)?;
        let mode = output.to_uppercase();
        if !matches!(mode.as_str(), "N" | "ADDRESS" | "BOTH") {
            return Err(Error::Value("output muss N, ADDRESS oder BOTH sein".into()));
        }
        let mut cells = Vec::new();
        for nn in refs {
            let a = n_to_address(&nn)?;
            let structural = vmap! {
                "realm" => a.realm(), "u" => a.u(), "v" => a.v_local(), "lane" => a.lane as i64,
                "lane_name" => lane_name(a.lane),
            };
            cells.push(match mode.as_str() {
                "N" => Val::Str(a.n_ref()),
                "ADDRESS" => structural,
                _ => vmap! {"N" => a.n_ref(), "address" => structural},
            });
        }
        Ok(vmap! {
            "anchor_or_member" => desc.get("N_ref").cloned().unwrap_or(Val::None), "layout_id" => lid as i64,
            "record" => &record, "count" => cells.len() as i64, "cells" => Val::List(cells), "output" => mode,
        })
    }

    // ------------------------------------------------------------------
    // Bereichsabfragen
    // ------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)] // bildet die Python-Signatur (Keyword-Argumente) nach
    pub fn range(
        &self,
        realm: &Nat,
        u_from: &Nat,
        u_to: &Nat,
        lane: Lane,
        dyn_mode: Option<&str>,
        dyn_v: Option<&Nat>,
        include_extensions: bool,
    ) -> Result<Vec<Val>> {
        let mut out = Vec::new();
        let mut u = u_from.clone();
        while &u <= u_to {
            if !u.is_zero() {
                if let Some(r) = self.get(realm, &u, lane, dyn_mode, dyn_v, include_extensions)? {
                    out.push(r);
                }
            }
            u = u.add_u64(1);
        }
        Ok(out)
    }

    pub fn select(&self, realm: &Nat, u_from: &Nat, u_to: &Nat, columns: &[Val], lane: Lane) -> Result<Vec<Val>> {
        let mut resolved = Vec::new();
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
        let mut out = Vec::new();
        let mut u = u_from.clone();
        while &u <= u_to {
            if !u.is_zero() {
                if let Some(rec) = self.get_internal(realm, &u, lane)? {
                    let mut row: Vec<(String, Val)> = vec![
                        ("_u".into(), Val::nat(&u)),
                        ("_lane".into(), Val::Int(lane as i64)),
                        ("_lane_name".into(), Val::str(lane_name(lane))),
                    ];
                    let by_v: BTreeMap<Nat, Val> = rec.by_v();
                    for (label, v) in &resolved {
                        set_or_push(&mut row, label, by_v.get(v).cloned().unwrap_or(Val::None));
                    }
                    out.push(Val::Map(row));
                }
            }
            u = u.add_u64(1);
        }
        Ok(out)
    }
}

pub(crate) fn set_or_push(items: &mut Vec<(String, Val)>, k: &str, v: Val) {
    if let Some(slot) = items.iter_mut().find(|(kk, _)| kk == k) {
        slot.1 = v;
    } else {
        items.push((k.to_string(), v));
    }
}
