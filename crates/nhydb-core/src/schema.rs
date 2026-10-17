//! Schema objects (`FieldDef` … `IndexDef`).
//!
//! `to_val`/`from_val` correspond exactly to the Python `to_dict`/`from_dict`
//! so that the JSON metadata blocks are identical.

use crate::address::{require_positive, validate_local_v};
use crate::codec::{enum_from_val, enum_to_val, int_like, val_to_nat, FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::nat::Nat;
use crate::textindex::{normalize_kind, normalize_options, IndexKind, KIND_FULLTEXT, KIND_TOKEN};
use crate::val::Val;
use crate::vmap;
use num_traits::ToPrimitive;

fn req<'a>(d: &'a Val, k: &str) -> Result<&'a Val> {
    d.get(k).ok_or_else(|| Error::Key(format!("'{k}'")))
}

fn req_nat(d: &Val, k: &str) -> Result<Nat> {
    val_to_nat(req(d, k)?).ok_or_else(|| Error::Value(format!("{k}: positive integer expected")))
}

fn req_u64(d: &Val, k: &str) -> Result<u64> {
    int_like(req(d, k)?)
        .and_then(|b| b.to_u64())
        .ok_or_else(|| Error::Value(format!("{k}: integer in range 0..2^64 expected")))
}

fn opt_str(d: &Val, k: &str) -> String {
    match d.get(k) {
        None | Some(Val::None) => String::new(),
        Some(v) => v.py_str(),
    }
}

// ----------------------------------------------------------------------
// FieldDef
// ----------------------------------------------------------------------

pub fn field_to_val(f: &FieldDef) -> Val {
    let mut d = vmap! {"v" => &f.v, "name" => f.name.as_str(), "type" => f.typ.name()};
    if let Some(e) = &f.enum_map {
        d.set("enum", enum_to_val(e));
    }
    if !f.description.is_empty() {
        d.set("description", Val::str(&f.description));
    }
    d
}

/// Python `repr` of a `FieldDef` (for error messages).
pub fn field_repr(v: &Nat, name: &str, typ: &str, enum_map: &Option<crate::codec::EnumMap>, description: &str) -> String {
    use crate::val::py_str_repr;
    let en = match enum_map {
        None => "None".to_string(),
        Some(e) => enum_to_val(e).py_repr(),
    };
    format!(
        "FieldDef(v={v}, name={}, type={}, enum={en}, description={})",
        py_str_repr(name),
        py_str_repr(typ),
        py_str_repr(description)
    )
}

/// `FieldDef.from_dict`; unknown types are reported as SchemaError
/// (the same error `ShapeDef` validation raises).
pub fn field_from_val(d: &Val) -> Result<FieldDef> {
    let v = req_nat(d, "v")?;
    let name = req(d, "name")?.py_str();
    let tname = req(d, "type")?.py_str();
    let enum_map = enum_from_val(d.get("enum").unwrap_or(&Val::None))?;
    let description = opt_str(d, "description");
    let typ = TypeCode::from_name(&tname).ok_or_else(|| {
        Error::Schema(format!(
            "invalid field {}; local v must be in 1..10^100",
            field_repr(&v, &name, &tname, &enum_map, &description)
        ))
    })?;
    Ok(FieldDef { v, name, typ, enum_map, description })
}

// ----------------------------------------------------------------------
// ShapeDef
// ----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ShapeDef {
    pub shape_id: u64,
    pub realm_id: Nat,
    pub name: String,
    /// sorted by v
    pub fields: Vec<FieldDef>,
    pub description: String,
}

