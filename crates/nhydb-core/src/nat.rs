//! `Nat`: nichtnegative Ganzzahl beliebiger Größe mit u64-Schnellpfad.
//!
//! Mathematische Koordinaten (Realm, u, lokales v, V, G, N) sind in der
//! NHyDB unbeschränkt (lokales v bis 10^100, N mit hunderten Stellen).
//! Der Großteil realer Werte passt aber in 64 Bit. `Nat` speichert deshalb
//! kleine Werte direkt und weicht nur bei Bedarf auf `BigUint` aus.
//!
//! Invariante: `Nat::B` enthält ausschließlich Werte > `u64::MAX`.
//! Dadurch sind die abgeleiteten `Eq`, `Ord` und `Hash` numerisch korrekt.

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{ToPrimitive, Zero};
use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Nat {
    S(u64),
    B(Box<BigUint>),
}

impl Default for Nat {
    fn default() -> Self {
        Nat::S(0)
    }
}

impl From<u64> for Nat {
    #[inline]
    fn from(v: u64) -> Self {
        Nat::S(v)
    }
}

impl From<u32> for Nat {
    #[inline]
    fn from(v: u32) -> Self {
        Nat::S(v as u64)
    }
}

impl From<usize> for Nat {
    #[inline]
    fn from(v: usize) -> Self {
        Nat::S(v as u64)
    }
}

impl From<u128> for Nat {
    #[inline]
    fn from(v: u128) -> Self {
        if v <= u64::MAX as u128 {
            Nat::S(v as u64)
        } else {
            Nat::B(Box::new(BigUint::from(v)))
        }
    }
}

impl From<BigUint> for Nat {
    #[inline]
    fn from(v: BigUint) -> Self {
        match v.to_u64() {
            Some(x) => Nat::S(x),
            None => Nat::B(Box::new(v)),
        }
    }
}

impl From<&BigUint> for Nat {
    fn from(v: &BigUint) -> Self {
        match v.to_u64() {
            Some(x) => Nat::S(x),
            None => Nat::B(Box::new(v.clone())),
        }
    }
}

impl Nat {
    pub const ZERO: Nat = Nat::S(0);
    pub const ONE: Nat = Nat::S(1);

    #[inline]
    pub fn is_zero(&self) -> bool {
        matches!(self, Nat::S(0))
    }

    #[inline]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Nat::S(v) => Some(*v),
            Nat::B(_) => None,
        }
    }

    pub fn to_big(&self) -> BigUint {
        match self {
            Nat::S(v) => BigUint::from(*v),
            Nat::B(b) => (**b).clone(),
        }
    }

    pub fn to_bigint(&self) -> BigInt {
        BigInt::from_biguint(Sign::Plus, self.to_big())
    }

    /// Konvertiert eine vorzeichenbehaftete Ganzzahl; negative Werte → None.
    pub fn from_bigint(v: &BigInt) -> Option<Nat> {
        if v.sign() == Sign::Minus {
            return None;
        }
        Some(Nat::from(v.magnitude().clone()))
    }

    pub fn from_i64(v: i64) -> Option<Nat> {
        if v < 0 { None } else { Some(Nat::S(v as u64)) }
    }

    #[inline]
    pub fn add(&self, o: &Nat) -> Nat {
        if let (Nat::S(a), Nat::S(b)) = (self, o) {
            if let Some(c) = a.checked_add(*b) {
                return Nat::S(c);
            }
        }
        Nat::from(self.to_big() + o.to_big())
    }

    #[inline]
    pub fn add_u64(&self, o: u64) -> Nat {
        if let Nat::S(a) = self {
            if let Some(c) = a.checked_add(o) {
                return Nat::S(c);
            }
        }
        Nat::from(self.to_big() + BigUint::from(o))
    }

    /// `self - o`, falls nichtnegativ.
    #[inline]
    pub fn checked_sub(&self, o: &Nat) -> Option<Nat> {
        match (self, o) {
            (Nat::S(a), Nat::S(b)) => a.checked_sub(*b).map(Nat::S),
            _ => {
                if self < o {
                    None
                } else {
                    Some(Nat::from(self.to_big() - o.to_big()))
                }
            }
        }
    }

    #[inline]
    pub fn sub_u64(&self, o: u64) -> Option<Nat> {
        self.checked_sub(&Nat::S(o))
    }

    #[inline]
    pub fn mul(&self, o: &Nat) -> Nat {
        if let (Nat::S(a), Nat::S(b)) = (self, o) {
            if let Some(c) = a.checked_mul(*b) {
                return Nat::S(c);
            }
            return Nat::from(*a as u128 * *b as u128);
        }
        Nat::from(self.to_big() * o.to_big())
    }

    /// Ganzzahlige Division mit Rest. `o` darf nicht 0 sein.
    pub fn div_rem(&self, o: &Nat) -> (Nat, Nat) {
        match (self, o) {
            (Nat::S(a), Nat::S(b)) => (Nat::S(a / b), Nat::S(a % b)),
            _ => {
                use num_integer::Integer;
                let (q, r) = self.to_big().div_rem(&o.to_big());
                (Nat::from(q), Nat::from(r))
            }
        }
    }

    pub fn pow10(exp: u32) -> Nat {
        Nat::from(num_traits::pow(BigUint::from(10u32), exp as usize))
    }

    pub fn parse_decimal(s: &str) -> Option<Nat> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if s.len() <= 19 {
            return s.parse::<u64>().ok().map(Nat::S);
        }
        BigUint::parse_bytes(s.as_bytes(), 10).map(Nat::from)
    }

    /// Anzahl Dezimalstellen.
    pub fn decimal_len(&self) -> usize {
        self.to_string().len()
    }

    // ------------------------------------------------------------------
    // VarUInt (Basis-128, 7 Datenbits pro Byte, little endian) – identisch
    // zu `enc_vu`/`dec_vu` der v0.14.
    // ------------------------------------------------------------------

    #[inline]
    pub fn put_vu(&self, out: &mut Vec<u8>) {
        match self {
            Nat::S(v) => put_vu64(out, *v),
            Nat::B(b) => put_vu_big(out, b),
        }
    }

    pub fn vu_len(&self) -> usize {
        match self {
            Nat::S(v) => vu64_len(*v),
            Nat::B(b) => {
                let bits = b.bits() as usize;
                bits.div_ceil(7).max(1)
            }
        }
    }
}

