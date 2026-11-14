//! Data types, field definitions and value codec (`enc_value`/`dec_value`).

use num_bigint::{BigInt, Sign};
use num_traits::{ToPrimitive, Zero};

use crate::error::{Error, Result};
use crate::nat::{get_vu, put_vi64, put_vi_big, put_vu64, zigzag_decode, Nat};
use crate::val::Val;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
#[repr(u8)]
pub enum TypeCode {
    U8 = 1,
    U16 = 2,
    U32 = 3,
    U64 = 4,
    I8 = 5,
    I16 = 6,
    I32 = 7,
    I64 = 8,
    F32 = 9,
    F64 = 10,
    Bool = 11,
    VarUInt = 12,
    VarInt = 13,
    Utf8 = 14,
    Bytes = 15,
}

pub const ALL_TYPES: [TypeCode; 15] = [
    TypeCode::U8,
    TypeCode::U16,
    TypeCode::U32,
    TypeCode::U64,
    TypeCode::I8,
    TypeCode::I16,
    TypeCode::I32,
    TypeCode::I64,
    TypeCode::F32,
    TypeCode::F64,
    TypeCode::Bool,
    TypeCode::VarUInt,
    TypeCode::VarInt,
    TypeCode::Utf8,
    TypeCode::Bytes,
];

impl TypeCode {
    pub fn name(self) -> &'static str {
        match self {
            TypeCode::U8 => "u8",
            TypeCode::U16 => "u16",
            TypeCode::U32 => "u32",
            TypeCode::U64 => "u64",
            TypeCode::I8 => "i8",
            TypeCode::I16 => "i16",
            TypeCode::I32 => "i32",
            TypeCode::I64 => "i64",
            TypeCode::F32 => "f32",
            TypeCode::F64 => "f64",
            TypeCode::Bool => "bool",
            TypeCode::VarUInt => "varuint",
            TypeCode::VarInt => "varint",
            TypeCode::Utf8 => "utf8",
            TypeCode::Bytes => "bytes",
        }
    }

    pub fn from_name(s: &str) -> Option<TypeCode> {
        ALL_TYPES.iter().copied().find(|t| t.name() == s)
    }

    pub fn from_code(c: u8) -> Option<TypeCode> {
        if (1..=15).contains(&c) { Some(ALL_TYPES[(c - 1) as usize]) } else { None }
    }

    pub fn code(self) -> u8 {
        self as u8
    }

    /// Size of fixed-width types in bytes.
    pub fn fixed_size(self) -> Option<usize> {
        Some(match self {
            TypeCode::U8 | TypeCode::I8 | TypeCode::Bool => 1,
            TypeCode::U16 | TypeCode::I16 => 2,
            TypeCode::U32 | TypeCode::I32 | TypeCode::F32 => 4,
            TypeCode::U64 | TypeCode::I64 | TypeCode::F64 => 8,
            _ => return None,
        })
    }

    /// Integer value range of fixed-width integer types.
    fn int_range(self) -> Option<(i128, i128)> {
        Some(match self {
            TypeCode::U8 => (0, u8::MAX as i128),
            TypeCode::U16 => (0, u16::MAX as i128),
            TypeCode::U32 => (0, u32::MAX as i128),
            TypeCode::U64 => (0, u64::MAX as i128),
            TypeCode::I8 => (i8::MIN as i128, i8::MAX as i128),
            TypeCode::I16 => (i16::MIN as i128, i16::MAX as i128),
            TypeCode::I32 => (i32::MIN as i128, i32::MAX as i128),
            TypeCode::I64 => (i64::MIN as i128, i64::MAX as i128),
            _ => return None,
        })
    }
}

/// Enum mapping label → code (insertion order preserved).
pub type EnumMap = Vec<(String, i64)>;

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDef {
    pub v: Nat,
    pub name: String,
    pub typ: TypeCode,
    pub enum_map: Option<EnumMap>,
    pub description: String,
}

impl FieldDef {
    pub fn new(v: Nat, name: impl Into<String>, typ: TypeCode) -> Self {
        FieldDef { v, name: name.into(), typ, enum_map: None, description: String::new() }
    }