impl ShapeDef {
    pub fn new(shape_id: u64, realm_id: Nat, name: String, mut fields: Vec<FieldDef>, description: String) -> Result<Self> {
        fields.sort_by(|a, b| a.v.cmp(&b.v));
        for w in fields.windows(2) {
            if w[0].v == w[1].v {
                return Err(Error::Schema("duplicate v in shape".into()));
            }
        }
        let mut names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        names.sort();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::Schema("duplicate field name in shape".into()));
        }
        for f in &fields {
            if f.v.is_zero() || validate_local_v(&f.v, None).is_err() {
                return Err(Error::Schema(format!(
                    "invalid field {}; local v must be in 1..10^100",
                    field_repr(&f.v, &f.name, f.typ.name(), &f.enum_map, &f.description)
                )));
            }
        }
        Ok(ShapeDef { shape_id, realm_id, name, fields, description })
    }

    pub fn by_v(&self, v: &Nat) -> Option<&FieldDef> {
        // Fields are sorted by v; BASE shapes are dense from v1.
        if let Nat::S(x) = v {
            if *x >= 1 && (*x as usize) <= self.fields.len() && self.fields[*x as usize - 1].v == *v {
                return Some(&self.fields[*x as usize - 1]);
            }
        }
        self.fields.binary_search_by(|f| f.v.cmp(v)).ok().map(|i| &self.fields[i])
    }

    pub fn by_name(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn to_val(&self) -> Val {
        vmap! {
            "shape_id" => self.shape_id as i64, "realm_id" => &self.realm_id,
            "name" => self.name.as_str(), "description" => self.description.as_str(),
            "fields" => Val::List(self.fields.iter().map(field_to_val).collect()),
        }
    }

    pub fn from_val(d: &Val) -> Result<Self> {
        let fields = match req(d, "fields")? {
            Val::List(l) => l.iter().map(field_from_val).collect::<Result<Vec<_>>>()?,
            _ => return Err(Error::Schema("fields must be a list".into())),
        };
        ShapeDef::new(req_u64(d, "shape_id")?, req_nat(d, "realm_id")?, req(d, "name")?.py_str(), fields, opt_str(d, "description"))
    }
}

// ----------------------------------------------------------------------
// RealmDef
// ----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RealmDef {
    pub realm_id: Nat,
    pub name: String,
    pub description: String,
    pub default_shape_id: Option<u64>,
    pub base_reserve_to: Nat,
}

impl RealmDef {
    pub fn to_val(&self) -> Val {
        vmap! {
            "realm_id" => &self.realm_id, "name" => self.name.as_str(),
            "description" => self.description.as_str(),
            "default_shape_id" => self.default_shape_id.map(|x| x as i64),
            "base_reserve_to" => &self.base_reserve_to,
        }
    }

    pub fn from_val(d: &Val) -> Result<Self> {
        let ds = match d.get("default_shape_id") {
            None | Some(Val::None) => None,
            Some(_) => Some(req_u64(d, "default_shape_id")?),
        };
        let brt = match d.get("base_reserve_to") {
            None | Some(Val::None) => Nat::ZERO,
            Some(v) => val_to_nat(v).unwrap_or(Nat::ZERO),
        };
        Ok(RealmDef {
            realm_id: req_nat(d, "realm_id")?,
            name: req(d, "name")?.py_str(),
            description: opt_str(d, "description"),
            default_shape_id: ds,
            base_reserve_to: brt,
        })
    }
}

// ----------------------------------------------------------------------
// ExtensionDef
// ----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ExtensionDef {
    pub realm_id: Nat,
    pub v: Nat,
    pub typ: TypeCode,
    pub name: String,
    pub enum_map: Option<crate::codec::EnumMap>,
    pub description: String,
    /// Precomputed field view (avoids allocations on the write/read path).
    pub field: FieldDef,
}

impl ExtensionDef {
    pub fn new(realm_id: Nat, v: Nat, typ: &str, name: String, enum_val: &Val, description: String) -> Result<Self> {
        require_positive("realm_id", &realm_id)?;
        validate_local_v(&v, Some(&realm_id))?;
        let typ = TypeCode::from_name(typ).ok_or_else(|| Error::Schema(format!("unknown extension type '{typ}'")))?;
        let enum_map = enum_from_val(enum_val)?;
        let field = FieldDef {
            v: v.clone(),
            name: if name.is_empty() { format!("v{v}") } else { name.clone() },
            typ,
            enum_map: enum_map.clone(),
            description: description.clone(),
        };
        Ok(ExtensionDef { realm_id, v, typ, name, enum_map, description, field })
    }

    pub fn to_val(&self) -> Val {
        let mut d = vmap! {"realm_id" => &self.realm_id, "v" => &self.v, "type" => self.typ.name()};
        if !self.name.is_empty() {
            d.set("name", Val::str(&self.name));
        }
        if let Some(e) = &self.enum_map {
            d.set("enum", enum_to_val(e));
        }
        if !self.description.is_empty() {
            d.set("description", Val::str(&self.description));
        }
        d
    }

