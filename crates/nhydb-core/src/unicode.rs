//! Unicode character properties exactly matching CPython.
//!
//! The tables in `unicode_tables.rs` are generated from CPython; all
//! 0x110000 code points can be checked against a running interpreter via
//! `prop_bits`.
//!
//! Deliberate simplification: `lower()` works per character without the
//! final-sigma rule of `str.lower()`; the engine only uses `lower()` for
//! comparisons with ASCII keywords, where the result is identical.

use crate::unicode_tables as t;

pub use t::{PYTHON_VERSION, UNICODE_VERSION};

#[inline]
fn in_ranges(r: &[(u32, u32)], cp: u32) -> bool {
    r.binary_search_by(|&(a, b)| {
        if cp < a {
            std::cmp::Ordering::Greater
        } else if cp > b {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

#[inline]
fn lookup(m: &[(u32, &'static str)], cp: u32) -> Option<&'static str> {
    m.binary_search_by_key(&cp, |&(c, _)| c).ok().map(|i| m[i].1)
}

/// `re` pattern `\w` (str, Unicode): `isalnum() or '_'`.
#[inline]
pub fn is_word(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    in_ranges(t::WORD, c as u32)
}

pub fn is_alpha(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    in_ranges(t::ALPHA, c as u32)
}

pub fn is_alnum(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric();
    }
    in_ranges(t::ALNUM, c as u32)
}

/// `str.isdigit()` (includes e.g. superscript digits).
pub fn is_digit(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    in_ranges(t::DIGIT, c as u32)
}

/// `str.isspace()`.
pub fn is_space(c: char) -> bool {
    if c.is_ascii() {
        return matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c' | '\x1c'..='\x1f');
    }
    in_ranges(t::SPACE, c as u32)
}

/// `str.isprintable()` (for `repr`).
pub fn is_printable(c: char) -> bool {
    if c.is_ascii() {
        return (' '..='~').contains(&c);
    }
    in_ranges(t::PRINTABLE, c as u32)
}

/// Decimal value (`unicodedata.decimal`) as accepted by `int()`/`float()`.
pub fn decimal_value(c: char) -> Option<u8> {
    if c.is_ascii_digit() {
        return Some(c as u8 - b'0');
    }
    t::DECIMAL.binary_search_by_key(&(c as u32), |&(cp, _)| cp).ok().map(|i| t::DECIMAL[i].1)
}

fn map_str(s: &str, m: &'static [(u32, &'static str)], ascii: fn(char) -> char) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(ascii(c));
        } else if let Some(r) = lookup(m, c as u32) {
            out.push_str(r);
        } else {
            out.push(c);
        }
    }
    out
}

/// `str.casefold()` (full Unicode case folding, e.g. `ß` → `ss`).
pub fn casefold(s: &str) -> String {
    map_str(s, t::CASEFOLD, |c| c.to_ascii_lowercase())
}

/// `str.upper()`.
pub fn upper(s: &str) -> String {
    map_str(s, t::UPPER, |c| c.to_ascii_uppercase())
}

/// `str.lower()` per character (see module documentation).
pub fn lower(s: &str) -> String {
    map_str(s, t::LOWER, |c| c.to_ascii_lowercase())
}

/// `str.strip()` without argument.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `str.isidentifier()`.
pub fn is_identifier(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        None => false,
        Some(c0) => {
            let start = if c0.is_ascii() { c0.is_ascii_alphabetic() || c0 == '_' } else { in_ranges(t::ID_START, c0 as u32) };
            start && it.all(|c| if c.is_ascii() { c.is_ascii_alphanumeric() || c == '_' } else { in_ranges(t::ID_CONTINUE, c as u32) })
        }
    }
}

