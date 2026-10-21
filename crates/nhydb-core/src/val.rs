//! Dynamic value (`Val`) for cell values, layout parameters and
//! dict-like API results, including unbounded integers.
//!
//! The JSON codec is compatible with Python's
//! `json.dumps(obj, ensure_ascii=False, sort_keys=True, separators=(",",":"))`
//! (`json_bytes`), so that metadata blocks are produced byte-identically.

use num_bigint::{BigInt, Sign};
use num_traits::{ToPrimitive, Zero};
use std::fmt::Write as _;

use crate::error::{Error, Result};
use crate::nat::Nat;

#[derive(Clone, Debug, Default)]
pub enum Val {
    #[default]
    None,
    Bool(bool),
    /// Integer within the i64 range.
    Int(i64),
    /// Integer outside the i64 range.
    Big(BigInt),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Val>),
    /// Ordered map (insertion order, like Python dicts).
    Map(Vec<(String, Val)>),
}


impl PartialEq for Val {
    /// Python-like equality: `1 == 1.0 == True`, dicts order-independent.
    fn eq(&self, other: &Self) -> bool {
        use Val::*;
        match (self, other) {
            (None, None) => true,
            (Str(a), Str(b)) => a == b,
            (Bytes(a), Bytes(b)) => a == b,
            (List(a), List(b)) => a == b,
            (Map(a), Map(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.iter().any(|(k2, v2)| k == k2 && v == v2))
            }
            _ => match (self.as_number(), other.as_number()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
        }
    }
}

/// Numeric view for comparisons.
#[derive(Debug, Clone)]
pub enum Num {
    I(BigInt),
    F(f64),
}

impl PartialEq for Num {
    fn eq(&self, o: &Self) -> bool {
        self.partial_cmp(o) == Some(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        match (self, o) {
            (Num::I(a), Num::I(b)) => Some(a.cmp(b)),
            (Num::F(a), Num::F(b)) => a.partial_cmp(b),
            (Num::I(a), Num::F(b)) => cmp_int_float(a, *b),
            (Num::F(a), Num::I(b)) => cmp_int_float(b, *a).map(|o| o.reverse()),
        }
    }
}

fn cmp_int_float(a: &BigInt, b: f64) -> Option<std::cmp::Ordering> {
    if b.is_nan() {
        return None;
    }
    if b.is_infinite() {
        return Some(if b > 0.0 { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater });
    }
    let fl = b.floor();
    let bi = BigInt::from(fl as i128); // sufficient for typical values
    let bi = if fl.abs() < 1e38 { bi } else { float_to_bigint(fl) };
    match a.cmp(&bi) {
        std::cmp::Ordering::Equal => {
            if b > fl { Some(std::cmp::Ordering::Less) } else { Some(std::cmp::Ordering::Equal) }
        }
        o => Some(o),
    }
}

fn float_to_bigint(f: f64) -> BigInt {
    use num_traits::FromPrimitive;
    BigInt::from_f64(f).unwrap_or_default()
}

impl Val {
    pub fn int<T: Into<i128>>(v: T) -> Val {
        let v: i128 = v.into();
        if v >= i64::MIN as i128 && v <= i64::MAX as i128 {
            Val::Int(v as i64)
        } else {
            Val::Big(BigInt::from(v))
        }
    }

    pub fn from_bigint(v: BigInt) -> Val {
        match v.to_i64() {
            Some(x) => Val::Int(x),
            None => Val::Big(v),
        }
    }

    pub fn nat(n: &Nat) -> Val {
        match n {
            Nat::S(v) if *v <= i64::MAX as u64 => Val::Int(*v as i64),
            _ => Val::Big(n.to_bigint()),
        }
    }

    pub fn opt_nat(n: Option<&Nat>) -> Val {
        n.map(Val::nat).unwrap_or(Val::None)
    }

    pub fn str<S: Into<String>>(s: S) -> Val {
        Val::Str(s.into())
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Val::None)
    }

    pub fn as_number(&self) -> Option<Num> {
        match self {
            Val::Bool(b) => Some(Num::I(BigInt::from(*b as i64))),
            Val::Int(i) => Some(Num::I(BigInt::from(*i))),
            Val::Big(b) => Some(Num::I(b.clone())),
            Val::Float(f) => Some(Num::F(*f)),
            _ => None,
        }
    }