    pub fn from_val(d: &Val) -> Result<Self> {
        ExtensionDef::new(
            req_nat(d, "realm_id")?,
            req_nat(d, "v")?,
            &req(d, "type")?.py_str(),
            opt_str(d, "name"),
            d.get("enum").unwrap_or(&Val::None),
            opt_str(d, "description"),
        )
    }

    pub fn as_field(&self) -> FieldDef {
        self.field.clone()
    }

    pub fn label(&self) -> String {
        if self.name.is_empty() { format!("v{}", self.v) } else { self.name.clone() }
    }
}

// ----------------------------------------------------------------------
// LayoutDef
// ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutKind {
    Classic,
    Column,
    Packed,
    Custom,
}

impl LayoutKind {
    pub fn name(self) -> &'static str {
        match self {
            LayoutKind::Classic => "CLASSIC",
            LayoutKind::Column => "COLUMN",
            LayoutKind::Packed => "PACKED",
            LayoutKind::Custom => "CUSTOM",
        }
    }

    pub fn parse(s: &str) -> Result<LayoutKind> {
        match s.trim().to_uppercase().as_str() {
            "CLASSIC" => Ok(LayoutKind::Classic),
            "COLUMN" => Ok(LayoutKind::Column),
            "PACKED" => Ok(LayoutKind::Packed),
            "CUSTOM" => Ok(LayoutKind::Custom),
            other => Err(Error::Schema(format!("unknown layout type '{other}'"))),
        }
    }

    pub fn is_builtin(self) -> bool {
        self != LayoutKind::Custom
    }
}

#[derive(Clone, Debug)]
pub struct LayoutDef {
    pub layout_id: u64,
    pub realm_id: Nat,
    pub name: String,
    pub kind: LayoutKind,
    pub u_start: Nat,
    pub u_end: Option<Nat>,
    pub priority: i64,
    pub shape_id: Option<u64>,
    pub base_reserve_to: Nat,
    /// dict (Val::Map)
    pub params: Val,
    /// dict (Val::Map)
    pub spacemap: Val,
    pub plugin: String,
    pub description: String,
}

/// Field description of a COLUMN/PACKED/CUSTOM layout (from `params.fields`).
#[derive(Clone, Debug)]
pub struct LayoutField<'a> {
    pub raw: &'a Val,
}

impl<'a> LayoutField<'a> {
    pub fn name_or(&self, default: impl FnOnce() -> String) -> String {
        match self.raw.get("name") {
            Some(v) => v.py_str(),
            None => default(),
        }
    }
    /// `str(f.get("name", "field"))`
    pub fn name(&self) -> String {
        self.name_or(|| "field".into())
    }
    /// `str(f.get("type", "bytes"))`
    pub fn type_name(&self) -> String {
        match self.raw.get("type") {
            Some(v) => v.py_str(),
            None => "bytes".into(),
        }
    }
    /// `int(f["u"])`
    pub fn u(&self) -> Result<Nat> {
        let v = self.raw.get("u").ok_or_else(|| Error::Key("'u'".into()))?;
        int_like(v)
            .map(|b| if b.sign() == num_bigint::Sign::Minus { Nat::ZERO } else { Nat::from(b.magnitude().clone()) })
            .ok_or_else(|| Error::Value(format!("invalid COLUMN u: {}", v.py_repr())))
    }
    pub fn u_raw(&self) -> Result<num_bigint::BigInt> {
        let v = self.raw.get("u").ok_or_else(|| Error::Key("'u'".into()))?;
        int_like(v).ok_or_else(|| Error::Value(format!("invalid COLUMN u: {}", v.py_repr())))
    }
    /// `int(f.get("offset", 0))` (may be negative)
    pub fn offset(&self) -> Result<num_bigint::BigInt> {
        match self.raw.get("offset") {
            None => Ok(num_bigint::BigInt::from(0)),
            Some(v) => int_like(v).ok_or_else(|| Error::Value(format!("invalid offset: {}", v.py_repr()))),
        }
    }
    pub fn enum_val(&self) -> &Val {
        self.raw.get("enum").unwrap_or(&Val::None)
    }
    pub fn description(&self) -> String {
        opt_str(self.raw, "description")
    }
    /// Field as FieldDef with the given v and name.
    pub fn to_field(&self, v: Nat, name: String) -> Result<FieldDef> {
        let tname = self.type_name();
        let typ = TypeCode::from_name(&tname).ok_or_else(|| Error::Schema(format!("unknown data type '{tname}'")))?;
        Ok(FieldDef { v, name, typ, enum_map: enum_from_val(self.enum_val())?, description: self.description() })
    }
}

