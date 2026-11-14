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
/// Maximum nesting of query expressions (parentheses/NOT/AND/OR levels) in
/// all parsers and converters (F015/F016). Flat AND/OR chains of any length
/// count as one level.
pub const MAX_EXPR_DEPTH: usize = 256;

/// Up to this range width, STRUCT_RANGE visits every u directly.
const STRUCT_RANGE_DIRECT_MAX: u64 = 4096;
/// RUN9 V3: candidates whose texts the TRIGRAM verification reads at once.
const VERIFY_TEXT_CHUNK: usize = 4096;

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
        Val::Float(f) if f.fract() != 0.0 => Err(Error::Value(format!("N must be an integer, got {}", v.py_repr()))),
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

impl Drop for Expr {
    /// Iterative drop: deep trees (e.g. long NOT chains built through the
    /// Rust API) must not overflow the stack (F015).
    fn drop(&mut self) {
        if let Expr::Bool { children, .. } = self {
            if !children.iter().any(|c| matches!(c, Expr::Bool { .. })) {
                return;
            }
            let mut stack = std::mem::take(children);
            while let Some(mut e) = stack.pop() {
                if let Expr::Bool { children, .. } = &mut e {
                    stack.append(children);
                }
            }
        }
    }
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

/// `expr_from_spec` (JSON specification). `and`/`or` lists become one
/// n-ary node; nesting is limited to [`MAX_EXPR_DEPTH`] (F015).
pub fn expr_from_spec(raw: &Val) -> Result<Expr> {
    expr_from_spec_at(raw, 0)
}

/// Message of the nesting limit (F015). N5: planning and evaluation of an
/// expression tree are recursive, so the limit stays at [`MAX_EXPR_DEPTH`];
/// the hint shows how to stay within it.
pub fn depth_message() -> String {
    format!(
        "query expression nested deeper than {MAX_EXPR_DEPTH} levels; every switch between AND, OR and NOT adds a level, \
         while a chain of one operator counts once whatever its length – combine the conditions into flat AND/OR \
         chains (e.g. an \"or\"/\"and\" list) instead of alternating (q & a) | b"
    )
}

pub fn depth_error() -> Error {
    plan_err(depth_message())
}

fn expr_from_spec_at(raw: &Val, depth: usize) -> Result<Expr> {
    if depth > MAX_EXPR_DEPTH {
        return Err(depth_error());
    }
    if !matches!(raw, Val::Map(_)) {
        return Err(plan_err("query expression must be an object"));
    }
    if matches!(raw.get("all"), Some(Val::Bool(true))) {
        return Ok(Expr::All);
    }
    for op in ["and", "or"] {
        if let Some(items) = raw.get(op) {
            let mut xs: Vec<Expr> = iter_items(items)?.iter().map(|x| expr_from_spec_at(x, depth + 1)).collect::<Result<_>>()?;
            return match xs.len() {
                0 => Err(plan_err(format!("{op} must not be empty"))),
                1 => Ok(xs.pop().unwrap()),
                _ => Ok(Expr::Bool { op: op.into(), children: xs }),
            };
        }
    }
    if let Some(x) = raw.get("not") {
        return Ok(Expr::negate(expr_from_spec_at(x, depth + 1)?));
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
    /// Restrict results to records routed to this layout (NHySQL
    /// `FROM <layout>`, F027).
    pub layout_id: Option<u64>,
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
            layout_id: None,
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
    /// RUN6: estimated bytes of the groups (`ResultGuard`)
    bytes: u64,
}

impl<'s> Aggregator<'s> {
    fn new(specs: &'s [AggSpec], groups: &'s [String]) -> Self {
        Aggregator { specs, groups, index: HashMap::new(), states: Vec::new(), bytes: 0 }
    }

    /// Estimated bytes of one group (key, states, map slot).
    fn group_cost(key: &[Val], specs: usize) -> u64 {
        (96 + key.iter().map(|v| std::mem::size_of::<Val>() + crate::db::val_heap(v)).sum::<usize>() + specs * 48) as u64
    }

    fn add(&mut self, get: &dyn Fn(&str) -> Val) -> Result<()> {
        let key: Vec<Val> = self.groups.iter().map(|g| get(g)).collect();
        let hk = group_key(&key)?;
        // F045: NaN != NaN – as with Python dict keys of separately decoded
        // floats, every row with a NaN group value forms its own group.
        let nan = key.iter().any(|v| matches!(v, Val::Float(f) if f.is_nan()));
        let slot = match self.index.get(&hk).filter(|_| !nan) {
            Some(&i) => i,
            None if nan => {
                self.bytes += Self::group_cost(&key, self.specs.len());
                self.states.push((key, self.specs.iter().map(|s| agg_init(&s.op)).collect()));
                self.states.len() - 1
            }
            None => {
                self.bytes += Self::group_cost(&key, self.specs.len()) * 2;
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

/// Order of `order_limit_indices` for totally ordered key columns (only
/// None/Int or only None/Str): None sorts after every value.
fn total_cmp_key(a: &Val, b: &Val) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Val::None, Val::None) => Equal,
        (Val::None, _) => Greater,
        (_, Val::None) => Less,
        (Val::Int(x), Val::Int(y)) => x.cmp(y),
        (Val::Str(x), Val::Str(y)) => x.cmp(y),
        _ => Equal,
    }
}

/// N1 (RUN8): books the rows of a top-k window in the result guard.
fn guard_rows(rows: Vec<Val>, guard: &mut ResultGuard) -> Result<Vec<Val>> {
    for row in &rows {
        guard.add(row)?;
    }
    Ok(rows)
}

/// N1 (RUN8): comparison of two keys of one totally ordered column (None
/// after every value; numbers by value as Python compares them; str by
/// code point).
fn order_cmp_total(a: &Val, b: &Val) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Val::None, Val::None) => Equal,
        (Val::None, _) => Greater,
        (_, Val::None) => Less,
        (Val::Int(x), Val::Int(y)) => x.cmp(y),
        (Val::Str(x), Val::Str(y)) => x.cmp(y),
        _ => {
            if py_lt(a, b, "<").unwrap_or(false) {
                Less
            } else if py_lt(b, a, "<").unwrap_or(false) {
                Greater
            } else {
                Equal
            }
        }
    }
}

fn num_f64(n: &Num) -> f64 {
    match n {
        Num::I(i) => i.to_f64().unwrap_or(f64::INFINITY),
        Num::F(f) => *f,
    }
}

/// R4-A6: estimates from this size on get a hint in `explain`.
const LARGE_RESULT_ROWS: u64 = 1_000_000;

/// R4-A6: rejects a query whose estimated result (bounded by LIMIT) exceeds
/// the configured `[query] max_result_rows`. Unknown estimates pass.
fn check_result_limit(est: Option<f64>, limit: &Val, max_rows: u64) -> Result<()> {
    let Some(mut rows) = est else { return Ok(()) };
    if !limit.is_none() {
        if let Ok(l) = py_int(limit) {
            rows = rows.min(l.max(BigInt::zero()).to_f64().unwrap_or(f64::INFINITY));
        }
    }
    if rows > max_rows as f64 {
        return Err(Error::QueryLimit(format!(
            "query rejected: about {rows:.0} result rows estimated, more than [query] max_result_rows = {max_rows}; \
             use LIMIT, COUNT(*), an aggregate or the streaming sql_iter()"
        )));
    }
    Ok(())
}

/// R4-A2: LIMIT/OFFSET window size `offset + limit` for a top-k selection;
/// `None` (keep everything) without LIMIT or when the values are not plain
/// integers (the regular slicing then reports the error, as before).
fn topk_window(limit: &Val, offset: &Val) -> Option<usize> {
    if limit.is_none() {
        return None;
    }
    let off = if offset.is_truthy() { py_int(offset).ok()? } else { BigInt::zero() };
    let lim = py_int(limit).ok()?;
    let off = off.max(BigInt::zero()).to_usize()?;
    let lim = lim.max(BigInt::zero()).to_usize()?;
    off.checked_add(lim)
}

/// R4-A2: keeps the `k` best items under the strict total order `rank`
/// (amortized linear: the buffer is cut back to `k` whenever it reaches
/// `2k`). Sorting the result by `rank` gives exactly the first `k` items of
/// a full sort.
fn topk_push<T>(buf: &mut Vec<T>, k: usize, item: T, rank: &dyn Fn(&T, &T) -> std::cmp::Ordering) {
    buf.push(item);
    if buf.len() >= 2 * k + 1024 {
        buf.select_nth_unstable_by(k - 1, |a, b| rank(a, b));
        buf.truncate(k);
    }
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

/// Sort key of `row` for ORDER BY `name` (`_apply_order_limit`).
fn order_key(row: &Val, name: &str) -> Val {
    if matches!(row.get("result"), Some(Val::Str(s)) if s == "record") {
        record_value(row, name)
    } else {
        row.get(name).cloned().unwrap_or(Val::None)
    }
}

/// `_apply_order_limit`
fn apply_order_limit(rows: Vec<Val>, order_by: Option<&Val>, limit: &Val, offset: &Val) -> Result<Vec<Val>> {
    let picked = order_limit_indices(rows.len(), order_by, limit, offset, &mut |i, name| Ok(order_key(&rows[i], name)))?;
    let mut slots: Vec<Option<Val>> = rows.into_iter().map(Some).collect();
    Ok(picked.into_iter().map(|i| slots[i].take().unwrap()).collect())
}

/// Ordering of `_apply_order_limit` on row indices (P007): one stable sort
/// per ORDER BY key, from the last key to the first, with the comparator of
/// the reference (None after values, Python `<`, TypeError → plan error),
/// then OFFSET/LIMIT. Only the keys are moved, so callers can build the
/// rows of the returned window afterwards. Keys that are all ints (or all
/// strings), apart from None, form a total order; they are sorted with the
/// standard stable sort, which yields exactly the same permutation.
fn order_limit_indices(
    n: usize,
    order_by: Option<&Val>,
    limit: &Val,
    offset: &Val,
    key: &mut dyn FnMut(usize, &str) -> Result<Val>,
) -> Result<Vec<usize>> {
    let ordering = normalize_order_by(order_by)?;
    let mut out: Vec<usize> = (0..n).collect();
    for (name, desc) in ordering.iter().rev() {
        let mut keyed: Vec<(Val, usize)> = Vec::with_capacity(out.len());
        for i in out {
            keyed.push((key(i, name)?, i));
        }
        let total = keyed.iter().all(|(k, _)| matches!(k, Val::None | Val::Int(_)))
            || keyed.iter().all(|(k, _)| matches!(k, Val::None | Val::Str(_)));
        if total {
            let cmp = |a: &Val, b: &Val| -> std::cmp::Ordering {
                use std::cmp::Ordering::*;
                match (a, b) {
                    (Val::None, Val::None) => Equal,
                    (Val::None, _) => Greater,
                    (_, Val::None) => Less,
                    (Val::Int(x), Val::Int(y)) => x.cmp(y),
                    (Val::Str(x), Val::Str(y)) => x.cmp(y),
                    _ => Equal,
                }
            };
            if *desc {
                keyed.sort_by(|a, b| cmp(&b.0, &a.0));
            } else {
                keyed.sort_by(|a, b| cmp(&a.0, &b.0));
            }
            out = keyed.into_iter().map(|(_, i)| i).collect();
            continue;
        }
        let mut lt = |a: &(Val, usize), b: &(Val, usize)| -> Result<bool> {
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
            Ok(v) => v.into_iter().map(|(_, i)| i).collect(),
            Err(Error::Type(_)) => return Err(plan_err(format!("ORDER BY {} compares incompatible values", repr(name)))),
            Err(e) => return Err(e),
        };
    }
    slice_simple(out, limit, offset)
}

/// Leaves of a same-operator chain in left-to-right order (iterative, F015).
fn flatten_bool<'e>(expr: &'e Expr, op: &str, out: &mut Vec<&'e Expr>) {
    let mut stack = vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            Expr::Bool { op: o, children } if o == op => stack.extend(children.iter().rev()),
            other => out.push(other),
        }
    }
}

/// `plan_expr` cannot fail for these shapes (estimates of FIELD leaves are
/// taken with `.ok()`, ALL only reads metadata), so planning them lazily
/// does not change which error a query raises first.
fn plan_is_infallible(expr: &Expr) -> bool {
    match expr {
        Expr::Field { .. } | Expr::All => true,
        Expr::Bool { op, .. } if op == "and" => {
            let mut leaves = Vec::new();
            flatten_bool(expr, "and", &mut leaves);
            leaves.iter().all(|l| matches!(l, Expr::Field { .. }))
        }
        _ => false,
    }
}

/// The single CLASSIC layout of a realm and its record u ranges (P007).
type ClassicUniverse<'a> = (&'a LayoutDef, Vec<(Nat, Nat)>);

/// All u of sorted, disjoint inclusive ranges (P007).
fn expand_ranges(ranges: &[(Nat, Nat)]) -> Vec<Nat> {
    let mut out = Vec::new();
    for (a, b) in ranges {
        let mut u = a.clone();
        while &u <= b {
            out.push(u.clone());
            u = u.add_u64(1);
        }
    }
    out
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
/// R4-B: u of the CLASSIC fields rows of a cursor in output order.
pub enum UWindow {
    List(std::collections::VecDeque<Nat>),
    /// `take` u after skipping `skip` u of sorted disjoint inclusive ranges
    Ranges { ranges: Vec<(Nat, Nat)>, skip: usize, take: usize },
    /// R4-C: merge of sorted runs in the result directory
    Merge(Box<RunMerge>),
}

impl UWindow {
    /// The next at most `n` u (consumed).
    fn next_chunk(&mut self, n: usize) -> Result<Vec<Nat>> {
        Ok(match self {
            UWindow::Merge(m) => m.next_chunk(n)?,
            UWindow::List(q) => {
                let k = n.min(q.len());
                q.drain(..k).collect()
            }
            UWindow::Ranges { ranges, skip, take } => {
                let mut out = Vec::with_capacity(n.min(*take));
                while out.len() < n && *take > 0 && !ranges.is_empty() {
                    let (a, b) = ranges[0].clone();
                    let len = b.checked_sub(&a).unwrap().add_u64(1).as_u64().map(|x| x as usize).unwrap_or(usize::MAX);
                    if *skip >= len {
                        *skip -= len;
                        ranges.remove(0);
                        continue;
                    }
                    let start = a.add_u64(*skip as u64);
                    *skip = 0;
                    let k = (len - (start.checked_sub(&a).unwrap().as_u64().unwrap() as usize)).min(n - out.len()).min(*take);
                    let mut u = start.clone();
                    for _ in 0..k {
                        out.push(u.clone());
                        u = u.add_u64(1);
                    }
                    *take -= k;
                    if u > b {
                        ranges.remove(0);
                    } else {
                        ranges[0].0 = u;
                    }
                }
                out
            }
        })
    }
}

/// Bytes of sort runs booked in the result budget: released exactly once,
/// when the runs are dropped (RD4-01/RD4-15: no second release by
/// `cleanup_results` or an error path).
struct RunBytes {
    budget: Option<std::sync::Arc<crate::results::DirBudget>>,
    bytes: u64,
}

impl RunBytes {
    fn release(&mut self, n: u64) {
        let n = n.min(self.bytes);
        self.bytes -= n;
        if let Some(b) = &self.budget {
            crate::container::Budget::release(&**b, n);
        }
    }
}

impl Drop for RunBytes {
    fn drop(&mut self) {
        let n = self.bytes;
        self.release(n);
    }
}

/// Runs merged at once in the final merge; more runs are merged in stages
/// (RD4-05: a bounded number of open files and read buffers). Level-0 runs
/// are merged into one run of the next level as soon as there are this
/// many, upper levels at four times as many (levels grow logarithmically).
const RUN_FANIN: usize = 32;
/// Runs merged at once by the final merge (each open run holds its read
/// buffer and one decoded chunk of `RUN_CHUNK_TARGET`, about 128 KiB).
const RUN_FINAL_FANIN: usize = 4 * RUN_FANIN;
/// Read buffer of one run during the merge.
const RUN_BUFFER: usize = 64 << 10;

/// One sorted run: an anonymous container file, its size and its merge
/// level (0: written from memory).
struct SortRun {
    file: std::fs::File,
    bytes: u64,
    level: u32,
}

/// Merges `group` into one run of level `level` (the bytes of the group are
/// released once the new run is complete).
fn merge_runs(group: Vec<SortRun>, level: u32, desc: &[bool], dir: &std::path::Path, bytes: &mut RunBytes) -> Result<SortRun> {
    let freed: u64 = group.iter().map(|r| r.bytes).sum();
    let mut m = RunMerge::open(group, desc, RunBytes { budget: None, bytes: 0 }, dir)?;
    let mut merged = write_run(dir, bytes, std::iter::from_fn(|| m.pop().transpose()))?;
    drop(m);
    bytes.release(freed);
    merged.level = level;
    Ok(merged)
}

/// Adds a run; full levels are merged upwards.
fn push_run(runs: &mut Vec<SortRun>, run: SortRun, desc: &[bool], dir: &std::path::Path, bytes: &mut RunBytes) -> Result<()> {
    runs.push(run);
    loop {
        let level = runs.last().map(|r| r.level).unwrap_or(0);
        let same: Vec<usize> = (0..runs.len()).filter(|i| runs[*i].level == level).collect();
        // RY-02: upper levels merge only at 4 × RUN_FANIN runs – a merge of
        // a level needs the space of its runs once more, so large merges
        // are rare (open files stay bounded: 32 + 128 per further level)
        if same.len() < if level == 0 { RUN_FANIN } else { 4 * RUN_FANIN } {
            return Ok(());
        }
        // runs of a level are contiguous at the end (levels only grow)
        let group: Vec<SortRun> = runs.drain(same[0]..).collect();
        let merged = merge_runs(group, level + 1, desc, dir, bytes)?;
        runs.push(merged);
    }
}

/// R4-C: k-way merge of sorted runs (`[key…, idx, payload]` per item: the
/// payload is the u of the CLASSIC path or the finished row, RUN6) under
/// the order of `order_limit_indices` (keys of one sortable class per
/// column, ties by the original position). The runs are anonymous
/// temporary files (RD4-01): they disappear with the merge, also after a
/// crash. `skip`/`take`: the OFFSET/LIMIT window of the merged order.
pub struct RunMerge {
    readers: Vec<crate::container::ContainerReader>,
    heads: Vec<Option<(Vec<Val>, i64, Val)>>,
    desc: Vec<bool>,
    skip: usize,
    take: Option<usize>,
    /// released when the merge is dropped
    _bytes: RunBytes,
}

fn run_item(v: Val, keys: usize) -> Result<(Vec<Val>, i64, Val)> {
    let bad = || Error::Corruption("sort run item malformed".into());
    let Val::List(mut l) = v else { return Err(bad()) };
    if l.len() != keys + 2 {
        return Err(bad());
    }
    let payload = l.pop().ok_or_else(bad)?;
    let idx = match l.pop() {
        Some(Val::Int(i)) => i,
        _ => return Err(bad()),
    };
    Ok((l, idx, payload))
}

/// RUN6: sort class of a key. Within one class Python `<` is a total
/// order, which the external sort reproduces (numbers of every kind
/// compared exactly, str by code points, bytes by bytes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KeyClass {
    Num,
    Str,
    Bytes,
    Other,
}

