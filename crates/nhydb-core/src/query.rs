//! Layout-independent query layer (QueryEngine).
//!
//! Rule: user predicate → provider → canonical candidate identities
//! (N or record_N) → result. Planner (`cardinality-cost-v1`), evaluation,
//! domain conversion, providers, aggregation, ORDER/LIMIT and BM25 search;
//! comparisons, equality, group keys and arithmetic follow Python rules.
//!
//! Performance optimizations without semantic change: without ORDER BY,
//! result rows are only built after LIMIT/OFFSET, the materialization scan
//! keeps no set of all dense records and computes record_N for CLASSIC
//! once per u group.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

use crate::address::{self, n_to_address, Lane};
use crate::db::layout::{lane_from_val, LayoutRef};
use crate::db::materialize::{RawCell, RawCellCursor, RecordNCursor};
use crate::db::read::GetMode;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::mathexpr::MathMap;
use crate::mathlayout::resolve_math_target;
use crate::nat::Nat;
use crate::schema::{IndexDef, LayoutDef, LayoutKind};
use crate::setops;
use crate::textindex::{self, IndexKind};
use crate::unicode;
use crate::val::{Num, Val, ValKey};
use crate::vmap;

pub const QUERY_VERSION: &str = "0.27";
pub const OPTIMIZER: &str = "cardinality-cost-v1";

/// Up to this range width, STRUCT_RANGE visits every u directly.
const STRUCT_RANGE_DIRECT_MAX: u64 = 4096;

fn plan_err(m: impl Into<String>) -> Error {
    Error::QueryPlan(m.into())
}

fn dom_err(m: impl Into<String>) -> Error {
    Error::QueryDomain(m.into())
}

fn repr(s: &str) -> String {
    Val::str(s).py_repr()
}

// ----------------------------------------------------------------------
// Python helpers
// ----------------------------------------------------------------------

/// Python type name for error messages.
pub fn py_type_name(v: &Val) -> &'static str {
    match v {
        Val::None => "NoneType",
        Val::Bool(_) => "bool",
        Val::Int(_) | Val::Big(_) => "int",
        Val::Float(_) => "float",
        Val::Str(_) => "str",
        Val::Bytes(_) => "bytes",
        Val::List(_) => "list",
        Val::Map(_) => "dict",
    }
}

/// Python `int(x)`.
pub fn py_int(v: &Val) -> Result<BigInt> {
    match v {
        Val::Bool(b) => Ok(BigInt::from(*b as i64)),
        Val::Int(i) => Ok(BigInt::from(*i)),
        Val::Big(b) => Ok(b.clone()),
        Val::Float(f) => {
            if f.is_nan() {
                Err(Error::Value("cannot convert float NaN to integer".into()))
            } else if f.is_infinite() {
                Err(Error::Value("cannot convert float infinity to integer".into()))
            } else {
                Ok(num_traits::FromPrimitive::from_f64(f.trunc()).unwrap_or_default())
            }
        }
        Val::Str(s) => {
            let t = unicode::strip(s);
            let ok = !t.is_empty()
                && !t.starts_with('_')
                && !t.ends_with('_')
                && !t.contains("__")
                && t.trim_start_matches(['+', '-']).chars().next().is_some_and(|c| c.is_ascii_digit());
            let cleaned = t.replace('_', "");
            match cleaned.parse::<BigInt>() {
                Ok(b) if ok => Ok(b),
                _ => Err(Error::Value(format!("invalid literal for int() with base 10: {}", v.py_repr()))),
            }
        }
        other => Err(Error::Type(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            py_type_name(other)
        ))),
    }
}

/// Python `float(x)`.
pub fn py_float(v: &Val) -> Result<f64> {
    match v {
        Val::Bool(b) => Ok(*b as i64 as f64),
        Val::Int(i) => Ok(*i as f64),
        Val::Big(b) => b.to_f64().ok_or_else(|| Error::Value("int too large to convert to float".into())),
        Val::Float(f) => Ok(*f),
        Val::Str(s) => {
            let t = unicode::strip(s).replace('_', "");
            let low = t.to_ascii_lowercase();
            let parsed = match low.trim_start_matches(['+', '-']) {
                "inf" | "infinity" | "nan" => t.parse::<f64>().ok().or_else(|| {
                    let neg = low.starts_with('-');
                    let x = if low.contains("nan") { f64::NAN } else { f64::INFINITY };
                    Some(if neg { -x } else { x })
                }),
                _ => t.parse::<f64>().ok(),
            };
            parsed.ok_or_else(|| Error::Value(format!("could not convert string to float: {}", v.py_repr())))
        }
        other => Err(Error::Type(format!("float() argument must be a string or a real number, not '{}'", py_type_name(other)))),
    }
}

/// Python `round(x, 3)` (correctly rounded via the decimal representation).
pub fn round3(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.3}").parse().unwrap_or(x)
}

/// Python `int / int` (correctly rounded true division, `b > 0`).
pub fn int_true_div(a: &BigInt, b: &BigInt) -> f64 {
    let lim = BigInt::from(1u64 << 53);
    if a.abs() <= lim && b.abs() <= lim {
        return a.to_f64().unwrap() / b.to_f64().unwrap();
    }
    let neg = a.is_negative() != b.is_negative();
    let (a, b) = (a.abs(), b.abs());
    if a.is_zero() {
        return if neg { -0.0 } else { 0.0 };
    }
    let shift: i64 = 55 + b.bits() as i64 - a.bits() as i64;
    let (q, r) = if shift >= 0 {
        let n = &a << (shift as usize);
        (&n / &b, &n % &b)
    } else {
        let d = &b << ((-shift) as usize);
        (&a / &d, &a % &d)
    };
    let sticky = !r.is_zero();
    let extra = q.bits() as i64 - 53;
    let (mut mant, exp) = if extra > 0 {
        let mask = (BigInt::from(1) << extra as usize) - 1;
        let rem = &q & &mask;
        let half = BigInt::from(1) << (extra as usize - 1);
        let mut m = &q >> extra as usize;
        let odd = (&m & BigInt::from(1)) == BigInt::from(1);
        if rem > half || (rem == half && (sticky || odd)) {
            m += 1;
        }
        (m, extra - shift)
    } else {
        (q, -shift)
    };
    if mant.is_zero() {
        mant = BigInt::from(0);
    }
    let m = mant.to_f64().unwrap();
    let out = m * 2f64.powi(exp as i32);
    if neg {
        -out
    } else {
        out
    }
}

/// Python `a < b`; `Err(TypeError)` for incomparable types.
pub fn py_lt(a: &Val, b: &Val, sym: &str) -> Result<bool> {
    match (a, b) {
        (Val::Str(x), Val::Str(y)) => Ok(x < y),
        (Val::Bytes(x), Val::Bytes(y)) => Ok(x < y),
        (Val::List(x), Val::List(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                if p != q {
                    return py_lt(p, q, sym);
                }
            }
            Ok(x.len() < y.len())
        }
        _ => match (a.as_number(), b.as_number()) {
            (Some(x), Some(y)) => Ok(x.partial_cmp(&y) == Some(std::cmp::Ordering::Less)),
            _ => Err(Error::Type(format!(
                "'{sym}' not supported between instances of '{}' and '{}'",
                py_type_name(a),
                py_type_name(b)
            ))),
        },
    }
}

/// Stable sort using only `lt` (like `list.sort`); `reverse` as in Python
/// (equal elements keep their original order).
pub fn py_sort<T>(mut items: Vec<T>, reverse: bool, lt: &mut dyn FnMut(&T, &T) -> Result<bool>) -> Result<Vec<T>> {
    if reverse {
        items.reverse();
    }
    let mut out = merge_sort(items, lt)?;
    if reverse {
        out.reverse();
    }
    Ok(out)
}

fn merge_sort<T>(mut items: Vec<T>, lt: &mut dyn FnMut(&T, &T) -> Result<bool>) -> Result<Vec<T>> {
    if items.len() <= 1 {
        return Ok(items);
    }
    let right = items.split_off(items.len() / 2);
    let left = merge_sort(items, lt)?;
    let right = merge_sort(right, lt)?;
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut li = left.into_iter().peekable();
    let mut ri = right.into_iter().peekable();
    loop {
        match (li.peek(), ri.peek()) {
            (Some(l), Some(r)) => {
                if lt(r, l)? {
                    out.push(ri.next().unwrap());
                } else {
                    out.push(li.next().unwrap());
                }
            }
            (Some(_), None) => out.push(li.next().unwrap()),
            (None, Some(_)) => out.push(ri.next().unwrap()),
            (None, None) => break,
        }
    }
    Ok(out)
}

/// Python `value in actual` for `_match_raw(..., "contains")`.
fn py_contains(actual: &Val, value: &Val) -> Result<bool> {
    Ok(match (actual, value) {
        (Val::Str(a), Val::Str(v)) => a.contains(v.as_str()),
        (Val::Bytes(a), Val::Bytes(v)) => v.is_empty() || a.windows(v.len()).any(|w| w == v.as_slice()),
        (Val::Bytes(a), v) if matches!(v, Val::Int(_) | Val::Big(_) | Val::Bool(_)) => {
            let b = py_int(v)?;
            match b.to_u8() {
                Some(x) if !b.is_negative() => a.contains(&x),
                _ => return Err(Error::Value("byte must be in range(0, 256)".into())),
            }
        }
        (Val::List(items), v) => items.iter().any(|x| x == v),
        (Val::Map(m), Val::Str(k)) => m.iter().any(|(kk, _)| kk == k),
        _ => false,
    })
}

/// `_match_raw`
pub fn match_raw(actual: &Val, op: &str, value: &Val, value2: &Val) -> Result<bool> {
    match op {
        "exists" => return Ok(!actual.is_none()),
        "eq" => return Ok(actual == value),
        "ne" => return Ok(actual != value),
        _ => {}
    }
    let r = match op {
        "lt" => Some(py_lt(actual, value, "<")),
        "le" => Some(le(actual, value)),
        "gt" => Some(py_lt(value, actual, ">")),
        "ge" => Some(le(value, actual)),
        "between" => Some(le(value, actual).and_then(|ok| if ok { le(actual, value2) } else { Ok(false) })),
        _ => None,
    };
    if let Some(r) = r {
        return match r {
            Ok(b) => Ok(b),
            Err(Error::Type(_)) => Ok(false),
            Err(e) => Err(e),
        };
    }
    if op == "contains" {
        return py_contains(actual, value);
    }
    Err(plan_err(format!("operator {} unknown", repr(op))))
}

/// Python `a <= b`.
fn le(a: &Val, b: &Val) -> Result<bool> {
    match (a, b) {
        (Val::Str(x), Val::Str(y)) => Ok(x <= y),
        (Val::Bytes(x), Val::Bytes(y)) => Ok(x <= y),
        (Val::List(_), Val::List(_)) => Ok(a == b || py_lt(a, b, "<=")?),
        _ => match (a.as_number(), b.as_number()) {
            (Some(x), Some(y)) => Ok(matches!(x.partial_cmp(&y), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal))),
            _ => Err(Error::Type(format!(
                "'<=' not supported between instances of '{}' and '{}'",
                py_type_name(a),
                py_type_name(b)
            ))),
        },
    }
}

/// `parse_n_reference` for arbitrary values.
pub fn parse_n_val(v: &Val) -> Result<Nat> {
    match v {
        Val::Str(s) => {
            let raw = unicode::strip(s);
            let raw = match raw.chars().next() {
                Some('N') | Some('n') => unicode::strip(&raw[1..]),
                _ => raw,
            };
            if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
                return Err(Error::Address(format!("invalid N reference {}", v.py_repr())));
            }
            let n = Nat::parse_decimal(raw).ok_or_else(|| Error::Address(format!("invalid N reference {}", v.py_repr())))?;
            address::require_positive("N", &n)?;
            Ok(n)
        }
        other => {
            let b = py_int(other)?;
            match Nat::from_bigint(&b) {
                Some(n) if !n.is_zero() => Ok(n),
                _ => Err(Error::Value("N must be > 0".into())),
            }
        }
    }
}

fn nat_or_zero(b: &BigInt) -> Nat {
    Nat::from_bigint(b).unwrap_or(Nat::ZERO)
}

fn dyn_n(realm: &BigInt, u: &BigInt, v: &BigInt, lane: Lane) -> Result<Nat> {
    address::dynadron_n(&nat_or_zero(realm), &nat_or_zero(u), &nat_or_zero(v), lane)
}

fn side_lane(side: i64) -> Lane {
    if side == 1 {
        address::NR
    } else {
        address::NL
    }
}