impl LayoutDef {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        layout_id: u64,
        realm_id: Nat,
        name: String,
        kind: &str,
        u_start: Nat,
        u_end: Option<Nat>,
        priority: i64,
        shape_id: Option<u64>,
        base_reserve_to: Nat,
        params: Val,
        spacemap: Val,
        plugin: String,
        description: String,
    ) -> Result<Self> {
        if layout_id == 0 {
            return Err(Error::Value("layout_id must be > 0".into()));
        }
        require_positive("realm_id", &realm_id)?;
        let kind = LayoutKind::parse(kind)?;
        require_positive("u_start", &u_start)?;
        if let Some(e) = &u_end {
            require_positive("u_end", e)?;
            if e < &u_start {
                return Err(Error::Schema("u_end must be >= u_start".into()));
            }
        }
        if !base_reserve_to.is_zero() {
            validate_local_v(&base_reserve_to, Some(&realm_id))?;
        }
        let params = if let Val::Map(_) = params { params } else { Val::map() };
        let spacemap = if let Val::Map(_) = spacemap { spacemap } else { Val::map() };
        Ok(LayoutDef {
            layout_id,
            realm_id,
            name,
            kind,
            u_start,
            u_end,
            priority,
            shape_id,
            base_reserve_to,
            params,
            spacemap,
            plugin,
            description,
        })
    }

    #[inline]
    pub fn contains_u(&self, u: &Nat) -> bool {
        u >= &self.u_start && self.u_end.as_ref().map(|e| u <= e).unwrap_or(true)
    }

    pub fn fields(&self) -> Vec<LayoutField<'_>> {
        match self.params.get("fields") {
            Some(Val::List(l)) => l.iter().map(|raw| LayoutField { raw }).collect(),
            _ => Vec::new(),
        }
    }

    pub fn param_nat(&self, key: &str) -> Option<Nat> {
        self.params.get(key).and_then(val_to_nat)
    }

    /// PACKED: `params["span"]`
    pub fn packed_span(&self) -> Result<Nat> {
        self.param_nat("span").filter(|s| !s.is_zero()).ok_or_else(|| Error::Key("'span'".into()))
    }

    /// PACKED: `params["u"]`
    pub fn packed_u(&self) -> Result<Nat> {
        self.param_nat("u").filter(|s| !s.is_zero()).ok_or_else(|| Error::Key("'u'".into()))
    }

    /// Width for the winner rule (`u_end-u_start` or 10^200).
    pub fn width(&self) -> Nat {
        match &self.u_end {
            Some(e) => e.checked_sub(&self.u_start).unwrap_or_default(),
            None => crate::address::unbounded_width().clone(),
        }
    }

    pub fn to_val(&self) -> Val {
        vmap! {
            "layout_id" => self.layout_id as i64, "realm_id" => &self.realm_id,
            "name" => self.name.as_str(), "kind" => self.kind.name(),
            "u_start" => &self.u_start, "u_end" => Val::opt_nat(self.u_end.as_ref()),
            "priority" => self.priority, "shape_id" => self.shape_id.map(|x| x as i64),
            "base_reserve_to" => &self.base_reserve_to,
            "params" => self.params.clone(), "spacemap" => self.spacemap.clone(),
            "plugin" => self.plugin.as_str(), "description" => self.description.as_str(),
        }
    }

    pub fn from_val(d: &Val) -> Result<Self> {
        let u_start = match d.get("u_start") {
            None => Nat::ONE,
            Some(v) => val_to_nat(v).unwrap_or(Nat::ZERO),
        };
        let u_end = match d.get("u_end") {
            None | Some(Val::None) => None,
            Some(v) => Some(val_to_nat(v).unwrap_or(Nat::ZERO)),
        };
        let shape_id = match d.get("shape_id") {
            None | Some(Val::None) => None,
            Some(_) => Some(req_u64(d, "shape_id")?),
        };
        let priority = d.get("priority").and_then(int_like).and_then(|b| b.to_i64()).unwrap_or(0);
        let brt = d.get("base_reserve_to").and_then(val_to_nat).unwrap_or(Nat::ZERO);
        let params = d.get("params").cloned().filter(|v| v.is_truthy()).unwrap_or(Val::map());
        let spacemap = d.get("spacemap").cloned().filter(|v| v.is_truthy()).unwrap_or(Val::map());
        LayoutDef::new(
            req_u64(d, "layout_id")?,
            req_nat(d, "realm_id")?,
            req(d, "name")?.py_str(),
            &req(d, "kind")?.py_str(),
            u_start,
            u_end,
            priority,
            shape_id,
            brt,
            params,
            spacemap,
            opt_str(d, "plugin"),
            opt_str(d, "description"),
        )
    }

    /// Sort key `(priority, -width, layout_id)`; larger wins.
    pub fn sort_cmp(a: &LayoutDef, b: &LayoutDef) -> std::cmp::Ordering {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.width().cmp(&a.width()))
            .then_with(|| a.layout_id.cmp(&b.layout_id))
    }
}