    pub fn enum_code(&self, label: &str) -> Option<i64> {
        self.enum_map.as_ref()?.iter().find(|(k, _)| k == label).map(|(_, v)| *v)
    }

    /// Reverse mapping code → label (for duplicate codes the last entry
    /// wins, as with a Python dict comprehension).
    pub fn enum_label(&self, code: i64) -> Option<&str> {
        self.enum_map.as_ref()?.iter().rev().find(|(_, v)| *v == code).map(|(k, _)| k.as_str())
    }
}

pub fn enum_to_val(e: &EnumMap) -> Val {
    Val::Map(e.iter().map(|(k, v)| (k.clone(), Val::Int(*v))).collect())
}

pub fn enum_from_val(v: &Val) -> Result<Option<EnumMap>> {
    match v {
        Val::None => Ok(None),
        Val::Map(m) => {
            let mut out = Vec::with_capacity(m.len());
            for (k, x) in m {
                let code = int_like(x).and_then(|b| b.to_i64()).ok_or_else(|| {
                    Error::Value(format!("enum code for {k:?} is not an integer"))
                })?;
                out.push((k.clone(), code));
            }
            Ok(Some(out))
        }
        _ => Err(Error::Schema("enum must be a dictionary".into())),
    }
}

/// Python `int(value)` for numeric values (floats are truncated,
/// numeric strings are parsed).
pub fn int_like(v: &Val) -> Option<BigInt> {
    match v {
        Val::Bool(b) => Some(BigInt::from(*b as i64)),
        Val::Int(i) => Some(BigInt::from(*i)),
        Val::Big(b) => Some(b.clone()),
        Val::Float(f) if f.is_finite() => {
            use num_traits::FromPrimitive;
            BigInt::from_f64(f.trunc())
        }
        Val::Str(s) => parse_py_int(s),
        _ => None,
    }
}

/// Python `int(str)` (base 10): surrounding Unicode whitespace, optional
/// sign, Unicode decimal digits, single underscores only between digits
/// (F031: `'1__0'`, `'_5'`, `'5_'` are rejected like in Python).
pub fn parse_py_int(s: &str) -> Option<BigInt> {
    let t = crate::unicode::strip(s);
    let (neg, body) = match t.as_bytes().first() {
        Some(b'+') => (false, &t[1..]),
        Some(b'-') => (true, &t[1..]),
        _ => (false, t),
    };
    let mut digits = String::with_capacity(body.len());
    let mut after_digit = false;
    for c in body.chars() {
        if c == '_' {
            if !after_digit {
                return None;
            }
            after_digit = false;
            continue;
        }
        digits.push((b'0' + crate::unicode::decimal_value(c)?) as char);
        after_digit = true;
    }
    if !after_digit {
        return None;
    }
    let v = BigInt::parse_bytes(digits.as_bytes(), 10)?;
    Some(if neg { -v } else { v })
}

fn value_error_fit(f: &FieldDef, value: &Val) -> Error {
    Error::Value(format!("{}: {} does not fit into {}", f.name, value.py_repr(), f.typ.name()))
}

/// Applies the enum mapping (`_enum_to_raw`).
fn enum_to_raw(f: &FieldDef, value: &Val) -> Result<Option<Val>> {
    if f.enum_map.is_none() {
        return Ok(None);
    }
    if let Val::Str(s) = value {
        return match f.enum_code(s) {
            Some(c) => Ok(Some(Val::Int(c))),
            None => Err(Error::Value(format!("{}: unknown enum value {}", f.name, value.py_repr()))),
        };
    }
    match int_like(value) {
        Some(b) => Ok(Some(Val::from_bigint(b))),
        None => Err(Error::Type(format!("int() argument must be a string or a number, not {}", value.py_repr()))),
    }
}