/// `str()` list of a Python sequence (str is iterated per character).
pub fn str_list(v: &Val) -> Vec<String> {
    match v {
        Val::None => Vec::new(),
        Val::List(l) => l.iter().map(|x| x.py_str()).collect(),
        Val::Str(s) => s.chars().map(|c| c.to_string()).collect(),
        Val::Map(m) => m.iter().map(|(k, _)| k.clone()).collect(),
        other => vec![other.py_str()],
    }
}

fn n_val(n: &Nat) -> Val {
    Val::nat(n)
}

fn n_ref(n: &Nat) -> String {
    format!("N{n}")
}

// ----------------------------------------------------------------------
// Expression tree
// ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    Record,
    Cell,
}

impl Domain {
    pub fn name(self) -> &'static str {
        match self {
            Domain::Record => "record",
            Domain::Cell => "cell",
        }
    }
    pub fn parse(s: &str) -> Option<Domain> {
        match s {
            "record" => Some(Domain::Record),
            "cell" => Some(Domain::Cell),
            _ => None,
        }
    }
}

fn candidate_domain(s: &str) -> Result<Domain> {
    Domain::parse(s).ok_or_else(|| dom_err(format!("invalid candidate domain {}", repr(s))))
}

/// Query node (`BoolExpr`, `FieldPredicate`, `StructPredicate`,
/// `MathPredicate`, `DirectNPredicate`, `AllPredicate`).
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Bool { op: String, children: Vec<Expr> },
    Field { name: String, op: String, value: Val, value2: Val, options: Val },
    Struct { name: String, op: String, value: Val, value2: Val },
    Math { provider: String, params: Val },
    DirectN { values: Vec<Val>, domain: String },
    All,
}

impl Expr {
    pub fn and(a: Expr, b: Expr) -> Expr {
        Expr::Bool { op: "and".into(), children: vec![a, b] }
    }
    pub fn or(a: Expr, b: Expr) -> Expr {
        Expr::Bool { op: "or".into(), children: vec![a, b] }
    }
    pub fn negate(a: Expr) -> Expr {
        Expr::Bool { op: "not".into(), children: vec![a] }
    }
    pub fn field(name: &str, op: &str, value: Val, value2: Val, options: Val) -> Expr {
        Expr::Field { name: name.into(), op: op.into(), value, value2, options }
    }
}

fn iter_items(v: &Val) -> Result<Vec<Val>> {
    Ok(match v {
        Val::List(l) => l.clone(),
        Val::Map(m) => m.iter().map(|(k, _)| Val::str(k.as_str())).collect(),
        Val::Str(s) => s.chars().map(|c| Val::Str(c.to_string())).collect(),
        Val::Bytes(b) => b.iter().map(|x| Val::Int(*x as i64)).collect(),
        other => return Err(Error::Type(format!("'{}' object is not iterable", py_type_name(other)))),
    })
}

fn dict_or_empty(v: Option<&Val>) -> Result<Val> {
    match v {
        None => Ok(Val::map()),
        Some(x) if !x.is_truthy() => Ok(Val::map()),
        Some(m @ Val::Map(_)) => Ok(m.clone()),
        Some(other) => Err(Error::Type(format!("'{}' object is not iterable", py_type_name(other)))),
    }
}

/// `expr_from_spec` (JSON specification).
pub fn expr_from_spec(raw: &Val) -> Result<Expr> {
    if !matches!(raw, Val::Map(_)) {
        return Err(plan_err("query expression must be an object"));
    }
    if matches!(raw.get("all"), Some(Val::Bool(true))) {
        return Ok(Expr::All);
    }
    for op in ["and", "or"] {
        if let Some(items) = raw.get(op) {
            let xs: Vec<Expr> = iter_items(items)?.iter().map(expr_from_spec).collect::<Result<_>>()?;
            let mut it = xs.into_iter();
            let Some(mut out) = it.next() else {
                return Err(plan_err(format!("{op} must not be empty")));
            };
            for x in it {
                out = if op == "and" { Expr::and(out, x) } else { Expr::or(out, x) };
            }
            return Ok(out);
        }
    }
    if let Some(x) = raw.get("not") {
        return Ok(Expr::negate(expr_from_spec(x)?));
    }
    let value_lo = || raw.get("value").or_else(|| raw.get("lo")).cloned().unwrap_or(Val::None);
    if let Some(f) = raw.get("field") {
        return Ok(Expr::Field {
            name: f.py_str(),
            op: raw.get("op").map(|o| o.py_str()).unwrap_or_else(|| "eq".into()),
            value: value_lo(),
            value2: raw.get("hi").cloned().unwrap_or(Val::None),
            options: dict_or_empty(raw.get("options"))?,
        });
    }
    if let Some(s) = raw.get("struct") {
        return Ok(Expr::Struct {
            name: s.py_str(),
            op: raw.get("op").map(|o| o.py_str()).unwrap_or_else(|| "eq".into()),
            value: value_lo(),
            value2: raw.get("hi").cloned().unwrap_or(Val::None),
        });
    }
    if let Some(m) = raw.get("math") {
        return Ok(Expr::Math { provider: m.py_str(), params: dict_or_empty(raw.get("params"))? });
    }
    if let Some(n) = raw.get("n") {
        let values = match n {
            Val::List(l) => l.clone(),
            other => vec![other.clone()],
        };
        let domain = raw.get("domain").map(|d| unicode::lower(&d.py_str())).unwrap_or_else(|| "cell".into());
        return Ok(Expr::DirectN { values, domain });
    }
    Err(plan_err(format!("query spec node unknown: {}", raw.py_repr())))
}

// ----------------------------------------------------------------------
// Context, candidates, providers
// ----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct QueryContext {
    pub realm: Option<BigInt>,
    pub lane: Lane,
}

impl QueryContext {
    /// Realm as filter; negative values match no realm (realm 0).
    pub fn realm_nat(&self) -> Option<Nat> {
        self.realm.as_ref().map(nat_or_zero)
    }
}

#[derive(Clone, Debug)]
pub struct CandidateSet {
    pub domain: Domain,
    pub ns: Vec<Nat>,
    pub source: String,
}

impl CandidateSet {
    fn new(domain: Domain, ns: Vec<Nat>, source: &str) -> Self {
        CandidateSet { domain, ns, source: source.into() }
    }
}

/// Runtime provider (e.g. a Python callable).
pub trait ExternalProvider: Send + Sync {
    /// Returns candidates (int/str N, mapping, tuple).
    fn call(&self, engine: &QueryEngine<'_>, ctx: &QueryContext, params: &Val) -> Result<Vec<Val>>;
    fn has_estimator(&self) -> bool;
    fn estimate(&self, engine: &QueryEngine<'_>, ctx: &QueryContext, params: &Val) -> Result<Val>;
}

#[derive(Clone)]
pub enum ProviderKind {
    Formula,
    NSequence,
    MathLayoutInputs,
    External(Arc<dyn ExternalProvider>),
}

#[derive(Clone)]
pub struct ProviderEntry {
    pub name: String,
    pub domain: Domain,
    pub description: String,
    pub kind: ProviderKind,
}

/// Provider registry of an engine (built-in: formula, n_sequence,
/// math_layout_inputs).
#[derive(Clone)]
pub struct Providers {
    pub map: HashMap<String, ProviderEntry>,
    pub order: Vec<String>,
}

impl Default for Providers {
    fn default() -> Self {
        let mut p = Providers { map: HashMap::new(), order: Vec::new() };
        p.insert(
            "formula",
            Domain::Cell,
            "Generate structural cells from deterministic u/V formulas over one integer parameter.",
            ProviderKind::Formula,
        );
        p.insert("n_sequence", Domain::Cell, "Generate canonical N candidates from an arithmetic sequence.", ProviderKind::NSequence);
        p.insert(
            "math_layout_inputs",
            Domain::Cell,
            "Resolve explicit input tuples through a persisted MATH_MAP layout.",
            ProviderKind::MathLayoutInputs,
        );
        p
    }
}

impl Providers {
    fn insert(&mut self, name: &str, domain: Domain, description: &str, kind: ProviderKind) {
        if !self.map.contains_key(name) {
            self.order.push(name.to_string());
        }
        self.map.insert(name.to_string(), ProviderEntry { name: name.into(), domain, description: description.into(), kind });
    }

    /// `register_math_provider`
    pub fn register(&mut self, name: &str, domain: &str, description: &str, provider: Arc<dyn ExternalProvider>) -> Result<()> {
        let key = unicode::strip(name).to_string();
        let d = unicode::lower(domain);
        if key.is_empty() {
            return Err(Error::Value("provider name must not be empty".into()));
        }
        let dom = Domain::parse(&d).ok_or_else(|| dom_err(format!("provider domain {} invalid", repr(&d))))?;
        self.insert(&key, dom, description, ProviderKind::External(provider));
        Ok(())
    }
}

/// Execution options of `execute`.
#[derive(Clone, Debug)]
pub struct ExecOptions {
    pub realm: Option<BigInt>,
    pub lane: Lane,
    pub return_mode: String,
    pub select: Option<Val>,
    pub aggregate: Option<Val>,
    pub group_by: Option<Val>,
    pub order_by: Option<Val>,
    pub limit: Val,
    pub offset: Val,
    pub explain: bool,
    pub k1: f64,
    pub b: f64,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            realm: None,
            lane: 0,
            return_mode: "record".into(),
            select: None,
            aggregate: None,
            group_by: None,
            order_by: None,
            limit: Val::None,
            offset: Val::Int(0),
            explain: false,
            k1: 1.2,
            b: 0.75,
        }
    }
}

#[derive(Clone, Debug)]
struct AggSpec {
    alias: String,
    op: String,
    field: String,
}

impl AggSpec {
    fn to_val(&self) -> Val {
        vmap! {"alias" => self.alias.as_str(), "op" => self.op.as_str(), "field" => self.field.as_str()}
    }
}

#[derive(Clone, Debug)]
enum Acc {
    Int(BigInt),
    Float(f64),
}

impl Acc {
    fn add(&mut self, v: &Val) {
        let n = match v {
            Val::Float(f) => Acc::Float(*f),
            other => Acc::Int(py_int(other).unwrap_or_default()),
        };
        *self = match (std::mem::replace(self, Acc::Int(BigInt::zero())), n) {
            (Acc::Int(a), Acc::Int(b)) => Acc::Int(a + b),
            (Acc::Int(a), Acc::Float(b)) => Acc::Float(a.to_f64().unwrap_or(f64::INFINITY) + b),
            (Acc::Float(a), Acc::Int(b)) => Acc::Float(a + b.to_f64().unwrap_or(f64::INFINITY)),
            (Acc::Float(a), Acc::Float(b)) => Acc::Float(a + b),
        };
    }
    fn to_val(&self) -> Val {
        match self {
            Acc::Int(i) => Val::from_bigint(i.clone()),
            Acc::Float(f) => Val::Float(*f),
        }
    }
}

#[derive(Clone, Debug)]
enum AggState {
    Count(BigInt),
    Sum(Acc),
    Ext(Val),
    Avg(Acc, BigInt),
}

fn agg_init(op: &str) -> AggState {
    match op {
        "count" => AggState::Count(BigInt::zero()),
        "sum" => AggState::Sum(Acc::Int(BigInt::zero())),
        "avg" => AggState::Avg(Acc::Int(BigInt::zero()), BigInt::zero()),
        _ => AggState::Ext(Val::None),
    }
}

fn is_numeric(v: &Val) -> bool {
    matches!(v, Val::Bool(_) | Val::Int(_) | Val::Big(_) | Val::Float(_))
}

fn group_key(vals: &[Val]) -> Result<Vec<ValKey>> {
    vals.iter()
        .map(|v| match v {
            Val::List(_) | Val::Map(_) => Err(Error::Type(format!("unhashable type: '{}'", py_type_name(v)))),
            other => Ok(ValKey(other.clone())),
        })
        .collect()
}

/// Grouped aggregation in insertion order (`_aggregate_records`).
struct Aggregator<'s> {
    specs: &'s [AggSpec],
    groups: &'s [String],
    index: HashMap<Vec<ValKey>, usize>,
    states: Vec<(Vec<Val>, Vec<AggState>)>,
}

impl<'s> Aggregator<'s> {
    fn new(specs: &'s [AggSpec], groups: &'s [String]) -> Self {
        Aggregator { specs, groups, index: HashMap::new(), states: Vec::new() }
    }

