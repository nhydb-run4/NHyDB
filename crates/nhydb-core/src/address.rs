//! Dynadron address mathematics of NHyDB (`normalize_lane` …
//! `cell_address`).
//!
//! Cell identity is `(R, u, v_local, lane)`. `N` is a purely
//! arithmetically derived reference; GZ (`G²`) and N0 (`G(G-1)`) are not
//! storage cells. All computations are exact (no floats, no u64 limit).

use num_bigint::BigUint;
use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::nat::Nat;
use crate::val::Val;
use crate::vmap;

pub type Lane = u8;
pub const NL: Lane = 0;
pub const NR: Lane = 1;

/// `T = 10^100` – constant local realm span.
pub fn realm_span() -> &'static Nat {
    static SPAN: OnceLock<Nat> = OnceLock::new();
    SPAN.get_or_init(|| Nat::pow10(100))
}

/// 10^200 – placeholder width of unbounded layout ranges.
pub fn unbounded_width() -> &'static Nat {
    static W: OnceLock<Nat> = OnceLock::new();
    W.get_or_init(|| Nat::pow10(200))
}

pub fn lane_name(lane: Lane) -> &'static str {
    if lane == NR { "NR" } else { "NL" }
}

/// `normalize_lane` for text input.
pub fn normalize_lane_str(s: &str) -> Result<Lane> {
    let x = s.trim().to_uppercase();
    match x.as_str() {
        "0" | "NL" | "L" | "DEFAULT" => Ok(NL),
        "1" | "NR" | "R" => Ok(NR),
        _ => Err(Error::Value("lane must be 0/NL or 1/NR".into())),
    }
}

/// `normalize_lane` for integers.
pub fn normalize_lane_int(v: i64) -> Result<Lane> {
    match v {
        0 => Ok(NL),
        1 => Ok(NR),
        _ => Err(Error::Value("lane must be 0 or 1".into())),
    }
}

pub fn lane_from_flags(flags: u16) -> Lane {
    (flags & 1) as Lane
}

pub fn lane_flags(lane: Lane) -> u16 {
    if lane == NR { 1 } else { 0 }
}

pub fn require_positive(name: &str, v: &Nat) -> Result<()> {
    if v.is_zero() {
        return Err(Error::Value(format!("{name} must be > 0")));
    }
    Ok(())
}

pub fn validate_local_v(v: &Nat, realm: Option<&Nat>) -> Result<()> {
    if v.is_zero() {
        return Err(Error::RealmLimit("local v must start at 1".into()));
    }
    if let Nat::B(_) = v {
        if v > realm_span() {
            let prefix = realm.map(|r| format!("Realm {r}: ")).unwrap_or_default();
            return Err(Error::RealmLimit(format!(
                "{prefix}local v limit reached. Allowed range is 1..10^100; use the next realm for further v positions."
            )));
        }
    }
    Ok(())
}

pub fn realm_v_bounds(realm: &Nat) -> Result<(Nat, Nat)> {
    require_positive("realm_id", realm)?;
    let r1 = realm.sub_u64(1).unwrap();
    let lo = r1.mul(realm_span()).add_u64(1);
    let hi = realm.mul(realm_span());
    Ok((lo, hi))
}

/// `V = (R-1)·T + v`.
pub fn global_v(realm: &Nat, local_v: &Nat) -> Result<Nat> {
    require_positive("realm_id", realm)?;
    validate_local_v(local_v, Some(realm))?;
    Ok(global_v_unchecked(realm, local_v))
}

#[inline]
fn global_v_unchecked(realm: &Nat, local_v: &Nat) -> Nat {
    if *realm == 1 {
        return local_v.clone();
    }
    realm.sub_u64(1).unwrap().mul(realm_span()).add(local_v)
}

/// `(R, v) = ((V-1) // T + 1, (V-1) % T + 1)`.
pub fn split_global_v(vg: &Nat) -> Result<(Nat, Nat)> {
    require_positive("V", vg)?;
    if let Nat::S(_) = vg {
        return Ok((Nat::ONE, vg.clone()));
    }
    let (q, r) = vg.sub_u64(1).unwrap().div_rem(realm_span());
    Ok((q.add_u64(1), r.add_u64(1)))
}

