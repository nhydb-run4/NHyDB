//! Realms, shapes, extensions, index definitions and address APIs.

use crate::address::{self, realm_span, validate_local_v, CellAddress, DynAddress, Lane};
use crate::codec::{FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::Nat;
use crate::schema::{ExtensionDef, IndexDef, RealmDef, ShapeDef};
use crate::textindex::{normalize_kind, normalize_options, IndexKind};
use crate::val::Val;
use crate::vmap;

use super::{Db, DynMode};

impl Db {
    pub fn create_realm(
        &mut self,
        realm_id: &Nat,
        name: &str,
        description: &str,
        default_shape_id: Option<u64>,
        base_reserve_to: &Nat,
    ) -> Result<RealmDef> {
        address::require_positive("realm_id", realm_id)?;
        if !base_reserve_to.is_zero() {
            validate_local_v(base_reserve_to, Some(realm_id))?;
        }
        let r = RealmDef {
            realm_id: realm_id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            default_shape_id,
            base_reserve_to: base_reserve_to.clone(),
        };
        self.append_block(BT_REALM, r.to_val().to_json()?.as_bytes(), 0)?;
        self.max_u.entry((realm_id.clone(), 0)).or_insert(Nat::ZERO);
        self.max_u.entry((realm_id.clone(), 1)).or_insert(Nat::ZERO);
        self.realms.insert(realm_id.clone(), r.clone());
        self.clear_decode_caches();
        Ok(r)
    }

    pub fn create_shape(
        &mut self,
        shape_id: u64,
        realm_id: &Nat,
        name: &str,
        fields: Vec<FieldDef>,
        description: &str,
    ) -> Result<ShapeDef> {
        self.require_realm(realm_id)?;
        self.deny_math_realm(realm_id)?;
        let s = ShapeDef::new(shape_id, realm_id.clone(), name.to_string(), fields, description.to_string())?;
        self.append_block(BT_SHAPE, s.to_val().to_json()?.as_bytes(), 0)?;
        self.shapes.insert(s.shape_id, s.clone());
        self.clear_decode_caches();
        Ok(s)
    }

    pub fn current_base_shape(&self, realm_id: &Nat) -> Option<&ShapeDef> {
        let r = self.realms.get(realm_id)?;
        self.shapes.get(&r.default_shape_id?)
    }

    pub fn base_max_v(&self, realm_id: &Nat) -> Nat {
        self.current_base_shape(realm_id)
            .and_then(|s| s.fields.iter().map(|f| f.v.clone()).max())
            .unwrap_or(Nat::ZERO)
    }

    fn validate_base_shape(&self, realm_id: &Nat, shape: &ShapeDef) -> Result<()> {
        for (i, f) in shape.fields.iter().enumerate() {
            if f.v != (i as u64 + 1) {
                return Err(Error::Schema("BASE fields must be contiguous starting at v=1: v1..vN".into()));
            }
        }
        let realm = self.require_realm(realm_id)?;
        if let Some(last) = shape.fields.last() {
            if !realm.base_reserve_to.is_zero() && last.v > realm.base_reserve_to {
                return Err(Error::Schema(format!(
                    "BASE exceeds reserved range v1..v{}",
                    realm.base_reserve_to
                )));
            }
        }
        for f in &shape.fields {
            if self.extension_defs.contains_key(&(realm_id.clone(), f.v.clone())) {
                return Err(Error::Schema(format!("v{} is already defined as an extension", f.v)));
            }
        }
        let Some(old) = self.current_base_shape(realm_id) else {
            return Ok(());
        };
        if shape.fields.len() < old.fields.len() {
            return Err(Error::Schema("BASE must not be shrunk".into()));
        }
        for (a, b) in old.fields.iter().zip(shape.fields.iter()) {
            if a != b {
                return Err(Error::Schema(format!("BASE must not redefine existing field v{}", a.v)));
            }
        }
        Ok(())
    }

    pub fn set_realm_default_shape(&mut self, realm_id: &Nat, shape_id: u64) -> Result<RealmDef> {
        self.require_realm(realm_id)?;
        let shape = match self.shapes.get(&shape_id) {
            Some(s) if &s.realm_id == realm_id => s.clone(),
            _ => return Err(Error::Schema("Shape does not match the realm".into())),
        };
        self.validate_base_shape(realm_id, &shape)?;
        let old = self.realms[realm_id].clone();
        self.create_realm(realm_id, &old.name, &old.description, Some(shape_id), &old.base_reserve_to)
    }

    pub fn extension_boundary(&self, realm_id: &Nat) -> Nat {
        let base = self.base_max_v(realm_id);
        let brt = self.realms.get(realm_id).map(|r| r.base_reserve_to.clone()).unwrap_or_default();
        base.max(brt)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn define_extension(
        &mut self,
        realm_id: &Nat,
        v: &Nat,
        typ: &str,
        name: &str,
        enum_val: &Val,
        description: &str,
        replace: bool,
    ) -> Result<ExtensionDef> {
        self.require_realm(realm_id)?;
        self.deny_math_realm(realm_id)?;
        validate_local_v(v, Some(realm_id))?;
        let boundary = self.extension_boundary(realm_id);
        if v <= &boundary {
            return Err(Error::Schema(format!(
                "v{v} lies in the BASE/reserve range v1..v{boundary}; extensions start after it"
            )));
        }
        let ext = ExtensionDef::new(realm_id.clone(), v.clone(), typ, name.to_string(), enum_val, description.to_string())?;
        if let Some(old) = self.extension_defs.get(&(realm_id.clone(), v.clone())) {
            if !replace {
                return Err(Error::Schema(format!("Extension v{v} is already defined")));
            }
            if old.typ != ext.typ || old.enum_map != ext.enum_map {
                return Err(Error::Schema("Type/enum of an existing extension must not be changed".into()));
            }
        }
        if !ext.name.is_empty() {
            if let Some(base) = self.current_base_shape(realm_id) {
                if base.by_name(&ext.name).is_some() {
                    return Err(Error::Schema(format!("Name '{}' already exists in BASE", ext.name)));
                }
            }
            if let Some(other) = self.extension_name_map.get(&(realm_id.clone(), ext.name.clone())) {
                if other != v {
                    return Err(Error::Schema(format!("Extension name '{}' is already taken", ext.name)));
                }
            }
        }
        self.append_block(BT_EXTENSION_DEF, ext.to_val().to_json()?.as_bytes(), 0)?;
        self.install_extension(ext.clone());
        self.clear_decode_caches();
        Ok(ext)
    }

    /// Column → local v (`resolve_v`). `column` is int or str.
    pub fn resolve_v(&self, realm_id: &Nat, column: &Val) -> Result<Nat> {
        self.require_realm(realm_id)?;
        match column {
            Val::Int(_) | Val::Big(_) | Val::Bool(_) => {
                let v = column.as_nat().unwrap_or(Nat::ZERO);
                validate_local_v(&v, Some(realm_id))?;
                Ok(v)
            }
            _ => {
                let text = column.py_str();
                if let Some(base) = self.current_base_shape(realm_id) {
                    if let Some(f) = base.by_name(&text) {
                        return Ok(f.v.clone());
                    }
                }
                if let Some(v) = self.extension_name_map.get(&(realm_id.clone(), text.clone())) {
                    return Ok(v.clone());
                }
                if let Some(rest) = text.strip_prefix('v') {
                    if let Some(v) = Nat::parse_decimal(rest) {
                        validate_local_v(&v, Some(realm_id))?;
                        return Ok(v);
                    }
                }
                Err(Error::Schema(format!("unknown column {}", column.py_repr())))
            }
        }
    }

    /// `create_index(index_id, realm_id, name, v, *, kind, options, tokenizer,
    /// casefold, ngram, description)`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_index_ext(
        &mut self,
        index_id: u64,
        realm_id: &Nat,
        name: &str,
        v: &Val,
        kind: &Val,
        options: &Val,
        tokenizer: Option<&Val>,
        casefold: Option<bool>,
        ngram: Option<&Val>,
        description: &str,
    ) -> Result<IndexDef> {
        self.require_realm(realm_id)?;
        self.deny_math_realm(realm_id)?;
        if self.index_name_map.contains_key(&(realm_id.clone(), name.to_string())) {
            return Err(Error::Schema("Index name exists".into()));
        }
        let source_v = self.resolve_v(realm_id, v)?;
        let kind = normalize_kind(kind)?;
        let mut opt = match options {
            Val::Map(_) => options.clone(),
            Val::None => Val::map(),
            other => return Err(Error::Type(format!("options must be a mapping, not {}", other.py_repr()))),
        };
        if let Some(t) = tokenizer {
            opt.set("tokenizer", t.clone());
        }
        if let Some(c) = casefold {
            opt.set("casefold", Val::Bool(c));
        }
        if let Some(n) = ngram {
            opt.set("ngram", n.clone());
        }
        let opt = normalize_options(kind, &opt)?;
        if kind != IndexKind::Value {
            let source_type = match self.current_base_shape(realm_id).and_then(|b| b.by_v(&source_v)) {
                Some(f) => Some(f.typ),
                None => self.extension_defs.get(&(realm_id.clone(), source_v.clone())).map(|e| e.field.typ),
            };
            if source_type != Some(TypeCode::Utf8) {
                let shown = match source_type {
                    Some(t) => Val::str(t.name()).py_repr(),
                    None => "None".into(),
                };
                return Err(Error::Schema(format!(
                    "{} index requires a utf8 source field; v{} has {}",
                    kind.name(),
                    source_v,
                    shown
                )));
            }
        }
        let (legacy_v, stored_source) =
            if kind == IndexKind::Value { (source_v, None) } else { (Nat::ZERO, Some(source_v)) };
        let idx = IndexDef {
            index_id,
            realm_id: realm_id.clone(),
            name: name.to_string(),
            v: legacy_v,
            description: description.to_string(),
            kind,
            source_v: stored_source,
            options: opt,
        };
        self.append_block(BT_INDEX_DEF, idx.to_val().to_json()?.as_bytes(), 0)?;
        self.index_defs.insert(index_id, idx.clone());
        self.index_name_map.insert((realm_id.clone(), name.to_string()), index_id);
        Ok(idx)
    }

    /// VALUE index.
    pub fn create_index(&mut self, index_id: u64, realm_id: &Nat, name: &str, v: &Val, description: &str) -> Result<IndexDef> {
        self.create_index_ext(index_id, realm_id, name, v, &Val::None, &Val::None, None, None, None, description)
    }

    pub fn realm_bounds(&self, realm_id: &Nat) -> Result<Val> {
        self.require_realm(realm_id)?;
        let (lo, hi) = address::realm_v_bounds(realm_id)?;
        Ok(vmap! {"realm" => realm_id, "v_min" => lo, "v_max" => hi, "local_v_min" => 1i64, "local_v_max" => realm_span()})
    }

    pub fn dyn_address(&self, realm_id: &Nat, u: &Nat, v: &Nat) -> Result<DynAddress> {
        self.require_realm(realm_id)?;
        address::dynadron_address(realm_id, u, v)
    }

    pub fn cell_address(&self, realm_id: &Nat, u: &Nat, v: &Nat, lane: Lane) -> Result<CellAddress> {
        self.require_realm(realm_id)?;
        address::cell_address(realm_id, u, v, lane)
    }

    pub fn resolve_n(&self, n: &Nat) -> Result<Val> {
        let a = address::n_to_address(n)?;
        let mut out = a.to_n_val();
        out.set("realm_defined", Val::Bool(self.realms.contains_key(a.realm())));
        Ok(out)
    }

    pub fn set_dyn_address_output(&mut self, mode: &str, v: Option<Nat>) -> Result<()> {
        let m = DynMode::parse(mode)?;
        if m == DynMode::Once && v.is_none() {
            return Err(Error::Value("dyn-address once requires v".into()));
        }
        if let Some(x) = &v {
            validate_local_v(x, None)?;
        }
        self.dyn_mode = m;
        self.dyn_v = v;
        Ok(())
    }

    pub(crate) fn resolve_dyn_output(&self, mode: Option<&str>, v: Option<&Nat>) -> Result<(DynMode, Option<Nat>)> {
        let m = match mode {
            None => self.dyn_mode,
            Some(s) => DynMode::parse(s)?,
        };
        let v = v.cloned().or_else(|| self.dyn_v.clone());
        if m == DynMode::Once {
            let Some(x) = &v else {
                return Err(Error::Value("dyn-address once requires dyn_v".into()));
            };
            validate_local_v(x, None)?;
        }
        Ok((m, v))
    }

    pub fn extension_catalog(&self, realm_id: &Nat) -> Result<Val> {
        self.require_realm(realm_id)?;
        Ok(Val::List(
            self.extension_defs
                .iter()
                .filter(|((rid, _), _)| rid == realm_id)
                .map(|(_, e)| e.to_val())
                .collect(),
        ))
    }

    pub fn extension_label(&self, realm_id: &Nat, v: &Nat) -> String {
        match self.extension_defs.get(&(realm_id.clone(), v.clone())) {
            Some(e) if !e.name.is_empty() => e.name.clone(),
            _ => format!("v{v}"),
        }
    }

    /// `_normalize_extensions`: pairs (key, value) → sorted (v, value).
    pub fn normalize_extensions(&self, realm_id: &Nat, items: &[(Val, Val)]) -> Result<Vec<(Nat, Val)>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let boundary = self.extension_boundary(realm_id);
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(items.len());
        for (key, value) in items {
            let v = self.resolve_v(realm_id, key)?;
            if v <= boundary {
                return Err(Error::Schema(format!(
                    "v{v} lies in the BASE/reserve range v1..v{boundary} and is not an extension"
                )));
            }
            if !seen.insert(v.clone()) {
                return Err(Error::Schema(format!("Extension v{v} duplicated in the record")));
            }
            if !value.is_none() && !self.extension_defs.contains_key(&(realm_id.clone(), v.clone())) {
                return Err(Error::Schema(format!(
                    "v{v} has a value but no extension type definition. Call define_extension() first."
                )));
            }
            out.push((v, value.clone()));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// `_decode_extensions` (EXT area after BASE in dense records).
    pub(crate) fn decode_extensions(&self, realm_id: &Nat, payload: &[u8], mut pos: usize) -> Result<(Vec<(Nat, Val)>, usize)> {
        if pos == payload.len() {
            return Ok((Vec::new(), pos));
        }
        if pos > payload.len() {
            return Err(Error::Corruption("Record ends before the extension area".into()));
        }
        if payload[pos] != EXT_TAG {
            return Err(Error::Corruption("unknown bytes after BASE; extension tag missing".into()));
        }
        pos += 1;
        let count = crate::nat::get_vu_u64(payload, &mut pos, "EXT count")?;
        let boundary = self.extension_boundary(realm_id);
        let mut prev = Nat::ZERO;
        let mut out = Vec::new();
        for _ in 0..count {
            let key = crate::nat::get_vu(payload, &mut pos)?;
            let (delta, has_value) = match &key {
                Nat::S(k) => (Nat::S(k >> 1), k & 1 == 1),
                Nat::B(b) => (Nat::from(&**b >> 1usize), b.bit(0)),
            };
            if delta.is_zero() {
                return Err(Error::Corruption("Extension v delta must be > 0".into()));
            }
            let v = prev.add(&delta);
            validate_local_v(&v, Some(realm_id))?;
            if v <= boundary {
                return Err(Error::Corruption(format!("Extension v{v} lies in the BASE/reserve range")));
            }
            let value = if has_value {
                let ext = self.extension_defs.get(&(realm_id.clone(), v.clone())).ok_or_else(|| {
                    Error::Corruption(format!("Extension v{v} contains a value but no type definition"))
                })?;
                crate::codec::decode_value(&ext.field, payload, &mut pos)?
            } else {
                Val::None
            };
            match out.iter_mut().find(|(vv, _): &&mut (Nat, Val)| *vv == v) {
                Some(slot) => slot.1 = value,
                None => out.push((v.clone(), value)),
            }
            prev = v;
        }
        Ok((out, pos))
    }

    /// Type of a declared extension.
    pub fn extension_type(&self, realm_id: &Nat, v: &Nat) -> Option<TypeCode> {
        self.extension_defs.get(&(realm_id.clone(), v.clone())).map(|e| e.typ)
    }
}