/// Classes seen in one sort column: two classes Python cannot compare make
/// the sort of `sql()` fail (`conflict`); NaN and containers have no total
/// order (`unsortable`), so they can be sorted only in memory.
#[derive(Clone, Default, Debug)]
struct ColClass {
    seen: Option<KeyClass>,
    conflict: bool,
    unsortable: bool,
}

impl ColClass {
    fn add(&mut self, v: &Val) {
        let c = match v {
            Val::None => return,
            Val::Float(f) => {
                if f.is_nan() {
                    self.unsortable = true;
                }
                KeyClass::Num
            }
            Val::Int(_) | Val::Big(_) | Val::Bool(_) => KeyClass::Num,
            Val::Str(_) => KeyClass::Str,
            Val::Bytes(_) => KeyClass::Bytes,
            _ => {
                self.unsortable = true;
                KeyClass::Other
            }
        };
        match self.seen {
            None => self.seen = Some(c),
            Some(s) if s != c => self.conflict = true,
            _ => {}
        }
    }
}

/// Order of one sort column for keys of one class (None after every value).
fn ext_cmp_key(a: &Val, b: &Val) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Val::None, Val::None) => Equal,
        (Val::None, _) => Greater,
        (_, Val::None) => Less,
        (Val::Int(x), Val::Int(y)) => x.cmp(y),
        (Val::Str(x), Val::Str(y)) => x.cmp(y),
        (Val::Bytes(x), Val::Bytes(y)) => x.cmp(y),
        _ => {
            if py_lt(a, b, "<").unwrap_or(false) {
                Less
            } else if py_lt(b, a, "<").unwrap_or(false) {
                Greater
            } else {
                Equal
            }
        }
    }
}

fn run_rank(desc: &[bool], ak: &[Val], ai: i64, bk: &[Val], bi: i64) -> std::cmp::Ordering {
    for (c, d) in desc.iter().enumerate() {
        let ord = ext_cmp_key(&ak[c], &bk[c]);
        let ord = if *d { ord.reverse() } else { ord };
        if ord.is_ne() {
            return ord;
        }
    }
    ai.cmp(&bi)
}

impl RunMerge {
    /// Opens the runs for one merge (their bytes go to `bytes`).
    fn open(runs: Vec<SortRun>, desc: &[bool], bytes: RunBytes, dir: &std::path::Path) -> Result<RunMerge> {
        let mut m = RunMerge { readers: Vec::with_capacity(runs.len()), heads: Vec::with_capacity(runs.len()), desc: desc.to_vec(), skip: 0, take: None, _bytes: bytes };
        for r in runs {
            let mut rd = crate::container::ContainerReader::from_file(r.file, dir, RUN_BUFFER)?;
            let h = match rd.next_value()? {
                Some(v) => Some(run_item(v, desc.len())?),
                None => None,
            };
            m.readers.push(rd);
            m.heads.push(h);
        }
        Ok(m)
    }

    /// The smallest item of all runs.
    fn pop(&mut self) -> Result<Option<(Vec<Val>, i64, Val)>> {
        let keys = self.desc.len();
        let mut best: Option<usize> = None;
        for i in 0..self.heads.len() {
            let Some(h) = &self.heads[i] else { continue };
            best = match best {
                Some(b) => {
                    let hb = self.heads[b].as_ref().unwrap();
                    if run_rank(&self.desc, &hb.0, hb.1, &h.0, h.1).is_le() { Some(b) } else { Some(i) }
                }
                None => Some(i),
            };
        }
        let Some(b) = best else { return Ok(None) };
        let item = self.heads[b].take().unwrap();
        self.heads[b] = match self.readers[b].next_value()? {
            Some(v) => Some(run_item(v, keys)?),
            None => None,
        };
        Ok(Some(item))
    }

    /// The next at most `n` payloads of the OFFSET/LIMIT window.
    fn next_payloads(&mut self, n: usize) -> Result<Vec<Val>> {
        let mut out = Vec::with_capacity(n.min(4096));
        while out.len() < n && self.take != Some(0) {
            let Some((_, _, p)) = self.pop()? else { break };
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            if let Some(t) = &mut self.take {
                *t -= 1;
            }
            out.push(p);
        }
        Ok(out)
    }

    fn next_chunk(&mut self, n: usize) -> Result<Vec<Nat>> {
        self.next_payloads(n)?.into_iter().map(|p| p.as_nat().ok_or_else(|| Error::Corruption("sort run item malformed".into()))).collect()
    }
}

/// RR6-01: estimated bytes of one entry of a top-k window (key, position,
/// u; the buffer holds up to two windows).
const TOPK_ENTRY_COST: usize = 256;

/// RUN6: a producer that hands each finished row of a query to the sink.
type RowFeed<'f> = &'f mut dyn FnMut(&mut dyn FnMut(Val) -> Result<()>) -> Result<()>;

/// RUN6: bound of what a collecting call (`sql()`, `execute()`, `search()`,
/// `sql_numpy`) holds in memory: the estimated bytes of the rows (and of
/// sort keys and aggregation groups) against `[query] max_result_bytes`.
/// Checked while collecting, so the call ends before the memory is used.
pub struct ResultGuard {
    limit: u64,
    used: u64,
    rows: u64,
    /// RUN¹⁰: checkpoint of the RAM limiter per MiB of collected bytes
    meter: crate::limiter::Meter,
}

impl ResultGuard {
    pub fn new(db: &Db) -> ResultGuard {
        ResultGuard {
            limit: db.config.query_max_result_bytes,
            used: 0,
            rows: 0,
            meter: crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Collect),
        }
    }

    /// Books one collected row.
    pub fn add(&mut self, row: &Val) -> Result<()> {
        self.rows += 1;
        self.add_bytes((std::mem::size_of::<Val>() + crate::db::val_heap(row)) as u64)
    }

    /// Books `n` further bytes (keys, groups).
    pub fn add_bytes(&mut self, n: u64) -> Result<()> {
        self.used += n;
        self.check()?;
        self.meter.grow(n).map(|_| ())
    }

    /// Sets the booked bytes of a structure that grows by itself.
    fn set_total(&mut self, n: u64) -> Result<()> {
        let grown = n.saturating_sub(self.used);
        self.used = self.used.max(n);
        self.check()?;
        self.meter.grow(grown).map(|_| ())
    }

    fn check(&self) -> Result<()> {
        if self.limit == 0 || self.used <= self.limit {
            return Ok(());
        }
        let rows = if self.rows > 0 { format!(" after {} rows", self.rows) } else { String::new() };
        Err(Error::QueryLimit(format!(
            "query result too large: about {} bytes{rows} (rows, sort keys or aggregation groups) exceed [query] \
             max_result_bytes = {}; sql(), execute(), search(), sql_numpy() and aggregations hold their whole result in \
             memory – for many rows use db.sql_iter(), db.sql_to_file() or db.submit_sql(), for many groups a coarser \
             GROUP BY or a WHERE; or narrow the query (LIMIT/WHERE) or raise max_result_bytes (0 = no limit)",
            self.used, self.limit
        )))
    }
}

/// RUN6: bounded sort of ORDER BY for the streaming consumers (`sql_iter`,
/// `sql_numpy`, `sql_to_file`, `submit_sql`) in every query path. Items
/// (keys, original position, payload) are collected in memory up to
/// `[results] sort_memory_bytes`; beyond, each full buffer is sorted and
/// written as a run to the result directory. Nothing is sorted in memory
/// beyond the budget:
/// * within the budget the order is that of `order_limit_indices` itself
///   (identical to `sql()`, also for NaN and its errors);
/// * beyond the budget the keys of each column must be of one class
///   (numbers, str or bytes; None anywhere); two classes Python cannot
///   compare give the error of `sql()`, NaN or containers a clear error.
struct ExtSorter {
    ordering: Vec<(String, bool)>,
    desc: Vec<bool>,
    max_mem: usize,
    dir: std::path::PathBuf,
    budget_limit: u64,
    bytes: RunBytes,
    runs: Vec<SortRun>,
    buf: Vec<(Vec<Val>, i64, Val)>,
    mem: usize,
    cols: Vec<ColClass>,
    /// the data cannot be sorted beyond the budget: items are no longer
    /// kept (the scan goes on for the errors of later items)
    blocked: bool,
    /// RUN¹⁰: elastic job of the RAM limiter; `SHRINK` writes the buffer
    /// as a run at once and frees its capacity
    meter: crate::limiter::Meter,
}

/// What a finished `ExtSorter` yields.
enum Sorted {
    /// payloads in their final order (sorted in memory, window applied)
    List(Vec<Val>),
    Merge(Box<RunMerge>),
}

impl ExtSorter {
    fn new(db: &Db, ordering: Vec<(String, bool)>) -> ExtSorter {
        let desc = ordering.iter().map(|(_, d)| *d).collect();
        let cols = vec![ColClass::default(); ordering.len()];
        ExtSorter {
            ordering,
            desc,
            max_mem: db.config.results_sort_memory_bytes.max(1 << 16),
            dir: crate::results::result_dir(&db.config),
            budget_limit: db.config.results_budget_bytes,
            bytes: RunBytes { budget: None, bytes: 0 },
            runs: Vec::new(),
            buf: Vec::new(),
            mem: 0,
            cols,
            blocked: false,
            meter: crate::limiter::Meter::new(crate::limiter::JobKind::Elastic, crate::limiter::JobPath::Sort),
        }
    }

    fn spillable(&self) -> bool {
        !self.cols.iter().any(|c| c.conflict || c.unsortable)
    }

    /// Adds one item; `cost` = estimated heap bytes of keys and payload.
    fn push(&mut self, keys: Vec<Val>, idx: i64, payload: Val, cost: usize) -> Result<()> {
        for (c, k) in self.cols.iter_mut().zip(&keys) {
            c.add(k);
        }
        if self.blocked {
            return Ok(());
        }
        let c = cost + 64 + 32 * keys.len();
        self.mem += c;
        self.buf.push((keys, idx, payload));
        if self.meter.grow(c as u64)? && self.spillable() && !self.buf.is_empty() {
            // SHRINK: this run goes to the result directory now and the
            // buffer gives its memory back
            self.spill()?;
            self.buf = Vec::new();
            return Ok(());
        }
        if self.mem >= self.max_mem {
            if !self.spillable() {
                self.blocked = true;
                self.buf = Vec::new();
                return Ok(());
            }
            self.spill()?;
        }
        Ok(())
    }

    fn spill(&mut self) -> Result<()> {
        let desc = self.desc.clone();
        self.buf.sort_by(|a, b| run_rank(&desc, &a.0, a.1, &b.0, b.1));
        if self.bytes.budget.is_none() {
            self.bytes.budget = Some(crate::results::dir_budget(&self.dir, self.budget_limit).map_err(|e| sort_dir_error(&self.dir, e))?);
        }
        // the buffer keeps its capacity for the next run (no regrowth)
        let ExtSorter { dir, bytes, buf, .. } = self;
        let run = write_run(dir, bytes, buf.drain(..).map(Ok)).map_err(|e| sort_dir_error(dir, e))?;
        push_run(&mut self.runs, run, &desc, &self.dir, &mut self.bytes).map_err(|e| sort_dir_error(&self.dir, e))?;
        self.mem = 0;
        Ok(())
    }

    /// The error of data that cannot be sorted beyond the budget: the one
    /// `sql()` raises for incomparable keys (the last such column, as the
    /// sort works from the last key to the first), else NaN/containers.
    fn unsortable_error(&self) -> Error {
        if let Some((name, _)) = self.ordering.iter().zip(&self.cols).rev().find(|(_, c)| c.conflict) {
            return plan_err(format!("ORDER BY {} compares incompatible values", repr(&name.0)));
        }
        let names: Vec<String> = self.ordering.iter().zip(&self.cols).filter(|(_, c)| c.unsortable).map(|(o, _)| o.0.clone()).collect();
        Error::QueryLimit(format!(
            "ORDER BY {}: the sort keys exceed [results] sort_memory_bytes = {} and contain NaN or containers, which have no \
             total order for a sort in runs; filter NaN out, add LIMIT/WHERE, sort fewer rows or raise sort_memory_bytes \
             (sql() sorts in memory)",
            names.join(", "),
            self.max_mem
        ))
    }

    /// Finishes the sort with the OFFSET/LIMIT window.
    fn finish(mut self, order_by: Option<&Val>, limit: &Val, offset: &Val) -> Result<Sorted> {
        if self.blocked || (!self.runs.is_empty() && !self.spillable()) {
            return Err(self.unsortable_error());
        }
        if self.runs.is_empty() {
            // in memory: exactly the order and errors of `sql()`
            let names: Vec<String> = self.ordering.iter().map(|(n, _)| n.clone()).collect();
            let buf = std::mem::take(&mut self.buf);
            let picked = order_limit_indices(buf.len(), order_by, limit, offset, &mut |i, name| {
                Ok(names.iter().position(|n| n == name).map(|p| buf[i].0[p].clone()).unwrap_or(Val::None))
            })?;
            let mut slots: Vec<Option<Val>> = buf.into_iter().map(|(_, _, p)| Some(p)).collect();
            return Ok(Sorted::List(picked.into_iter().map(|i| slots[i].take().unwrap()).collect()));
        }
        // the window as `slice_simple` computes it (same errors)
        let off = if offset.is_truthy() { py_int(offset)? } else { BigInt::zero() };
        let skip = off.max(BigInt::zero()).to_usize().unwrap_or(usize::MAX);
        let take = if limit.is_none() { None } else { Some(py_int(limit)?.max(BigInt::zero()).to_usize().unwrap_or(usize::MAX)) };
        if !self.buf.is_empty() {
            self.spill()?;
        }
        let desc = self.desc.clone();
        while self.runs.len() > RUN_FINAL_FANIN {
            let k = (self.runs.len() - RUN_FINAL_FANIN + 1).min(RUN_FINAL_FANIN);
            self.runs.sort_by_key(|r| r.bytes);
            let group: Vec<SortRun> = self.runs.drain(..k).collect();
            let level = group.iter().map(|r| r.level).max().unwrap_or(0) + 1;
            let merged = merge_runs(group, level, &desc, &self.dir, &mut self.bytes).map_err(|e| sort_dir_error(&self.dir, e))?;
            self.runs.push(merged);
        }
        let runs = std::mem::take(&mut self.runs);
        let bytes = std::mem::replace(&mut self.bytes, RunBytes { budget: None, bytes: 0 });
        let mut m = RunMerge::open(runs, &desc, bytes, &self.dir)?;
        m.skip = skip;
        m.take = take;
        Ok(Sorted::Merge(Box::new(m)))
    }
}