    /// Integer view (bool counts as int, as in Python).
    pub fn as_bigint(&self) -> Option<BigInt> {
        match self {
            Val::Bool(b) => Some(BigInt::from(*b as i64)),
            Val::Int(i) => Some(BigInt::from(*i)),
            Val::Big(b) => Some(b.clone()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Val::Bool(b) => Some(*b as i64),
            Val::Int(i) => Some(*i),
            Val::Big(b) => b.to_i64(),
            _ => None,
        }
    }

    pub fn as_nat(&self) -> Option<Nat> {
        match self {
            Val::Bool(b) => Some(Nat::S(*b as u64)),
            Val::Int(i) => Nat::from_i64(*i),
            Val::Big(b) => Nat::from_bigint(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<Val>> {
        match self {
            Val::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Vec<(String, Val)>> {
        match self {
            Val::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Val::None => false,
            Val::Bool(b) => *b,
            Val::Int(i) => *i != 0,
            Val::Big(b) => !b.is_zero(),
            Val::Float(f) => *f != 0.0,
            Val::Str(s) => !s.is_empty(),
            Val::Bytes(b) => !b.is_empty(),
            Val::List(l) => !l.is_empty(),
            Val::Map(m) => !m.is_empty(),
        }
    }

    /// Key access on map values.
    pub fn get(&self, key: &str) -> Option<&Val> {
        match self {
            Val::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Val> {
        match self {
            Val::Map(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Sets/replaces a map entry (its position is preserved).
    pub fn set<K: Into<String>>(&mut self, key: K, value: Val) {
        if let Val::Map(m) = self {
            let key = key.into();
            if let Some(slot) = m.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = value;
            } else {
                m.push((key, value));
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Val> {
        if let Val::Map(m) = self {
            if let Some(i) = m.iter().position(|(k, _)| k == key) {
                return Some(m.remove(i).1);
            }
        }
        None
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn map() -> Val {
        Val::Map(Vec::new())
    }

    /// Python `str()` for scalar values (for `utf8` fields: `str(value)`).
    pub fn py_str(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            _ => self.py_repr(),
        }
    }

    /// Python `repr()`.
    pub fn py_repr(&self) -> String {
        match self {
            Val::None => "None".into(),
            Val::Bool(b) => if *b { "True".into() } else { "False".into() },
            Val::Int(i) => i.to_string(),
            Val::Big(b) => b.to_string(),
            Val::Float(f) => py_float_repr(*f),
            Val::Str(s) => py_str_repr(s),
            Val::Bytes(b) => py_bytes_repr(b),
            Val::List(l) => {
                let parts: Vec<String> = l.iter().map(|x| x.py_repr()).collect();
                format!("[{}]", parts.join(", "))
            }
            Val::Map(m) => {
                let parts: Vec<String> =
                    m.iter().map(|(k, v)| format!("{}: {}", py_str_repr(k), v.py_repr())).collect();
                format!("{{{}}}", parts.join(", "))
            }
        }
    }

    // ------------------------------------------------------------------
    // JSON
    // ------------------------------------------------------------------

    /// `json.dumps(..., ensure_ascii=False, sort_keys=True, separators=(",",":"))`.
    pub fn to_json(&self) -> Result<String> {
        let mut out = String::new();
        self.write_json(&mut out, true, false)?;
        Ok(out)
    }

    /// `json.dumps(..., sort_keys=True, separators=(",",":"))` with the
    /// Python default `ensure_ascii=True` (non-ASCII as `\uXXXX`,
    /// characters outside the BMP as a surrogate pair). Used for
    /// MATH fingerprints.
    pub fn to_json_ascii(&self) -> Result<String> {
        let mut out = String::new();
        self.write_json(&mut out, true, true)?;
        Ok(out)
    }

    /// JSON without key sorting (insertion order).
    pub fn to_json_unsorted(&self) -> Result<String> {
        let mut out = String::new();
        self.write_json(&mut out, false, false)?;
        Ok(out)
    }

    fn write_json(&self, out: &mut String, sort: bool, ascii: bool) -> Result<()> {
        match self {
            Val::None => out.push_str("null"),
            Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Val::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Val::Big(b) => {
                let _ = write!(out, "{b}");
            }
            Val::Float(f) => {
                if f.is_nan() {
                    out.push_str("NaN")
                } else if f.is_infinite() {
                    out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" })
                } else {
                    out.push_str(&py_float_repr(*f))
                }
            }
            Val::Str(s) => json_str_mode(out, s, ascii),
            Val::Bytes(_) => {
                return Err(Error::Type("Object of type bytes is not JSON serializable".into()));
            }
            Val::List(l) => {
                out.push('[');
                for (i, x) in l.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    x.write_json(out, sort, ascii)?;
                }
                out.push(']');
            }
            Val::Map(m) => {
                out.push('{');
                let mut items: Vec<&(String, Val)> = m.iter().collect();
                if sort {
                    items.sort_by(|a, b| a.0.cmp(&b.0));
                }
                for (i, (k, v)) in items.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json_str_mode(out, k, ascii);
                    out.push(':');
                    v.write_json(out, sort, ascii)?;
                }
                out.push('}');
            }
        }
        Ok(())
    }

    pub fn from_json(s: &str) -> Result<Val> {
        let mut p = JsonParser { s: s.as_bytes(), i: 0, depth: 0 };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(Error::Corruption("JSON: unexpected trailing characters".into()));
        }
        Ok(v)
    }
}

fn json_str_mode(out: &mut String, s: &str, ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", unit);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `repr(float)`: shortest round-trip representation,
/// exponential notation for exponents < -4 or >= 16.
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's `{:e}` yields the shortest round-trip digit sequence.
    let sci = format!("{:e}", f);
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let neg = mant.starts_with('-');
    let mant = mant.trim_start_matches('-');
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let e = exp as usize;
            if digits.len() > e + 1 {
                out.push_str(&digits[..e + 1]);
                out.push('.');
                out.push_str(&digits[e + 1..]);
            } else {
                out.push_str(&digits);
                for _ in 0..(e + 1 - digits.len()) {
                    out.push('0');
                }
                out.push_str(".0");
            }
        } else {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    out
}

/// Python's `repr(str)`.
pub fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !crate::unicode::is_printable(c) => {
                let cp = c as u32;
                if cp <= 0xff {
                    let _ = write!(out, "\\x{:02x}", cp);
                } else if cp <= 0xffff {
                    let _ = write!(out, "\\u{:04x}", cp);
                } else {
                    let _ = write!(out, "\\U{:08x}", cp);
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

pub fn py_bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { '"' } else { '\'' };
    let mut out = String::from("b");
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            c if c as char == quote => {
                out.push('\\');
                out.push(c as char);
            }
            0x20..=0x7e => out.push(c as char),
            c => {
                let _ = write!(out, "\\x{:02x}", c);
            }
        }
    }
    out.push(quote);
    out
}

/// Nesting limit of the JSON parser (R009): metadata JSON is flat; deeper
/// input is rejected instead of risking a stack overflow.
const JSON_MAX_DEPTH: usize = 256;

struct JsonParser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

impl JsonParser<'_> {
    fn err(&self, m: &str) -> Error {
        Error::Corruption(format!("invalid JSON at {}: {m}", self.i))
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn lit(&mut self, t: &str) -> bool {
        if self.s[self.i..].starts_with(t.as_bytes()) {
            self.i += t.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Val> {
        if self.depth >= JSON_MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        self.depth += 1;
        let r = self.value_inner();
        self.depth -= 1;
        r
    }

    fn value_inner(&mut self) -> Result<Val> {
        self.ws();
        let Some(&c) = self.s.get(self.i) else {
            return Err(self.err("unexpected end"));
        };
        match c {
            b'{' => {
                self.i += 1;
                let mut m = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Val::Map(m));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return Err(self.err("key expected"));
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return Err(self.err("':' expected"));
                    }
                    self.i += 1;
                    let v = self.value()?;
                    // Python json: later duplicate keys win.
                    if let Some(slot) = m.iter_mut().find(|(kk, _): &&mut (String, Val)| *kk == k) {
                        slot.1 = v;
                    } else {
                        m.push((k, v));
                    }
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Val::Map(m));
                        }
                        _ => return Err(self.err("',' or '}' expected")),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut l = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Val::List(l));
                }
                loop {
                    l.push(self.value()?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Val::List(l));
                        }
                        _ => return Err(self.err("',' or ']' expected")),
                    }
                }
            }
            b'"' => Ok(Val::Str(self.string()?)),
            b't' if self.lit("true") => Ok(Val::Bool(true)),
            b'f' if self.lit("false") => Ok(Val::Bool(false)),
            b'n' if self.lit("null") => Ok(Val::None),
            b'N' if self.lit("NaN") => Ok(Val::Float(f64::NAN)),
            b'I' if self.lit("Infinity") => Ok(Val::Float(f64::INFINITY)),
            b'-' if self.lit("-Infinity") => Ok(Val::Float(f64::NEG_INFINITY)),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.err("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Val> {
        let start = self.i;
        if self.s[self.i] == b'-' {
            self.i += 1;
        }
        let mut is_float = false;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' | b'+' | b'-' => is_float = true,
                _ => break,
            }
            self.i += 1;
        }
        let txt = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("number"))?;
        // JSON grammar (as Python's json): no leading zeros like `01` (F056)
        let digits = txt.strip_prefix('-').unwrap_or(txt).as_bytes();
        if digits.len() > 1 && digits[0] == b'0' && digits[1].is_ascii_digit() {
            return Err(self.err("leading zeros"));
        }
        if is_float {
            txt.parse::<f64>().map(Val::Float).map_err(|_| self.err("number"))
        } else if let Ok(v) = txt.parse::<i64>() {
            Ok(Val::Int(v))
        } else {
            BigInt::parse_bytes(txt.as_bytes(), 10).map(Val::Big).ok_or_else(|| self.err("number"))
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        let h = self.s.get(self.i..self.i + 4).ok_or_else(|| self.err("\\u"))?;
        let h = std::str::from_utf8(h).map_err(|_| self.err("\\u"))?;
        let v = u32::from_str_radix(h, 16).map_err(|_| self.err("\\u"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String> {
        self.i += 1; // "
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'"' && self.s[self.i] != b'\\' {
                self.i += 1;
            }
            out.push_str(std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("UTF-8"))?);
            match self.s.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let Some(&e) = self.s.get(self.i) else {
                        return Err(self.err("escape"));
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) && self.s[self.i..].starts_with(b"\\u") {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    self.i = save;
                                }
                            }
                            // F056: a lone surrogate has no UTF-8 form; NHyDB never
                            // writes one, so it is rejected instead of replaced.
                            let Some(c) = char::from_u32(cp) else {
                                return Err(self.err("lone surrogate in \\u escape"));
                            };
                            out.push(c);
                        }
                        _ => return Err(self.err("escape")),
                    }
                }
                _ => return Err(self.err("unterminated string")),
            }
        }
    }
}