// ----------------------------------------------------------------------
// IndexDef
// ----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct IndexDef {
    pub index_id: u64,
    pub realm_id: Nat,
    pub name: String,
    /// Legacy field `v`: source field for VALUE, otherwise 0 (protects older readers).
    pub v: Nat,
    pub description: String,
    pub kind: IndexKind,
    /// Source field of the non-VALUE families.
    pub source_v: Option<Nat>,
    /// Normalized, persisted family options.
    pub options: Val,
}

impl IndexDef {
    /// Logical source field (`field_v`).
    pub fn field_v(&self) -> &Nat {
        self.source_v.as_ref().unwrap_or(&self.v)
    }

    /// `IndexDef.to_dict()` including forward-compatibility protection for FULLTEXT.
    pub fn to_val(&self) -> Val {
        if self.kind == IndexKind::Fulltext {
            let compat = vmap! {
                "tokenizer" => self.options.get("tokenizer").map(|v| v.py_str()).unwrap_or_else(|| "simple".into()),
                "casefold" => self.options.get("casefold").map(|v| v.is_truthy()).unwrap_or(true),
            };
            return vmap! {
                "index_id" => self.index_id as i64, "realm_id" => &self.realm_id,
                "name" => self.name.as_str(), "v" => 0i64, "description" => self.description.as_str(),
                "kind" => KIND_TOKEN, "source_v" => Val::None, "options" => compat,
                "v024_kind" => KIND_FULLTEXT, "v024_source_v" => self.field_v(),
                "v024_options" => self.options.clone(),
            };
        }
        vmap! {
            "index_id" => self.index_id as i64, "realm_id" => &self.realm_id,
            "name" => self.name.as_str(), "v" => &self.v, "description" => self.description.as_str(),
            "kind" => self.kind.name(), "source_v" => Val::opt_nat(self.source_v.as_ref()),
            "options" => self.options.clone(),
        }
    }

    pub fn from_val(d: &Val) -> Result<Self> {
        let base = |kind: IndexKind, v: Nat, source_v: Option<Nat>, options: Val| -> Result<IndexDef> {
            Ok(IndexDef {
                index_id: req_u64(d, "index_id")?,
                realm_id: req_nat(d, "realm_id")?,
                name: req(d, "name")?.py_str(),
                v,
                description: opt_str(d, "description"),
                kind,
                source_v,
                options,
            })
        };
        if d.get("v024_kind").map(|k| crate::unicode::upper(&k.py_str()) == KIND_FULLTEXT).unwrap_or(false) {
            let opts = normalize_options(IndexKind::Fulltext, d.get("v024_options").unwrap_or(&Val::None))?;
            return base(IndexKind::Fulltext, Nat::ZERO, Some(req_nat(d, "v024_source_v")?), opts);
        }
        let kind = normalize_kind(d.get("kind").unwrap_or(&Val::None))?;
        let opts = normalize_options(kind, d.get("options").unwrap_or(&Val::None))?;
        let source_v = match d.get("source_v") {
            None | Some(Val::None) => None,
            Some(_) => Some(req_nat(d, "source_v")?),
        };
        base(kind, req_nat(d, "v")?, source_v, opts)
    }
}