/// RD4-06 (RUN6): the errors of the sort runs name the cause and the
/// remedies; the error class (`OSError` with errno, `DatabaseSizeLimitError`)
/// stays the same.
fn sort_dir_error(dir: &std::path::Path, e: Error) -> Error {
    let help = "remedies: raise [results] budget_bytes or sort_memory_bytes, free disk space or give write access \
                (or set [results] temp_dir to another directory), run nhydb.cleanup_results(), or narrow the query \
                (LIMIT/WHERE)";
    let what = format!("ORDER BY without enough memory needs sort runs in the result directory {}", dir.display());
    match e {
        Error::SizeLimit(m) => Error::SizeLimit(format!("{what}: {m}; {help}")),
        Error::IoPath(io, p) => Error::IoHint(Box::new((io, Some(p.into_path_buf()), format!("{what} ({help})")))),
        Error::Io(io) => Error::IoHint(Box::new((io, None, format!("{what} ({help})")))),
        other => other,
    }
}

/// Writes sorted items as one anonymous run (bytes booked in `bytes`).
fn write_run(dir: &std::path::Path, bytes: &mut RunBytes, items: impl Iterator<Item = Result<(Vec<Val>, i64, Val)>>) -> Result<SortRun> {
    let budget = bytes.budget.clone().map(|b| b as std::sync::Arc<dyn crate::container::Budget>);
    let mut w = crate::container::ContainerWriter::create_anonymous(dir, "nhydb.sortrun/1", false, budget)?;
    for it in items {
        let (k, idx, payload) = it?;
        let mut item = k;
        item.push(Val::Int(idx));
        item.push(payload);
        w.push(&Val::List(item))?;
    }
    let (file, n) = w.finish_anonymous(&Val::map())?;
    bytes.bytes += n;
    Ok(SortRun { file, bytes: n, level: 0 })
}

/// R4-B: where the rows of a `RowCursor` come from.
pub enum RowSource {
    /// complete rows
    Rows(std::collections::VecDeque<Val>),
    /// CLASSIC fields rows of the u in `us` (built per chunk)
    Classic { realm: Nat, layout_id: u64, lane: Lane, us: UWindow, fields: Vec<String> },
    /// general fields rows of the N in `ns[lo..hi]`; the other N are only
    /// checked (`row_precheck`), as `sliced_rows` does
    General { ns: std::collections::VecDeque<Nat>, lo: usize, hi: usize, pos: usize, fields: Vec<String> },
    /// RUN6: finished rows from a merge of sorted runs (bounded ORDER BY)
    Sorted(Box<RunMerge>),
}

impl RowSource {
    fn classic(realm: Nat, lay: &LayoutDef, lane: Lane, us: UWindow, fields: Vec<String>) -> RowSource {
        RowSource::Classic { realm, layout_id: lay.layout_id, lane, us, fields }
    }
}

/// R4-B: rows of a query in chunks (`QueryEngine::open_rows`). The cursor
/// holds no reference to the database; every `next_batch` gets an engine.
pub struct RowCursor {
    src: RowSource,
    /// the result of `execute` without its rows (count, select, fastpath …)
    pub result: Val,
}

impl RowCursor {
    /// A cursor over complete rows.
    pub fn from_rows(rows: Vec<Val>) -> RowCursor {
        RowCursor { src: RowSource::Rows(rows.into()), result: Val::map() }
    }

    /// The next at most `n` rows (empty: done). `anchor_sink`: see
    /// `QueryEngine::classic_field_rows` (a caller that collects every row
    /// keeps the error order of `execute`; `None` raises per chunk).
    pub fn next_batch(&mut self, eng: &QueryEngine<'_>, n: usize, anchor_sink: Option<&mut Option<Error>>) -> Result<Vec<Val>> {
        let n = n.max(1);
        match &mut self.src {
            RowSource::Rows(q) => {
                let k = n.min(q.len());
                Ok(q.drain(..k).collect())
            }
            RowSource::Sorted(m) => m.next_payloads(n),
            RowSource::Classic { realm, layout_id, lane, us, fields } => {
                let chunk = us.next_chunk(n)?;
                if chunk.is_empty() {
                    return Ok(Vec::new());
                }
                let lay = eng.db.layouts.get(layout_id).ok_or_else(|| Error::Schema("the layout of the query no longer exists".into()))?;
                let mut sink = anchor_sink;
                match eng.classic_field_rows(realm, lay, &chunk, fields, *lane, sink.as_deref_mut())? {
                    Some(rows) => Ok(rows),
                    None => {
                        let raws = eng.classic_rows(realm, lay, &chunk, fields, *lane)?;
                        eng.classic_build(lay, raws, fields, *lane, sink)
                    }
                }
            }
            RowSource::General { ns, lo, hi, pos, fields } => {
                let mut out = Vec::new();
                while out.len() < n {
                    let Some(nn) = ns.pop_front() else { break };
                    let i = *pos;
                    *pos += 1;
                    if i >= *lo && i < *hi {
                        let rec = eng.record_envelope_light(&nn)?;
                        // RC4-02: a record without any cell left (deleted
                        // after the open) is skipped like in the CLASSIC u
                        // path, not returned as a row of None; a u with only
                        // sparse cells stays a row as in `sql()`
                        if rec.get("present").is_some_and(|p| !p.is_truthy()) {
                            continue;
                        }
                        let mut row = vmap! {"record_N" => rec.get("record_N").cloned().unwrap_or(Val::None)};
                        for f in fields.iter() {
                            row.set(f.clone(), record_value(&rec, f));
                        }
                        out.push(row);
                    } else {
                        eng.row_precheck(&nn, GetMode::Record)?;
                    }
                }
                Ok(out)
            }
        }
    }
}

/// R4-A5: candidates of the CLASSIC u fast path – an explicit sorted u
/// list, or the metadata universe as sorted disjoint ranges that is expanded
/// only where every u is needed (a LIMIT window is cut from the ranges).
struct UCands {
    list: std::cell::OnceCell<Vec<Nat>>,
    ranges: Option<Vec<(Nat, Nat)>>,
    len: usize,
}

impl UCands {
    fn list(us: Vec<Nat>) -> Self {
        let len = us.len();
        UCands { list: std::cell::OnceCell::from(us), ranges: None, len }
    }

    /// `None` if the number of u does not fit into `usize`.
    fn ranges(ranges: Vec<(Nat, Nat)>) -> Option<Self> {
        let mut len: usize = 0;
        for (a, b) in &ranges {
            let n = b.checked_sub(a)?.add_u64(1).as_u64().and_then(|n| usize::try_from(n).ok())?;
            len = len.checked_add(n)?;
        }
        Some(UCands { list: std::cell::OnceCell::new(), ranges: Some(ranges), len })
    }

    fn len(&self) -> usize {
        self.len
    }

    /// The explicit list, `None` for an unexpanded universe.
    fn list_ref(&self) -> Option<&[Nat]> {
        if self.ranges.is_some() { None } else { self.list.get().map(|v| v.as_slice()) }
    }

    /// Every u (expands the ranges once).
    fn all(&self) -> &[Nat] {
        self.list.get_or_init(|| expand_ranges(self.ranges.as_deref().unwrap_or(&[])))
    }

    /// The LIMIT/OFFSET window as a lazy `UWindow` (same errors as
    /// `window`).
    fn lazy_window(&self, limit: &Val, offset: &Val) -> Result<UWindow> {
        let Some(ranges) = self.ranges.as_ref().filter(|_| self.list.get().is_none()) else {
            return Ok(UWindow::List(self.window(limit, offset)?.into()));
        };
        let off = if offset.is_truthy() { py_int(offset)? } else { BigInt::zero() };
        let off = off.max(BigInt::zero()).to_usize().unwrap_or(usize::MAX);
        let mut take = self.len.saturating_sub(off);
        if !limit.is_none() {
            let lim = py_int(limit)?.max(BigInt::zero()).to_usize().unwrap_or(usize::MAX);
            take = take.min(lim);
        }
        Ok(UWindow::Ranges { ranges: ranges.clone(), skip: off, take })
    }

