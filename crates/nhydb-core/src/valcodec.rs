//! Value codec (0.28.2 round 4, stage C): a self-describing binary form of
//! `Val` for result containers (and later the OER export). One tag byte per
//! value, little endian, varints as in the file format (`nat::put_vu64`).
//! Encoding is canonical (an `Int` within i64 is never written as `Big`),
//! decoding is bounded (nesting depth, lengths within the input) and turns
//! every malformed input into `Error::Corruption`.
//!
//! | tag | value | body |
//! |---|---|---|
//! | 0 | None | – |
//! | 1 / 2 | False / True | – |
//! | 3 | Int (i64) | zigzag varint |
//! | 4 | Big (outside i64) | sign byte (0 = positive, 1 = negative), varint n, n magnitude bytes little endian |
//! | 5 | Float | 8 bytes IEEE 754 little endian (bit-exact, also NaN payloads) |
//! | 6 | Str | varint n, n bytes UTF-8 |
//! | 7 | Bytes | varint n, n bytes |
//! | 8 | List | varint n, n values |
//! | 9 | Map | varint n, n × (varint k, k bytes UTF-8 key, value) |

use num_bigint::{BigInt, Sign};

use crate::error::{Error, Result};
use crate::nat::put_vu64;
use crate::val::Val;

/// Deepest accepted nesting of lists and maps when decoding.
pub const MAX_DEPTH: usize = 256;

const T_NONE: u8 = 0;
const T_FALSE: u8 = 1;
const T_TRUE: u8 = 2;
const T_INT: u8 = 3;
const T_BIG: u8 = 4;
const T_FLOAT: u8 = 5;
const T_STR: u8 = 6;
const T_BYTES: u8 = 7;
const T_LIST: u8 = 8;
const T_MAP: u8 = 9;

fn bad(m: &str) -> Error {
    Error::Corruption(format!("value codec: {m}"))
}

/// Appends the encoding of `v`.
pub fn encode(v: &Val, out: &mut Vec<u8>) {
    match v {
        Val::None => out.push(T_NONE),
        Val::Bool(false) => out.push(T_FALSE),
        Val::Bool(true) => out.push(T_TRUE),
        Val::Int(i) => {
            out.push(T_INT);
            put_vu64(out, ((*i << 1) ^ (*i >> 63)) as u64);
        }
        Val::Big(b) => match i64::try_from(b) {
            Ok(i) => encode(&Val::Int(i), out),
            Err(_) => {
                out.push(T_BIG);
                let (sign, mag) = b.to_bytes_le();
                out.push(u8::from(sign == Sign::Minus));
                put_vu64(out, mag.len() as u64);
                out.extend_from_slice(&mag);
            }
        },
        Val::Float(f) => {
            out.push(T_FLOAT);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Val::Str(s) => {
            out.push(T_STR);
            put_vu64(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Val::Bytes(b) => {
            out.push(T_BYTES);
            put_vu64(out, b.len() as u64);
            out.extend_from_slice(b);
        }
        Val::List(l) => {
            out.push(T_LIST);
            put_vu64(out, l.len() as u64);
            for x in l {
                encode(x, out);
            }
        }
        Val::Map(m) => {
            out.push(T_MAP);
            put_vu64(out, m.len() as u64);
            for (k, x) in m {
                put_vu64(out, k.len() as u64);
                out.extend_from_slice(k.as_bytes());
                encode(x, out);
            }
        }
    }
}

/// Encoding of `v` as a new vector.
pub fn encode_vec(v: &Val) -> Vec<u8> {
    let mut out = Vec::new();
    encode(v, &mut out);
    out
}

/// Own VarUInt reader (u64): keeps the codec self-contained and the hot
/// readers of the file format (`nat::get_vu64`) with their call sites.
fn read_vu64(buf: &[u8], p: &mut usize, what: &str) -> Result<u64> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*p).ok_or_else(|| bad(&format!("{what} truncated")))?;
        *p += 1;
        if shift == 63 && b > 1 {
            return Err(bad(&format!("{what} too large")));
        }
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(bad(&format!("{what} too large")));
        }
    }
}