/// Result of `dynadron_address` (all values exact).
#[derive(Clone, Debug, PartialEq)]
pub struct DynAddress {
    pub realm: Nat,
    pub u: Nat,
    pub v_local: Nat,
    /// global coordinate V
    pub v: Nat,
    pub g: Nat,
    pub station: Nat,
    pub stufenbreite: Nat,
    pub stufe: Nat,
    pub stufenstation: Nat,
    pub nl: Nat,
    pub nr: Nat,
}

impl DynAddress {
    pub fn to_val(&self) -> Val {
        vmap! {
            "realm" => &self.realm, "u" => &self.u, "v_local" => &self.v_local,
            "v" => &self.v, "G" => &self.g, "station" => &self.station,
            "stufenbreite" => &self.stufenbreite, "stufe" => &self.stufe,
            "stufenstation" => &self.stufenstation, "NL" => &self.nl, "NR" => &self.nr,
        }
    }

    /// `_flatten_dyn_address`
    pub fn flatten_into(&self, out: &mut Val) {
        out.set("_dyn_realm", Val::nat(&self.realm));
        out.set("_dyn_u", Val::nat(&self.u));
        out.set("_dyn_v_local", Val::nat(&self.v_local));
        out.set("_dyn_v", Val::nat(&self.v));
        out.set("_dyn_G", Val::nat(&self.g));
        out.set("_dyn_station", Val::nat(&self.station));
        out.set("_dyn_stufenbreite", Val::nat(&self.stufenbreite));
        out.set("_dyn_stufe", Val::nat(&self.stufe));
        out.set("_dyn_stufenstation", Val::nat(&self.stufenstation));
        out.set("_dyn_NL", Val::nat(&self.nl));
        out.set("_dyn_NR", Val::nat(&self.nr));
    }
}

fn square(g: &Nat) -> Nat {
    g.mul(g)
}

/// `N` for lane NL: `G² - u`; NR: `G² + V`.
fn n_pair(u: &Nat, vg: &Nat) -> (Nat, Nat, Nat) {
    let g = u.add(vg);
    // fast path: everything in u128
    if let (Nat::S(a), Nat::S(b)) = (u, vg) {
        let gg = *a as u128 + *b as u128;
        if gg < (1u128 << 63) {
            let sq = gg * gg;
            return (Nat::from(gg), Nat::from(sq - *a as u128), Nat::from(sq + *b as u128));
        }
    }
    let sq = square(&g);
    let nl = sq.checked_sub(u).unwrap();
    let nr = sq.add(vg);
    (g, nl, nr)
}

pub fn dynadron_address(realm: &Nat, u: &Nat, local_v: &Nat) -> Result<DynAddress> {
    require_positive("realm_id", realm)?;
    require_positive("u", u)?;
    validate_local_v(local_v, Some(realm))?;
    let vg = global_v_unchecked(realm, local_v);
    let (g, nl, nr) = n_pair(u, &vg);
    let station = vg.sub_u64(1).unwrap();
    let stufenbreite = u.add_u64(2);
    let (q, r) = station.div_rem(&stufenbreite);
    Ok(DynAddress {
        realm: realm.clone(),
        u: u.clone(),
        v_local: local_v.clone(),
        v: vg,
        g,
        station,
        stufenbreite,
        stufe: q.add_u64(1),
        stufenstation: r,
        nl,
        nr,
    })
}

/// Only the N number (without extra metadata); validates inputs.
pub fn dynadron_n(realm: &Nat, u: &Nat, local_v: &Nat, lane: Lane) -> Result<Nat> {
    require_positive("realm_id", realm)?;
    require_positive("u", u)?;
    validate_local_v(local_v, Some(realm))?;
    let vg = global_v_unchecked(realm, local_v);
    let (_g, nl, nr) = n_pair(u, &vg);
    Ok(if lane == NR { nr } else { nl })
}

/// Full cell address (`cell_address`).
#[derive(Clone, Debug, PartialEq)]
pub struct CellAddress {
    pub dyn_: DynAddress,
    pub lane: Lane,
    pub n: Nat,
}

impl CellAddress {
    pub fn realm(&self) -> &Nat {
        &self.dyn_.realm
    }
    pub fn u(&self) -> &Nat {
        &self.dyn_.u
    }
    pub fn v_local(&self) -> &Nat {
        &self.dyn_.v_local
    }
    pub fn n_ref(&self) -> String {
        format!("N{}", self.n)
    }

