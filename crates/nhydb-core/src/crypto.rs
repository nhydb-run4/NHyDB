//! Kryptografische Grundfunktionen für den externen Layoutplan (v0.27).
//!
//! Ausschließlich etablierte RustCrypto-Implementierungen, keine eigene
//! Kryptografie: AES-256-GCM (AEAD), HMAC-SHA256, SHA-256, Base64,
//! Zufallszahlen des Betriebssystems, UUIDv4. Byte- und JSON-Verträge
//! entsprechen `nhydb_math_v027.layout_seal/layout_open/layout_mac`.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;

pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    getrandom::fill(&mut v).map_err(|e| Error::NHyDB(format!("Zufallsquelle des Betriebssystems nicht verfuegbar: {e}")))?;
    Ok(v)
}

/// Zufällige UUID Version 4 in Textform (wie `str(uuid.uuid4())`).
pub fn uuid4() -> Result<String> {
    let mut b = random_bytes(16)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    Ok(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

pub fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC akzeptiert jede Schluessellaenge");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// Konstanter Zeitvergleich (wie `hmac.compare_digest`).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    d == 0
}

pub fn b64encode(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Strikte Dekodierung (wie `base64.b64decode(..., validate=True)`).
pub fn b64decode(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(|e| Error::Corruption(format!("Base64 ungueltig: {e}")))
}

fn cipher(key: &[u8]) -> Result<Aes256Gcm> {
    if key.len() != KEY_LEN {
        return Err(Error::Value("Layout-Key muss 32 Byte haben".into()));
    }
    let k = Key::<Aes256Gcm>::try_from(key).map_err(|_| Error::Value("Layout-Key muss 32 Byte haben".into()))?;
    Ok(Aes256Gcm::new(&k))
}

/// AES-256-GCM mit explizitem Nonce; Ergebnis = Ciphertext ‖ 16-Byte-Tag.
pub fn aes_gcm_seal_with_nonce(key: &[u8], nonce: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let n = Nonce::try_from(nonce).map_err(|_| Error::Value("Nonce muss 12 Byte haben".into()))?;
    cipher(key)?
        .encrypt(&n, Payload { msg: plaintext, aad })
        .map_err(|_| Error::NHyDB("AES-GCM-Verschluesselung fehlgeschlagen".into()))
}

/// AES-256-GCM-Entschlüsselung inkl. Tag-Prüfung.
pub fn aes_gcm_open(key: &[u8], nonce: &[u8], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let n = Nonce::try_from(nonce).map_err(|_| Error::Corruption("Layoutplan: Nonce-Laenge ungueltig".into()))?;
    cipher(key)?
        .decrypt(&n, Payload { msg: ciphertext, aad })
        .map_err(|_| Error::Corruption("Layoutplan: Entschluesselung/Authentifizierung fehlgeschlagen (AES-GCM-Tag ungueltig)".into()))
}

/// `layout_seal`: zufälliger 12-Byte-Nonce, Rückgabe (nonce_b64, ciphertext_b64).
pub fn layout_seal(key: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<(String, String)> {
    let nonce = random_bytes(NONCE_LEN)?;
    let ct = aes_gcm_seal_with_nonce(key, &nonce, plaintext, aad)?;
    Ok((b64encode(&nonce), b64encode(&ct)))
}

/// `layout_open`
pub fn layout_open(key: &[u8], nonce_b64: &str, ciphertext_b64: &str, aad: &[u8]) -> Result<Vec<u8>> {
    let nonce = b64decode(nonce_b64)?;
    let ct = b64decode(ciphertext_b64)?;
    aes_gcm_open(key, &nonce, &ct, aad)
}

/// `layout_mac`: domänengetrennter HMAC über den gesamten Plan-Umschlag.
pub fn layout_mac(key: &[u8], payload: &[u8]) -> String {
    let mut dk = b"NHYDB027-LAYOUT-MAC:".to_vec();
    dk.extend_from_slice(key);
    hex(&hmac_sha256(&sha256(&dk), payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answers() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // RFC 4231 Testfall 2
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // NIST GCM Testfall 14 (AES-256, Nullschlüssel, 16 Nullbytes)
        let ct = aes_gcm_seal_with_nonce(&[0u8; 32], &[0u8; 12], &[0u8; 16], b"").unwrap();
        assert_eq!(hex(&ct), "cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919");
        let pt = aes_gcm_open(&[0u8; 32], &[0u8; 12], &ct, b"").unwrap();
        assert_eq!(pt, vec![0u8; 16]);
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert!(matches!(aes_gcm_open(&[0u8; 32], &[0u8; 12], &bad, b""), Err(Error::Corruption(_))));
        assert!(aes_gcm_open(&[0u8; 32], &[0u8; 12], &ct, b"x").is_err());
    }

    #[test]
    fn seal_roundtrip_and_uuid() {
        let key = random_bytes(32).unwrap();
        let (n, c) = layout_seal(&key, b"{\"a\":1}", b"aad").unwrap();
        assert_eq!(layout_open(&key, &n, &c, b"aad").unwrap(), b"{\"a\":1}");
        let u = uuid4().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(b64decode("not base64!").is_err());
    }
}