fn take_len(buf: &[u8], p: &mut usize) -> Result<usize> {
    let n = read_vu64(buf, p, "length")?;
    let n = usize::try_from(n).map_err(|_| bad("length too large"))?;
    if n > buf.len() - *p {
        return Err(bad("length beyond the end of the input"));
    }
    Ok(n)
}

fn take_bytes<'a>(buf: &'a [u8], p: &mut usize) -> Result<&'a [u8]> {
    let n = take_len(buf, p)?;
    let s = &buf[*p..*p + n];
    *p += n;
    Ok(s)
}

fn take_str(buf: &[u8], p: &mut usize) -> Result<String> {
    let b = take_bytes(buf, p)?;
    String::from_utf8(b.to_vec()).map_err(|_| bad("invalid UTF-8"))
}

/// Decodes one value at `*p` and moves `*p` behind it.
pub fn decode(buf: &[u8], p: &mut usize) -> Result<Val> {
    decode_at(buf, p, 0)
}

fn decode_at(buf: &[u8], p: &mut usize, depth: usize) -> Result<Val> {
    let tag = *buf.get(*p).ok_or_else(|| bad("value truncated"))?;
    *p += 1;
    Ok(match tag {
        T_NONE => Val::None,
        T_FALSE => Val::Bool(false),
        T_TRUE => Val::Bool(true),
        T_INT => {
            let z = read_vu64(buf, p, "integer")?;
            Val::Int(((z >> 1) as i64) ^ -((z & 1) as i64))
        }
        T_BIG => {
            let sign = *buf.get(*p).ok_or_else(|| bad("integer truncated"))?;
            *p += 1;
            if sign > 1 {
                return Err(bad("invalid sign of a big integer"));
            }
            let mag = take_bytes(buf, p)?;
            // RD4-13: canonical magnitude (no zero byte at the top)
            if mag.last().is_none_or(|b| *b == 0) {
                return Err(bad("big integer magnitude not canonical"));
            }
            let b = BigInt::from_bytes_le(if sign == 1 { Sign::Minus } else { Sign::Plus }, mag);
            if i64::try_from(&b).is_ok() {
                return Err(bad("big integer within the i64 range (not canonical)"));
            }
            Val::Big(b)
        }
        T_FLOAT => {
            let b = buf.get(*p..*p + 8).ok_or_else(|| bad("float truncated"))?;
            *p += 8;
            Val::Float(f64::from_bits(u64::from_le_bytes(b.try_into().unwrap())))
        }
        T_STR => Val::Str(take_str(buf, p)?),
        T_BYTES => Val::Bytes(take_bytes(buf, p)?.to_vec()),
        T_LIST | T_MAP => {
            if depth >= MAX_DEPTH {
                return Err(bad("nested deeper than the limit"));
            }
            // every element takes at least one byte: the count is bounded
            let n = take_len(buf, p)?;
            if tag == T_LIST {
                let mut l = Vec::with_capacity(n);
                for _ in 0..n {
                    l.push(decode_at(buf, p, depth + 1)?);
                }
                Val::List(l)
            } else {
                let mut m: Vec<(String, Val)> = Vec::with_capacity(n);
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for _ in 0..n {
                    let k = take_str(buf, p)?;
                    // RD4-13: a key occurs once (a map is not a list of pairs)
                    let dup = if n <= 16 { m.iter().any(|(x, _)| *x == k) } else { !seen.insert(k.clone()) };
                    if dup {
                        return Err(bad(&format!("duplicate map key {k:?}")));
                    }
                    m.push((k, decode_at(buf, p, depth + 1)?));
                }
                Val::Map(m)
            }
        }
        _ => return Err(bad(&format!("unknown tag {tag}"))),
    })
}

/// Decodes exactly one value filling `buf`.
pub fn decode_all(buf: &[u8]) -> Result<Val> {
    let mut p = 0;
    let v = decode(buf, &mut p)?;
    if p != buf.len() {
        return Err(bad("trailing bytes"));
    }
    Ok(v)
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "valcodec_tests.rs"]
mod tests;