    /// Dict in the shape of `cell_address`.
    pub fn to_val(&self) -> Val {
        let mut out = self.dyn_.to_val();
        out.set("lane", Val::Int(self.lane as i64));
        out.set("lane_name", Val::str(lane_name(self.lane)));
        out.set("N", Val::nat(&self.n));
        out.set("N_ref", Val::Str(self.n_ref()));
        out
    }

    /// Dict in the shape of `n_to_dynadron_address`.
    pub fn to_n_val(&self) -> Val {
        vmap! {
            "N" => &self.n, "N_ref" => self.n_ref(), "realm" => &self.dyn_.realm,
            "u" => &self.dyn_.u, "v_local" => &self.dyn_.v_local, "v" => &self.dyn_.v,
            "G" => &self.dyn_.g, "lane" => self.lane as i64, "lane_name" => lane_name(self.lane),
            "NL" => &self.dyn_.nl, "NR" => &self.dyn_.nr,
        }
    }
}

pub fn cell_address(realm: &Nat, u: &Nat, local_v: &Nat, lane: Lane) -> Result<CellAddress> {
    let d = dynadron_address(realm, u, local_v)?;
    let n = if lane == NR { d.nr.clone() } else { d.nl.clone() };
    Ok(CellAddress { dyn_: d, lane, n })
}

/// `parse_n_reference` for text input (`"N123"`, `"123"`).
pub fn parse_n_reference(s: &str) -> Result<Nat> {
    let raw = s.trim();
    let raw = if raw.as_bytes().first().map(|c| c.eq_ignore_ascii_case(&b'N')).unwrap_or(false) {
        raw[1..].trim()
    } else {
        raw
    };
    let n = Nat::parse_decimal(raw).ok_or_else(|| Error::Address(format!("invalid N reference '{s}'")))?;
    require_positive("N", &n)?;
    Ok(n)
}

fn isqrt(n: &Nat) -> Nat {
    match n {
        Nat::S(v) => {
            let mut r = (*v as f64).sqrt() as u64;
            while (r as u128) * (r as u128) > *v as u128 {
                r -= 1;
            }
            while ((r + 1) as u128) * ((r + 1) as u128) <= *v as u128 {
                r += 1;
            }
            Nat::S(r)
        }
        Nat::B(b) => Nat::from(BigUint::sqrt(b)),
    }
}