impl fmt::Display for Nat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Nat::S(v) => write!(f, "{v}"),
            Nat::B(b) => write!(f, "{b}"),
        }
    }
}

impl PartialEq<u64> for Nat {
    fn eq(&self, other: &u64) -> bool {
        matches!(self, Nat::S(v) if v == other)
    }
}

impl PartialOrd<u64> for Nat {
    fn partial_cmp(&self, other: &u64) -> Option<Ordering> {
        Some(match self {
            Nat::S(v) => v.cmp(other),
            Nat::B(_) => Ordering::Greater,
        })
    }
}

// ----------------------------------------------------------------------
// VarUInt-Primitive
// ----------------------------------------------------------------------

#[inline]
pub fn put_vu64(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

#[inline]
pub fn vu64_len(n: u64) -> usize {
    let bits = 64 - (n | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

fn put_vu_big(out: &mut Vec<u8>, b: &BigUint) {
    let digits = b.to_radix_le(128);
    let last = digits.len() - 1;
    for (i, d) in digits.iter().enumerate() {
        if i < last {
            out.push(d | 0x80);
        } else {
            out.push(*d);
        }
    }
}

/// VarUInt mit Ergebnis als u64; liefert `None`, falls der Wert nicht in
/// 64 Bit passt (Position bleibt dann unverändert). Fehler bei
/// abgeschnittenen Eingaben.
#[inline]
pub fn get_vu64(buf: &[u8], pos: &mut usize) -> Result<Option<u64>> {
    let mut p = *pos;
    let mut value: u128 = 0;
    let mut shift: u32 = 0;
    loop {
        let Some(&b) = buf.get(p) else {
            return Err(Error::Corruption("VarUInt endet zu frueh".into()));
        };
        p += 1;
        value |= ((b & 0x7F) as u128) << shift;
        if b & 0x80 == 0 {
            if value > u64::MAX as u128 {
                return Ok(None);
            }
            *pos = p;
            return Ok(Some(value as u64));
        }
        shift += 7;
        if shift > 63 {
            return Ok(None);
        }
    }
}

/// Dekodiert eine VarUInt beliebiger Größe.
pub fn get_vu(buf: &[u8], pos: &mut usize) -> Result<Nat> {
    let start = *pos;
    if let Some(v) = get_vu64(buf, pos)? {
        return Ok(Nat::S(v));
    }
    let mut p = start;
    let mut digits: Vec<u8> = Vec::new();
    let mut shift = 0usize;
    loop {
        let Some(&b) = buf.get(p) else {
            return Err(Error::Corruption("VarUInt endet zu frueh".into()));
        };
        p += 1;
        digits.push(b & 0x7F);
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 16384 {
            return Err(Error::Corruption("VarUInt unplausibel gross".into()));
        }
    }
    *pos = p;
    let big = BigUint::from_radix_le(&digits, 128).unwrap_or_default();
    Ok(Nat::from(big))
}

/// Strenger u64-Dekoder für physische Felder (Längen, Zähler, IDs).
#[inline]
pub fn get_vu_u64(buf: &[u8], pos: &mut usize, what: &str) -> Result<u64> {
    match get_vu64(buf, pos)? {
        Some(v) => Ok(v),
        None => Err(Error::Corruption(format!("{what}: Wert zu gross"))),
    }
}

/// ZigZag-artige Abbildung der v0.14 (`2n` für n>=0, sonst `-2n-1`).
pub fn put_vi_big(out: &mut Vec<u8>, n: &BigInt) {
    let z: BigUint = if n.sign() == Sign::Minus {
        let m = n.magnitude();
        m * 2u32 - 1u32
    } else {
        n.magnitude() * 2u32
    };
    Nat::from(z).put_vu(out);
}

#[inline]
pub fn put_vi64(out: &mut Vec<u8>, n: i64) {
    let n = n as i128;
    let z = if n >= 0 { 2 * n } else { -2 * n - 1 } as u128;
    if z <= u64::MAX as u128 {
        put_vu64(out, z as u64);
    } else {
        Nat::from(z).put_vu(out);
    }
}

/// Dekodiert eine VarInt; Rückgabe als BigInt-fähiger Wert über `Nat`.
pub fn zigzag_decode(z: &Nat) -> BigInt {
    match z {
        Nat::S(v) => {
            if v % 2 == 0 {
                BigInt::from(v / 2)
            } else {
                -BigInt::from(v / 2) - 1
            }
        }
        Nat::B(b) => {
            let half: BigUint = &**b >> 1usize;
            if (&**b % 2u32).is_zero() {
                BigInt::from_biguint(Sign::Plus, half)
            } else {
                -BigInt::from_biguint(Sign::Plus, half) - 1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(n: &Nat) -> Vec<u8> {
        let mut v = Vec::new();
        n.put_vu(&mut v);
        v
    }

    #[test]
    fn vu_roundtrip_small_and_big() {
        for x in [0u64, 1, 127, 128, 16383, 16384, 1 << 35, u64::MAX - 1, u64::MAX] {
            let n = Nat::S(x);
            let b = enc(&n);
            assert_eq!(b.len(), n.vu_len());
            let mut p = 0;
            assert_eq!(get_vu(&b, &mut p).unwrap(), n);
            assert_eq!(p, b.len());
        }
        let big = Nat::pow10(100);
        let b = enc(&big);
        assert_eq!(b.len(), 48); // wie im v0.14-Benchmark (v10^100 << 3 → 48 B)
        let mut p = 0;
        assert_eq!(get_vu(&b, &mut p).unwrap(), big);
        let over = Nat::S(u64::MAX).add_u64(1);
        let b = enc(&over);
        let mut p = 0;
        assert_eq!(get_vu(&b, &mut p).unwrap(), over);
    }

    #[test]
    fn vu_matches_python_bytes() {
        assert_eq!(enc(&Nat::S(300)), vec![0xAC, 0x02]);
        assert_eq!(enc(&Nat::S(0)), vec![0]);
        assert_eq!(enc(&Nat::S(0x200000)), vec![0x80, 0x80, 0x80, 0x01]);
    }

    #[test]
    fn truncated_is_error() {
        let mut p = 0;
        assert!(get_vu(&[0x80, 0x80], &mut p).is_err());
    }

    #[test]
    fn ordering_and_arith() {
        let a = Nat::S(u64::MAX);
        let b = a.add_u64(5);
        assert!(b > a);
        assert_eq!(b.checked_sub(&a).unwrap(), Nat::S(5));
        assert_eq!(Nat::S(7).mul(&Nat::S(6)), Nat::S(42));
        let (q, r) = Nat::pow10(30).div_rem(&Nat::S(7));
        assert_eq!(q.mul(&Nat::S(7)).add(&r), Nat::pow10(30));
    }

    #[test]
    fn zigzag() {
        for x in [-5i64, -1, 0, 1, 2, 1 << 40, -(1 << 40)] {
            let mut out = Vec::new();
            put_vi64(&mut out, x);
            let mut p = 0;
            let z = get_vu(&out, &mut p).unwrap();
            assert_eq!(zigzag_decode(&z), BigInt::from(x));
        }
        let mut out = Vec::new();
        put_vi64(&mut out, i64::MIN);
        let mut p = 0;
        assert_eq!(zigzag_decode(&get_vu(&out, &mut p).unwrap()), BigInt::from(i64::MIN));
    }
}