/// Whether an identifier would be changed by the Python parser's NFKC
/// normalization (characters with a compatibility decomposition).
pub fn identifier_needs_nfkc(s: &str) -> bool {
    if s.is_ascii() {
        return false;
    }
    // per-character compatibility changes, or a sequence that is not NFC
    // (e.g. base letter + combining mark that composes, F043)
    s.chars().any(|c| !c.is_ascii() && in_ranges(t::NFKC_CHANGES, c as u32)) || !is_nfc(s)
}

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// Canonical combining class (`unicodedata.combining`).
pub fn combining_class(c: char) -> u8 {
    let cp = c as u32;
    t::CCC
        .binary_search_by(|&(a, b, _)| {
            if cp < a {
                std::cmp::Ordering::Greater
            } else if cp > b {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .map(|i| t::CCC[i].2)
        .unwrap_or(0)
}

fn decompose_into(c: char, out: &mut Vec<char>) {
    let cp = c as u32;
    if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
        let si = cp - S_BASE;
        out.push(char::from_u32(L_BASE + si / N_COUNT).unwrap());
        out.push(char::from_u32(V_BASE + (si % N_COUNT) / T_COUNT).unwrap());
        if !si.is_multiple_of(T_COUNT) {
            out.push(char::from_u32(T_BASE + si % T_COUNT).unwrap());
        }
    } else if let Some(d) = lookup(t::CANON_DECOMP, cp) {
        out.extend(d.chars());
    } else {
        out.push(c);
    }
}

fn compose_pair(a: char, b: char) -> Option<char> {
    let (a, b) = (a as u32, b as u32);
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        return char::from_u32(S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a) && (a - S_BASE).is_multiple_of(T_COUNT) && (T_BASE + 1..T_BASE + T_COUNT).contains(&b) {
        return char::from_u32(a + (b - T_BASE));
    }
    t::COMPOSE.binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b))).ok().and_then(|i| char::from_u32(t::COMPOSE[i].2))
}

/// Unicode normalization form C (`unicodedata.normalize("NFC", s)`).
pub fn nfc(s: &str) -> String {
    let mut d: Vec<char> = Vec::with_capacity(s.len());
    for c in s.chars() {
        decompose_into(c, &mut d);
    }
    // canonical ordering: stable sort of each run of non-starters by ccc
    let mut i = 0;
    while i < d.len() {
        if combining_class(d[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < d.len() && combining_class(d[i]) != 0 {
            i += 1;
        }
        d[start..i].sort_by_key(|c| combining_class(*c));
    }
    // canonical composition
    let mut out: Vec<char> = Vec::with_capacity(d.len());
    let mut starter: Option<usize> = None;
    let mut between = false;
    let mut last_cc = 0u8;
    for &c in &d {
        let cc = combining_class(c);
        if let Some(si) = starter {
            let blocked = between && (last_cc == 0 || last_cc >= cc);
            if !blocked {
                if let Some(comp) = compose_pair(out[si], c) {
                    out[si] = comp;
                    continue;
                }
            }
        }
        if cc == 0 {
            starter = Some(out.len());
            between = false;
        } else {
            between = true;
        }
        last_cc = cc;
        out.push(c);
    }
    out.into_iter().collect()
}

/// `unicodedata.is_normalized("NFC", s)`.
pub fn is_nfc(s: &str) -> bool {
    s.is_ascii() || nfc(s) == s
}

/// Property bits of a code point (for exhaustive comparison against Python).
/// Bit 0 word, 1 alpha, 2 alnum, 3 digit, 4 space, 5 printable, 6 id_start,
/// 7 id_continue.
pub fn prop_bits(cp: u32) -> u8 {
    let Some(c) = char::from_u32(cp) else { return 0 };
    let mut b = 0u8;
    let flags = [
        is_word(c),
        is_alpha(c),
        is_alnum(c),
        is_digit(c),
        is_space(c),
        is_printable(c),
        is_identifier(c.encode_utf8(&mut [0; 4])),
        is_identifier(&format!("a{c}")),
    ];
    for (i, f) in flags.iter().enumerate() {
        if *f {
            b |= 1 << i;
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_checks() {
        assert!(is_word('_') && is_word('ä') && is_word('٣') && !is_word('-') && !is_word('\u{0301}'));
        assert!(is_digit('²') && decimal_value('²').is_none() && decimal_value('٣') == Some(3));
        assert_eq!(casefold("Straße ΣΑΣ"), "strasse σασ");
        assert_eq!(upper("ſelect ı"), "SELECT I");
        assert!(is_space('\u{3000}') && is_space('\x1f') && !is_space('\u{200b}'));
        assert!(is_identifier("äx_1") && !is_identifier("1a") && !is_identifier(""));
        assert!(identifier_needs_nfkc("ﬁ") && !identifier_needs_nfkc("fi"));
        assert_eq!(strip("\u{3000} a \n"), "a");
        // F043: NFC sequences
        assert!(is_nfc("caf\u{e9}") && !is_nfc("cafe\u{301}"));
        assert!(identifier_needs_nfkc("cafe\u{301}") && !identifier_needs_nfkc("caf\u{e9}"));
        assert!(is_nfc("x\u{304}")); // no precomposed form
        assert_eq!(nfc("\u{e9}\u{323}"), "\u{1eb9}\u{301}");
        assert_eq!(nfc("\u{1100}\u{1161}\u{11a8}"), "\u{ac01}");
        assert!(is_nfc("\u{ac01}") && !is_nfc("\u{1100}\u{1161}"));
    }
}