/// Arithmetic inverse `N → (R, u, v_local, lane)` (`n_to_dynadron_address`).
pub fn n_to_address(n: &Nat) -> Result<CellAddress> {
    require_positive("N", n)?;
    let m = isqrt(n);
    let d = n.checked_sub(&square(&m)).unwrap();
    let (u, vg, lane) = if d.is_zero() {
        return Err(Error::Address(format!("N{n} is a GZ point, not an NHyDB storage cell")));
    } else if d < m {
        let u = m.checked_sub(&d).unwrap();
        (u, d, NR)
    } else if d == m {
        return Err(Error::Address(format!("N{n} is an N0 point, not an NHyDB storage cell")));
    } else {
        let g = m.add_u64(1);
        let u = square(&g).checked_sub(n).unwrap();
        let vg = g.checked_sub(&u).unwrap();
        (u, vg, NL)
    };
    let (realm, local_v) = split_global_v(&vg)?;
    let addr = cell_address(&realm, &u, &local_v, lane)?;
    if &addr.n != n {
        return Err(Error::Address(format!("N{n} could not be resolved unambiguously")));
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nat(s: &str) -> Nat {
        Nat::parse_decimal(s).unwrap()
    }

    #[test]
    fn examples_from_docs() {
        let a = n_to_address(&Nat::S(589)).unwrap();
        assert_eq!((a.realm(), a.u(), a.v_local(), a.lane), (&Nat::S(1), &Nat::S(11), &Nat::S(13), NR));
        assert_eq!(dynadron_n(&Nat::S(1), &Nat::S(11), &Nat::S(13), NL).unwrap(), Nat::S(565));
        let b = n_to_address(&Nat::S(391)).unwrap();
        assert_eq!((b.u(), b.v_local(), b.lane), (&Nat::S(9), &Nat::S(11), NL));
        for bad in [552u64, 576, 600, 4] {
            assert!(matches!(n_to_address(&Nat::S(bad)), Err(Error::Address(_))), "{bad}");
        }
        for (good, lane) in [(553u64, NL), (575, NL), (577, NR), (599, NR)] {
            let a = n_to_address(&Nat::S(good)).unwrap();
            assert_eq!(a.lane, lane);
            assert_eq!(a.dyn_.g, Nat::S(24));
        }
        assert_eq!(cell_address(&Nat::ONE, &Nat::ONE, &Nat::ONE, NL).unwrap().n, Nat::S(3));
    }

    #[test]
    fn r23_u14_v744() {
        let n = dynadron_n(&Nat::S(23), &Nat::S(14), &Nat::S(744), NL).unwrap();
        let g = Nat::S(22).mul(realm_span()).add_u64(758);
        let expected = g.mul(&g).checked_sub(&Nat::S(14)).unwrap();
        assert_eq!(n, expected);
        assert_eq!(n.to_string().len(), 203);
        let back = n_to_address(&n).unwrap();
        assert_eq!((back.realm(), back.u(), back.v_local(), back.lane), (&Nat::S(23), &Nat::S(14), &Nat::S(744), NL));
        let nr = dynadron_n(&Nat::S(23), &Nat::S(14), &Nat::S(744), NR).unwrap();
        let back = n_to_address(&nr).unwrap();
        assert_eq!(back.lane, NR);
        assert_eq!(back.v_local(), &Nat::S(744));
    }

    #[test]
    fn realm_edges() {
        let t = realm_span().clone();
        assert_eq!(split_global_v(&t).unwrap(), (Nat::ONE, t.clone()));
        assert_eq!(split_global_v(&t.add_u64(1)).unwrap(), (Nat::S(2), Nat::ONE));
        for (r, v) in [(Nat::ONE, t.clone()), (Nat::S(2), Nat::ONE), (nat("123456789012345678901234567890"), nat("99"))] {
            for lane in [NL, NR] {
                for u in [Nat::ONE, nat("98765432109876543210")] {
                    let a = cell_address(&r, &u, &v, lane).unwrap();
                    let b = n_to_address(&a.n).unwrap();
                    assert_eq!(a, b);
                }
            }
        }
        assert!(validate_local_v(&t, None).is_ok());
        assert!(matches!(validate_local_v(&t.add_u64(1), None), Err(Error::RealmLimit(_))));
        assert!(matches!(validate_local_v(&Nat::ZERO, None), Err(Error::RealmLimit(_))));
        assert!(matches!(dynadron_address(&Nat::ZERO, &Nat::ONE, &Nat::ONE), Err(Error::Value(_))));
        assert!(matches!(dynadron_address(&Nat::ONE, &Nat::ZERO, &Nat::ONE), Err(Error::Value(_))));
    }

    #[test]
    fn exhaustive_small_range() {
        // Every number 1..200000 is either GZ/N0 or exactly invertible.
        let mut valid = 0;
        for n in 1u64..200_000 {
            let m = (n as f64).sqrt() as u64;
            let m = if (m + 1) * (m + 1) <= n { m + 1 } else if m * m > n { m - 1 } else { m };
            let is_gz = m * m == n;
            let is_n0 = m * (m + 1) == n;
            match n_to_address(&Nat::S(n)) {
                Ok(a) => {
                    assert!(!is_gz && !is_n0);
                    let back = dynadron_n(a.realm(), a.u(), a.v_local(), a.lane).unwrap();
                    assert_eq!(back, Nat::S(n));
                    valid += 1;
                }
                Err(_) => assert!(is_gz || is_n0, "{n}"),
            }
        }
        assert!(valid > 199_000);
    }

    #[test]
    fn parse_refs() {
        assert_eq!(parse_n_reference("N589").unwrap(), Nat::S(589));
        assert_eq!(parse_n_reference(" n 12 ").unwrap(), Nat::S(12));
        assert!(matches!(parse_n_reference("Nx"), Err(Error::Address(_))));
        assert!(matches!(parse_n_reference("0"), Err(Error::Value(_))));
    }
}