/// Hashable key with Python dict semantics for index values
/// (`1`, `1.0` and `True` are the same key).
#[derive(Clone, Debug)]
pub struct ValKey(pub Val);

impl ValKey {
    fn canonical(&self) -> Canon<'_> {
        match &self.0 {
            Val::None => Canon::None,
            Val::Bool(b) => Canon::Int(BigInt::from(*b as i64)),
            Val::Int(i) => Canon::Int(BigInt::from(*i)),
            Val::Big(b) => Canon::Int(b.clone()),
            Val::Float(f) => {
                if f.fract() == 0.0 && f.is_finite() {
                    Canon::Int(float_to_bigint(*f))
                } else {
                    Canon::Float(f.to_bits())
                }
            }
            Val::Str(s) => Canon::Str(s),
            Val::Bytes(b) => Canon::Bytes(b),
            other => Canon::Other(other.py_repr()),
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
enum Canon<'a> {
    None,
    Int(BigInt),
    Float(u64),
    Str(&'a str),
    Bytes(&'a [u8]),
    Other(String),
}

impl PartialEq for ValKey {
    fn eq(&self, o: &Self) -> bool {
        self.canonical() == o.canonical()
    }
}
impl Eq for ValKey {}
impl std::hash::Hash for ValKey {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.canonical().hash(h)
    }
}

/// Python ordering comparison `a < b` / `a == b` / `a > b`.
/// `None` means `TypeError` (incomparable types), as in CPython:
/// numbers (bool/int/float) compare exactly with each other, `str` with `str`
/// (code point order), `bytes` with `bytes`, lists lexicographically;
/// `None`, dicts and mixed types are not orderable.
/// NaN yields "neither less nor greater", as in Python (here `None`).
pub fn py_cmp(a: &Val, b: &Val) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    match (a, b) {
        (Val::Str(x), Val::Str(y)) => Some(x.cmp(y)),
        (Val::Bytes(x), Val::Bytes(y)) => Some(x.cmp(y)),
        (Val::List(x), Val::List(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                if p != q {
                    return py_cmp(p, q);
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        _ => match (a.as_number(), b.as_number()) {
            (Some(x), Some(y)) => x.partial_cmp(&y),
            _ => {
                let _ = Ordering::Equal;
                None
            }
        },
    }
}

/// Type class used to decide whether a Python `sorted()` over keys succeeds
/// without `TypeError` (any comparison sort has to compare elements of
/// different classes with each other).
fn order_class(v: &Val) -> u8 {
    match v {
        Val::Bool(_) | Val::Int(_) | Val::Big(_) | Val::Float(_) => 0,
        Val::Str(_) => 1,
        Val::Bytes(_) => 2,
        Val::List(_) => 3,
        Val::None => 4,
        Val::Map(_) => 5,
    }
}

/// `sorted(keys)` with fallback `sorted(keys, key=repr)` on `TypeError`
/// (directory order of SECONDARY blocks).
pub fn py_sort_keys<T>(items: &mut [T], key: impl Fn(&T) -> &Val) {
    let natural = match items.first() {
        None => true,
        Some(first) => {
            let c = order_class(key(first));
            let all_same = items.iter().all(|x| order_class(key(x)) == c);
            all_same && (items.len() < 2 || c <= 3)
                && (c != 3 || items.windows(2).all(|w| py_cmp(key(&w[0]), key(&w[1])).is_some()))
        }
    };
    if natural {
        items.sort_by(|a, b| py_cmp(key(a), key(b)).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        items.sort_by_key(|x| key(x).py_repr());
    }
}

/// Sort key for posting-list directories (`repr(value)`).
pub fn repr_sort_key(v: &Val) -> String {
    v.py_repr()
}

impl From<bool> for Val {
    fn from(b: bool) -> Self {
        Val::Bool(b)
    }
}
impl From<&str> for Val {
    fn from(s: &str) -> Self {
        Val::Str(s.to_string())
    }
}
impl From<String> for Val {
    fn from(s: String) -> Self {
        Val::Str(s)
    }
}
impl From<i64> for Val {
    fn from(i: i64) -> Self {
        Val::Int(i)
    }
}
impl From<&Nat> for Val {
    fn from(n: &Nat) -> Self {
        Val::nat(n)
    }
}
impl From<Nat> for Val {
    fn from(n: Nat) -> Self {
        Val::nat(&n)
    }
}
impl From<f64> for Val {
    fn from(f: f64) -> Self {
        Val::Float(f)
    }
}
impl<T: Into<Val>> From<Option<T>> for Val {
    fn from(o: Option<T>) -> Self {
        o.map(Into::into).unwrap_or(Val::None)
    }
}

/// Convenience constructor for ordered maps: `vmap! { "a" => 1, ... }`.
#[macro_export]
macro_rules! vmap {
    () => { $crate::val::Val::Map(Vec::new()) };
    ($($k:expr => $v:expr),+ $(,)?) => {
        $crate::val::Val::Map(vec![$(($k.to_string(), $crate::val::Val::from($v))),+])
    };
}

impl Val {
    pub fn sign_is_negative(&self) -> bool {
        match self {
            Val::Int(i) => *i < 0,
            Val::Big(b) => b.sign() == Sign::Minus,
            Val::Float(f) => *f < 0.0,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn json_depth_limit_r009() {
        let ok = format!("{}1{}", "[".repeat(255), "]".repeat(255));
        assert!(Val::from_json(&ok).is_ok());
        for n in [257usize, 100_000] {
            let deep = format!("{}1{}", "[".repeat(n), "]".repeat(n));
            assert!(matches!(Val::from_json(&deep), Err(Error::Corruption(_))));
            let deep = "{\"a\":".repeat(n) + "1" + &"}".repeat(n);
            assert!(matches!(Val::from_json(&deep), Err(Error::Corruption(_))));
        }
        assert!(Val::from_json("01").is_err() && Val::from_json("-00").is_err());
        assert!(Val::from_json("0").is_ok() && Val::from_json("-0.5").is_ok() && Val::from_json("10").is_ok());
    }

    #[test]
    fn json_lone_surrogate_is_rejected_f056() {
        for bad in [r#""\ud800""#, r#""\udc00""#, r#""a\ud83dx""#, r#""\ud83d\u0041""#] {
            assert!(matches!(Val::from_json(bad), Err(Error::Corruption(_))), "{bad}");
        }
        assert_eq!(Val::from_json(r#""\ud83d\ude00""#).unwrap(), Val::Str("\u{1F600}".into()));
        assert_eq!(Val::from_json(r#""\u00e4""#).unwrap(), Val::Str("ä".into()));
    }

    use super::*;

    #[test]
    fn float_repr_like_python() {
        assert_eq!(py_float_repr(1.0), "1.0");
        assert_eq!(py_float_repr(9.5), "9.5");
        assert_eq!(py_float_repr(0.1), "0.1");
        assert_eq!(py_float_repr(1e16), "1e+16");
        assert_eq!(py_float_repr(1e15), "1000000000000000.0");
        assert_eq!(py_float_repr(0.0001), "0.0001");
        assert_eq!(py_float_repr(0.00001), "1e-05");
        assert_eq!(py_float_repr(-2.5e-7), "-2.5e-07");
        assert_eq!(py_float_repr(123456.789), "123456.789");
        assert_eq!(py_float_repr(1e100), "1e+100");
    }

    #[test]
    fn json_roundtrip_and_sorting() {
        let v = vmap! {"b" => 1i64, "a" => "x\"y\nü", "c" => Val::Big(BigInt::from(10u32).pow(100))};
        let s = v.to_json().unwrap();
        assert!(s.starts_with("{\"a\":\"x\\\"y\\nü\",\"b\":1,\"c\":1000"));
        let back = Val::from_json(&s).unwrap();
        assert_eq!(back, v);
        let f = Val::from_json("[1.5,-2,true,null,NaN]").unwrap();
        assert_eq!(f.as_list().unwrap().len(), 5);
    }

    #[test]
    fn valkey_semantics() {
        use std::collections::HashSet;
        let mut s = HashSet::new();
        s.insert(ValKey(Val::Int(1)));
        assert!(s.contains(&ValKey(Val::Float(1.0))));
        assert!(s.contains(&ValKey(Val::Bool(true))));
        assert!(!s.contains(&ValKey(Val::Str("1".into()))));
    }

    #[test]
    fn repr() {
        assert_eq!(py_str_repr("abc"), "'abc'");
        assert_eq!(py_str_repr("it's"), "\"it's\"");
        assert_eq!(Val::Bool(true).py_repr(), "True");
    }

    #[test]
    fn json_ascii_mode_matches_python() {
        // python: json.dumps({"b":"ä€😀\x7f","a":[1,"x"]}, sort_keys=True, separators=(",",":"))
        let v = Val::from_json(r#"{"b":"\u00e4\u20ac\ud83d\ude00\u007f","a":[1,"x"]}"#).unwrap();
        assert_eq!(v.to_json_ascii().unwrap(), r#"{"a":[1,"x"],"b":"\u00e4\u20ac\ud83d\ude00\u007f"}"#);
        assert_eq!(v.to_json().unwrap(), "{\"a\":[1,\"x\"],\"b\":\"ä€😀\u{7f}\"}");
    }
}