/// Encodes a value according to the field type (including enum) and appends the bytes.
pub fn encode_value(f: &FieldDef, value: &Val, out: &mut Vec<u8>) -> Result<()> {
    // E1 (R9E-01): a typed (DENSE) field cannot store `None`, whatever its
    // type – no silent `"None"` (utf8) or `False` (bool) as in v0.14. The
    // SPARSE writer stores a null cell without calling this (`is_null`).
    if let Val::None = value {
        return Err(Error::Value(format!(
            "field '{}': a DENSE field cannot store None (store a null cell with put_cell(..., None), remove a value with delete_cell)",
            f.name
        )));
    }
    let mapped = enum_to_raw(f, value)?;
    let value = mapped.as_ref().unwrap_or(value);
    encode_raw(f, f.typ, value, out)
}

/// Encoding without enum mapping.
pub fn encode_raw(f: &FieldDef, typ: TypeCode, value: &Val, out: &mut Vec<u8>) -> Result<()> {
    match typ {
        TypeCode::F32 => {
            let x = float_like(f, value)?;
            let y = x as f32;
            if x.is_finite() && y.is_infinite() {
                return Err(value_error_fit(f, value));
            }
            out.extend_from_slice(&y.to_le_bytes());
        }
        TypeCode::F64 => {
            let x = float_like(f, value)?;
            out.extend_from_slice(&x.to_le_bytes());
        }
        TypeCode::Bool => out.push(value.is_truthy() as u8),
        TypeCode::VarUInt => {
            let n = int_like(value).ok_or_else(|| {
                Error::Type(format!("int() argument must be a string or a number, not {}", value.py_repr()))
            })?;
            if n.sign() == Sign::Minus {
                return Err(Error::Value("varuint expects n>=0".into()));
            }
            match n.to_u64() {
                Some(x) => put_vu64(out, x),
                None => Nat::from(n.magnitude().clone()).put_vu(out),
            }
        }
        TypeCode::VarInt => {
            let n = int_like(value).ok_or_else(|| {
                Error::Type(format!("int() argument must be a string or a number, not {}", value.py_repr()))
            })?;
            match n.to_i64() {
                Some(x) => put_vi64(out, x),
                None => put_vi_big(out, &n),
            }
        }
        TypeCode::Utf8 => {
            if let Val::Bytes(_) = value {
                // F039: Python's str(b'x') would silently store "b'x'"
                return Err(Error::Type(format!("{}: utf8 field expects str, not bytes", f.name)));
            }
            let s = value.py_str();
            put_vu64(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        TypeCode::Bytes => {
            let raw: Vec<u8> = match value {
                Val::Bytes(b) => {
                    put_vu64(out, b.len() as u64);
                    out.extend_from_slice(b);
                    return Ok(());
                }
                Val::Bool(_) | Val::Int(_) | Val::Big(_) => {
                    // Python `bytes(n)`: n zero bytes (F023: bounded, fallible)
                    let n = int_like(value).unwrap_or_default();
                    if n.sign() == Sign::Minus {
                        return Err(Error::Value("negative count".into()));
                    }
                    let n = n.to_u64().filter(|n| *n <= BYTES_FROM_COUNT_MAX).ok_or_else(|| {
                        Error::Value(format!(
                            "{}: bytes({}) exceeds the limit of {} zero bytes for an integer count",
                            f.name,
                            value.py_repr(),
                            BYTES_FROM_COUNT_MAX
                        ))
                    })? as usize;
                    reserve(out, n.saturating_add(10))?;
                    put_vu64(out, n as u64);
                    out.resize(out.len() + n, 0);
                    return Ok(());
                }
                Val::List(l) => {
                    let mut v = Vec::new();
                    reserve(&mut v, l.len())?;
                    for x in l {
                        match x.as_i64() {
                            Some(b) if (0..=255).contains(&b) => v.push(b as u8),
                            _ => return Err(Error::Value("bytes must be in range(0, 256)".into())),
                        }
                    }
                    v
                }
                Val::Str(_) => return Err(Error::Type("string argument without an encoding".into())),
                other => {
                    return Err(Error::Type(format!("cannot convert {} to bytes", other.py_repr())));
                }
            };
            put_vu64(out, raw.len() as u64);
            out.extend_from_slice(&raw);
        }
        _ => {
            // fixed-width integer types (struct.pack requires real integers)
            let (lo, hi) = typ.int_range().unwrap();
            let n: i128 = match value {
                Val::Bool(b) => *b as i128,
                Val::Int(i) => *i as i128,
                Val::Big(b) => b.to_i128().ok_or_else(|| value_error_fit(f, value))?,
                _ => return Err(value_error_fit(f, value)),
            };
            if n < lo || n > hi {
                return Err(value_error_fit(f, value));
            }
            let size = typ.fixed_size().unwrap();
            out.extend_from_slice(&n.to_le_bytes()[..size]);
        }
    }
    Ok(())
}

/// `float(value)` for f32/f64 fields. Integers whose conversion is not
/// finite raise `ValueError` instead of silently becoming `inf` (F039;
/// Python's `struct.pack` raises `OverflowError` here).
fn float_like(f: &FieldDef, v: &Val) -> Result<f64> {
    let x = match v {
        Val::Bool(b) => *b as i64 as f64,
        Val::Int(i) => *i as f64,
        Val::Big(b) => b.to_f64().unwrap_or(f64::INFINITY),
        Val::Float(x) => return Ok(*x),
        _ => return Err(value_error_fit(f, v)),
    };
    if !x.is_finite() {
        return Err(Error::Value(format!("{}: int too large to convert to float", f.name)));
    }
    Ok(x)
}

/// Largest `n` accepted for `bytes(n)` on a bytes field (1 GiB). Larger
/// zero-filled values can still be stored explicitly as `b"\0" * n`.
pub const BYTES_FROM_COUNT_MAX: u64 = 1 << 30;

/// Fallible reservation: allocation failure becomes an exception, never an abort.
fn reserve(v: &mut Vec<u8>, extra: usize) -> Result<()> {
    v.try_reserve(extra).map_err(|_| Error::Memory(format!("cannot allocate {extra} bytes for a value")))
}

/// Decodes a value (including reverse enum mapping) starting at `*pos`.
pub fn decode_value(f: &FieldDef, buf: &[u8], pos: &mut usize) -> Result<Val> {
    let raw = decode_raw(f, f.typ, buf, pos)?;
    Ok(enum_from_raw(f, raw))
}

pub fn enum_from_raw(f: &FieldDef, raw: Val) -> Val {
    if f.enum_map.is_none() {
        return raw;
    }
    let code = match int_like(&raw) {
        Some(b) => b,
        None => return raw,
    };
    match code.to_i64().and_then(|c| f.enum_label(c)) {
        Some(l) => Val::Str(l.to_string()),
        None => Val::Str(format!("<UNKNOWN_ENUM:{}>", raw.py_repr())),
    }
}

pub fn decode_raw(f: &FieldDef, typ: TypeCode, buf: &[u8], pos: &mut usize) -> Result<Val> {
    if let Some(size) = typ.fixed_size() {
        let end = pos.checked_add(size).filter(|e| *e <= buf.len()).ok_or_else(|| {
            let _ = &f.name;
            Error::Corruption("fixed-width value incomplete".into())
        })?;
        let b = &buf[*pos..end];
        *pos = end;
        return Ok(match typ {
            TypeCode::U8 => Val::Int(b[0] as i64),
            TypeCode::I8 => Val::Int(b[0] as i8 as i64),
            TypeCode::Bool => Val::Bool(b[0] != 0),
            TypeCode::U16 => Val::Int(u16::from_le_bytes([b[0], b[1]]) as i64),
            TypeCode::I16 => Val::Int(i16::from_le_bytes([b[0], b[1]]) as i64),
            TypeCode::U32 => Val::Int(u32::from_le_bytes(b.try_into().unwrap()) as i64),
            TypeCode::I32 => Val::Int(i32::from_le_bytes(b.try_into().unwrap()) as i64),
            TypeCode::F32 => Val::Float(f32::from_le_bytes(b.try_into().unwrap()) as f64),
            TypeCode::U64 => Val::int(u64::from_le_bytes(b.try_into().unwrap())),
            TypeCode::I64 => Val::Int(i64::from_le_bytes(b.try_into().unwrap())),
            TypeCode::F64 => Val::Float(f64::from_le_bytes(b.try_into().unwrap())),
            _ => unreachable!(),
        });
    }
    match typ {
        TypeCode::VarUInt => {
            let n = get_vu(buf, pos)?;
            Ok(Val::nat(&n))
        }
        TypeCode::VarInt => {
            let z = get_vu(buf, pos)?;
            Ok(Val::from_bigint(zigzag_decode(&z)))
        }
        TypeCode::Utf8 | TypeCode::Bytes => {
            let n = get_vu(buf, pos)?;
            let n = n.as_u64().ok_or_else(|| Error::Corruption("length value ends beyond the payload".into()))?;
            let end = pos.checked_add(n as usize).filter(|e| *e <= buf.len()).ok_or_else(|| {
                Error::Corruption("length value ends beyond the payload".into())
            })?;
            let raw = &buf[*pos..end];
            *pos = end;
            if typ == TypeCode::Utf8 {
                match std::str::from_utf8(raw) {
                    Ok(s) => Ok(Val::Str(s.to_string())),
                    Err(e) => Err(Error::Corruption(format!("{}: invalid UTF-8: {e}", f.name))),
                }
            } else {
                Ok(Val::Bytes(raw.to_vec()))
            }
        }
        _ => unreachable!(),
    }
}

/// Skips an encoded value without materializing it.
pub fn skip_value(f: &FieldDef, buf: &[u8], pos: &mut usize) -> Result<()> {
    skip_raw(f.typ, buf, pos, || f.name.clone())
}

/// Skips a value of type `typ`; `name` supplies the field name for the
/// error message only.
pub fn skip_raw(typ: TypeCode, buf: &[u8], pos: &mut usize, name: impl FnOnce() -> String) -> Result<()> {
    if let Some(size) = typ.fixed_size() {
        let end = pos.checked_add(size).filter(|e| *e <= buf.len());
        let Some(end) = end else {
            let _ = name;
            return Err(Error::Corruption("fixed-width value incomplete".into()));
        };
        *pos = end;
        return Ok(());
    }
    match typ {
        TypeCode::VarUInt | TypeCode::VarInt => {
            loop {
                let Some(&b) = buf.get(*pos) else {
                    return Err(Error::Corruption("Varuint ends prematurely".into()));
                };
                *pos += 1;
                if b < 0x80 {
                    return Ok(());
                }
            }
        }
        _ => {
            let n = get_vu(buf, pos)?.as_u64().unwrap_or(u64::MAX);
            let end = pos.checked_add(n as usize).filter(|e| *e <= buf.len()).ok_or_else(|| {
                Error::Corruption("length value points beyond the record".into())
            })?;
            *pos = end;
            Ok(())
        }
    }
}

/// Encoded length of a value of type `typ` that starts at `buf[0]`, from
/// as few bytes as needed (fixed width, varint end, length prefix):
/// `Ok(None)` if `buf` ends before the length is known. Lets a stream skip
/// a value without reading it (memory-bounded merge, RB-01).
pub fn raw_extent(typ: TypeCode, buf: &[u8]) -> Result<Option<usize>> {
    if let Some(size) = typ.fixed_size() {
        return Ok(Some(size));
    }
    let vu_end = |b: &[u8]| b.iter().position(|x| *x < 0x80).map(|i| i + 1);
    match typ {
        TypeCode::VarUInt | TypeCode::VarInt => Ok(vu_end(buf)),
        _ => {
            let Some(e) = vu_end(buf) else { return Ok(None) };
            let mut p = 0usize;
            let n = get_vu(&buf[..e], &mut p)?.as_u64().ok_or_else(|| Error::Corruption("length value out of range".into()))?;
            let n = usize::try_from(n).ok().and_then(|n| n.checked_add(e)).ok_or_else(|| Error::Corruption("length value out of range".into()))?;
            Ok(Some(n))
        }
    }
}

/// Python `int(x)` as Nat (for positive parameters).
pub fn val_to_nat(v: &Val) -> Option<Nat> {
    let b = int_like(v)?;
    if b.sign() == Sign::Minus { None } else { Some(Nat::from(b.magnitude().clone())) }
}

pub fn bigint_is_zero(b: &BigInt) -> bool {
    b.is_zero()
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "codec_tests.rs"]
mod tests;