    /// `slice_simple` of all u (same errors), without expanding the ranges.
    fn window(&self, limit: &Val, offset: &Val) -> Result<Vec<Nat>> {
        let Some(ranges) = self.ranges.as_deref().filter(|_| self.list.get().is_none()) else {
            return slice_simple(self.all().to_vec(), limit, offset);
        };
        let off = if offset.is_truthy() { py_int(offset)? } else { BigInt::zero() };
        let off = off.max(BigInt::zero()).to_usize().unwrap_or(usize::MAX);
        let mut take = self.len.saturating_sub(off);
        if !limit.is_none() {
            let lim = py_int(limit)?.max(BigInt::zero()).to_usize().unwrap_or(usize::MAX);
            take = take.min(lim);
        }
        let mut out = Vec::with_capacity(take);
        let mut skip = off;
        for (a, b) in ranges {
            if out.len() == take {
                break;
            }
            let n = b.checked_sub(a).unwrap().add_u64(1).as_u64().unwrap() as usize;
            if skip >= n {
                skip -= n;
                continue;
            }
            let mut u = a.add_u64(skip as u64);
            skip = 0;
            while &u <= b && out.len() < take {
                out.push(u.clone());
                u = u.add_u64(1);
            }
        }
        Ok(out)
    }
}

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

    /// F010: an index on `v` answers a predicate on `name` exactly like the
    /// scan only if every layout of the realm is CLASSIC and shows `v` under
    /// this name (and no other v under the same name). Otherwise the scan
    /// (the reference semantics) is used.
    fn index_semantics_ok(&self, realm: &Nat, name: &str, v: &Nat) -> bool {
        let lays = self.db.realm_layouts(realm);
        if lays.is_empty() {
            return false;
        }
        let ext_label = self.db.extension_defs.get(&(realm.clone(), v.clone())).map(|e| e.field.name.clone());
        for l in lays {
            if l.kind != LayoutKind::Classic {
                return false;
            }
            let shape = l.shape_id.and_then(|s| self.db.shapes.get(&s));
            let label = match shape.and_then(|sh| sh.by_v(v)).map(|f| f.name.clone()).or_else(|| ext_label.clone()) {
                Some(n) if !n.is_empty() => n,
                _ => format!("v{v}"),
            };
            if label != name {
                return false;
            }
            if shape.and_then(|sh| sh.by_name(name)).is_some_and(|f| &f.v != v) {
                return false;
            }
        }
        if self.db.extension_name_map.get(&(realm.clone(), name.to_string())).is_some_and(|ev| ev != v) {
            return false;
        }
        true
    }

    /// Every u of the realm is routed to some layout (a layout starting at
    /// u=1 without upper bound), so index postings need no routing filter.
    fn routes_every_u(&self, realm: &Nat) -> bool {
        self.db.realm_layouts(realm).iter().any(|l| l.u_start <= Nat::ONE && l.u_end.is_none())
    }

    /// Index postings restricted to u that the scan can see (routed to a
    /// layout); no-op when every u is routed.
    fn routed_us(&self, realm: &Nat, mut us: Vec<Nat>) -> Vec<Nat> {
        if !self.routes_every_u(realm) {
            us.retain(|u| self.db.layout_at(realm, u).is_some());
        }
        us
    }

    /// `_find_usable_index`
    fn find_usable_index(&self, field: &str, ctx: &QueryContext, op: &str, options: &Val, value: &Val) -> Option<&'a IndexDef> {
        let realm = ctx.realm_nat()?;
        let v = self.db.resolve_v(&realm, &Val::str(field)).ok()?;
        if !self.index_semantics_ok(&realm, field, &v) {
            return None;
        }
        for idx in self.db.index_defs.values() {
            if idx.realm_id != realm || idx.field_v() != &v {
                continue;
            }
            match textindex::index_supports_predicate(idx.kind, op, &idx.options, options, value) {
                Ok(true) => {}
                _ => continue,
            }
            // AP9: fresh, or stale with an overlay (base \ mask ∪ overlay)
            if self.db.index_usable(idx.index_id, ctx.lane) {
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
        for (est, _p, leaf, idx) in ranked {
            let Expr::Field { name, op, value, value2, options } = leaf else { unreachable!() };
            // R4-A4: a much larger posting is only probed for the u found so far
            if let Some(a) = acc.as_ref().filter(|a| est.as_ref().is_some_and(|e| *e > BigInt::from(a.len()) * 4)) {
                if let Some(m) = self.db.index_candidates_within(idx.index_id, op, value, value2, ctx.lane, a)? {
                    acc = Some(m);
                    if acc.as_ref().is_some_and(|a| a.is_empty()) {
                        break;
                    }
                    continue;
                }
            }
            let mut us = self.db.index_candidates(idx.index_id, op, value, value2, ctx.lane, options)?;
            if idx.kind == IndexKind::Trigram && (op == "contains" || op == "icontains") {
                us = self.verify_text(&realm, us, name, op, value, options, ctx.lane)?;
            }
            let lay = self.plain_classic_layout(ctx).unwrap();
            us.retain(|u| lay.contains_u(u));
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

    /// R4-A1: AND/OR/NOT trees whose leaves are all indexed FIELD predicates
    /// over one plain CLASSIC layout, evaluated in compact u space (the
    /// general path maps every posting u to its record N through the layout,
    /// ≈ 0.7 µs per hit). Same evaluation order, short-circuit and set
    /// semantics as `eval_planned`; CLASSIC record_N is strictly increasing
    /// in u, so u order is N order. NOT needs the metadata universe
    /// (`classic_universe_ranges`); without it the general path is used.
    /// `None`: not applicable (nothing was evaluated).
    fn classic_u_tree(&self, expr: &Expr, plan: Option<&Val>, ctx: &QueryContext) -> Result<Option<Vec<Nat>>> {
        let Some(lay) = self.plain_classic_layout(ctx) else { return Ok(None) };
        // NOT (and a field scan) need the exact universe: with overwritten
        // segments, DELETE entries or SPARSE cells it comes from the winner
        // view (`classic_universe_ranges`; `None` there = general path)
        let mut has_not = false;
        if !self.u_tree_supported(expr, ctx, &mut has_not) {
            return Ok(None);
        }
        let mut universe: Option<Vec<(Nat, Nat)>> = None;
        self.eval_u(expr, plan, ctx, lay, &mut universe)
    }

    fn u_tree_supported(&self, expr: &Expr, ctx: &QueryContext, has_not: &mut bool) -> bool {
        match expr {
            Expr::Bool { op, children } if op == "not" => {
                *has_not = true;
                children.first().is_some_and(|c| self.u_tree_supported(c, ctx, has_not))
            }
            Expr::Bool { op, .. } if op == "and" || op == "or" => {
                let mut kids = Vec::new();
                flatten_bool(expr, op, &mut kids);
                kids.iter().all(|k| self.u_tree_supported(k, ctx, has_not))
            }
            Expr::Field { name, op, value, options, .. } => {
                self.find_usable_index(name, ctx, op, options, value).is_some() || {
                    // R4-B: field scan in u space (metadata universe needed)
                    *has_not = true;
                    self.scan_field_v(name, ctx).is_some()
                }
            }
            _ => false,
        }
    }

    /// R4-B: the local v of `name` if a FIELD predicate on it can be
    /// evaluated by reading only that field of the CLASSIC records: every
    /// layout of the realm shows v under this name (`index_semantics_ok`,
    /// as the scan of the general path sees it).
    fn scan_field_v(&self, name: &str, ctx: &QueryContext) -> Option<Nat> {
        let realm = ctx.realm_nat()?;
        let v = self.db.resolve_v(&realm, &Val::str(name)).ok()?;
        self.index_semantics_ok(&realm, name, &v).then_some(v)
    }

    fn eval_u(&self, expr: &Expr, plan: Option<&Val>, ctx: &QueryContext, lay: &LayoutDef, universe: &mut Option<Vec<(Nat, Nat)>>) -> Result<Option<Vec<Nat>>> {
        self.eval_u_within(expr, plan, ctx, lay, universe, None)
    }

    /// `within`: an AND sibling already restricted the result to these u
    /// (sorted); a field scan then reads only them (same intersection).
    fn eval_u_within(&self, expr: &Expr, plan: Option<&Val>, ctx: &QueryContext, lay: &LayoutDef, universe: &mut Option<Vec<(Nat, Nat)>>, within: Option<&[Nat]>) -> Result<Option<Vec<Nat>>> {
        let us = self.eval_u_node(expr, plan, ctx, lay, universe, within)?;
        // RUN¹⁰: checkpoint of the RAM limiter after every large u set
        if let Some(us) = &us {
            crate::limiter::job::step(
                crate::limiter::JobKind::Rigid,
                crate::limiter::JobPath::Candidates,
                (us.capacity() * std::mem::size_of::<Nat>()) as u64,
            )?;
        }
        Ok(us)
    }

    fn eval_u_node(&self, expr: &Expr, plan: Option<&Val>, ctx: &QueryContext, lay: &LayoutDef, universe: &mut Option<Vec<(Nat, Nat)>>, within: Option<&[Nat]>) -> Result<Option<Vec<Nat>>> {
        match expr {
            Expr::Bool { op, children } => {
                if op == "not" {
                    let Some(child) = self.eval_u(&children[0], Self::child_plan(plan, 0), ctx, lay, universe)? else {
                        return Ok(None);
                    };
                    if universe.is_none() {
                        match self.classic_universe_ranges(ctx, false)? {
                            Some((_, r)) => *universe = Some(r),
                            None => return Ok(None),
                        }
                    }
                    return Ok(Some(setops::ranges_minus_sorted(universe.as_ref().unwrap(), &child)));
                }
                if children.is_empty() {
                    return Ok(Some(Vec::new()));
                }
                let mut kids = Vec::new();
                flatten_bool(expr, op, &mut kids);
                let given: Vec<Option<&Val>> = (0..kids.len()).map(|i| Self::child_plan(plan, i)).collect();
                let mut computed: Vec<Option<Val>> = vec![None; kids.len()];
                let mut order: Vec<usize> = (0..kids.len()).collect();
                if op == "and" {
                    for (i, k) in kids.iter().enumerate() {
                        if given[i].is_none() {
                            computed[i] = Some(self.plan_expr(k, ctx, Domain::Record)?);
                        }
                    }
                    let mut ranked = Vec::with_capacity(kids.len());
                    for i in 0..kids.len() {
                        let p = given[i].or(computed[i].as_ref()).unwrap();
                        ranked.push((est_key(p.get("estimated_rows")), est_key(p.get("estimated_cost")), i));
                    }
                    ranked.sort_by(|a, b| cmp_est(&a.0, &b.0).then_with(|| cmp_est(&a.1, &b.1)).then(a.2.cmp(&b.2)));
                    order = ranked.into_iter().map(|x| x.2).collect();
                }
                let kid_plan = |i: usize| given[i].or(computed[i].as_ref());
                let Some(mut acc) = self.eval_u(kids[order[0]], kid_plan(order[0]), ctx, lay, universe)? else {
                    return Ok(None);
                };
                for &i in &order[1..] {
                    if op == "and" && acc.is_empty() {
                        break;
                    }
                    let w = if op == "and" { Some(acc.as_slice()) } else { None };
                    let Some(child) = self.eval_u_within(kids[i], kid_plan(i), ctx, lay, universe, w)? else {
                        return Ok(None);
                    };
                    acc = if op == "and" { setops::intersect_sorted(&acc, &child) } else { setops::union_sorted(&acc, &child) };
                }
                Ok(Some(acc))
            }
            Expr::Field { name, op, value, value2, options } => {
                let (access, idx, _rows, _cost) = self.choose_field_access(name, op, value, value2, options, ctx, Domain::Record);
                let Some(idx) = idx else {
                    return self.scan_field_u(name, op, value, value2, options, ctx, lay, universe, within);
                };
                let realm = ctx.realm_nat().unwrap();
                let mut us = self.db.index_candidates(idx.index_id, op, value, value2, ctx.lane, options)?;
                if access == "TRIGRAM_INDEX" {
                    us = self.verify_text(&realm, us, name, op, value, options, ctx.lane)?;
                }
                let mut us = self.routed_us(&realm, us);
                us.retain(|u| lay.contains_u(u));
                Ok(Some(setops::sorted_unique(us)))
            }
            _ => Ok(None),
        }
    }

    /// R4-A3: a fresh VALUE index on `name` that holds every record of the
    /// plain CLASSIC universe of `ctx` exactly once: every u is routed, no
    /// overlays, and (checked by the walk) the posting counts sum up to the
    /// number of records. A VALUE index stores one key per record (lists and
    /// dicts as one container key), so every record then has a value.
    /// Returns the index and the record count.
    fn covering_value_index(&self, name: &str, ctx: &QueryContext) -> Result<Option<(&'a IndexDef, u64)>> {
        let Some(realm) = ctx.realm_nat() else { return Ok(None) };
        if !self.routes_every_u(&realm) {
            return Ok(None);
        }
        // AP9: the key walk reads the tree directly – only a fresh index
        // (a stale one takes the regular path through its overlay)
        let Some(idx) = self
            .find_usable_index(name, ctx, "eq", &Val::None, &Val::None)
            .filter(|i| i.kind == IndexKind::Value && self.db.index_is_fresh(i.index_id, ctx.lane))
        else {
            return Ok(None);
        };
        let Some((_, ranges)) = self.classic_universe_ranges(ctx, false)? else { return Ok(None) };
        let mut total = Nat::ZERO;
        for (a, b) in &ranges {
            total = total.add(&b.checked_sub(a).unwrap().add_u64(1));
        }
        Ok(total.as_u64().map(|t| (idx, t)))
    }

    /// R4-A3: all entries of a covering VALUE index in key order, with
    /// `f(key, entry)`. `Ok(false)`: the index does not qualify after all
    /// (counts do not cover the universe, keys not uniformly `Int` or `Str`
    /// as in `total_cmp_key`, or not strictly ascending in that order) –
    /// the caller then takes the regular path.
    fn walk_covering_index(&self, idx: &IndexDef, lane: Lane, universe: u64, ints_only: bool, f: &mut dyn FnMut(&Val, &crate::db::secindex::SecEntry) -> Result<()>) -> Result<bool> {
        let mut ok = true;
        let mut total: u64 = 0;
        let mut prev: Option<Val> = None;
        let mut kind: Option<bool> = None; // Some(true) Int, Some(false) Str
        self.db.sec_scan(idx.index_id, lane, None, &mut |e| {
            let key = e.value()?;
            let k = match &key {
                Val::Int(_) => true,
                Val::Str(_) if !ints_only => false,
                _ => {
                    ok = false;
                    return Ok(false);
                }
            };
            if kind.is_some_and(|x| x != k) || prev.as_ref().is_some_and(|p| total_cmp_key(p, &key).is_ge()) {
                ok = false;
                return Ok(false);
            }
            kind = Some(k);
            total = total.saturating_add(e.raw_count);
            f(&key, e)?;
            prev = Some(key);
            Ok(true)
        })?;
        Ok(ok && total == universe)
    }

    /// Posting u of an entry restricted to the candidates `us` (sorted);
    /// `all`: the candidates are the whole universe.
    fn entry_matches(&self, idx: &IndexDef, lane: Lane, e: &crate::db::secindex::SecEntry, us: &[Nat], all: bool) -> Result<Vec<Nat>> {
        let p = self.db.sec_posting_us(idx.index_id, lane, e)?;
        Ok(if all { p } else { setops::intersect_sorted(&p, us) })
    }

    /// R4-A3: `ORDER BY <indexed field> … LIMIT` from the key order of a
    /// covering VALUE index (ties in u order, as the stable sort): only the
    /// postings are read, no record. `None`: not applicable.
    fn index_order_window(&self, ctx: &QueryContext, us: Option<&[Nat]>, n_us: usize, fields: &[String], o: &ExecOptions) -> Result<Option<Vec<Nat>>> {
        let Ok(ordering) = normalize_order_by(o.order_by.as_ref()) else { return Ok(None) };
        let Some(need) = topk_window(&o.limit, &o.offset) else { return Ok(None) };
        let [(name, desc)] = ordering.as_slice() else { return Ok(None) };
        if !fields.contains(name) || need >= n_us {
            return Ok(None);
        }
        let Some((idx, universe)) = self.covering_value_index(name, ctx)? else { return Ok(None) };
        let all = us.is_none() || n_us as u64 == universe;
        let us = us.unwrap_or(&[]);
        let lay = self.plain_classic_layout(ctx).unwrap();
        // ascending: the first `need` matches; descending: a window of the
        // last entries holding at least `need` matches (decoded at the end
        // when the candidates are the universe)
        let mut asc: Vec<Nat> = Vec::new();
        let mut win: std::collections::VecDeque<(crate::db::secindex::SecEntry, Vec<Nat>, u64)> = Default::default();
        let mut in_win: u64 = 0;
        let ok = self.walk_covering_index(idx, ctx.lane, universe, false, &mut |_k, e| {
            if !*desc {
                if (asc.len() as u64) < need as u64 {
                    let mut m = self.entry_matches(idx, ctx.lane, e, us, all)?;
                    m.retain(|u| lay.contains_u(u));
                    asc.extend(m);
                }
                return Ok(());
            }
            let (m, c) = if all {
                (Vec::new(), e.raw_count)
            } else {
                let mut m = self.entry_matches(idx, ctx.lane, e, us, false)?;
                m.retain(|u| lay.contains_u(u));
                let c = m.len() as u64;
                (m, c)
            };
            if c == 0 {
                return Ok(());
            }
            win.push_back((e.clone(), m, c));
            in_win += c;
            while win.front().is_some_and(|f| in_win - f.2 >= need as u64) {
                in_win -= win.pop_front().unwrap().2;
            }
            Ok(())
        })?;
        if !ok {
            return Ok(None);
        }
        let mut out: Vec<Nat> = if *desc {
            let mut v = Vec::new();
            for (e, m, _) in win.into_iter().rev() {
                if all {
                    let mut p = self.db.sec_posting_us(idx.index_id, ctx.lane, &e)?;
                    p.retain(|u| lay.contains_u(u));
                    v.extend(p);
                } else {
                    v.extend(m);
                }
                if v.len() >= need {
                    break;
                }
            }
            v
        } else {
            asc
        };
        out.truncate(need);
        Ok(Some(slice_simple(out, &o.limit, &o.offset)?))
    }

    /// RC4-01: the index keeps the first of equal values (1, 1.0 and True
    /// are one key), so its keys are the record values only if every record
    /// of the lane stores the field as an integer (no enum) or as utf8: every
    /// shape of the dense segments declares it so, and no sparse cell can
    /// carry another type. Otherwise aggregates take the regular path.
    fn index_keys_are_record_values(&self, ctx: &QueryContext, v: &Nat) -> bool {
        use crate::codec::TypeCode as T;
        let Some(realm) = ctx.realm_nat() else { return false };
        let key = (realm.clone(), ctx.lane);
        if self.db.sparse.get(&key).is_some_and(|s| !s.idx.is_empty()) {
            return false;
        }
        let Some(dense) = self.db.dense.get(&key) else { return false };
        let mut kind: Option<bool> = None; // Some(true) integer, Some(false) utf8
        for sid in &dense.shapes {
            let Some(f) = self.db.shapes.get(sid).and_then(|sh| sh.by_v(v)) else { continue };
            if f.enum_map.is_some() {
                return false;
            }
            let k = match f.typ {
                T::U8 | T::U16 | T::U32 | T::U64 | T::I8 | T::I16 | T::I32 | T::I64 | T::VarUInt | T::VarInt => true,
                T::Utf8 => false,
                _ => return false,
            };
            if kind.is_some_and(|x| x != k) {
                return false;
            }
            kind = Some(k);
        }
        kind.is_some()
    }

    /// R4-A3: COUNT/SUM/AVG/MIN/MAX of one indexed field, optionally grouped
    /// by the same field, from a covering VALUE index (posting counts, keys;
    /// with a WHERE the postings are intersected with the candidates). Same
    /// rows as `Aggregator`: groups in the order of their first record (u),
    /// SUM/AVG only over `Int` keys. `None`: not applicable.
    fn index_aggregate(&self, ctx: &QueryContext, us: Option<&[Nat]>, n_us: usize, specs: &[AggSpec], groups: &[String]) -> Result<Option<Vec<Val>>> {
        let mut field: Option<&str> = groups.first().map(|g| g.as_str());
        if groups.len() > 1 {
            return Ok(None);
        }
        for s in specs {
            if s.field == "*" {
                continue;
            }
            match field {
                None => field = Some(s.field.as_str()),
                Some(f) if f == s.field => {}
                Some(_) => return Ok(None),
            }
        }
        let Some(name) = field else { return Ok(None) };
        let mut guard = ResultGuard::new(self.db);
        let Some((idx, universe)) = self.covering_value_index(name, ctx)? else { return Ok(None) };
        if !self.index_keys_are_record_values(ctx, idx.field_v()) {
            return Ok(None);
        }
        let all = us.is_none() || n_us as u64 == universe;
        let us = us.unwrap_or(&[]);
        let numeric = specs.iter().any(|s| s.op == "sum" || s.op == "avg");
        let lay = self.plain_classic_layout(ctx).unwrap();
        // (key, count, first u) per key with at least one candidate
        let mut per: Vec<(Val, u64, Nat)> = Vec::new();
        let grouped = !groups.is_empty();
        let ok = self.walk_covering_index(idx, ctx.lane, universe, numeric, &mut |k, e| {
            let (c, first) = if all && !grouped {
                (e.raw_count, Nat::ZERO)
            } else {
                let mut m = self.entry_matches(idx, ctx.lane, e, us, all)?;
                m.retain(|u| lay.contains_u(u));
                (m.len() as u64, m.first().cloned().unwrap_or(Nat::ZERO))
            };
            if c > 0 {
                guard.add_bytes((96 + std::mem::size_of::<Val>() + crate::db::val_heap(k)) as u64)?;
                per.push((k.clone(), c, first));
            }
            Ok(())
        })?;
        if !ok {
            return Ok(None);
        }
        let row_of = |items: &[(Val, u64, Nat)], key: Option<&Val>| -> Val {
            let mut row = Val::map();
            if let (Some(g), Some(k)) = (groups.first(), key) {
                row.set(g.clone(), k.clone());
            }
            let cnt: BigInt = items.iter().map(|x| BigInt::from(x.1)).sum();
            let sum = || -> BigInt { items.iter().map(|(k, c, _)| py_int(k).unwrap_or_default() * BigInt::from(*c)).sum() };
            for s in specs {
                let v = match s.op.as_str() {
                    "count" => Val::from_bigint(cnt.clone()),
                    "sum" => Val::from_bigint(sum()),
                    "avg" if cnt.is_zero() => Val::None,
                    "avg" => Val::Float(int_true_div(&sum(), &cnt)),
                    "min" => items.first().map(|x| x.0.clone()).unwrap_or(Val::None),
                    _ => items.last().map(|x| x.0.clone()).unwrap_or(Val::None),
                };
                row.set(s.alias.clone(), v);
            }
            row
        };
        if !grouped {
            return Ok(Some(vec![row_of(&per, None)]));
        }
        per.sort_by(|a, b| a.2.cmp(&b.2));
        Ok(Some(per.iter().map(|x| row_of(std::slice::from_ref(x), Some(&x.0))).collect()))
    }

    /// R4-C/RUN6: `ORDER BY` for a streaming consumer over the CLASSIC
    /// fields path: the sort keys are read chunk-wise (only the key fields)
    /// and sorted by `ExtSorter` (memory up to `[results] sort_memory_bytes`,
    /// beyond that sorted runs in the result directory). Same order as
    /// `order_limit_indices` (stable), any LIMIT/OFFSET window. `None` if a
    /// key is not a selected BASE field read this way (`record_N`, labels of
    /// other fields): the caller then sorts the finished rows, also bounded.
    /// Errors: read errors of all keys, then anchor errors, then sort errors.
    fn external_order(&self, realm: &Nat, lay: &LayoutDef, us: &[Nat], fields: &[String], o: &ExecOptions, lane: Lane) -> Result<Option<UWindow>> {
        // R9C-02: `get_fields` reads DENSE values only; with sparse cells in
        // this lane a record may exist without a DENSE record (or a key may
        // live in a sparse cell). The caller then sorts the finished rows.
        if self.db.sparse.contains_key(&(realm.clone(), lane)) {
            return Ok(None);
        }
        let Ok(ordering) = normalize_order_by(o.order_by.as_ref()) else { return Ok(None) };
        if ordering.is_empty() {
            return Ok(None);
        }
        // key column → index into the selected fields (by label as in
        // `classic_field_rows`), or a constant None
        let mut vs: Vec<Nat> = Vec::new();
        let mut src: Vec<Option<usize>> = Vec::new();
        for (n, _) in &ordering {
            if !fields.contains(n) {
                if n == "record_N" {
                    return Ok(None);
                }
                src.push(None);
                continue;
            }
            let Ok(v) = self.db.resolve_v(realm, &Val::str(n.as_str())) else { return Ok(None) };
            let label = self.db.current_base_shape(realm).and_then(|b| b.by_v(&v)).map(|f| f.name.clone()).unwrap_or_else(|| n.clone());
            if &label != n || ["_u", "_lane", "_lane_name"].contains(&n.as_str()) {
                return Ok(None);
            }
            src.push(Some(vs.len()));
            vs.push(v);
        }
        let mut sorter = ExtSorter::new(self.db, ordering);
        let mut st = crate::db::dense::DenseReadState::default();
        let mut anchor_err: Option<Error> = None;
        for (i, u) in us.iter().enumerate() {
            if u.is_zero() {
                continue;
            }
            let Some(vals) = self.db.get_fields(realm, u, lane, &vs, &mut st)? else { continue };
            if let Err(e) = self.db.logical_address_in(lay, u, &Val::None, lane) {
                anchor_err.get_or_insert(e);
            }
            let key: Vec<Val> = src.iter().map(|s| s.and_then(|j| vals[j].clone()).unwrap_or(Val::None)).collect();
            let cost: usize = key.iter().map(crate::db::val_heap).sum::<usize>() + if let Nat::B(b) = u { b.bits() as usize / 8 + 32 } else { 0 };
            sorter.push(key, i as i64, Val::nat(u), cost)?;
        }
        if let Some(e) = anchor_err.take() {
            return Err(e);
        }
        Ok(Some(match sorter.finish(o.order_by.as_ref(), &o.limit, &o.offset)? {
            Sorted::List(us) => UWindow::List(us.into_iter().map(|p| p.as_nat().unwrap_or(Nat::ZERO)).collect()),
            Sorted::Merge(m) => UWindow::Merge(m),
        }))
    }

    /// RUN6: CLASSIC field rows of `us` for a collecting call, chunk-wise
    /// under `[query] max_result_bytes` (same rows and the same first error
    /// as one build: read errors of all rows before anchor errors).
    fn classic_rows_collect(&self, realm: &Nat, lay: &LayoutDef, us: &[Nat], fields: &[String], lane: Lane) -> Result<Vec<Val>> {
        let mut guard = ResultGuard::new(self.db);
        let mut out = Vec::with_capacity(us.len().min(1 << 16));
        let mut sink: Option<Error> = None;
        for chunk in us.chunks(4096) {
            let rows = match self.classic_field_rows(realm, lay, chunk, fields, lane, Some(&mut sink))? {
                Some(rows) => rows,
                None => {
                    let raws = self.classic_rows(realm, lay, chunk, fields, lane)?;
                    self.classic_build(lay, raws, fields, lane, Some(&mut sink))?
                }
            };
            for row in rows {
                guard.add(&row)?;
                out.push(row);
            }
        }
        match sink {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// RUN6: sorts finished rows for a streaming consumer (`ExtSorter`, the
    /// row is the payload): the general path and CLASSIC keys that
    /// `external_order` cannot read. `rows` yields the rows in candidate
    /// order; the keys are those of `apply_order_limit` (`order_key`).
    /// An invalid ORDER BY is reported after every row was built (as
    /// `sql()`: the rows first, then the ordering).
    fn sort_rows_bounded(&self, rows: RowFeed<'_>, o: &ExecOptions) -> Result<RowSource> {
        let ordering = match normalize_order_by(o.order_by.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                rows(&mut |_| Ok(()))?;
                return Err(e);
            }
        };
        let names: Vec<String> = ordering.iter().map(|(n, _)| n.clone()).collect();
        let mut sorter = ExtSorter::new(self.db, ordering);
        let mut i = 0i64;
        rows(&mut |row| {
            let key: Vec<Val> = names.iter().map(|n| order_key(&row, n)).collect();
            let cost = crate::db::val_heap(&row) + key.iter().map(crate::db::val_heap).sum::<usize>();
            sorter.push(key, i, row, cost)?;
            i += 1;
            Ok(())
        })?;
        Ok(match sorter.finish(o.order_by.as_ref(), &o.limit, &o.offset)? {
            Sorted::List(rows) => RowSource::Rows(rows.into()),
            Sorted::Merge(m) => RowSource::Sorted(m),
        })
    }

    /// R4-A2: `ORDER BY … LIMIT` over the CLASSIC fields path with a top-k
    /// window instead of keys for every row. Applies when the LIMIT/OFFSET
    /// window is smaller than the candidate set and every ORDER BY name is
    /// a selected column or a constant (not a computed `record_N`). The
    /// result equals the stable multi-key sort of `order_limit_indices` if
    /// every key column is totally ordered there (only None/Int or only
    /// None/Str); otherwise `None` and the caller sorts everything (same
    /// values and errors as before). Errors: the same reads in the same
    /// order (all rows, then the anchor N of every u).
    /// N1 (RUN8): the ORDER BY … LIMIT window of collected rows without
    /// keeping every row. `build(i)` makes row `i` of `n` in candidate order
    /// (same reads and first error as building all of them). While every
    /// key column is totally ordered (None with only numbers without NaN, or
    /// None with only str), only the best `2k + 1024` rows stay in memory
    /// and the window equals that of `apply_order_limit` over all rows (a
    /// stable sort, ties in candidate order). `None` without LIMIT, if the
    /// window covers every row or as soon as a key column is not totally
    /// ordered: the caller then collects and sorts all rows as before.
    fn topk_rows(&self, n: usize, build: &mut dyn FnMut(usize) -> Result<Val>, o: &ExecOptions) -> Result<Option<Vec<Val>>> {
        let Ok(ordering) = normalize_order_by(o.order_by.as_ref()) else { return Ok(None) };
        let Some(k) = topk_window(&o.limit, &o.offset) else { return Ok(None) };
        if ordering.is_empty() || k >= n {
            return Ok(None);
        }
        let desc: Vec<bool> = ordering.iter().map(|(_, d)| *d).collect();
        let rank = |a: &(Vec<Val>, usize, Val), b: &(Vec<Val>, usize, Val)| -> std::cmp::Ordering {
            for (c, d) in desc.iter().enumerate() {
                let ord = order_cmp_total(&a.0[c], &b.0[c]);
                let ord = if *d { ord.reverse() } else { ord };
                if ord.is_ne() {
                    return ord;
                }
            }
            a.1.cmp(&b.1)
        };
        // per key column: saw a number, saw a str
        let mut class = vec![(false, false); ordering.len()];
        let mut kept: Vec<(Vec<Val>, usize, Val)> = Vec::new();
        for i in 0..n {
            let row = build(i)?;
            let mut key = Vec::with_capacity(ordering.len());
            for (c, (name, _)) in ordering.iter().enumerate() {
                let v = order_key(&row, name);
                match &v {
                    Val::None => {}
                    Val::Int(_) | Val::Big(_) | Val::Bool(_) => class[c].0 = true,
                    Val::Float(f) if !f.is_nan() => class[c].0 = true,
                    Val::Str(_) => class[c].1 = true,
                    _ => return Ok(None),
                }
                if class[c].0 && class[c].1 {
                    return Ok(None);
                }
                key.push(v);
            }
            if k > 0 {
                topk_push(&mut kept, k, (key, i, row), &rank);
            }
        }
        kept.sort_by(rank);
        let rows: Vec<Val> = kept.into_iter().map(|(_, _, r)| r).collect();
        Ok(Some(slice_simple(rows, &o.limit, &o.offset)?))
    }

    fn ordered_topk(&self, realm: &Nat, lay: &LayoutDef, us: &[Nat], fields: &[String], o: &ExecOptions, lane: Lane) -> Result<Option<Vec<Nat>>> {
        let Ok(ordering) = normalize_order_by(o.order_by.as_ref()) else { return Ok(None) };
        let Some(k) = topk_window(&o.limit, &o.offset) else { return Ok(None) };
        if ordering.is_empty() || k >= us.len() || ordering.iter().any(|(n, _)| n == "record_N" && !fields.contains(n)) {
            return Ok(None);
        }
        let selected: Vec<bool> = ordering.iter().map(|(n, _)| fields.contains(n)).collect();
        let desc: Vec<bool> = ordering.iter().map(|(_, d)| *d).collect();
        // per key column: saw Int, saw Str, saw anything else
        let mut class = vec![(false, false, false); ordering.len()];
        let rank = |a: &(Vec<Val>, usize), b: &(Vec<Val>, usize)| -> std::cmp::Ordering {
            for (c, d) in desc.iter().enumerate() {
                let ord = total_cmp_key(&a.0[c], &b.0[c]);
                let ord = if *d { ord.reverse() } else { ord };
                if ord.is_ne() {
                    return ord;
                }
            }
            a.1.cmp(&b.1)
        };
        let mut kept: Vec<(Vec<Val>, usize)> = Vec::new();
        let mut row_us: Vec<Nat> = Vec::with_capacity(us.len());
        self.classic_rows_chunked(realm, lay, us, fields, lane, &mut |raw| {
            let mut key = Vec::with_capacity(ordering.len());
            for (c, (n, _)) in ordering.iter().enumerate() {
                let v = if selected[c] { raw.get(n).cloned().unwrap_or(Val::None) } else { Val::None };
                match &v {
                    Val::None => {}
                    Val::Int(_) => class[c].0 = true,
                    Val::Str(_) => class[c].1 = true,
                    _ => class[c].2 = true,
                }
                key.push(v);
            }
            let i = row_us.len();
            row_us.push(raw.get("_u").and_then(|x| x.as_nat()).unwrap_or(Nat::ZERO));
            if k > 0 && class.iter().all(|(i, s, x)| !x && !(*i && *s)) {
                topk_push(&mut kept, k, (key, i), &rank);
            }
            Ok(())
        })?;
        if class.iter().any(|(i, s, x)| *x || (*i && *s)) {
            return Ok(None);
        }
        // same first error as building every row (anchor N per u)
        for u in &row_us {
            self.db.logical_address_in(lay, u, &Val::None, lane)?;
        }
        kept.sort_by(rank);
        let picked: Vec<usize> = slice_simple(kept.into_iter().map(|(_, i)| i).collect(), &o.limit, &o.offset)?;
        Ok(Some(picked.into_iter().map(|i| row_us[i].clone()).collect()))
    }

    /// R4-B: FIELD predicate by reading only that field of every record of
    /// the metadata universe (or of `within`), in u order. Same matches as
    /// the scan of `eval_field` (`data_get(name)`: a record without the
    /// field never matches, `exists` matches every present value).
    #[allow(clippy::too_many_arguments)]
    fn scan_field_u(&self, name: &str, op: &str, value: &Val, value2: &Val, options: &Val, ctx: &QueryContext, lay: &LayoutDef, universe: &mut Option<Vec<(Nat, Nat)>>, within: Option<&[Nat]>) -> Result<Option<Vec<Nat>>> {
        let Some(v) = self.scan_field_v(name, ctx) else { return Ok(None) };
        let realm = ctx.realm_nat().unwrap();
        let key = (realm.clone(), ctx.lane);
        // SPARSE cells (stage 2): values from the newest SPARSE state of v
        // (`Tomb` = no cell), pure sparse records included – as the cell
        // cursor; `get_fields` alone would miss pure sparse records
        let sparse = self.db.sparse.contains_key(&key);
        let overlay = self.db.overlay_keys.contains(&key) || sparse;
        // the record view labels fields by the shape of their segment: every
        // stored shape must show v under `name` (or not have v) and no other
        // v under `name`
        if let Some(idx) = self.db.dense.get(&key) {
            for sid in &idx.shapes {
                let Some(sh) = self.db.shapes.get(sid) else { return Ok(None) };
                if sh.by_v(&v).is_some_and(|f| f.name != name) || sh.by_name(name).is_some_and(|f| f.v != v) {
                    return Ok(None);
                }
            }
        }
        let text_op = matches!(op, "contains" | "icontains" | "token" | "tokens_all" | "tokens_any" | "fulltext" | "phrase");
        let field_ok = |val: &Option<Val>| -> Result<bool> {
            Ok(match val {
                None => false,
                Some(_) if op == "exists" => true,
                Some(a) if text_op => textindex::predicate_matches(a, op, value, options)?,
                Some(a) => match_raw(a, op, value, value2)?,
            })
        };
        let vs = [v];
        if overlay && within.is_none() {
            // 0.28.2/scan: overwritten segments / DELETE entries – every live
            // u of the winner view read once from its serving segment, in
            // file order (no `resolve_segment` per u, no single reads per
            // record); the universe comes from the same view
            let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Candidates);
            let Some(view) = self.db.winner_view(&key, &mut meter)? else { return Ok(None) };
            let eff = if sparse {
                match self.db.sparse_effect(&key, &view, Some(&vs[0]), &mut meter)? {
                    Some(e) => Some(e),
                    None => return Ok(None),
                }
            } else {
                None
            };
            if universe.is_none() {
                match self.classic_universe_from_view(ctx, lay, &view, eff.as_ref(), false, &mut meter)? {
                    Some((_, r)) => *universe = Some(r),
                    None => return Ok(None),
                }
            }
            let mut out = self.db.scan_view_fields(&view, eff.as_ref(), &lay.u_start, lay.u_end.as_ref(), &vs, &mut meter, &mut |_u, vals| field_ok(&vals[0]))?;
            // the universe is the view's live u within the layout bounds;
            // kept as the bound of the result as on the regular way
            let uni = universe.as_ref().unwrap();
            out.retain(|u| {
                let i = uni.partition_point(|(_, b)| b < u);
                i < uni.len() && &uni[i].0 <= u
            });
            out.retain(|u| lay.contains_u(u));
            return Ok(Some(out));
        }
        if universe.is_none() {
            match self.classic_universe_ranges(ctx, false)? {
                Some((_, r)) => *universe = Some(r),
                None => return Ok(None),
            }
        }
        let mut seg_cache = crate::db::dense::DenseReadState::default();
        let mut out = Vec::new();
        let mut test = |u: &Nat, out: &mut Vec<Nat>| -> Result<()> {
            let val = if sparse {
                // the cell of v as the cell cursor shows it: the newest
                // SPARSE state, else the DENSE value (pure sparse u: none)
                // (F1: a SPARSE state older than the DENSE record is hidden)
                self.db.live_cell_value(&realm, u, &vs[0], ctx.lane)?
            } else {
                let Some(vals) = self.db.get_fields(&realm, u, ctx.lane, &vs, &mut seg_cache)? else { return Ok(()) };
                vals.into_iter().next().flatten()
            };
            if field_ok(&val)? {
                out.push(u.clone());
            }
            Ok(())
        };
        match within {
            Some(us) => {
                for u in us {
                    test(u, &mut out)?;
                }
            }
            None => {
                for (a, b) in universe.as_ref().unwrap() {
                    let mut u = a.clone();
                    while &u <= b {
                        test(&u, &mut out)?;
                        u = u.add_u64(1);
                    }
                }
            }
        }
        out.retain(|u| lay.contains_u(u));
        Ok(Some(out))
    }

    /// R4-B: output rows of the CLASSIC fields path (`record_N` + the
    /// selected fields) built directly from the selected field values,
    /// without the intermediate `select_us_ext` rows. Same values as
    /// `classic_rows` followed by the row build (labels as in
    /// `select_us_ext`, the last column with a label wins) and the same
    /// first error (read errors of all rows before anchor errors). `None`:
    /// not applicable (structural or unknown names, sparse cells, or names
    /// of the internal row keys) – the caller takes the regular path.
    /// `anchor_sink`: anchor errors are recorded there (first one) instead
    /// of being returned, so that a caller reading in several chunks keeps
    /// the order "read errors of all rows first".
    fn classic_field_rows(&self, realm: &Nat, lay: &LayoutDef, us: &[Nat], fields: &[String], lane: Lane, anchor_sink: Option<&mut Option<Error>>) -> Result<Option<Vec<Val>>> {
        if self.db.sparse.contains_key(&(realm.clone(), lane)) {
            return Ok(None);
        }
        const META: [&str; 3] = ["_u", "_lane", "_lane_name"];
        let mut vs = Vec::with_capacity(fields.len());
        let mut labels = Vec::with_capacity(fields.len());
        for f in fields {
            let Ok(v) = self.db.resolve_v(realm, &Val::str(f.as_str())) else { return Ok(None) };
            let mut label = f.clone();
            if let Some(sf) = self.db.current_base_shape(realm).and_then(|b| b.by_v(&v)) {
                label = sf.name.clone();
            }
            if META.contains(&label.as_str()) || META.contains(&f.as_str()) {
                return Ok(None);
            }
            vs.push(v);
            labels.push(label);
        }
        // output field j takes the value of the last column labelled like it
        let src: Vec<Option<usize>> = fields.iter().map(|f| labels.iter().rposition(|l| l == f)).collect();
        let mut out = Vec::with_capacity(us.len());
        let mut seg_cache = crate::db::dense::DenseReadState::default();
        let mut anchor_err: Option<Error> = None;
        for u in us {
            if u.is_zero() {
                continue;
            }
            let Some(vals) = self.db.get_fields(realm, u, lane, &vs, &mut seg_cache)? else { continue };
            let rn = match self.record_anchor_ref(lay, u, lane) {
                Ok(r) => r,
                Err(e) => {
                    anchor_err.get_or_insert(e);
                    continue;
                }
            };
            let mut row = Vec::with_capacity(1 + fields.len());
            row.push(("record_N".to_string(), Val::Str(rn)));
            for (f, s) in fields.iter().zip(&src) {
                let v = s.and_then(|i| vals[i].clone()).unwrap_or(Val::None);
                crate::db::read::set_or_push(&mut row, f, v);
            }
            out.push(Val::Map(row));
        }
        match (anchor_err, anchor_sink) {
            (Some(e), Some(sink)) => {
                sink.get_or_insert(e);
                Ok(Some(out))
            }
            (Some(e), None) => Err(e),
            (None, _) => Ok(Some(out)),
        }
    }

    /// The row build of the CLASSIC fields path from `classic_rows` rows
    /// (`record_N` + the selected fields; F046). Anchor errors go to
    /// `anchor_sink` when given (see `classic_field_rows`).
    fn classic_build(&self, lay: &LayoutDef, raws: Vec<Val>, fields: &[String], lane: Lane, mut anchor_sink: Option<&mut Option<Error>>) -> Result<Vec<Val>> {
        let mut out = Vec::with_capacity(raws.len());
        for row in raws {
            let u = row.get("_u").and_then(|x| x.as_nat()).unwrap_or(Nat::ZERO);
            let rn = match (self.record_anchor_ref(lay, &u, lane), anchor_sink.as_deref_mut()) {
                (Ok(r), _) => r,
                (Err(e), Some(sink)) => {
                    sink.get_or_insert(e);
                    continue;
                }
                (Err(e), None) => return Err(e),
            };
            let mut o2 = vmap! {"record_N" => rn};
            for f in fields {
                o2.set(f.clone(), row.get(f).cloned().unwrap_or(Val::None));
            }
            out.push(o2);
        }
        Ok(out)
    }

    /// Rows of CLASSIC records `us` with the values of `names` under the
    /// general-path semantics (`_record_value`): data columns via
    /// `select_us_ext`, structural names (`u`, `record_N`, …) and unknown
    /// columns via the light record envelope (F046).
    fn classic_rows(&self, realm: &Nat, lay: &LayoutDef, us: &[Nat], names: &[String], lane: Lane) -> Result<Vec<Val>> {
        let (mut known, mut other) = (Vec::new(), Vec::new());
        for n in names {
            if self.db.resolve_v(realm, &Val::str(n.as_str())).is_ok() {
                known.push(Val::str(n.as_str()));
            } else if !other.contains(n) {
                other.push(n.clone());
            }
        }
        let mut rows = self.db.select_us_ext(realm, us, &known, lane, true)?;
        if !other.is_empty() {
            for row in rows.iter_mut() {
                let u = row.get("_u").and_then(|x| x.as_nat()).unwrap_or(Nat::ZERO);
                let rn = self.db.logical_address_in(lay, &u, &Val::None, lane)?.n;
                let rec = self.record_envelope_light(&rn)?;
                for n in &other {
                    row.set(n.clone(), record_value(&rec, n));
                }
            }
        }
        Ok(rows)
    }

    /// `classic_rows` streamed in chunks of 4096 u (P007: bounded memory for
    /// ALL over large realms). Only when every name is a resolvable column;
    /// otherwise (structural or unknown names via the record envelope) the
    /// rows are built at once, exactly as `classic_rows`, so the order of
    /// errors is unchanged.
    fn classic_rows_chunked(&self, realm: &Nat, lay: &LayoutDef, us: &[Nat], names: &[String], lane: Lane, f: &mut dyn FnMut(Val) -> Result<()>) -> Result<()> {
        let all_known = names.iter().all(|n| self.db.resolve_v(realm, &Val::str(n.as_str())).is_ok());
        if !all_known {
            for raw in self.classic_rows(realm, lay, us, names, lane)? {
                f(raw)?;
            }
            return Ok(());
        }
        for chunk in us.chunks(4096) {
            for raw in self.classic_rows(realm, lay, chunk, names, lane)? {
                f(raw)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn verify_text(&self, realm: &Nat, us: Vec<Nat>, name: &str, op: &str, value: &Val, options: &Val, lane: Lane) -> Result<Vec<Nat>> {
        // RUN9 V3: the texts of the candidates are read and checked in
        // chunks, so only one chunk of texts is in memory (same result)
        let mut verified = Vec::new();
        let cols = [Val::str(name)];
        // RUN¹⁰: checkpoint per chunk of texts read (from 1 MiB on)
        let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::TextVerify);
        for chunk in us.chunks(VERIFY_TEXT_CHUNK) {
            let rows = self.db.select_us_ext(realm, chunk, &cols, lane, true)?;
            meter.grow(rows.iter().map(|r| (std::mem::size_of::<Val>() + crate::db::val_heap(r)) as u64).sum())?;
            for raw in rows {
                let actual = raw.get(name).cloned().unwrap_or(Val::None);
                if textindex::predicate_matches(&actual, op, value, options)? {
                    if let Some(u) = raw.get("_u").and_then(|x| x.as_nat()) {
                        verified.push(u);
                    }
                }
            }
        }
        Ok(verified)
    }

    /// P007: the materialized RECORD universe of `ctx` as sorted, disjoint
    /// u ranges, read from the segment metadata only. `Some` only when the
    /// result is provably identical to the scan: one plain CLASSIC layout in
    /// the realm, no overlays on `(realm, lane)` (no sparse cells, no
    /// DELETE, no overlapping segments – every u of a segment is then a live
    /// record with at least one cell), every segment shape has fields, and
    /// the anchor N of both ends of every range is computable (CLASSIC
    /// record_N = anchor of u, strictly increasing in u). With `strict_shape`
    /// additionally every segment uses the realm's current BASE shape (=
    /// layout shape) and the realm has no extensions, so that the CLASSIC
    /// row builder shows exactly the columns of the general record envelope.
    fn classic_universe_ranges(&self, ctx: &QueryContext, strict_shape: bool) -> Result<Option<ClassicUniverse<'a>>> {
        let Some(lay) = self.plain_classic_layout(ctx) else { return Ok(None) };
        let realm = ctx.realm_nat().unwrap();
        let key = (realm.clone(), ctx.lane);
        let sparse = self.db.sparse.contains_key(&key);
        // the CLASSIC row builder (strict_shape) has no SPARSE overlay
        if sparse && strict_shape {
            return Ok(None);
        }
        if strict_shape {
            let base = self.db.realms.get(&realm).and_then(|r| r.default_shape_id);
            if lay.shape_id.is_none() || base != lay.shape_id || self.db.extension_defs.keys().any(|(r, _)| r == &realm) {
                return Ok(None);
            }
        }
        if self.db.overlay_keys.contains(&key) || sparse {
            // 0.28.2/scan: overwritten segments, DELETE entries or SPARSE
            // cells – the segment metadata is not the universe; the winner
            // view (with the effect of the SPARSE cells) is
            let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Candidates);
            let Some(view) = self.db.winner_view(&key, &mut meter)? else { return Ok(None) };
            let eff = if sparse {
                match self.db.sparse_effect(&key, &view, None, &mut meter)? {
                    Some(e) => Some(e),
                    None => return Ok(None),
                }
            } else {
                None
            };
            return self.classic_universe_from_view(ctx, lay, &view, eff.as_ref(), strict_shape, &mut meter);
        }
        let mut ranges = Vec::new();
        let Some(idx) = self.db.dense.get(&key) else { return Ok(Some((lay, ranges))) };
        let all_segs = idx.idx.collect_by_start(self.db)?;
        for seg in &all_segs {
            match self.db.shapes.get(&seg.shape_id) {
                Some(sh) if !sh.fields.is_empty() => {}
                _ => return Ok(None),
            }
            if strict_shape && Some(seg.shape_id) != lay.shape_id {
                return Ok(None);
            }
            if seg.count == 0 {
                continue;
            }
            let a = seg.u_start.clone().max(lay.u_start.clone());
            let b = match &lay.u_end {
                Some(e) => seg.u_end.clone().min(e.clone()),
                None => seg.u_end.clone(),
            };
            if a > b {
                continue;
            }
            for u in [&a, &b] {
                if self.db.logical_address_in(lay, u, &Val::None, ctx.lane).is_err() {
                    return Ok(None);
                }
            }
            ranges.push((a, b));
        }
        // without overlays the segments are disjoint; check anyway
        for w in ranges.windows(2) {
            if w[1].0 <= w[0].1 {
                return Ok(None);
            }
        }
        // G3-01: the segment metadata (u_start, count) comes from the DATA
        // prefixes, which are not CRC-covered on open. Every DATA block of
        // the realm/lane is CRC-checked once per handle (verified registry,
        // P001) before its metadata is used, as the general path would read
        // (and check) them; later calls only look up the registry.
        let mut last = None;
        for seg in &all_segs {
            if last != Some(seg.block_offset) {
                self.db.ensure_verified_dense(seg)?;
                last = Some(seg.block_offset);
            }
        }
        Ok(Some((lay, ranges)))
    }

    /// 0.28.2/scan: `classic_universe_ranges` of a realm/lane with
    /// overwritten DENSE segments or DELETE entries (no SPARSE cells), from
    /// the winner view: the live u of the view are exactly the records the
    /// cell cursor visits (newest segment per u, newer DELETE hides u; every
    /// record has a cell because every segment shape has fields). The same
    /// conditions as without overlays hold for *every* segment, also for
    /// fully overwritten ones: shapes with fields, with `strict_shape` the
    /// layout shape; G3-01: every DATA block of the lane is CRC-checked
    /// before its metadata is used; anchors of all range ends computable.
    /// Stage 2 (`eff`): minus the u whose every cell is tombstoned, plus the
    /// pure sparse records (the cell cursor's record set, S6).
    fn classic_universe_from_view(&self, ctx: &QueryContext, lay: &'a LayoutDef, view: &crate::db::winners::WinnerView, eff: Option<&crate::db::winners::SparseEffect>, strict_shape: bool, meter: &mut crate::limiter::Meter) -> Result<Option<ClassicUniverse<'a>>> {
        for seg in &view.segs {
            match self.db.shapes.get(&seg.shape_id) {
                Some(sh) if !sh.fields.is_empty() => {}
                _ => return Ok(None),
            }
            if strict_shape && Some(seg.shape_id) != lay.shape_id {
                return Ok(None);
            }
        }
        let mut last = None;
        for seg in &view.segs {
            if last != Some(seg.block_offset) {
                self.db.ensure_verified_dense(seg)?;
                last = Some(seg.block_offset);
            }
        }
        let live = match eff {
            Some(e) => view.record_ranges(e, meter)?,
            None => view.live_ranges(meter)?,
        };
        let mut ranges = Vec::new();
        for (a, b) in live {
            let a = a.max(lay.u_start.clone());
            let b = match &lay.u_end {
                Some(e) => b.min(e.clone()),
                None => b,
            };
            if a > b {
                continue;
            }
            for u in [&a, &b] {
                if self.db.logical_address_in(lay, u, &Val::None, ctx.lane).is_err() {
                    return Ok(None);
                }
            }
            ranges.push((a, b));
        }
        for w in ranges.windows(2) {
            if w[1].0 <= w[0].1 {
                return Ok(None);
            }
        }
        Ok(Some((lay, ranges)))
    }

    fn record_anchor_ref(&self, lay: &LayoutDef, u: &Nat, lane: Lane) -> Result<String> {
        Ok(self.db.logical_address_in(lay, u, &Val::None, lane)?.n_ref())
    }

    // ------------------------- public interface -------------------------

    /// `explain`
    pub fn explain(&self, expr: &Expr, realm: Option<BigInt>, lane: Lane, return_mode: &str) -> Result<Val> {
        self.db.check_open()?;
        let domain = domain_for_return(&unicode::lower(return_mode), return_mode)?;
        let ctx = QueryContext { realm, lane };
        let plan = self.plan_expr(expr, &ctx, domain)?;
        let hints = self.result_hints(&plan, &unicode::lower(return_mode));
        let mut r = vmap! {
            "query_version" => QUERY_VERSION, "optimizer" => OPTIMIZER, "target_domain" => domain.name(),
            "context" => vmap! {"realm" => ctx.realm.clone().map(Val::from_bigint).unwrap_or(Val::None), "lane" => lane as i64},
            "plan" => plan,
        };
        // R4-A6: only for large estimates (the reference has no such key)
        if !hints.is_empty() {
            r.set("hints", Val::List(hints));
        }
        Ok(r)
    }

    /// R4-A6: hints of `explain` for large estimated results (at least
    /// `LARGE_RESULT_ROWS` rows, or more than `[query] max_result_rows`).
    fn result_hints(&self, plan: &Val, return_mode: &str) -> Vec<Val> {
        let Some(est) = est_key(plan.get("estimated_rows")).map(|n| num_f64(&n)) else { return Vec::new() };
        let mut out = Vec::new();
        let max = self.db.config.query_max_result_rows;
        let rows_out = !matches!(return_mode, "count" | "aggregate");
        if est >= LARGE_RESULT_ROWS as f64 {
            out.push(Val::str(format!(
                "large result: about {est:.0} rows estimated{}",
                if rows_out { "; LIMIT, COUNT(*), an aggregate or the streaming sql_iter() keep the memory small" } else { "" }
            )));
        }
        if max > 0 && rows_out && est > max as f64 {
            out.push(Val::str(format!("exceeds [query] max_result_rows = {max}: the query is rejected unless a LIMIT keeps it below")));
        }
        out
    }

    fn base_result(&self, domain: Domain, ret: &str, count: usize) -> Val {
        vmap! {
            "query_version" => QUERY_VERSION, "optimizer" => OPTIMIZER, "domain" => domain.name(),
            "return" => ret, "count" => count as i64,
        }
    }

    /// `execute`
    pub fn execute(&self, expr: &Expr, o: &ExecOptions) -> Result<Val> {
        Ok(self.execute_impl(expr, o, false, true)?.0)
    }

    /// R4-B: the rows of `execute` for a streaming consumer. Count,
    /// aggregate and ranked results and ordered rows of the general path are
    /// complete; the field rows of the CLASSIC u path (any order, LIMIT
    /// window) and the unordered field rows of the general path are built
    /// in chunks by `RowCursor::next_batch`. Same rows in the same order;
    /// errors of a row surface with its chunk. `enforce_limit`: whether
    /// `[query] max_result_rows` applies (a consumer that collects every
    /// row, e.g. `sql_numpy`; not for iterators).
    pub fn open_rows(&self, expr: &Expr, o: &ExecOptions, enforce_limit: bool) -> Result<RowCursor> {
        let (mut r, src) = self.execute_impl(expr, o, true, enforce_limit)?;
        let src = match src {
            Some(s) => s,
            None => match r.remove("rows") {
                Some(Val::List(rows)) => RowSource::Rows(rows.into()),
                _ => RowSource::Rows(Default::default()),
            },
        };
        Ok(RowCursor { src, result: r })
    }

    fn execute_impl(&self, expr: &Expr, o: &ExecOptions, defer: bool, enforce_limit: bool) -> Result<(Val, Option<RowSource>)> {
        self.db.check_open()?;
        let rm0 = unicode::lower(&o.return_mode);
        if rm0 == "ranked" {
            return Ok((self.search(expr, o.realm.clone(), o.lane, o.select.as_ref(), &o.limit, &o.offset, o.explain, o.k1, o.b)?, None));
        }
        let agg_mode = o.aggregate.as_ref().is_some_and(|a| a.is_truthy()) || o.group_by.as_ref().is_some_and(|g| g.is_truthy()) || rm0 == "aggregate";
        let rm = if agg_mode { "aggregate".to_string() } else { rm0 };
        let domain = domain_for_return(&rm, &rm)?;
        let ctx = QueryContext { realm: o.realm.clone(), lane: o.lane };
        // The plan is needed for `explain` and by the general path. Expressions
        // whose planning cannot fail (FIELD leaves, AND of FIELD leaves, ALL)
        // are planned lazily, so the index fast paths below skip it; all
        // others are planned up front as before (same first error).
        let mut plan: Option<Val> = if plan_is_infallible(expr) { None } else { Some(self.plan_expr(expr, &ctx, domain)?) };
        let take_plan = |plan: &mut Option<Val>| -> Result<Val> {
            match plan.take() {
                Some(p) => Ok(p),
                None => self.plan_expr(expr, &ctx, domain),
            }
        };
        // R4-A6: optional bound of the estimated result rows (opt-in, TOML)
        let max_rows = self.db.config.query_max_result_rows;
        if max_rows > 0 && enforce_limit && !matches!(rm.as_str(), "count" | "aggregate") {
            if plan.is_none() {
                plan = Some(self.plan_expr(expr, &ctx, domain)?);
            }
            let est = plan.as_ref().and_then(|p| est_key(p.get("estimated_rows")));
            check_result_limit(est.map(|n| num_f64(&n)), &o.limit, max_rows)?;
        }
        // F027: a layout filter matters only if the realm has other layouts.
        let layout_filter = match (o.layout_id, ctx.realm_nat()) {
            (Some(id), Some(r)) if self.db.realm_layouts(&r).iter().any(|l| l.layout_id != id) => Some(id),
            _ => None,
        };

        // POSTING_COUNT: exact posting metadata answers COUNT directly.
        if rm == "count" && layout_filter.is_none() {
            if let Expr::Field { name, op, value, value2, options } = expr {
                let routed = ctx.realm_nat().is_some_and(|r| self.routes_every_u(&r));
                if let Some(idx) = self.find_usable_index(name, &ctx, op, options, value).filter(|_| routed) {
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
                            r.set("plan", take_plan(&mut plan)?);
                        }
                        return Ok((r, None));
                    }
                }
            }
        }

        // P007: ALL over a plain CLASSIC realm without overlays – the record
        // universe comes from the segment metadata (no record decoding); the
        // result is identical to the general path, so no fastpath marker.
        let mut all_us: Option<UCands> = None;
        if domain == Domain::Record && layout_filter.is_none() && matches!(expr, Expr::All) {
            if rm == "count" {
                if let Some((_, ranges)) = self.classic_universe_ranges(&ctx, false)? {
                    let mut total = Nat::ZERO;
                    for (a, b) in &ranges {
                        total = total.add(&b.checked_sub(a).unwrap().add_u64(1));
                    }
                    if let Some(n) = total.as_u64().and_then(|n| usize::try_from(n).ok()) {
                        let mut r = self.base_result(Domain::Record, &rm, n);
                        r.set("returned", Val::Int(1));
                        if o.explain {
                            r.set("plan", take_plan(&mut plan)?);
                        }
                        return Ok((r, None));
                    }
                }
            } else if let Some((_, ranges)) = self.classic_universe_ranges(&ctx, true)? {
                // R4-A5: the universe stays as ranges; only what is needed
                // (a LIMIT window, or every u for sorting) is expanded
                all_us = UCands::ranges(ranges);
            }
        }
        let fast_marker = all_us.is_none();

        // CLASSIC_U: indexed predicates stay in compact u space.
        let mut fast_marker = fast_marker;
        let fast = if all_us.is_some() {
            all_us
        } else if domain == Domain::Record && layout_filter.is_none() {
            match self.fast_classic_index_us(expr, &ctx)? {
                Some(us) => Some(UCands::list(us)),
                None => {
                    // R4-A1: OR/NOT trees in u space; output identical to the
                    // general path, so no fastpath marker (as P007 ALL)
                    let us = self.classic_u_tree(expr, plan.as_ref(), &ctx)?;
                    if us.is_some() {
                        fast_marker = false;
                    }
                    us.map(UCands::list)
                }
            }
        } else {
            None
        };
        if let Some(cands) = fast {
            let lay = self.plain_classic_layout(&ctx).unwrap();
            let realm = ctx.realm_nat().unwrap();
            let mut r = self.base_result(Domain::Record, &rm, cands.len());
            if fast_marker {
                r.set("fastpath", Val::str("CLASSIC_U"));
            }
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
                    // R4-A3: from a covering VALUE index when possible
                    if let Some(rows) = self.index_aggregate(&ctx, cands.list_ref(), cands.len(), &specs, &groups)? {
                        let rows = apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?;
                        r.set("aggregate", Val::List(specs.iter().map(|s| s.to_val()).collect()));
                        r.set("group_by", Val::List(groups.iter().map(|g| Val::str(g.as_str())).collect()));
                        r.set("returned", Val::Int(rows.len() as i64));
                        r.set("rows", Val::List(rows));
                        move_last(&mut r, "returned");
                        if o.explain {
                            r.set("plan", take_plan(&mut plan)?);
                        }
                        return Ok((r, None));
                    }
                    let mut agg = Aggregator::new(&specs, &groups);
                    let mut guard = ResultGuard::new(self.db);
                    // P007: rows in chunks (bounded memory); an aggregation
                    // error is reported after every row was built, as before
                    let mut agg_err: Option<Error> = None;
                    self.classic_rows_chunked(&realm, lay, cands.all(), &needed, ctx.lane, &mut |raw| {
                        if agg_err.is_none() {
                            if let Err(e) = agg.add(&|k: &str| raw.get(k).cloned().unwrap_or(Val::None)) {
                                agg_err = Some(e);
                            }
                        }
                        guard.set_total(agg.bytes)
                    })?;
                    if let Some(e) = agg_err {
                        return Err(e);
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
                    // F046: same row shape and column semantics as the general
                    // path (`record_N` + `_record_value` per field; unknown
                    // columns are None); ordering applies to the output rows.
                    let build = |raws: Vec<Val>| -> Result<Vec<Val>> {
                        let mut out = Vec::with_capacity(raws.len());
                        for row in raws {
                            let u = row.get("_u").and_then(|x| x.as_nat()).unwrap_or(Nat::ZERO);
                            let mut o2 = vmap! {"record_N" => self.record_anchor_ref(lay, &u, ctx.lane)?};
                            for f in &fields {
                                o2.set(f.clone(), row.get(f).cloned().unwrap_or(Val::None));
                            }
                            out.push(o2);
                        }
                        Ok(out)
                    };
                    // RUN6: a streaming consumer sorts the finished rows of
                    // every case the key paths below cannot take, bounded
                    // (`sort_rows_bounded`); rows chunk-wise, anchor errors
                    // after all read errors as in the full build
                    let classic_rows_bounded = |src: &mut dyn FnMut(Val) -> Result<()>| -> Result<()> {
                        let mut sink: Option<Error> = None;
                        for chunk in cands.all().chunks(4096) {
                            let rows = match self.classic_field_rows(&realm, lay, chunk, &fields, ctx.lane, Some(&mut sink))? {
                                Some(rows) => rows,
                                None => {
                                    let raws = self.classic_rows(&realm, lay, chunk, &fields, ctx.lane)?;
                                    self.classic_build(lay, raws, &fields, ctx.lane, Some(&mut sink))?
                                }
                            };
                            for row in rows {
                                src(row)?;
                            }
                        }
                        match sink {
                            Some(e) => Err(e),
                            None => Ok(()),
                        }
                    };
                    if ordered && defer && fields.iter().any(|f| f == "result") {
                        r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                        let src = self.sort_rows_bounded(&mut |f| classic_rows_bounded(f), o)?;
                        return Ok((r, Some(src)));
                    }
                    let rows = if ordered && fields.iter().any(|f| f == "result") {
                        // a selected column named `result` can make a row look
                        // like a record envelope to the ordering: keep the full build
                        let mut guard = ResultGuard::new(self.db);
                        let mut raws = Vec::new();
                        for chunk in cands.all().chunks(4096) {
                            for raw in self.classic_rows(&realm, lay, chunk, &fields, ctx.lane)? {
                                // raw row and finished row exist together
                                guard.add(&raw)?;
                                guard.add(&raw)?;
                                raws.push(raw);
                            }
                        }
                        let rows = build(raws)?;
                        apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?
                    } else if ordered {
                        // P007: sort on the keys of the would-be output rows
                        // (record_N + selected fields), collected chunk-wise,
                        // and build only the window.
                        // R4-A2: with LIMIT only a top-k window of the sort
                        // keys is kept (None if not applicable or if a key
                        // column turns out not to be totally ordered).
                        // RR6-01: for a streaming consumer the top-k windows
                        // only while OFFSET+LIMIT fits the sort budget (their
                        // memory grows with the window, not with the budget)
                        let window_fits = !defer
                            || topk_window(&o.limit, &o.offset).is_some_and(|k| k.saturating_mul(TOPK_ENTRY_COST) <= self.db.config.results_sort_memory_bytes.max(1 << 16));
                        let picked = if !window_fits {
                            None
                        } else {
                            match self.index_order_window(&ctx, cands.list_ref(), cands.len(), &fields, o)? {
                                Some(p) => Some(p),
                                None => self.ordered_topk(&realm, lay, cands.all(), &fields, o, ctx.lane)?,
                            }
                        };
                        // R4-C/RUN6: a streaming consumer sorts in runs, any
                        // LIMIT/OFFSET, never unbounded in memory
                        if picked.is_none() && defer {
                            r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                            if let Some(w) = self.external_order(&realm, lay, cands.all(), &fields, o, ctx.lane)? {
                                return Ok((r, Some(RowSource::classic(realm, lay, ctx.lane, w, fields))));
                            }
                            let src = self.sort_rows_bounded(&mut |f| classic_rows_bounded(f), o)?;
                            return Ok((r, Some(src)));
                        }
                        let picked = match picked {
                            Some(p) => p,
                            None => {
                                let names: Vec<String> = match normalize_order_by(o.order_by.as_ref()) {
                                    Ok(v) => v.into_iter().map(|(n, _)| n).collect(),
                                    Err(_) => Vec::new(), // reported by order_limit_indices below
                                };
                                let mut row_us: Vec<Nat> = Vec::with_capacity(cands.len());
                                let mut keys: Vec<Vec<Val>> = Vec::with_capacity(cands.len());
                                let mut kguard = ResultGuard::new(self.db);
                                self.classic_rows_chunked(&realm, lay, cands.all(), &fields, ctx.lane, &mut |raw| {
                                    let k: Vec<Val> = names
                                        .iter()
                                        .map(|n| if fields.iter().any(|f| f == n) { raw.get(n).cloned().unwrap_or(Val::None) } else { Val::None })
                                        .collect();
                                    kguard.add_bytes((64 + k.iter().map(|v| std::mem::size_of::<Val>() + crate::db::val_heap(v)).sum::<usize>()) as u64)?;
                                    keys.push(k);
                                    row_us.push(raw.get("_u").and_then(|x| x.as_nat()).unwrap_or(Nat::ZERO));
                                    Ok(())
                                })?;
                                // same first error as building every row (anchor N per u)
                                for u in &row_us {
                                    self.db.logical_address_in(lay, u, &Val::None, ctx.lane)?;
                                }
                                let mut key = |i: usize, name: &str| -> Result<Val> {
                                    let pos = names.iter().position(|n| n == name);
                                    if fields.iter().any(|f| f == name) {
                                        Ok(pos.map(|p| keys[i][p].clone()).unwrap_or(Val::None))
                                    } else if name == "record_N" {
                                        Ok(Val::Str(self.record_anchor_ref(lay, &row_us[i], ctx.lane)?))
                                    } else {
                                        Ok(Val::None)
                                    }
                                };
                                let picked = order_limit_indices(row_us.len(), o.order_by.as_ref(), &o.limit, &o.offset, &mut key)?;
                                picked.into_iter().map(|i| row_us[i].clone()).collect()
                            }
                        };
                        if defer {
                            r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                            return Ok((r, Some(RowSource::classic(realm, lay, ctx.lane, UWindow::List(picked.into()), fields))));
                        }
                        self.classic_rows_collect(&realm, lay, &picked, &fields, ctx.lane)?
                    } else {
                        if defer {
                            r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                            let w = cands.lazy_window(&o.limit, &o.offset)?;
                            return Ok((r, Some(RowSource::classic(realm, lay, ctx.lane, w, fields))));
                        }
                        let sl = cands.window(&o.limit, &o.offset)?;
                        self.classic_rows_collect(&realm, lay, &sl, &fields, ctx.lane)?
                    };
                    r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                    r.set("rows", Val::List(rows));
                    let n = r.get("rows").and_then(|x| x.as_list()).map(|l| l.len()).unwrap_or(0);
                    r.set("returned", Val::Int(n as i64));
                }
                "record" => {
                    // F046: the same record envelope as `get_n` in the general path.
                    let get = |u: &Nat| -> Result<Val> {
                        let rn = self.db.logical_address_in(lay, u, &Val::None, ctx.lane)?.n;
                        self.db.get_n(&rn, GetMode::Record)
                    };
                    let mut guard = ResultGuard::new(self.db);
                    let mut collect = |us: &[Nat]| -> Result<Vec<Val>> {
                        let mut out = Vec::with_capacity(us.len().min(1 << 16));
                        for u in us {
                            let row = get(u)?;
                            guard.add(&row)?;
                            out.push(row);
                        }
                        Ok(out)
                    };
                    let rows = if ordered {
                        let all = cands.all();
                        match self.topk_rows(all.len(), &mut |i| get(&all[i]), o)? {
                            Some(rows) => guard_rows(rows, &mut guard)?,
                            None => {
                                let rows = collect(all)?;
                                apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?
                            }
                        }
                    } else {
                        collect(&cands.window(&o.limit, &o.offset)?)?
                    };
                    r.set("returned", Val::Int(rows.len() as i64));
                    r.set("rows", Val::List(rows));
                    move_last(&mut r, "returned");
                }
                "n" => {
                    let sl = cands.window(&o.limit, &o.offset)?;
                    let mut guard = ResultGuard::new(self.db);
                    let refs = sl
                        .iter()
                        .map(|u| {
                            let v = self.record_anchor_ref(lay, u, ctx.lane).map(Val::Str)?;
                            guard.add(&v)?;
                            Ok(v)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    r.set("returned", Val::Int(refs.len() as i64));
                    r.set("refs", Val::List(refs));
                    move_last(&mut r, "returned");
                }
                _ => return Err(plan_err(format!("unknown return_mode {}", repr(&rm)))),
            }
            if o.explain {
                r.set("plan", take_plan(&mut plan)?);
            }
            return Ok((r, None));
        }

        let plan = take_plan(&mut plan)?;
        let mut c = self.eval_planned(expr, Some(&plan), &ctx, domain)?;
        if let Some(id) = layout_filter {
            c.ns.retain(|n| {
                n_to_address(n).ok().and_then(|a| self.db.layout_at(a.realm(), a.u()).map(|l| l.layout_id)) == Some(id)
            });
        }
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
                let mut guard = ResultGuard::new(self.db);
                for n in &c.ns {
                    let rec = self.record_envelope_light(n)?;
                    agg.add(&|k: &str| record_value(&rec, k))?;
                    guard.set_total(agg.bytes)?;
                }
                let rows = apply_order_limit(agg.rows(), o.order_by.as_ref(), &o.limit, &o.offset)?;
                r.set("aggregate", Val::List(specs.iter().map(|s| s.to_val()).collect()));
                r.set("group_by", Val::List(groups.iter().map(|g| Val::str(g.as_str())).collect()));
                r.set("rows", Val::List(rows.clone()));
                r.set("returned", Val::Int(rows.len() as i64));
            }
            "count" => r.set("returned", Val::Int(1)),
            "n" => {
                let mut guard = ResultGuard::new(self.db);
                let mut refs: Vec<Val> = Vec::new();
                for n in slice_simple(c.ns.clone(), &o.limit, &o.offset)?.iter() {
                    let v = Val::Str(n_ref(n));
                    guard.add(&v)?;
                    refs.push(v);
                }
                let returned = refs.len() as i64;
                r.set("refs", Val::List(refs));
                r.set("returned", Val::Int(returned));
            }
            "cell" | "record" => {
                let mode = if rm == "cell" { GetMode::Cell } else { GetMode::Record };
                let mut guard = ResultGuard::new(self.db);
                let rows = if ordered {
                    match self.topk_rows(c.ns.len(), &mut |i| self.db.get_n(&c.ns[i], mode), o)? {
                        Some(rows) => guard_rows(rows, &mut guard)?,
                        None => {
                            let mut rows = Vec::new();
                            for n in &c.ns {
                                let row = self.db.get_n(n, mode)?;
                                guard.add(&row)?;
                                rows.push(row);
                            }
                            apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?
                        }
                    }
                } else {
                    self.sliced_rows(&c.ns, &o.limit, &o.offset, mode, &|n| self.db.get_n(n, mode), &mut guard)?
                };
                let returned = rows.len() as i64;
                r.set("rows", Val::List(rows));
                r.set("returned", Val::Int(returned));
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
                if defer && !ordered {
                    r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                    let idx: Vec<usize> = slice_simple((0..c.ns.len()).collect(), &o.limit, &o.offset)?;
                    let (lo, hi) = match (idx.first(), idx.last()) {
                        (Some(a), Some(b)) => (*a, *b + 1),
                        _ => (usize::MAX, usize::MAX),
                    };
                    return Ok((r, Some(RowSource::General { ns: c.ns.into(), lo, hi, pos: 0, fields })));
                }
                if defer && ordered {
                    // RUN6: bounded sort of the finished rows (runs in the
                    // result directory beyond [results] sort_memory_bytes)
                    r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                    let src = self.sort_rows_bounded(
                        &mut |f| {
                            for n in &c.ns {
                                f(build(n)?)?;
                            }
                            Ok(())
                        },
                        o,
                    )?;
                    return Ok((r, Some(src)));
                }
                let mut guard = ResultGuard::new(self.db);
                let rows = if ordered {
                    match self.topk_rows(c.ns.len(), &mut |i| build(&c.ns[i]), o)? {
                        Some(rows) => guard_rows(rows, &mut guard)?,
                        None => {
                            let mut rows = Vec::new();
                            for n in &c.ns {
                                let row = build(n)?;
                                guard.add(&row)?;
                                rows.push(row);
                            }
                            apply_order_limit(rows, o.order_by.as_ref(), &o.limit, &o.offset)?
                        }
                    }
                } else {
                    self.sliced_rows(&c.ns, &o.limit, &o.offset, GetMode::Record, &build, &mut guard)?
                };
                r.set("select", Val::List(fields.iter().map(|f| Val::str(f.as_str())).collect()));
                let returned = rows.len() as i64;
                r.set("rows", Val::List(rows));
                r.set("returned", Val::Int(returned));
            }
            _ => return Err(plan_err(format!("unknown return_mode {}", repr(&rm)))),
        }
        if o.explain {
            r.set("plan", plan);
        }
        Ok((r, None))
    }

    /// Build rows only for the LIMIT/OFFSET window. Candidates outside the
    /// window are pre-checked in the original order, so that exactly the
    /// same first error arises as with a full build.
    fn sliced_rows(&self, ns: &[Nat], limit: &Val, offset: &Val, mode: GetMode, build: &dyn Fn(&Nat) -> Result<Val>, guard: &mut ResultGuard) -> Result<Vec<Val>> {
        let idx: Vec<usize> = slice_simple((0..ns.len()).collect(), limit, offset)?;
        let (lo, hi) = match (idx.first(), idx.last()) {
            (Some(a), Some(b)) => (*a, *b + 1),
            _ => (usize::MAX, usize::MAX),
        };
        let mut out = Vec::with_capacity(idx.len());
        for (i, n) in ns.iter().enumerate() {
            if i >= lo && i < hi {
                let row = build(n)?;
                guard.add(&row)?;
                out.push(row);
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
        self.db.check_open()?;
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
        let mode = unicode::lower(&options.get("mode").map(|m| m.py_str()).unwrap_or_else(|| "all".into()));
        // R4-A4: in a conjunction large postings are decoded only for the
        // documents of the rarest term
        let (postings, dfs) = if op == "phrase" || mode == "all" {
            self.db.fulltext_payloads_conj(idx.index_id, &terms, ctx.lane)?
        } else {
            let p = self.db.fulltext_payloads(idx.index_id, &terms, ctx.lane)?;
            let d = p.iter().map(|(_, x)| x.len() as u64).collect();
            (p, d)
        };
        let tp: Vec<(String, &[textindex::FtPosting])> = postings.iter().map(|(t, p)| (t.clone(), p.as_slice())).collect();
        let candidates = if op == "phrase" {
            textindex::phrase_candidate_docs(&sequence, &tp)
        } else {
            textindex::fulltext_candidate_docs(&tp, &mode)?
        };
        let max_rows = self.db.config.query_max_result_rows;
        if max_rows > 0 {
            check_result_limit(Some(candidates.len() as f64), limit, max_rows)?;
        }
        // R4-A2: candidates in ascending u, scored in chunks (document
        // lengths of one chunk at a time, paged since 0.28.2), kept in a
        // top-k window when LIMIT is given. Same values, order, count and
        // error order as scoring everything and sorting (`bm25_scores`).
        let mut cands = candidates;
        cands.sort_unstable();
        cands.dedup();
        const CHUNK: usize = 65_536;
        let corpus0 = self.db.fulltext_doc_lengths(idx.index_id, ctx.lane, &cands[..cands.len().min(CHUNK)])?;
        let realm_n = ctx.realm_nat().unwrap();
        let rank = |a: &(Nat, f64), b: &(Nat, f64)| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0));
        let window = topk_window(limit, offset);
        let mut kept: Vec<(Nat, f64)> = Vec::new();
        let mut total = 0usize;
        if let Some(mut stream) = textindex::Bm25Stream::new(&tp, &dfs, corpus0.total_docs, corpus0.total_tokens, k1, b)? {
            let mut keep = |kept: &mut Vec<(Nat, f64)>, item: (Nat, f64)| {
                total += 1;
                match window {
                    Some(0) => {}
                    Some(k) => topk_push(kept, k, item, &rank),
                    None => kept.push(item),
                }
            };
            // with an AND filter all scores are collected first: the filter
            // is evaluated only if a document is scored, after every
            // document length was read (same first error as before)
            let mut scored: Option<Vec<(Nat, f64)>> = filter_expr.as_ref().map(|_| Vec::new());
            for (ci, chunk) in cands.chunks(CHUNK).enumerate() {
                let corpus = if ci == 0 { None } else { Some(self.db.fulltext_doc_lengths(idx.index_id, ctx.lane, chunk)?) };
                let lengths = &corpus.as_ref().unwrap_or(&corpus0).lengths;
                for u in chunk {
                    let Some(score) = stream.score(u, lengths.get(u).copied().unwrap_or(0)) else { continue };
                    if !lay.contains_u(u) {
                        continue;
                    }
                    match scored.as_mut() {
                        Some(v) => v.push((u.clone(), score)),
                        None => keep(&mut kept, (u.clone(), score)),
                    }
                }
            }
            if let (Some(f), Some(scored)) = (&filter_expr, scored) {
                if !scored.is_empty() {
                    let allowed = match self.fast_classic_index_us(f, &ctx)? {
                        Some(us) => setops::sorted_unique(us),
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
                    let mut ai = 0usize;
                    for item in scored {
                        while ai < allowed.len() && allowed[ai] < item.0 {
                            ai += 1;
                        }
                        if ai < allowed.len() && allowed[ai] == item.0 {
                            keep(&mut kept, item);
                        }
                    }
                }
            }
        }
        drop(cands);
        kept.sort_by(rank);
        let sliced = slice_simple(kept, limit, offset)?;
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
        let mut guard = ResultGuard::new(self.db);
        guard.add_bytes((sliced.len() * 48) as u64)?;
        let mut rows = Vec::with_capacity(sliced.len());
        for (u, score) in &sliced {
            let mut row = vmap! {"record_N" => self.record_anchor_ref(lay, u, ctx.lane)?, "relevance" => *score};
            let raw = data_by_u.get(u);
            for f in &fields {
                row.set(f.clone(), raw.and_then(|r| r.get(f)).cloned().unwrap_or(Val::None));
            }
            guard.add(&row)?;
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
        let dense: u64 = self.db.dense.get(&key).map(|ix| ix.records).unwrap_or(0);
        let sparse = self.db.sparse.get(&key);
        let groups: u64 = sparse.map(|ix| ix.groups).unwrap_or(0);
        if domain == Domain::Record {
            if !self.db.overlay_keys.contains(&key) && dense > 0 {
                return Some(BigInt::from(dense));
            }
            let max_u = self.db.max_u.get(&key).map(|n| n.to_bigint()).unwrap_or_default();
            return Some(max_u.max(BigInt::from(dense.max(groups))));
        }
        let cells: u64 = sparse.map(|ix| ix.cells).unwrap_or(0);
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
        self.eval_planned(expr, None, ctx, domain)
    }

    /// Child plan `i` of a BOOL plan node (same flattened order as `eval`).
    fn child_plan(plan: Option<&Val>, i: usize) -> Option<&Val> {
        match plan?.get("children")? {
            Val::List(l) => l.get(i),
            _ => None,
        }
    }

    /// `eval` with the plan of `expr` already computed by `plan_expr` for the
    /// same context and domain (P022): AND nodes take the estimates of their
    /// children from it instead of re-planning every subtree on every level
    /// (quadratic in the nesting depth). Without a plan the children are
    /// planned on demand, exactly as before.
    fn eval_planned(&self, expr: &Expr, plan: Option<&Val>, ctx: &QueryContext, domain: Domain) -> Result<CandidateSet> {
        let c = self.eval_node(expr, plan, ctx, domain)?;
        // RUN¹⁰: checkpoint of the RAM limiter after every large candidate set
        crate::limiter::job::step(
            crate::limiter::JobKind::Rigid,
            crate::limiter::JobPath::Candidates,
            (c.ns.capacity() * std::mem::size_of::<Nat>()) as u64,
        )?;
        Ok(c)
    }

    fn eval_node(&self, expr: &Expr, plan: Option<&Val>, ctx: &QueryContext, domain: Domain) -> Result<CandidateSet> {
        match expr {
            Expr::Bool { op, children } => {
                if op == "not" {
                    let child = self.eval_planned(&children[0], Self::child_plan(plan, 0), ctx, domain)?;
                    let ns = self.universe_minus(ctx, domain, &child.ns)?;
                    return Ok(CandidateSet::new(domain, ns, "NOT"));
                }
                if children.is_empty() {
                    return Ok(CandidateSet::new(domain, Vec::new(), &op.to_uppercase()));
                }
                let kids: Vec<&Expr> = if op == "and" || op == "or" {
                    let mut v = Vec::new();
                    flatten_bool(expr, op, &mut v);
                    v
                } else {
                    children.iter().collect()
                };
                let given: Vec<Option<&Val>> = (0..kids.len()).map(|i| Self::child_plan(plan, i)).collect();
                let mut computed: Vec<Option<Val>> = vec![None; kids.len()];
                let mut order: Vec<usize> = (0..kids.len()).collect();
                if op == "and" {
                    for (i, k) in kids.iter().enumerate() {
                        if given[i].is_none() {
                            computed[i] = Some(self.plan_expr(k, ctx, domain)?);
                        }
                    }
                    let mut ranked = Vec::with_capacity(kids.len());
                    for i in 0..kids.len() {
                        let p = given[i].or(computed[i].as_ref()).unwrap();
                        ranked.push((est_key(p.get("estimated_rows")), est_key(p.get("estimated_cost")), i));
                    }
                    ranked.sort_by(|a, b| cmp_est(&a.0, &b.0).then_with(|| cmp_est(&a.1, &b.1)).then(a.2.cmp(&b.2)));
                    order = ranked.into_iter().map(|x| x.2).collect();
                }
                let kid_plan = |i: usize| given[i].or(computed[i].as_ref());
                let mut acc = self.eval_planned(kids[order[0]], kid_plan(order[0]), ctx, domain)?;
                for &i in &order[1..] {
                    if op == "and" && acc.ns.is_empty() {
                        break;
                    }
                    let child = self.eval_planned(kids[i], kid_plan(i), ctx, domain)?;
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
            let us = self.routed_us(&realm, us);
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
                self.stored_us_in(&realm, ctx.lane, &lo_n, &hi_n)?
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

    /// P006: every u in `[lo, hi]` that has a dense segment or a sparse u
    /// group in `(realm, lane)`, sorted and unique, found via the interval
    /// indexes (no scan of the whole realm). A superset of the u that
    /// `get_record_view` reports as present (it needs a dense record or a
    /// sparse group); the caller still decides presence per u, so the result
    /// equals iterating every u of the range.
    fn stored_us_in(&self, realm: &Nat, lane: Lane, lo: &Nat, hi: &Nat) -> Result<Vec<Nat>> {
        let key = (realm.clone(), lane);
        let mut out: Vec<Nat> = Vec::new();
        if let Some(ix) = self.db.dense.get(&key) {
            let mut segs = Vec::new();
            ix.idx.overlapping(self.db, lo, hi, &mut segs)?;
            let mut ranges: Vec<(Nat, Nat)> =
                segs.iter().map(|s| (s.u_start.clone().max(lo.clone()), s.u_end.clone().min(hi.clone()))).collect();
            ranges.sort();
            let mut next = Nat::ZERO;
            for (a, b) in ranges {
                let mut u = if a < next { next.clone() } else { a };
                while u <= b {
                    out.push(u.clone());
                    u = u.add_u64(1);
                }
                if u > next {
                    next = u;
                }
            }
        }
        if let Some(sx) = self.db.sparse.get(&key) {
            let mut segs = Vec::new();
            sx.idx.overlapping(self.db, lo, hi, &mut segs)?;
            for seg in &segs {
                let bi = self.db.sparse_block_index(seg)?;
                let start = bi.partition_point(|(u, _, _)| u < lo);
                for (u, _, _) in bi[start..].iter() {
                    if u > hi {
                        break;
                    }
                    out.push(u.clone());
                }
            }
        }
        Ok(setops::sorted_unique(out))
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
    /// RUN9 V6: `universe(ctx, domain) \ child` (`child` sorted, unique)
    /// without materializing the universe: every N is checked against
    /// `child` as it is produced, so only the result list is built. The
    /// universe arrives in ascending order on the usual paths; should an N
    /// arrive out of order, the rest is collected unfiltered and the list is
    /// sorted, deduplicated and filtered once more at the end (same result
    /// as `difference_sorted(universe, child)`).
    fn universe_minus(&self, ctx: &QueryContext, domain: Domain, child: &[Nat]) -> Result<Vec<Nat>> {
        struct Minus<'c> {
            child: &'c [Nat],
            j: usize,
            sorted: bool,
            /// last N seen (kept or filtered): the order check needs it, not
            /// the last one kept
            last: Option<Nat>,
            out: Vec<Nat>,
            /// RUN¹⁰: checkpoint when the result list doubles its capacity
            meter: crate::limiter::Meter,
        }
        impl Minus<'_> {
            fn push(&mut self, n: Nat) -> Result<()> {
                if self.sorted {
                    if self.last.as_ref().is_some_and(|l| *l >= n) {
                        self.sorted = false;
                        self.last = None;
                    } else {
                        self.last = Some(n.clone());
                        while self.j < self.child.len() && self.child[self.j] < n {
                            self.j += 1;
                        }
                        if self.j < self.child.len() && self.child[self.j] == n {
                            return Ok(());
                        }
                    }
                }
                if self.out.len() == self.out.capacity() {
                    self.meter.grow((self.out.capacity().max(4) * std::mem::size_of::<Nat>()) as u64)?;
                }
                self.out.push(n);
                Ok(())
            }
            fn finish(self) -> Vec<Nat> {
                if self.sorted {
                    return self.out;
                }
                let mut out = setops::sorted_unique(self.out);
                let (child, mut j) = (self.child, 0usize);
                out.retain(|n| {
                    while j < child.len() && child[j] < *n {
                        j += 1;
                    }
                    !(j < child.len() && child[j] == *n)
                });
                out
            }
        }
        let mut m = Minus {
            child,
            j: 0,
            sorted: true,
            last: None,
            out: Vec::new(),
            meter: crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Candidates),
        };
        let realm_f = ctx.realm_nat();
        if domain == Domain::Record {
            if let Some((lay, ranges)) = self.classic_universe_ranges(ctx, false)? {
                for (a, b) in &ranges {
                    let mut u = a.clone();
                    while &u <= b {
                        m.push(self.db.logical_address_in(lay, &u, &Val::None, ctx.lane)?.n)?;
                        u = u.add_u64(1);
                    }
                }
                return Ok(m.finish());
            }
            let mut cur = RecordNCursor::new(self.db, realm_f.as_ref(), Some(ctx.lane));
            loop {
                let batch = cur.next_batch_nat(self.db, 4096)?;
                if batch.is_empty() {
                    break;
                }
                for n in batch {
                    m.push(n)?;
                }
            }
        } else {
            RawCellCursor::new(self.db, realm_f.as_ref(), Some(ctx.lane)).for_each(self.db, |c| {
                m.push(address::dynadron_n(&c.realm, &c.u, &c.v, c.lane)?)?;
                Ok(true)
            })?;
        }
        Ok(m.finish())
    }

    fn universe(&self, ctx: &QueryContext, domain: Domain) -> Result<CandidateSet> {
        let realm_f = ctx.realm_nat();
        let mut refs = Vec::new();
        if domain == Domain::Record {
            // P007: record_N straight from the segment metadata (no decoding)
            if let Some((lay, ranges)) = self.classic_universe_ranges(ctx, false)? {
                for u in expand_ranges(&ranges) {
                    refs.push(self.db.logical_address_in(lay, &u, &Val::None, ctx.lane)?.n);
                }
                return Ok(CandidateSet::new(domain, setops::sorted_unique(refs), "MATERIALIZED_UNIVERSE"));
            }
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
        layout_id: None,
    })
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "query_tests.rs"]
mod tests;