    fn add(&mut self, get: &dyn Fn(&str) -> Val) -> Result<()> {
        let key: Vec<Val> = self.groups.iter().map(|g| get(g)).collect();
        let hk = group_key(&key)?;
        let slot = match self.index.get(&hk) {
            Some(&i) => i,
            None => {
                self.states.push((key, self.specs.iter().map(|s| agg_init(&s.op)).collect()));
                self.index.insert(hk, self.states.len() - 1);
                self.states.len() - 1
            }
        };
        for (i, spec) in self.specs.iter().enumerate() {
            let value = if spec.field == "*" { Val::None } else { get(&spec.field) };
            let st = &mut self.states[slot].1[i];
            match st {
                AggState::Count(c) => {
                    if spec.field == "*" || !value.is_none() {
                        *c += 1;
                    }
                }
                AggState::Sum(acc) => {
                    if value.is_none() {
                        continue;
                    }
                    if !is_numeric(&value) {
                        return Err(plan_err(format!("SUM({}) requires numeric values", spec.field)));
                    }
                    acc.add(&value);
                }
                AggState::Avg(acc, cnt) => {
                    if value.is_none() {
                        continue;
                    }
                    if !is_numeric(&value) {
                        return Err(plan_err(format!("AVG({}) requires numeric values", spec.field)));
                    }
                    acc.add(&value);
                    *cnt += 1;
                }
                AggState::Ext(cur) => {
                    if value.is_none() {
                        continue;
                    }
                    let take = if cur.is_none() {
                        true
                    } else if spec.op == "min" {
                        py_lt(&value, cur, "<")?
                    } else {
                        py_lt(cur, &value, ">")?
                    };
                    if take {
                        *cur = value;
                    }
                }
            }
        }
        Ok(())
    }

    fn rows(mut self) -> Vec<Val> {
        if self.states.is_empty() && self.groups.is_empty() {
            self.states.push((Vec::new(), self.specs.iter().map(|s| agg_init(&s.op)).collect()));
        }
        let mut out = Vec::with_capacity(self.states.len());
        for (key, st) in self.states {
            let mut row = Val::map();
            for (i, g) in self.groups.iter().enumerate() {
                row.set(g.clone(), key[i].clone());
            }
            for (spec, s) in self.specs.iter().zip(st.iter()) {
                let v = match s {
                    AggState::Count(c) => Val::from_bigint(c.clone()),
                    AggState::Sum(acc) => acc.to_val(),
                    AggState::Ext(v) => v.clone(),
                    AggState::Avg(acc, cnt) => {
                        if cnt.is_zero() {
                            Val::None
                        } else {
                            match acc {
                                Acc::Int(t) => Val::Float(int_true_div(t, cnt)),
                                Acc::Float(f) => Val::Float(f / cnt.to_f64().unwrap_or(f64::INFINITY)),
                            }
                        }
                    }
                };
                row.set(spec.alias.clone(), v);
            }
            out.push(row);
        }
        out
    }
}

fn normalize_aggregates(aggregate: Option<&Val>) -> Result<Vec<AggSpec>> {
    let Some(Val::Map(m)) = aggregate else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (alias, raw) in m {
        let (op, field) = match raw {
            Val::Map(_) => (
                unicode::lower(&raw.get("op").map(|o| o.py_str()).unwrap_or_default()),
                raw.get("field").map(|f| f.py_str()).unwrap_or_else(|| "*".into()),
            ),
            Val::List(l) if l.len() == 2 => (unicode::lower(&l[0].py_str()), l[1].py_str()),
            other => (unicode::lower(alias), other.py_str()),
        };
        if !matches!(op.as_str(), "count" | "sum" | "min" | "max" | "avg") {
            return Err(plan_err(format!("aggregate {} unknown", repr(&op))));
        }
        if op != "count" && field == "*" {
            return Err(plan_err(format!("{} requires a field", op.to_uppercase())));
        }
        out.push(AggSpec { alias: alias.clone(), op, field });
    }
    Ok(out)
}

fn normalize_order_by(order_by: Option<&Val>) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    let Some(ob) = order_by else {
        return Ok(out);
    };
    if ob.is_none() {
        return Ok(out);
    }
    for raw in iter_items(ob)? {
        match &raw {
            Val::Str(s) => {
                let desc = s.starts_with('-');
                out.push((if desc { s[1..].to_string() } else { s.clone() }, desc));
            }
            Val::Map(_) => {
                let name = raw.get("field").map(|f| f.py_str()).unwrap_or_else(|| "None".into());
                let dir = raw.get("direction").map(|d| d.py_str()).unwrap_or_else(|| "asc".into());
                out.push((name, unicode::lower(&dir) == "desc"));
            }
            Val::List(l) if l.len() == 2 => out.push((l[0].py_str(), unicode::lower(&l[1].py_str()) == "desc")),
            other => return Err(plan_err(format!("ORDER BY entry invalid: {}", other.py_repr()))),
        }
    }
    Ok(out)
}

fn has_order(order_by: Option<&Val>) -> bool {
    match order_by {
        None | Some(Val::None) => false,
        Some(v) => v.is_truthy(),
    }
}

/// `_slice_simple`
fn slice_simple<T: Clone>(values: Vec<T>, limit: &Val, offset: &Val) -> Result<Vec<T>> {
    let off = if offset.is_truthy() { py_int(offset)? } else { BigInt::zero() };
    let off = off.max(BigInt::zero()).to_usize().unwrap_or(usize::MAX);
    let mut it: Vec<T> = values.into_iter().skip(off).collect();
    if !limit.is_none() {
        let lim = py_int(limit)?.max(BigInt::zero()).to_usize().unwrap_or(usize::MAX);
        it.truncate(lim);
    }
    Ok(it)
}

/// `_record_value`
fn record_value(rec: &Val, name: &str) -> Val {
    if let Some(Val::Map(data)) = rec.get("data") {
        if let Some((_, v)) = data.iter().find(|(k, _)| k == name) {
            return v.clone();
        }
    }
    let anchor = |k: &str| rec.get("anchor_address").and_then(|a| a.get(k)).cloned().unwrap_or(Val::None);
    match name {
        "u" => anchor("u"),
        "v" => anchor("v"),
        "record_N" | "realm" | "lane" | "lane_name" | "layout_id" | "layout_name" | "layout_kind" | "object" => {
            rec.get(name).cloned().unwrap_or(Val::None)
        }
        _ => Val::None,
    }
}

/// `_apply_order_limit`
fn apply_order_limit(rows: Vec<Val>, order_by: Option<&Val>, limit: &Val, offset: &Val) -> Result<Vec<Val>> {
    let mut out = rows;
    let ordering = normalize_order_by(order_by)?;
    for (name, desc) in ordering.iter().rev() {
        let keyed: Vec<(Val, Val)> = out
            .into_iter()
            .map(|row| {
                let k = if matches!(row.get("result"), Some(Val::Str(s)) if s == "record") {
                    record_value(&row, name)
                } else {
                    row.get(name).cloned().unwrap_or(Val::None)
                };
                (k, row)
            })
            .collect();
        let mut lt = |a: &(Val, Val), b: &(Val, Val)| -> Result<bool> {
            let (an, bn) = (a.0.is_none(), b.0.is_none());
            if an != bn {
                return Ok(!an && bn);
            }
            if a.0 == b.0 {
                return Ok(false);
            }
            py_lt(&a.0, &b.0, "<")
        };
        out = match py_sort(keyed, *desc, &mut lt) {
            Ok(v) => v.into_iter().map(|(_, r)| r).collect(),
            Err(Error::Type(_)) => return Err(plan_err(format!("ORDER BY {} compares incompatible values", repr(name)))),
            Err(e) => return Err(e),
        };
    }
    slice_simple(out, limit, offset)
}

fn flatten_bool<'e>(expr: &'e Expr, op: &str, out: &mut Vec<&'e Expr>) {
    match expr {
        Expr::Bool { op: o, children } if o == op => {
            for c in children {
                flatten_bool(c, op, out);
            }
        }
        other => out.push(other),
    }
}

fn est_key(v: Option<&Val>) -> Option<Num> {
    match v {
        None | Some(Val::None) => None,
        Some(x) => x.as_number(),
    }
}

fn cmp_est(a: &Option<Num>, b: &Option<Num>) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (None, None) => Equal,
        (None, Some(_)) => Greater,
        (Some(_), None) => Less,
        (Some(x), Some(y)) => x.partial_cmp(y).unwrap_or(Equal),
    }
}

fn cost_of(v: Option<&Val>) -> f64 {
    match v {
        Some(Val::Float(f)) => *f,
        Some(x) if x.is_truthy() => py_float(x).unwrap_or(0.0),
        _ => 0.0,
    }
}

// ----------------------------------------------------------------------
// QueryEngine
// ----------------------------------------------------------------------

/// Execution context of a query over an open database.
pub struct QueryEngine<'a> {
    pub db: &'a Db,
    pub providers: &'a Providers,
    /// Host object of the binding layer (e.g. the Python QueryEngine) that is
    /// passed through to runtime providers.
    pub host: Option<&'a (dyn Any + Send + Sync)>,
}

impl<'a> QueryEngine<'a> {
    pub fn new(db: &'a Db, providers: &'a Providers) -> Self {
        QueryEngine { db, providers, host: None }
    }

    fn max_bits(&self) -> u64 {
        self.db.config.math_max_intermediate_bits
    }

    fn plain_classic_layout(&self, ctx: &QueryContext) -> Option<&'a LayoutDef> {
        let rn = ctx.realm_nat()?;
        let ids = self.db.layouts_by_realm.get(&rn)?;
        let lays: Vec<&LayoutDef> = ids.iter().filter_map(|i| self.db.layouts.get(i)).collect();
        if lays.len() != 1 || lays[0].kind != LayoutKind::Classic {
            return None;
        }
        Some(lays[0])
    }

    /// `_find_usable_index`
    fn find_usable_index(&self, field: &str, ctx: &QueryContext, op: &str, options: &Val, value: &Val) -> Option<&'a IndexDef> {
        let realm = ctx.realm_nat()?;
        let v = self.db.resolve_v(&realm, &Val::str(field)).ok()?;
        for idx in self.db.index_defs.values() {
            if idx.realm_id != realm || idx.field_v() != &v {
                continue;
            }
            match textindex::index_supports_predicate(idx.kind, op, &idx.options, options, value) {
                Ok(true) => {}
                _ => continue,
            }
            if self.db.index_is_fresh(idx.index_id, ctx.lane) {
                return Some(idx);
            }
        }
        None
    }

    fn est_rows(&self, idx: &IndexDef, op: &str, value: &Val, value2: &Val, options: &Val, lane: Lane) -> Result<Option<BigInt>> {
        let info = self.db.index_candidate_estimate(idx.index_id, op, value, value2, lane, options)?;
        Ok(match info.get("estimated_rows") {
            None | Some(Val::None) => None,
            Some(v) => Some(py_int(v)?),
        })
    }

    /// `_fast_classic_index_us`: indexed FIELD/AND expressions in CLASSIC u space.
    fn fast_classic_index_us(&self, expr: &Expr, ctx: &QueryContext) -> Result<Option<Vec<Nat>>> {
        if self.plain_classic_layout(ctx).is_none() {
            return Ok(None);
        }
        let mut leaves = Vec::new();
        match expr {
            Expr::Bool { op, .. } if op == "and" => flatten_bool(expr, "and", &mut leaves),
            other => leaves.push(other),
        }
        let mut ranked: Vec<(Option<BigInt>, usize, &Expr, &IndexDef)> = Vec::new();
        for (pos, leaf) in leaves.iter().enumerate() {
            let Expr::Field { name, op, value, value2, options } = leaf else {
                return Ok(None);
            };
            let Some(idx) = self.find_usable_index(name, ctx, op, options, value) else {
                return Ok(None);
            };
            let est = self.est_rows(idx, op, value, value2, options, ctx.lane)?;
            ranked.push((est, pos, leaf, idx));
        }
        if ranked.is_empty() {
            return Ok(None);
        }
        ranked.sort_by(|a, b| match (&a.0, &b.0) {
            (None, None) => a.1.cmp(&b.1),
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(x), Some(y)) => x.cmp(y).then(a.1.cmp(&b.1)),
        });
        let realm = ctx.realm_nat().unwrap();
        let mut acc: Option<Vec<Nat>> = None;
        for (_e, _p, leaf, idx) in ranked {
            let Expr::Field { name, op, value, value2, options } = leaf else { unreachable!() };
            let mut us = self.db.index_candidates(idx.index_id, op, value, value2, ctx.lane, options)?;
            if idx.kind == IndexKind::Trigram && (op == "contains" || op == "icontains") {
                us = self.verify_text(&realm, us, name, op, value, options, ctx.lane)?;
            }
            acc = Some(match acc {
                None => us,
                Some(a) => setops::intersect_sorted(&a, &us),
            });
            if acc.as_ref().is_some_and(|a| a.is_empty()) {
                break;
            }
        }
        Ok(Some(acc.unwrap_or_default()))
    }

    #[allow(clippy::too_many_arguments)]
    fn verify_text(&self, realm: &Nat, us: Vec<Nat>, name: &str, op: &str, value: &Val, options: &Val, lane: Lane) -> Result<Vec<Nat>> {
        let mut verified = Vec::new();
        for raw in self.db.select_us_ext(realm, &us, &[Val::str(name)], lane, true)? {
            let actual = raw.get(name).cloned().unwrap_or(Val::None);
            if textindex::predicate_matches(&actual, op, value, options)? {
                if let Some(u) = raw.get("_u").and_then(|x| x.as_nat()) {
                    verified.push(u);
                }
            }
        }
        Ok(verified)
    }

    fn record_anchor_ref(&self, lay: &LayoutDef, u: &Nat, lane: Lane) -> Result<String> {
        Ok(self.db.logical_address_in(lay, u, &Val::None, lane)?.n_ref())
    }

    // ------------------------- public interface -------------------------

    /// `explain`
    pub fn explain(&self, expr: &Expr, realm: Option<BigInt>, lane: Lane, return_mode: &str) -> Result<Val> {
        let domain = domain_for_return(&unicode::lower(return_mode), return_mode)?;
        let ctx = QueryContext { realm, lane };
        Ok(vmap! {
            "query_version" => QUERY_VERSION, "optimizer" => OPTIMIZER, "target_domain" => domain.name(),
            "context" => vmap! {"realm" => ctx.realm.clone().map(Val::from_bigint).unwrap_or(Val::None), "lane" => lane as i64},
            "plan" => self.plan_expr(expr, &ctx, domain)?,
        })
    }

    fn base_result(&self, domain: Domain, ret: &str, count: usize) -> Val {
        vmap! {
            "query_version" => QUERY_VERSION, "optimizer" => OPTIMIZER, "domain" => domain.name(),
            "return" => ret, "count" => count as i64,
        }
    }

    /// `execute`
    pub fn execute(&self, expr: &Expr, o: &ExecOptions) -> Result<Val> {
        let rm0 = unicode::lower(&o.return_mode);
        if rm0 == "ranked" {
            return self.search(expr, o.realm.clone(), o.lane, o.select.as_ref(), &o.limit, &o.offset, o.explain, o.k1, o.b);
        }
        let agg_mode = o.aggregate.as_ref().is_some_and(|a| a.is_truthy()) || o.group_by.as_ref().is_some_and(|g| g.is_truthy()) || rm0 == "aggregate";
        let rm = if agg_mode { "aggregate".to_string() } else { rm0 };
        let domain = domain_for_return(&rm, &rm)?;
        let ctx = QueryContext { realm: o.realm.clone(), lane: o.lane };
        let plan = self.plan_expr(expr, &ctx, domain)?;

        // POSTING_COUNT: exact posting metadata answers COUNT directly.
        if rm == "count" {
            if let Expr::Field { name, op, value, value2, options } = expr {
                if let Some(idx) = self.find_usable_index(name, &ctx, op, options, value) {
                    let est = self.db.index_candidate_estimate(idx.index_id, op, value, value2, ctx.lane, options)?;
                    if est.get("exact").is_some_and(|e| e.is_truthy()) {
                        let count = match est.get("estimated_rows") {
                            Some(v) if v.is_truthy() => py_int(v)?,
                            _ => BigInt::zero(),
                        };
                        let mut r = vmap! {
                            "query_version" => QUERY_VERSION, "optimizer" => OPTIMIZER, "domain" => "record",
                            "return" => "count", "count" => Val::from_bigint(count), "returned" => 1i64,
                            "fastpath" => "POSTING_COUNT",
                        };
                        if o.explain {
                            r.set("plan", plan);
                        }
                        return Ok(r);
                    }
                }
            }
        }

        // CLASSIC_U: indexed predicates stay in compact u space.
        let fast = if domain == Domain::Record { self.fast_classic_index_us(expr, &ctx)? } else { None };
        if let Some(us) = fast {
            let lay = self.plain_classic_layout(&ctx).unwrap();
            let realm = ctx.realm_nat().unwrap();
            let mut r = self.base_result(Domain::Record, &rm, us.len());
            r.set("fastpath", Val::str("CLASSIC_U"));
            let ordered = has_order(o.order_by.as_ref());
            match rm.as_str() {
                "count" => r.set("returned", Val::Int(1)),
                "aggregate" => {
                    let specs = normalize_aggregates(o.aggregate.as_ref())?;
                    let groups = o.group_by.as_ref().map(str_list).unwrap_or_default();
                    if specs.is_empty() {
                        return Err(plan_err("aggregation requires at least one aggregate definition"));
                    }
                    let mut needed: Vec<String> = Vec::new();
                    for n in groups.iter().cloned().chain(specs.iter().filter(|s| s.field != "*").map(|s| s.field.clone())) {
                        if !needed.contains(&n) {
                            needed.push(n);
                        }
                    }
                    let cols: Vec<Val> = needed.iter().map(|n| Val::str(n.as_str())).collect();
                    let mut agg = Aggregator::new(&specs, &groups);
                    for raw in self.db.select_us_ext(&realm, &us, &cols, ctx.lane, true)? {
                        agg.add(&|k: &str| raw.get(k).cloned().unwrap_or(Val::None))?;
                    }
                    let rows = apply_order_limit(agg.rows(), o.order_by.as_ref(), &o.limit, &o.offset)?;
                    r.set("aggregate", Val::List(specs.iter().map(|s| s.to_val()).collect()));
                    r.set("group_by", Val::List(groups.iter().map(|g| Val::str(g.as_str())).collect()));
                    r.set("returned", Val::Int(rows.len() as i64));
                    r.set("rows", Val::List(rows));
                    // Key order: rows before returned
                    move_last(&mut r, "returned");
                }
                "fields" => {
                    let fields = o.select.as_ref().map(str_list).unwrap_or_default();
                    if fields.is_empty() {
                        return Err(plan_err("return_mode='fields' requires at least one select field"));
                    }
                    let cols: Vec<Val> = fields.iter().map(|n| Val::str(n.as_str())).collect();
                    let rows = if ordered {
                        let rows = self.db.select_us_ext(&realm, &us, &cols, ctx.lane, true)?;
                        apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?
                    } else {
                        let sl = slice_simple(us.clone(), &o.limit, &o.offset)?;
                        self.db.select_us_ext(&realm, &sl, &cols, ctx.lane, true)?
                    };
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        let u = row.get("_u").and_then(|x| x.as_nat()).unwrap_or(Nat::ZERO);
                        let mut o2 = vmap! {"record_N" => self.record_anchor_ref(lay, &u, ctx.lane)?};
                        for f in &fields {
                            o2.set(f.clone(), row.get(f).cloned().unwrap_or(Val::None));
                        }
                        out.push(o2);
                    }
                    r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                    r.set("rows", Val::List(out));
                    let n = r.get("rows").and_then(|x| x.as_list()).map(|l| l.len()).unwrap_or(0);
                    r.set("returned", Val::Int(n as i64));
                }
                "record" => {
                    let lr = LayoutRef::Id(lay.layout_id);
                    let rows = if ordered {
                        let rows = us.iter().map(|u| self.db.get_record_view(&lr, u, ctx.lane, None)).collect::<Result<Vec<_>>>()?;
                        apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?
                    } else {
                        slice_simple(us.clone(), &o.limit, &o.offset)?
                            .iter()
                            .map(|u| self.db.get_record_view(&lr, u, ctx.lane, None))
                            .collect::<Result<Vec<_>>>()?
                    };
                    r.set("returned", Val::Int(rows.len() as i64));
                    r.set("rows", Val::List(rows));
                    move_last(&mut r, "returned");
                }
                "n" => {
                    let sl = slice_simple(us.clone(), &o.limit, &o.offset)?;
                    let refs = sl.iter().map(|u| self.record_anchor_ref(lay, u, ctx.lane).map(Val::Str)).collect::<Result<Vec<_>>>()?;
                    r.set("returned", Val::Int(refs.len() as i64));
                    r.set("refs", Val::List(refs));
                    move_last(&mut r, "returned");
                }
                _ => return Err(plan_err(format!("unknown return_mode {}", repr(&rm)))),
            }
            if o.explain {
                r.set("plan", plan);
            }
            return Ok(r);
        }

        let c = self.eval(expr, &ctx, domain)?;
        let mut r = self.base_result(c.domain, &rm, c.ns.len());
        let ordered = has_order(o.order_by.as_ref());
        match rm.as_str() {
            "aggregate" => {
                let specs = normalize_aggregates(o.aggregate.as_ref())?;
                let groups = o.group_by.as_ref().map(str_list).unwrap_or_default();
                if specs.is_empty() {
                    return Err(plan_err("aggregation requires at least one aggregate definition"));
                }
                let mut agg = Aggregator::new(&specs, &groups);
                for n in &c.ns {
                    let rec = self.record_envelope_light(n)?;
                    agg.add(&|k: &str| record_value(&rec, k))?;
                }
                let rows = apply_order_limit(agg.rows(), o.order_by.as_ref(), &o.limit, &o.offset)?;
                r.set("aggregate", Val::List(specs.iter().map(|s| s.to_val()).collect()));
                r.set("group_by", Val::List(groups.iter().map(|g| Val::str(g.as_str())).collect()));
                r.set("rows", Val::List(rows.clone()));
                r.set("returned", Val::Int(rows.len() as i64));
            }
            "count" => r.set("returned", Val::Int(1)),
            "n" => {
                let refs: Vec<Val> = slice_simple(c.ns.clone(), &o.limit, &o.offset)?.iter().map(|n| Val::Str(n_ref(n))).collect();
                r.set("refs", Val::List(refs.clone()));
                r.set("returned", Val::Int(refs.len() as i64));
            }
            "cell" | "record" => {
                let mode = if rm == "cell" { GetMode::Cell } else { GetMode::Record };
                let rows = if ordered {
                    let rows = c.ns.iter().map(|n| self.db.get_n(n, mode)).collect::<Result<Vec<_>>>()?;
                    apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?
                } else {
                    self.sliced_rows(&c.ns, &o.limit, &o.offset, mode, &|n| self.db.get_n(n, mode))?
                };
                r.set("rows", Val::List(rows.clone()));
                r.set("returned", Val::Int(rows.len() as i64));
            }
            "fields" => {
                let fields = o.select.as_ref().map(str_list).unwrap_or_default();
                if fields.is_empty() {
                    return Err(plan_err("return_mode='fields' requires at least one select field"));
                }
                let build = |n: &Nat| -> Result<Val> {
                    let rec = self.record_envelope_light(n)?;
                    let mut row = vmap! {"record_N" => rec.get("record_N").cloned().unwrap_or(Val::None)};
                    for f in &fields {
                        row.set(f.clone(), record_value(&rec, f));
                    }
                    Ok(row)
                };
                let rows = if ordered {
                    let rows = c.ns.iter().map(build).collect::<Result<Vec<_>>>()?;
                    apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?
                } else {
                    self.sliced_rows(&c.ns, &o.limit, &o.offset, GetMode::Record, &build)?
                };
                r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                r.set("rows", Val::List(rows.clone()));
                r.set("returned", Val::Int(rows.len() as i64));
            }
            _ => return Err(plan_err(format!("unknown return_mode {}", repr(&rm)))),
        }
        if o.explain {
            r.set("plan", plan);
        }
        Ok(r)
    }

    /// Build rows only for the LIMIT/OFFSET window. Candidates outside the
    /// window are pre-checked in the original order, so that exactly the
    /// same first error arises as with a full build.
    fn sliced_rows(&self, ns: &[Nat], limit: &Val, offset: &Val, mode: GetMode, build: &dyn Fn(&Nat) -> Result<Val>) -> Result<Vec<Val>> {
        let idx: Vec<usize> = slice_simple((0..ns.len()).collect(), limit, offset)?;
        let (lo, hi) = match (idx.first(), idx.last()) {
            (Some(a), Some(b)) => (*a, *b + 1),
            _ => (usize::MAX, usize::MAX),
        };
        let mut out = Vec::with_capacity(idx.len());
        for (i, n) in ns.iter().enumerate() {
            if i >= lo && i < hi {
                out.push(build(n)?);
            } else {
                self.row_precheck(n, mode)?;
            }
        }
        Ok(out)
    }

    /// Error check of `get_n(n, mode)` without reading payload data.
    fn row_precheck(&self, n: &Nat, mode: GetMode) -> Result<()> {
        let a = n_to_address(n)?;
        if !self.db.realms.contains_key(a.realm()) {
            return Ok(());
        }
        let Some(lay) = self.db.layout_at(a.realm(), a.u()) else {
            return if mode == GetMode::Record {
                Err(Error::Schema("address does not belong to a resolvable logical record".into()))
            } else {
                Ok(())
            };
        };
        if lay.kind == LayoutKind::Custom {
            self.db.get_n(n, mode)?;
            return Ok(());
        }
        let interp = self.db.interpret_layout_address(lay, &a)?;
        if mode == GetMode::Record {
            let Some(rec) = Db::interp_record(&interp) else {
                return Err(Error::Schema("address does not belong to a resolvable logical record".into()));
            };
            self.db.logical_address_in(lay, &rec, &Val::None, a.lane)?;
        }
        Ok(())
    }

    /// `execute_spec`
    pub fn execute_spec(&self, spec: &Val) -> Result<Val> {
        let expr = match spec.get("where") {
            None => Expr::All,
            Some(w) => expr_from_spec(w)?,
        };
        self.execute(&expr, &exec_options_from_spec(spec)?)
    }

    fn extract_fulltext_conjunction<'e>(&self, expr: &'e Expr) -> Result<(&'e Expr, Option<Expr>)> {
        let mut leaves = Vec::new();
        match expr {
            Expr::Bool { op, .. } if op == "and" => flatten_bool(expr, "and", &mut leaves),
            other => leaves.push(other),
        }
        let ft: Vec<usize> = leaves
            .iter()
            .enumerate()
            .filter(|(_, x)| matches!(x, Expr::Field { op, .. } if op == "fulltext" || op == "phrase"))
            .map(|(i, _)| i)
            .collect();
        if ft.len() != 1 {
            return Err(plan_err("ranked FULLTEXT search requires exactly one fulltext/phrase predicate"));
        }
        let pred = leaves[ft[0]];
        let mut others = leaves.iter().enumerate().filter(|(i, _)| *i != ft[0]).map(|(_, x)| (*x).clone());
        let Some(mut out) = others.next() else {
            return Ok((pred, None));
        };
        for x in others {
            out = Expr::and(out, x);
        }
        Ok((pred, Some(out)))
    }

    /// `search`: BM25 over exactly one FULLTEXT/PHRASE predicate plus AND filters.
    #[allow(clippy::too_many_arguments)]
    pub fn search(&self, expr: &Expr, realm: Option<BigInt>, lane: Lane, select: Option<&Val>, limit: &Val, offset: &Val, explain: bool, k1: f64, b: f64) -> Result<Val> {
        let ctx = QueryContext { realm, lane };
        if ctx.realm.is_none() {
            return Err(plan_err("FULLTEXT search requires a realm"));
        }
        let Some(lay) = self.plain_classic_layout(&ctx) else {
            return Err(plan_err("ranked FULLTEXT search is implemented for CLASSIC record u only"));
        };
        let (pred, filter_expr) = self.extract_fulltext_conjunction(expr)?;
        let Expr::Field { name, op, value, options, .. } = pred else { unreachable!() };
        let idx = match self.find_usable_index(name, &ctx, op, options, value) {
            Some(i) if i.kind == IndexKind::Fulltext => i,
            _ => return Err(plan_err(format!("no fresh FULLTEXT index for field {}", repr(name)))),
        };
        let sequence = textindex::query_fulltext_sequence(value, &idx.options)?;
        let mut terms: Vec<String> = Vec::new();
        for t in &sequence {
            if !terms.contains(t) {
                terms.push(t.clone());
            }
        }
        let (postings, corpus) = self.db.fulltext_payloads(idx.index_id, &terms, ctx.lane)?;
        let tp: Vec<(String, &[textindex::FtPosting])> = postings.iter().map(|(t, p)| (t.clone(), p.as_slice())).collect();
        let candidates = if op == "phrase" {
            textindex::phrase_candidate_docs(&sequence, &tp)
        } else {
            let mode = unicode::lower(&options.get("mode").map(|m| m.py_str()).unwrap_or_else(|| "all".into()));
            textindex::fulltext_candidate_docs(&tp, &mode)?
        };
        let mut scores = textindex::bm25_scores(&tp, &corpus, Some(&candidates), k1, b)?;
        let realm_n = ctx.realm_nat().unwrap();
        if let Some(f) = &filter_expr {
            if !scores.is_empty() {
                let filter_us = match self.fast_classic_index_us(f, &ctx)? {
                    Some(us) => us,
                    None => {
                        let cset = self.eval(f, &ctx, Domain::Record)?;
                        let mut us = Vec::new();
                        for n in &cset.ns {
                            let Ok(a) = n_to_address(n) else { continue };
                            if a.realm() == &realm_n && a.lane == ctx.lane {
                                us.push(a.u().clone());
                            }
                        }
                        setops::sorted_unique(us)
                    }
                };
                let allowed: HashSet<Nat> = filter_us.into_iter().collect();
                scores.retain(|(u, _)| allowed.contains(u));
            }
        }
        scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let total = scores.len();
        let sliced = slice_simple(scores, limit, offset)?;
        let fields: Vec<String> = select.map(str_list).unwrap_or_default().into_iter().filter(|x| x != "relevance").collect();
        let mut data_by_u: HashMap<Nat, Val> = HashMap::new();
        if !fields.is_empty() && !sliced.is_empty() {
            let mut wanted: Vec<Nat> = sliced.iter().map(|(u, _)| u.clone()).collect();
            wanted.sort();
            let cols: Vec<Val> = fields.iter().map(|f| Val::str(f.as_str())).collect();
            for row in self.db.select_us_ext(&realm_n, &wanted, &cols, ctx.lane, true)? {
                if let Some(u) = row.get("_u").and_then(|x| x.as_nat()) {
                    data_by_u.insert(u, row);
                }
            }
        }
        let mut rows = Vec::with_capacity(sliced.len());
        for (u, score) in &sliced {
            let mut row = vmap! {"record_N" => self.record_anchor_ref(lay, u, ctx.lane)?, "relevance" => *score};
            let raw = data_by_u.get(u);
            for f in &fields {
                row.set(f.clone(), raw.and_then(|r| r.get(f)).cloned().unwrap_or(Val::None));
            }
            rows.push(row);
        }
        let mut sel: Vec<Val> = fields.iter().map(|f| Val::str(f.as_str())).collect();
        sel.push(Val::str("relevance"));
        let mut r = vmap! {
            "query_version" => QUERY_VERSION, "optimizer" => OPTIMIZER, "domain" => "record", "return" => "ranked",
            "count" => total as i64, "returned" => rows.len() as i64, "fastpath" => "FULLTEXT_BM25",
            "index_id" => idx.index_id as i64, "index_name" => idx.name.as_str(),
            "query_terms" => Val::List(terms.iter().map(|t| Val::str(t.as_str())).collect()),
            "k1" => k1, "b" => b, "select" => Val::List(sel), "rows" => Val::List(rows),
        };
        if explain {
            r.set("plan", self.plan_expr(expr, &ctx, Domain::Record)?);
        }
        Ok(r)
    }

    /// `_record_envelope_light`: cheap projection for a known CLASSIC anchor.
    fn record_envelope_light(&self, n: &Nat) -> Result<Val> {
        let light = (|| -> Result<Option<Val>> {
            let a = n_to_address(n)?;
            let Some(lay) = self.db.layout_at(a.realm(), a.u()) else { return Ok(None) };
            if lay.kind != LayoutKind::Classic {
                return Ok(None);
            }
            let anchor = self.db.logical_address_in(lay, a.u(), &Val::None, a.lane)?;
            if &anchor.n != n {
                return Ok(None);
            }
            let Some(raw) = self.db.get(a.realm(), a.u(), a.lane, None, None, true)? else { return Ok(None) };
            let data: Vec<(String, Val)> = raw.as_map().map(|m| m.iter().filter(|(k, _)| !k.starts_with('_')).cloned().collect()).unwrap_or_default();
            Ok(Some(vmap! {
                "result" => "record", "record_N" => anchor.n_ref(), "realm" => a.realm(), "lane" => a.lane as i64,
                "lane_name" => address::lane_name(a.lane), "layout_id" => lay.layout_id as i64,
                "layout_name" => lay.name.as_str(), "layout_kind" => lay.kind.name(), "object" => a.u(),
                "anchor_address" => vmap! {"realm" => a.realm(), "u" => a.u(), "v" => a.v_local(), "lane" => a.lane as i64},
                "data" => Val::Map(data),
            }))
        })();
        match light {
            Ok(Some(v)) => Ok(v),
            _ => self.db.get_n(n, GetMode::Record),
        }
    }

    // ------------------------- planning -------------------------

    fn estimate_universe_rows(&self, ctx: &QueryContext, domain: Domain) -> Option<BigInt> {
        let rn = ctx.realm_nat()?;
        let key = (rn.clone(), ctx.lane);
        let dense: u64 = self.db.dense.get(&key).map(|ix| ix.segs.iter().map(|s| s.count).sum()).unwrap_or(0);
        let sparse = self.db.sparse.get(&key);
        let groups: u64 = sparse.map(|ix| ix.segs.iter().map(|s| s.group_count).sum()).unwrap_or(0);
        if domain == Domain::Record {
            if !self.db.overlay_keys.contains(&key) && dense > 0 {
                return Some(BigInt::from(dense));
            }
            let max_u = self.db.max_u.get(&key).map(|n| n.to_bigint()).unwrap_or_default();
            return Some(max_u.max(BigInt::from(dense.max(groups))));
        }
        let cells: u64 = sparse.map(|ix| ix.segs.iter().map(|s| s.cell_count).sum()).unwrap_or(0);
        let width = self.db.current_base_shape(&rn).map(|b| b.fields.len()).unwrap_or(1) as u64;
        Some(BigInt::from(dense) * width + cells)
    }

    #[allow(clippy::too_many_arguments)] // mirrors the Python signature (keyword arguments)
    fn choose_field_access(&self, name: &str, op: &str, value: &Val, value2: &Val, options: &Val, ctx: &QueryContext, domain: Domain) -> (&'static str, Option<&'a IndexDef>, Val, f64) {
        let idx = self.find_usable_index(name, ctx, op, options, value);
        let universe = self.estimate_universe_rows(ctx, domain);
        let Some(idx) = idx else {
            let f = universe.as_ref().and_then(|u| u.to_f64()).unwrap_or(0.0);
            return ("MATERIALIZED_SCAN", None, universe.map(Val::from_bigint).unwrap_or(Val::None), f);
        };
        let rows = self.est_rows(idx, op, value, value2, options, ctx.lane).ok().flatten();
        let rf = rows.as_ref().and_then(|r| r.to_f64()).unwrap_or(0.0);
        let (access, extra) = match idx.kind {
            IndexKind::Value => ("VALUE_INDEX", 0.0),
            IndexKind::Token => ("TOKEN_INDEX", 0.0),
            IndexKind::Trigram => ("TRIGRAM_INDEX", rf),
            IndexKind::Fulltext => ("FULLTEXT_INDEX", 0.5 * rf),
        };
        (access, Some(idx), rows.map(Val::from_bigint).unwrap_or(Val::None), 8.0 + rf + extra)
    }

    fn provider_estimate(&self, p: &ProviderEntry, ctx: &QueryContext, params: &Val) -> Option<Val> {
        let r = match &p.kind {
            ProviderKind::Formula | ProviderKind::NSequence => (|| -> Result<Val> {
                let start = py_int(params.get("start").unwrap_or(&Val::Int(1)))?;
                let stop = py_int(params.get("stop").ok_or_else(|| Error::Key("'stop'".into()))?)?;
                let step = py_int(params.get("step").unwrap_or(&Val::Int(1)))?;
                Ok(Val::from_bigint(sequence_count(&start, &stop, &step)))
            })(),
            ProviderKind::MathLayoutInputs => match params.get("inputs") {
                None | Some(Val::None) => Ok(Val::None),
                Some(Val::List(l)) => Ok(Val::Int(l.len() as i64)),
                Some(Val::Str(s)) => Ok(Val::Int(s.chars().count() as i64)),
                Some(Val::Map(m)) => Ok(Val::Int(m.len() as i64)),
                Some(Val::Bytes(b)) => Ok(Val::Int(b.len() as i64)),
                Some(_) => Err(Error::Type("object has no len()".into())),
            },
            ProviderKind::External(e) => {
                if !e.has_estimator() {
                    return None;
                }
                e.estimate(self, ctx, params)
            }
        };
        match r {
            Ok(Val::None) | Err(_) => None,
            Ok(v) => Some(v),
        }
    }

    /// `_plan_expr`
    pub fn plan_expr(&self, expr: &Expr, ctx: &QueryContext, domain: Domain) -> Result<Val> {
        let universe = || self.estimate_universe_rows(ctx, domain);
        match expr {
            Expr::Bool { op, children } => {
                let flat: Vec<&Expr> = if op == "and" || op == "or" {
                    let mut v = Vec::new();
                    flatten_bool(expr, op, &mut v);
                    v
                } else {
                    children.iter().collect()
                };
                let plans: Vec<Val> = flat.iter().map(|x| self.plan_expr(x, ctx, domain)).collect::<Result<_>>()?;
                let rows_of = |p: &Val| p.get("estimated_rows").cloned().unwrap_or(Val::None);
                let (order, est, cost): (Vec<usize>, Val, f64) = if op == "and" {
                    let mut order: Vec<usize> = (0..plans.len()).collect();
                    order.sort_by(|&a, &b| {
                        cmp_est(&est_key(plans[a].get("estimated_rows")), &est_key(plans[b].get("estimated_rows")))
                            .then_with(|| cmp_est(&est_key(plans[a].get("estimated_cost")), &est_key(plans[b].get("estimated_cost"))))
                            .then(a.cmp(&b))
                    });
                    let mut best: Option<Val> = None;
                    for &i in &order {
                        let r = rows_of(&plans[i]);
                        if r.is_none() {
                            continue;
                        }
                        best = Some(match best {
                            None => r,
                            Some(cur) => {
                                if py_lt(&r, &cur, "<")? {
                                    r
                                } else {
                                    cur
                                }
                            }
                        });
                    }
                    let cost = order.iter().map(|&i| cost_of(plans[i].get("estimated_cost"))).sum();
                    (order, best.unwrap_or(Val::None), cost)
                } else if op == "or" {
                    let order: Vec<usize> = (0..plans.len()).collect();
                    let known: Vec<Val> = plans.iter().map(rows_of).collect();
                    let est = if known.iter().any(|x| x.is_none()) {
                        Val::None
                    } else {
                        let mut s = BigInt::zero();
                        for k in &known {
                            s += py_int(k)?;
                        }
                        if let Some(u) = universe() {
                            s = s.min(u);
                        }
                        Val::from_bigint(s)
                    };
                    let cost = plans.iter().map(|p| cost_of(p.get("estimated_cost"))).sum();
                    (order, est, cost)
                } else {
                    let child = plans.first().map(rows_of).unwrap_or(Val::None);
                    let u = universe();
                    let est = match (&u, &child) {
                        (Some(u), c) if !c.is_none() => Val::from_bigint((u - py_int(c)?).max(BigInt::zero())),
                        _ => Val::None,
                    };
                    let base = u.as_ref().and_then(|x| x.to_f64()).unwrap_or(0.0);
                    let cost = base + plans.iter().map(|p| cost_of(p.get("estimated_cost"))).sum::<f64>();
                    (vec![0], est, cost)
                };
                let kernel = match op.as_str() {
                    "and" => "intersect_sorted_ints",
                    "or" => "union_sorted_ints",
                    "not" => "difference_sorted_ints",
                    other => return Err(Error::Key(repr(other))),
                };
                Ok(vmap! {
                    "node" => op.to_uppercase(), "domain" => domain.name(), "children" => Val::List(plans),
                    "execution_order" => Val::List(order.into_iter().map(|i| Val::Int(i as i64)).collect()),
                    "estimated_rows" => est, "estimated_cost" => round3(cost), "kernel" => kernel,
                })
            }
            Expr::Field { name, op, value, value2, options } => {
                let (access, idx, rows, cost) = self.choose_field_access(name, op, value, value2, options, ctx, domain);
                let stats = match idx {
                    None => Val::None,
                    Some(i) => vmap! {
                        "kind" => i.kind.name(), "distinct_keys" => self.db.secondary_entry_count(i.index_id, ctx.lane) as i64,
                        "cardinality_source" => "posting.raw_count",
                    },
                };
                Ok(vmap! {
                    "node" => "FIELD", "field" => name.as_str(), "op" => op.as_str(), "domain" => domain.name(),
                    "access" => access, "index_id" => idx.map(|i| Val::Int(i.index_id as i64)).unwrap_or(Val::None),
                    "index_name" => idx.map(|i| Val::str(i.name.as_str())).unwrap_or(Val::None),
                    "index_kind" => idx.map(|i| Val::str(i.kind.name())).unwrap_or(Val::None),
                    "estimated_rows" => rows, "estimated_cost" => round3(cost), "statistics" => stats,
                })
            }
            Expr::Struct { name, op, value, value2 } => {
                let (access, rows, cost): (&str, Val, f64) = if (name == "N" || name == "record_N") && op == "eq" {
                    ("DIRECT_N", Val::Int(1), 1.0)
                } else {
                    match self.struct_fast_access(name, op, value, value2, ctx) {
                        Some("STRUCT_DIRECT") => ("STRUCT_DIRECT", Val::Int(1), 1.0),
                        Some(_) => {
                            let rows: BigInt = (py_int(value2)? - py_int(value)? + BigInt::from(1)).max(BigInt::zero());
                            let c = 2.0 + rows.to_f64().unwrap_or(f64::INFINITY);
                            ("STRUCT_RANGE", Val::from_bigint(rows), c)
                        }
                        None => {
                            let u = universe();
                            let c = u.as_ref().and_then(|x| x.to_f64()).unwrap_or(0.0);
                            ("MATERIALIZED_SCAN", u.map(Val::from_bigint).unwrap_or(Val::None), c)
                        }
                    }
                };
                Ok(vmap! {
                    "node" => "STRUCT", "name" => name.as_str(), "op" => op.as_str(), "domain" => domain.name(),
                    "access" => access, "estimated_rows" => rows, "estimated_cost" => round3(cost),
                })
            }
            Expr::DirectN { values, domain: input } => {
                let rows = values.len() as i64;
                Ok(vmap! {
                    "node" => "DIRECT_N", "input_domain" => input.as_str(), "domain" => domain.name(),
                    "count" => rows, "estimated_rows" => rows, "estimated_cost" => rows as f64,
                })
            }
            Expr::All => {
                let u = universe();
                let c = u.as_ref().and_then(|x| x.to_f64()).unwrap_or(0.0);
                Ok(vmap! {
                    "node" => "ALL", "domain" => domain.name(), "access" => "MATERIALIZED_UNIVERSE",
                    "estimated_rows" => u.map(Val::from_bigint).unwrap_or(Val::None), "estimated_cost" => c,
                })
            }
            Expr::Math { provider, params } => {
                let Some(p) = self.providers.map.get(provider) else {
                    return Ok(vmap! {
                        "node" => "MATH", "provider" => provider.as_str(), "domain" => domain.name(),
                        "access" => "MISSING_PROVIDER", "estimated_rows" => Val::None, "estimated_cost" => Val::None,
                    });
                };
                let est = self.provider_estimate(p, ctx, params);
                let cost = match &est {
                    None => Val::None,
                    Some(e) => Val::Float(round3(4.0 + py_float(e)?)),
                };
                Ok(vmap! {
                    "node" => "MATH", "provider" => p.name.as_str(), "provider_domain" => p.domain.name(),
                    "domain" => domain.name(), "access" => "MATH_PROVIDER", "description" => p.description.as_str(),
                    "estimated_rows" => est.unwrap_or(Val::None), "estimated_cost" => cost,
                })
            }
        }
    }

    // ------------------------- evaluation -------------------------

    /// `_eval`
    pub fn eval(&self, expr: &Expr, ctx: &QueryContext, domain: Domain) -> Result<CandidateSet> {
        match expr {
            Expr::Bool { op, children } => {
                if op == "not" {
                    let child = self.eval(&children[0], ctx, domain)?;
                    let universe = self.universe(ctx, domain)?;
                    return Ok(CandidateSet::new(domain, setops::difference_sorted(&universe.ns, &child.ns), "NOT"));
                }
                if children.is_empty() {
                    return Ok(CandidateSet::new(domain, Vec::new(), &op.to_uppercase()));
                }
                let mut kids: Vec<&Expr> = if op == "and" || op == "or" {
                    let mut v = Vec::new();
                    flatten_bool(expr, op, &mut v);
                    v
                } else {
                    children.iter().collect()
                };
                if op == "and" {
                    let mut ranked = Vec::with_capacity(kids.len());
                    for (i, k) in kids.iter().enumerate() {
                        let plan = self.plan_expr(k, ctx, domain)?;
                        ranked.push((est_key(plan.get("estimated_rows")), est_key(plan.get("estimated_cost")), i, *k));
                    }
                    ranked.sort_by(|a, b| cmp_est(&a.0, &b.0).then_with(|| cmp_est(&a.1, &b.1)).then(a.2.cmp(&b.2)));
                    kids = ranked.into_iter().map(|x| x.3).collect();
                }
                let mut acc = self.eval(kids[0], ctx, domain)?;
                for k in &kids[1..] {
                    if op == "and" && acc.ns.is_empty() {
                        break;
                    }
                    let child = self.eval(k, ctx, domain)?;
                    acc = match op.as_str() {
                        "and" => CandidateSet::new(domain, setops::intersect_sorted(&acc.ns, &child.ns), "AND"),
                        "or" => CandidateSet::new(domain, setops::union_sorted(&acc.ns, &child.ns), "OR"),
                        other => return Err(plan_err(format!("bool operator {} unknown", repr(other)))),
                    };
                }
                Ok(acc)
            }
            Expr::Field { name, op, value, value2, options } => self.eval_field(name, op, value, value2, options, ctx, domain),
            Expr::Struct { name, op, value, value2 } => self.eval_struct(name, op, value, value2, ctx, domain),
            Expr::DirectN { values, domain: input } => {
                let ns = values.iter().map(parse_n_val).collect::<Result<Vec<_>>>()?;
                let base = CandidateSet::new(candidate_domain(input)?, setops::sorted_unique(ns), "DIRECT_N");
                self.convert_domain(base, domain)
            }
            Expr::All => self.universe(ctx, domain),
            Expr::Math { provider, params } => self.eval_math(provider, params, ctx, domain),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_field(&self, name: &str, op: &str, value: &Val, value2: &Val, options: &Val, ctx: &QueryContext, domain: Domain) -> Result<CandidateSet> {
        let (access, idx, _rows, _cost) = self.choose_field_access(name, op, value, value2, options, ctx, domain);
        if let Some(idx) = idx {
            let realm = ctx.realm_nat().unwrap();
            let mut us = self.db.index_candidates(idx.index_id, op, value, value2, ctx.lane, options)?;
            if access == "TRIGRAM_INDEX" {
                us = self.verify_text(&realm, us, name, op, value, options, ctx.lane)?;
            }
            let sv = idx.field_v();
            let mut refs = Vec::with_capacity(us.len());
            for u in &us {
                let n = address::dynadron_n(&realm, u, sv, ctx.lane)?;
                if domain == Domain::Cell {
                    refs.push(n);
                } else if let Some(r) = self.db.record_n_nat(&realm, u, sv, ctx.lane)? {
                    refs.push(r);
                }
            }
            return Ok(CandidateSet::new(domain, setops::sorted_unique(refs), access));
        }
        let text_op = matches!(op, "contains" | "icontains" | "token" | "tokens_all" | "tokens_any" | "fulltext" | "phrase");
        let matches = |actual: &Val| -> Result<bool> {
            if text_op {
                textindex::predicate_matches(actual, op, value, options)
            } else {
                match_raw(actual, op, value, value2)
            }
        };
        let mut refs = Vec::new();
        if domain == Domain::Cell {
            self.scan_cell_labels(ctx, &mut |c, label, _env| {
                if label != name {
                    return Ok(());
                }
                if op == "exists" || matches(&c.value)? {
                    refs.push(address::dynadron_n(&c.realm, &c.u, &c.v, c.lane)?);
                }
                Ok(())
            })?;
            return Ok(CandidateSet::new(domain, setops::sorted_unique(refs), "MATERIALIZED_SCAN"));
        }
        self.scan_records(ctx, &mut |rn, rec| {
            let ok = match rec.data_get(name) {
                None => false,
                Some(_) if op == "exists" => true,
                Some(a) => matches(a)?,
            };
            if ok {
                refs.push(rn.clone());
            }
            Ok(())
        })?;
        Ok(CandidateSet::new(domain, setops::sorted_unique(refs), "MATERIALIZED_SCAN"))
    }

    /// `_classic_u_fast_layout`
    fn classic_u_fast_layout(&self, name: &str, op: &str, value: &Val, value2: &Val, ctx: &QueryContext) -> Option<&'a LayoutDef> {
        if name != "u" || (op != "eq" && op != "between") {
            return None;
        }
        let realm = ctx.realm_nat()?;
        let lo = py_int(value).ok()?;
        let hi = if op == "eq" { lo.clone() } else { py_int(value2).ok()? };
        if lo <= BigInt::zero() || hi < lo {
            return None;
        }
        let lo_n = Nat::from_bigint(&lo)?;
        let hi_n = Nat::from_bigint(&hi)?;
        let lay = self.db.layout_at(&realm, &lo_n)?;
        if lay.kind != LayoutKind::Classic || !lay.contains_u(&hi_n) {
            return None;
        }
        for id in self.db.layouts_by_realm.get(&realm).map(|v| v.as_slice()).unwrap_or(&[]) {
            let Some(other) = self.db.layouts.get(id) else { continue };
            if other.layout_id == lay.layout_id {
                continue;
            }
            let other_hi = other.u_end.clone().unwrap_or_else(|| hi_n.clone());
            let overlaps = other.u_start <= hi_n && other_hi >= lo_n;
            if overlaps && LayoutDef::sort_cmp(other, lay).is_gt() {
                return None;
            }
        }
        Some(lay)
    }

    fn struct_fast_access(&self, name: &str, op: &str, value: &Val, value2: &Val, ctx: &QueryContext) -> Option<&'static str> {
        self.classic_u_fast_layout(name, op, value, value2, ctx)?;
        Some(if op == "eq" { "STRUCT_DIRECT" } else { "STRUCT_RANGE" })
    }

    fn eval_struct(&self, name: &str, op: &str, value: &Val, value2: &Val, ctx: &QueryContext, domain: Domain) -> Result<CandidateSet> {
        if name == "N" && op == "eq" {
            let base = CandidateSet::new(Domain::Cell, vec![parse_n_val(value)?], "DIRECT_N");
            return self.convert_domain(base, domain);
        }
        if name == "record_N" && op == "eq" {
            let base = CandidateSet::new(Domain::Record, vec![parse_n_val(value)?], "DIRECT_N");
            return self.convert_domain(base, domain);
        }
        if let Some(lay) = self.classic_u_fast_layout(name, op, value, value2, ctx) {
            let lo = py_int(value)?;
            let hi = if op == "eq" { lo.clone() } else { py_int(value2)? };
            let lr = LayoutRef::Id(lay.layout_id);
            let mut refs = Vec::new();
            let lo_n = Nat::from_bigint(&lo).unwrap();
            let hi_n = Nat::from_bigint(&hi).unwrap();
            // Large ranges: visit only materialized u (equivalent to iterating
            // every u of the range, since only existing records contribute
            // and the CLASSIC layout owns the range).
            let span = &hi - &lo + BigInt::from(1);
            let us: Vec<Nat> = if span > BigInt::from(STRUCT_RANGE_DIRECT_MAX) {
                let realm = ctx.realm_nat().unwrap();
                let mut seen: Vec<Nat> = Vec::new();
                RawCellCursor::new(self.db, Some(&realm), Some(ctx.lane)).for_each(self.db, |c| {
                    if c.u >= lo_n && c.u <= hi_n && seen.last() != Some(&c.u) {
                        seen.push(c.u);
                    }
                    Ok(true)
                })?;
                setops::sorted_unique(seen)
            } else {
                let mut v = Vec::new();
                let mut u = lo_n.clone();
                while u <= hi_n {
                    v.push(u.clone());
                    u = u.add_u64(1);
                }
                v
            };
            for u in us {
                let rec = self.db.get_record_view(&lr, &u, ctx.lane, None)?;
                if rec.get("present").is_some_and(|p| p.is_truthy()) {
                    if domain == Domain::Record {
                        refs.push(parse_n_val(rec.get("record_N").unwrap())?);
                    } else if let Some(Val::List(cells)) = rec.get("cells") {
                        for c in cells {
                            refs.push(parse_n_val(c.get("N").unwrap())?);
                        }
                    }
                }
            }
            let src = if op == "eq" { "STRUCT_DIRECT" } else { "STRUCT_RANGE" };
            return Ok(CandidateSet::new(domain, setops::sorted_unique(refs), src));
        }
        let realm_f = ctx.realm_nat();
        let mut refs = Vec::new();
        if domain == Domain::Cell {
            RawCellCursor::new(self.db, realm_f.as_ref(), Some(ctx.lane)).for_each(self.db, |c| {
                let env = self.db.materialized_cell_envelope(&c.realm, &c.u, &c.v, c.value, c.lane)?;
                let actual = struct_value_cell(name, &env)?;
                if match_raw(&actual, op, value, value2)? {
                    refs.push(parse_n_val(env.get("N").unwrap())?);
                }
                Ok(true)
            })?;
        } else {
            self.scan_records(ctx, &mut |rn, rec| {
                let actual = rec.struct_value(name, rn)?;
                if match_raw(&actual, op, value, value2)? {
                    refs.push(rn.clone());
                }
                Ok(())
            })?;
        }
        Ok(CandidateSet::new(domain, setops::sorted_unique(refs), "STRUCTURAL_SCAN"))
    }

    fn eval_math(&self, provider: &str, params: &Val, ctx: &QueryContext, domain: Domain) -> Result<CandidateSet> {
        let Some(p) = self.providers.map.get(provider) else {
            return Err(plan_err(format!("MATH provider {} is not registered", repr(provider))));
        };
        let ns: Vec<Nat> = match &p.kind {
            ProviderKind::Formula => self.provider_formula(ctx, params)?,
            ProviderKind::NSequence => self.provider_n_sequence(ctx, params)?,
            ProviderKind::MathLayoutInputs => self.provider_math_layout_inputs(params)?,
            ProviderKind::External(e) => {
                let items = e.call(self, ctx, params)?;
                items.iter().map(|it| normalize_provider_item(it, p.domain)).collect::<Result<_>>()?
            }
        };
        let base = CandidateSet::new(p.domain, setops::sorted_unique(ns), &format!("MATH:{}", p.name));
        self.convert_domain(base, domain)
    }

    /// `_convert_domain`
    fn convert_domain(&self, c: CandidateSet, target: Domain) -> Result<CandidateSet> {
        if c.domain == target {
            return Ok(c);
        }
        let mut refs = Vec::new();
        if c.domain == Domain::Cell {
            for n in &c.ns {
                let msg = || dom_err(format!("CELL N{n} has no resolvable RECORD identity"));
                match self.db.record_n_from_n_nat(n) {
                    Ok(Some(r)) => refs.push(r),
                    _ => return Err(msg()),
                }
            }
            return Ok(CandidateSet::new(Domain::Record, setops::sorted_unique(refs), &format!("{}->RECORD", c.source)));
        }
        for n in &c.ns {
            let rec = self.db.get_n(n, GetMode::Record)?;
            if let Some(Val::List(cells)) = rec.get("cells") {
                for cell in cells {
                    refs.push(parse_n_val(cell.get("N").unwrap())?);
                }
            }
        }
        Ok(CandidateSet::new(Domain::Cell, setops::sorted_unique(refs), &format!("{}->CELL", c.source)))
    }

    /// `_universe`
    fn universe(&self, ctx: &QueryContext, domain: Domain) -> Result<CandidateSet> {
        let realm_f = ctx.realm_nat();
        let mut refs = Vec::new();
        if domain == Domain::Record {
            let mut cur = RecordNCursor::new(self.db, realm_f.as_ref(), Some(ctx.lane));
            loop {
                let batch = cur.next_batch_nat(self.db, 4096)?;
                if batch.is_empty() {
                    break;
                }
                refs.extend(batch);
            }
        } else {
            RawCellCursor::new(self.db, realm_f.as_ref(), Some(ctx.lane)).for_each(self.db, |c| {
                refs.push(address::dynadron_n(&c.realm, &c.u, &c.v, c.lane)?);
                Ok(true)
            })?;
        }
        Ok(CandidateSet::new(domain, setops::sorted_unique(refs), "MATERIALIZED_UNIVERSE"))
    }


    // ------------------------- materialization scans -------------------------

    /// Record scan in canonical order (`iter_materialized_record_ns` +
    /// `get_n_record`). CLASSIC records are built directly from the cells of
    /// their u group (same labels as `get_record_view`), without re-reading
    /// and without computing N per cell; other layouts use the full record
    /// view.
    fn scan_records(&self, ctx: &QueryContext, f: &mut dyn FnMut(&Nat, &RecView<'_>) -> Result<()>) -> Result<()> {
        let realm_f = ctx.realm_nat();
        let mut seen: HashSet<Nat> = HashSet::new();
        let mut group: Vec<RawCell> = Vec::new();
        let mut flush = |group: &mut Vec<RawCell>, seen: &mut HashSet<Nat>| -> Result<()> {
            if group.is_empty() {
                return Ok(());
            }
            let (realm, lane, u) = (group[0].realm.clone(), group[0].lane, group[0].u.clone());
            let lay = self.db.layout_at(&realm, &u);
            if let Some(l) = lay.filter(|l| l.kind == LayoutKind::Classic) {
                let rn = match self.db.logical_address_in(l, &u, &Val::None, lane) {
                    Ok(a) => a.n,
                    Err(Error::Schema(_) | Error::RealmLimit(_) | Error::Key(_) | Error::Value(_)) => {
                        group.clear();
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                };
                // CLASSIC u groups are unique within the scan (no set kept)
                {
                    let resolver = self.db.field_resolver(&realm, &u);
                    let mut data: Vec<(String, Val)> = Vec::with_capacity(group.len());
                    for c in group.iter() {
                        let label = match resolver.get(self.db, &realm, &u, &c.v)? {
                            Some(m) if !m.name.is_empty() => m.name.clone(),
                            _ => format!("v{}", c.v),
                        };
                        let label = if data.iter().any(|(k, _)| *k == label) { format!("{label}@u{u}v{}", c.v) } else { label };
                        crate::db::read::set_or_push(&mut data, &label, c.value.clone());
                    }
                    f(&rn, &RecView::Classic { data, realm: &realm, lane, u: &u, lay: l })?;
                }
            } else {
                for c in group.iter() {
                    if let Some(rn) = self.db.cell_record_n(&c.realm, &c.u, &c.v, c.lane)? {
                        if seen.insert(rn.clone()) {
                            let rec = self.db.get_n(&rn, GetMode::Record)?;
                            f(&rn, &RecView::Full(rec))?;
                        }
                    }
                }
            }
            group.clear();
            Ok(())
        };
        RawCellCursor::new(self.db, realm_f.as_ref(), Some(ctx.lane)).for_each(self.db, |c| {
            if let Some(g) = group.first() {
                if g.u != c.u || g.lane != c.lane || g.realm != c.realm {
                    flush(&mut group, &mut seen)?;
                }
            }
            group.push(c);
            Ok(true)
        })?;
        flush(&mut group, &mut seen)
    }

    /// Cell scan with a label per cell; CLASSIC via the field resolver of the
    /// u group, otherwise via the full envelope.
    fn scan_cell_labels(&self, ctx: &QueryContext, f: &mut CellLabelFn<'_>) -> Result<()> {
        let realm_f = ctx.realm_nat();
        let mut cur_key: Option<(Nat, Nat)> = None;
        let mut classic: Option<crate::db::layout::FieldResolver<'_>> = None;
        RawCellCursor::new(self.db, realm_f.as_ref(), Some(ctx.lane)).for_each(self.db, |c| {
            let key = (c.realm.clone(), c.u.clone());
            if cur_key.as_ref() != Some(&key) {
                classic = match self.db.layout_at(&c.realm, &c.u) {
                    Some(l) if l.kind == LayoutKind::Classic => Some(self.db.field_resolver(&c.realm, &c.u)),
                    _ => None,
                };
                cur_key = Some(key);
            }
            match &classic {
                Some(res) => {
                    let label = match res.get(self.db, &c.realm, &c.u, &c.v)? {
                        Some(m) if !m.name.is_empty() => m.name.clone(),
                        _ => format!("v{}", c.v),
                    };
                    f(&c, &label, None)?;
                }
                None => {
                    let env = self.db.materialized_cell_envelope(&c.realm, &c.u, &c.v, c.value.clone(), c.lane)?;
                    let label = env.get("field").map(|x| x.py_str()).unwrap_or_default();
                    f(&c, &label, Some(&env))?;
                }
            }
            Ok(true)
        })
    }

    // ------------------------- built-in providers -------------------------

    fn provider_formula(&self, ctx: &QueryContext, params: &Val) -> Result<Vec<Nat>> {
        let parameter = params.get("parameter").map(|p| p.py_str()).unwrap_or_else(|| "k".into());
        let start = py_int(params.get("start").unwrap_or(&Val::Int(1)))?;
        let stop = py_int(params.get("stop").ok_or_else(|| Error::Key("'stop'".into()))?)?;
        let step = py_int(params.get("step").unwrap_or(&Val::Int(1)))?;
        if step.is_zero() {
            return Err(plan_err("formula step must not be 0"));
        }
        let u_expr = params.get("u").ok_or_else(|| Error::Key("'u'".into()))?.py_str();
        let v_expr = if let Some(v) = params.get("V") {
            v.py_str()
        } else if let Some(v) = params.get("v") {
            let Some(r) = &ctx.realm else {
                return Err(plan_err("formula with local v requires query context realm"));
            };
            format!("({r}-1)*{}+({})", address::realm_span(), v.py_str())
        } else {
            return Err(plan_err("formula requires V=... or v=..."));
        };
        let side = params.get("side").cloned().unwrap_or_else(|| Val::str("NL"));
        let mapping = MathMap::compile("query_formula", &[parameter], &u_expr, &v_expr, &side, &address::realm_span().to_bigint())?;
        let mut out = Vec::new();
        let mut k = start;
        let positive = step.is_positive();
        while if positive { k <= stop } else { k >= stop } {
            let t = mapping.target(&[Val::from_bigint(k.clone())], self.max_bits())?;
            k += &step;
            if let Some(r) = &ctx.realm {
                if &t.logical_realm != r {
                    continue;
                }
            }
            out.push(dyn_n(&t.logical_realm, &t.u, &t.v_local, side_lane(t.side))?);
        }
        Ok(out)
    }

    fn provider_n_sequence(&self, ctx: &QueryContext, params: &Val) -> Result<Vec<Nat>> {
        let start = py_int(params.get("start").ok_or_else(|| Error::Key("'start'".into()))?)?;
        let stop = py_int(params.get("stop").ok_or_else(|| Error::Key("'stop'".into()))?)?;
        let step = py_int(params.get("step").unwrap_or(&Val::Int(1)))?;
        if step.is_zero() {
            return Err(plan_err("n_sequence step must not be 0"));
        }
        let strict = params.get("strict").is_some_and(|s| s.is_truthy());
        let realm = ctx.realm_nat();
        let mut out = Vec::new();
        let mut n = start;
        let positive = step.is_positive();
        while if positive { n <= stop } else { n >= stop } {
            let cur = n.clone();
            n += &step;
            let Some(nn) = Nat::from_bigint(&cur).filter(|x| !x.is_zero()) else {
                return Err(Error::Value("N must be > 0".into()));
            };
            let a = match n_to_address(&nn) {
                Ok(a) => a,
                Err(Error::Address(m)) => {
                    if strict {
                        return Err(Error::Address(m));
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            if let Some(r) = &realm {
                if a.realm() != r {
                    continue;
                }
            }
            out.push(nn);
        }
        Ok(out)
    }

    fn provider_math_layout_inputs(&self, params: &Val) -> Result<Vec<Nat>> {
        let lid = py_int(params.get("layout_id").ok_or_else(|| Error::Key("'layout_id'".into()))?)?;
        let rows = match params.get("inputs") {
            None | Some(Val::None) => return Err(plan_err("math_layout_inputs requires inputs=[...]")),
            Some(r) => r,
        };
        let rec = match lid.to_u64() {
            Some(l) => self.db.math_record(l)?,
            None => return Err(Error::MathLayoutNotFound(format!("MATH layout {lid} does not exist"))),
        };
        let mut out = Vec::new();
        for row in iter_items(rows)? {
            let args: Vec<Val> = match &row {
                Val::Int(_) | Val::Big(_) | Val::Bool(_) => vec![Val::from_bigint(py_int(&row)?)],
                other => iter_items(other)?.iter().map(|x| py_int(x).map(Val::from_bigint)).collect::<Result<_>>()?,
            };
            let t = rec.spec.mapping.target(&args, self.max_bits())?;
            let r = resolve_math_target(&rec.realm_map, &t)?;
            out.push(dyn_n(&r.physical_realm, &r.u, &r.v_local, side_lane(r.side))?);
        }
        Ok(out)
    }
}

/// Callback of the cell scan: raw cell, label, full envelope if available.
type CellLabelFn<'f> = dyn FnMut(&RawCell, &str, Option<&Val>) -> Result<()> + 'f;

/// Record view of the scan: CLASSIC compact, otherwise full view.
enum RecView<'x> {
    Classic { data: Vec<(String, Val)>, realm: &'x Nat, lane: Lane, u: &'x Nat, lay: &'x LayoutDef },
    Full(Val),
}

impl RecView<'_> {
    fn data_get(&self, name: &str) -> Option<&Val> {
        let data = match self {
            RecView::Classic { data, .. } => data.as_slice(),
            RecView::Full(rec) => rec.get("data").and_then(|d| d.as_map()).map(|m| m.as_slice()).unwrap_or(&[]),
        };
        data.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// `_struct_value_record`
    fn struct_value(&self, name: &str, rn: &Nat) -> Result<Val> {
        match self {
            RecView::Full(rec) => struct_value_record(name, rec),
            RecView::Classic { realm, lane, u, lay, .. } => Ok(match name {
                "N" | "record_N" => n_val(rn),
                "u" | "object" => Val::nat(u),
                "v" => Val::Int(1),
                "realm" => Val::nat(realm),
                "lane" => Val::Int(*lane as i64),
                "layout_id" => Val::Int(lay.layout_id as i64),
                "layout_kind" => Val::str(lay.kind.name()),
                "layout_name" => Val::str(lay.name.as_str()),
                other => return Err(plan_err(format!("structural term {} unknown", repr(other)))),
            }),
        }
    }
}

fn move_last(m: &mut Val, key: &str) {
    if let Some(v) = m.remove(key) {
        m.set(key, v);
    }
}

fn sequence_count(start: &BigInt, stop: &BigInt, step: &BigInt) -> BigInt {
    if step.is_zero() || (step.is_positive() && start > stop) || (step.is_negative() && start < stop) {
        return BigInt::zero();
    }
    let d = stop - start;
    // Python floor division
    let (q, r) = (&d / step, &d % step);
    let q = if !r.is_zero() && (r.is_negative() != step.is_negative()) { q - 1 } else { q };
    q + 1
}

fn domain_for_return(mode: &str, shown: &str) -> Result<Domain> {
    match mode {
        "cell" => Ok(Domain::Cell),
        "record" | "fields" | "count" | "n" | "aggregate" | "ranked" => Ok(Domain::Record),
        _ => Err(plan_err(format!("return_mode {} unknown", repr(shown)))),
    }
}

fn struct_value_cell(name: &str, cell: &Val) -> Result<Val> {
    Ok(match name {
        "N" => n_val(&parse_n_val(cell.get("N").unwrap_or(&Val::None))?),
        "record_N" => match cell.get("record_N") {
            None | Some(Val::None) => Val::None,
            Some(r) => n_val(&parse_n_val(r)?),
        },
        "realm" | "u" | "v" | "lane" | "layout_id" | "layout_kind" | "layout_name" | "object" => cell.get(name).cloned().unwrap_or(Val::None),
        other => return Err(plan_err(format!("structural term {} unknown", repr(other)))),
    })
}

fn struct_value_record(name: &str, rec: &Val) -> Result<Val> {
    let anchor = |k: &str| rec.get("anchor_address").and_then(|a| a.get(k)).cloned().unwrap_or(Val::None);
    Ok(match name {
        "N" | "record_N" => n_val(&parse_n_val(rec.get("record_N").unwrap_or(&Val::None))?),
        "u" => anchor("u"),
        "v" => anchor("v"),
        "realm" | "lane" | "layout_id" | "layout_kind" | "layout_name" | "object" => rec.get(name).cloned().unwrap_or(Val::None),
        other => return Err(plan_err(format!("structural term {} unknown", repr(other)))),
    })
}

/// `_normalize_provider_item`
pub fn normalize_provider_item(item: &Val, domain: Domain) -> Result<Nat> {
    match item {
        Val::Int(_) | Val::Big(_) | Val::Bool(_) | Val::Str(_) => return parse_n_val(item),
        Val::Map(_) => {
            let key = if domain == Domain::Record && item.contains_key("record_N") { "record_N" } else { "N" };
            if let Some(v) = item.get(key) {
                return parse_n_val(v);
            }
            if let (Some(r), Some(u), Some(v)) = (item.get("realm"), item.get("u"), item.get("v")) {
                let lane = lane_from_val(item.get("lane").unwrap_or(&Val::Int(0)))?;
                return dyn_n(&py_int(r)?, &py_int(u)?, &py_int(v)?, lane);
            }
        }
        Val::List(l) if l.len() == 3 || l.len() == 4 => {
            let lane = if l.len() == 3 { 0 } else { lane_from_val(&l[3])? };
            return dyn_n(&py_int(&l[0])?, &py_int(&l[1])?, &py_int(&l[2])?, lane);
        }
        _ => {}
    }
    Err(dom_err(format!("provider returned invalid candidate: {}", item.py_repr())))
}

/// Options from a JSON specification (`execute_spec`).
pub fn exec_options_from_spec(spec: &Val) -> Result<ExecOptions> {
    let realm = match spec.get("realm") {
        None | Some(Val::None) => None,
        Some(r) => Some(py_int(r)?),
    };
    let lane = lane_from_val(spec.get("lane").unwrap_or(&Val::Int(0)))?;
    let opt = |k: &str| match spec.get(k) {
        None | Some(Val::None) => None,
        Some(v) => Some(v.clone()),
    };
    Ok(ExecOptions {
        realm,
        lane,
        return_mode: spec.get("return").map(|r| r.py_str()).unwrap_or_else(|| "record".into()),
        select: opt("select"),
        aggregate: opt("aggregate"),
        group_by: opt("group_by"),
        order_by: opt("order_by"),
        limit: spec.get("limit").cloned().unwrap_or(Val::None),
        offset: Val::from_bigint(py_int(spec.get("offset").unwrap_or(&Val::Int(0)))?),
        explain: spec.get("explain").is_some_and(|e| e.is_truthy()),
        k1: py_float(spec.get("k1").unwrap_or(&Val::Float(1.2)))?,
        b: py_float(spec.get("b").unwrap_or(&Val::Float(0.75)))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(round3(0.0625), 0.062);
        assert_eq!(round3(12.3456), 12.346);
        assert_eq!(int_true_div(&BigInt::from(7), &BigInt::from(2)), 3.5);
        let big = BigInt::from(10).pow(30);
        assert_eq!(int_true_div(&(big.clone() + 1), &BigInt::from(3)), 3.333333333333333e29);
        assert_eq!(sequence_count(&BigInt::from(1), &BigInt::from(100), &BigInt::from(2)), BigInt::from(50));
        assert_eq!(sequence_count(&BigInt::from(10), &BigInt::from(1), &BigInt::from(-3)), BigInt::from(4));
        assert!(match_raw(&Val::Int(1), "eq", &Val::Float(1.0), &Val::None).unwrap());
        assert!(!match_raw(&Val::str("a"), "lt", &Val::Int(1), &Val::None).unwrap());
        assert!(match_raw(&Val::Int(5), "between", &Val::Int(1), &Val::Int(5)).unwrap());
        assert!(match_raw(&Val::Int(5), "foo", &Val::Int(1), &Val::None).is_err());
        let v = py_sort(vec![3, 1, 2, 1], true, &mut |a: &i32, b: &i32| Ok(a < b)).unwrap();
        assert_eq!(v, vec![3, 2, 1, 1]);
        assert_eq!(py_int(&Val::str(" 1_000 ")).unwrap(), BigInt::from(1000));
        assert!(py_int(&Val::str("1__0")).is_err());
    }
}
