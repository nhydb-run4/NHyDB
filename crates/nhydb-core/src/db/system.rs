//! Systemadressraum R0,u1 und externer, verschlüsselter Layoutplan (v0.27).
//!
//! Pro Datenbank drei Dateien: `X.nhydb`, `X.nhydb.layout.enc` (AES-256-GCM-
//! versiegelte Schema-Snapshots, höchstens zwei Generationen, HMAC über den
//! Umschlag) und `X.nhydb.layout.key` (32-Byte-Schlüssel, Rechte 0600).
//! R0,u1 ist ein DB-interner `SYSTEM`-Block je Schema-Generation; er bindet
//! Generation und SHA-256 des aktiven Snapshots. Ohne passenden Schlüssel,
//! Plan, Authentifizierung oder Generation wird das Öffnen verweigert
//! (fail closed). JSON-Verträge und Bytes entsprechen Python-v0.27.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::address::realm_span;
use crate::crypto;
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::Nat;
use crate::platform::{self, Durability};
use crate::val::Val;
use crate::vmap;

pub const KEY_CONTRACT: &str = "NHYDB027_KEY_V1";
pub const PLAN_CONTRACT: &str = "NHYDB027_EXTERNAL_LAYOUT_V1";
pub const SCHEMA_CONTRACT: &str = "NHYDB027_SCHEMA_V1";
pub const R0_CONTRACT: &str = "NHYDB027_R0U1_V1";
pub const CRYPTO_CONTRACT: &str = "AES-256-GCM + HMAC-SHA256";
/// Unveränderliche R0-Zellen (Python prüft 1, 2, 3, 4, 7, 8).
pub const IMMUTABLE_CELLS: [&str; 6] = ["1", "2", "3", "4", "7", "8"];
/// R0,u1,v9: feste mathematische Systemvorgaben dieser Datenbank.
pub const MATH_CONSTANTS_CELL: &str = "9";

/// Wert der Zelle v9 (`{"lane_NL":0,"lane_NR":1,"realm_span":10**100}`).
pub fn math_constants_cell() -> Val {
    vmap! {"lane_NL" => 0i64, "lane_NR" => 1i64, "realm_span" => realm_span()}
}

/// Laufzeitzustand des Systemadressraums einer geöffneten Datenbank.
#[derive(Debug, Clone)]
pub struct SystemState {
    /// Aktueller R0-Datensatz `{"contract":…, "cells":{…}}`.
    pub record: Val,
    pub key: Vec<u8>,
    pub layout_path: PathBuf,
    pub key_path: PathBuf,
    pub entries: Vec<Val>,
    pub active_entry: Val,
    /// Schema seit dem letzten Commit geändert.
    pub layout_dirty: bool,
    /// Fehler während eines Schema-Commits: weitere Schreibzugriffe gesperrt.
    pub poisoned: bool,
    /// Ein SYSTEM-Block der Kette enthält v9 (feste Systemvorgaben, Rust).
    pub math_constants_stored: bool,
}

impl SystemState {
    pub fn cell(&self, v: &str) -> Option<&Val> {
        self.record.get("cells").and_then(|c| c.get(v))
    }
    pub fn db_id(&self) -> String {
        self.cell("1").map(|v| v.py_str()).unwrap_or_default()
    }
    pub fn db_name(&self) -> String {
        self.cell("2").map(|v| v.py_str()).unwrap_or_default()
    }
    pub fn generation(&self) -> Result<u64> {
        self.cell("5")
            .and_then(|v| v.as_i64())
            .filter(|g| *g >= 0)
            .map(|g| g as u64)
            .ok_or_else(|| Error::Corruption("R0,u1 Layoutgeneration ungueltig".into()))
    }
}

pub fn default_sidecars(db: &Path) -> (PathBuf, PathBuf) {
    let mut a = db.as_os_str().to_owned();
    a.push(".layout.enc");
    let mut b = db.as_os_str().to_owned();
    b.push(".layout.key");
    (PathBuf::from(a), PathBuf::from(b))
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn read_json(path: &Path) -> Result<Val> {
    let raw = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound(format!("[Errno 2] No such file or directory: '{}'", path.display()))
        } else {
            Error::Io(e)
        }
    })?;
    let s = std::str::from_utf8(&raw).map_err(|_| Error::Corruption(format!("{}: kein UTF-8", path.display())))?;
    Val::from_json(s).map_err(|e| Error::Corruption(format!("{}: JSON ungueltig ({e})", path.display())))
}

// ----------------------------------------------------------------------
// Schlüsseldatei
// ----------------------------------------------------------------------

pub fn key_create(path: &Path, db_id: &str, db_name: &str, mode: Durability) -> Result<Vec<u8>> {
    let key = crypto::random_bytes(crypto::KEY_LEN)?;
    let raw = vmap! {
        "contract" => KEY_CONTRACT, "db_id" => db_id, "db_name" => db_name,
        "key_b64" => crypto::b64encode(&key),
    }
    .to_json()?;
    let dir = platform::parent_dir(path);
    std::fs::create_dir_all(&dir)?;
    let mut f = platform::create_new(path, true)?;
    f.write_all(raw.as_bytes())?;
    f.flush()?;
    platform::sync_file(&f, mode)?;
    drop(f);
    platform::sync_dir(&dir, mode)?;
    Ok(key)
}

pub fn key_load(path: &Path, db_id: &str, db_name: &str) -> Result<Vec<u8>> {
    let obj = read_json(path)?;
    let ok = obj.get("contract").and_then(|v| v.as_str()) == Some(KEY_CONTRACT)
        && obj.get("db_id").and_then(|v| v.as_str()) == Some(db_id)
        && obj.get("db_name").and_then(|v| v.as_str()) == Some(db_name);
    if !ok {
        return Err(Error::Corruption("Falsche Layout-Key-Datei (Datenbank-ID oder Name stimmt nicht)".into()));
    }
    let b64 = obj.get("key_b64").and_then(|v| v.as_str()).ok_or_else(|| Error::Corruption("Layout-Key fehlt".into()))?;
    let key = crypto::b64decode(b64)?;
    if key.len() != crypto::KEY_LEN {
        return Err(Error::Corruption("Layout-Key-Laenge ungueltig".into()));
    }
    Ok(key)
}

// ----------------------------------------------------------------------
// Plan-Datei (Umschlag mit 1–2 versiegelten Generationen)
// ----------------------------------------------------------------------

fn entry_aad(db_id: &str, generation: u64, digest: &str) -> Vec<u8> {
    format!("NHYDB027:{db_id}:{generation}:{digest}").into_bytes()
}

/// `_layout_seal_entry`: Klartext = kanonisches JSON des Snapshots.
pub fn seal_entry(db_id: &str, generation: u64, snapshot_json: &[u8], key: &[u8]) -> Result<Val> {
    let digest = crypto::sha256_hex(snapshot_json);
    let (nonce, ct) = crypto::layout_seal(key, snapshot_json, &entry_aad(db_id, generation, &digest))?;
    Ok(vmap! {"generation" => generation as i64, "sha256" => digest.as_str(), "nonce" => nonce, "ciphertext" => ct})
}

fn envelope_content(db_id: &str, entries: &[Val]) -> Val {
    vmap! {"contract" => PLAN_CONTRACT, "db_id" => db_id, "entries" => Val::List(entries.to_vec())}
}

/// `_layout_envelope`
pub fn envelope_bytes(db_id: &str, entries: &[Val], key: &[u8]) -> Result<Vec<u8>> {
    let mut content = envelope_content(db_id, entries);
    let mac = crypto::layout_mac(key, content.to_json()?.as_bytes());
    content.set("mac", Val::str(mac));
    Ok(content.to_json()?.into_bytes())
}

/// `_layout_read_envelope`: Integrität (HMAC) und Zugehörigkeit prüfen.
pub fn read_envelope(path: &Path, db_id: &str, key: &[u8]) -> Result<Vec<Val>> {
    let mut env = read_json(path)?;
    let mac = env.remove("mac");
    if env.get("contract").and_then(|v| v.as_str()) != Some(PLAN_CONTRACT) || env.get("db_id").and_then(|v| v.as_str()) != Some(db_id) {
        return Err(Error::Corruption("Externer Layoutplan gehoert nicht zur Datenbank".into()));
    }
    let expected = crypto::layout_mac(key, env.to_json()?.as_bytes());
    match mac.as_ref().and_then(|m| m.as_str()) {
        Some(m) if crypto::ct_eq(m.as_bytes(), expected.as_bytes()) => {}
        _ => return Err(Error::Corruption("Layoutplan: Integritaetspruefung fehlgeschlagen".into())),
    }
    match env.get("entries").and_then(|e| e.as_list()) {
        Some(l) if (1..=2).contains(&l.len()) => Ok(l.clone()),
        _ => Err(Error::Corruption("Layoutplan: ungueltige Generationsliste".into())),
    }
}

pub fn entry_generation(e: &Val) -> Result<u64> {
    e.get("generation")
        .and_then(|g| g.as_i64())
        .filter(|g| *g >= 0)
        .map(|g| g as u64)
        .ok_or_else(|| Error::Corruption("Layoutplan: Generation ungueltig".into()))
}

pub fn entry_digest(e: &Val) -> Result<&str> {
    e.get("sha256").and_then(|g| g.as_str()).ok_or_else(|| Error::Corruption("Layoutplan: Hash fehlt".into()))
}

/// `_layout_open_entry`: entschlüsseln, Hash und Vertrag prüfen.
pub fn open_entry(db_id: &str, entry: &Val, key: &[u8]) -> Result<Val> {
    let generation = entry_generation(entry)?;
    let digest = entry_digest(entry)?;
    let nonce = entry.get("nonce").and_then(|v| v.as_str()).ok_or_else(|| Error::Corruption("Layoutplan: Nonce fehlt".into()))?;
    let ct = entry.get("ciphertext").and_then(|v| v.as_str()).ok_or_else(|| Error::Corruption("Layoutplan: Ciphertext fehlt".into()))?;
    let raw = crypto::layout_open(key, nonce, ct, &entry_aad(db_id, generation, digest))?;
    if crypto::sha256_hex(&raw) != digest {
        return Err(Error::Corruption("Layoutplan-Snapshot-Hash falsch".into()));
    }
    let text = std::str::from_utf8(&raw).map_err(|_| Error::Corruption("Layoutplan-Snapshot kein UTF-8".into()))?;
    let snap = Val::from_json(text)?;
    if snap.get("contract").and_then(|v| v.as_str()) != Some(SCHEMA_CONTRACT) {
        return Err(Error::Corruption("Layoutplan-Snapshot-Contract falsch".into()));
    }
    Ok(snap)
}

/// `_atomic_layout_replace`: temporäre Datei im Zielordner (0600), schreiben,
/// synchronisieren, atomar austauschen, Verzeichnis synchronisieren.
pub fn atomic_write(path: &Path, data: &[u8], mode: Durability) -> Result<()> {
    let dir = platform::parent_dir(path);
    std::fs::create_dir_all(&dir)?;
    let tmp = loop {
        let suffix = crypto::hex(&crypto::random_bytes(6)?);
        let t = dir.join(format!(".{}.{suffix}.tmp", file_name(path)));
        if !t.exists() {
            break t;
        }
    };
    let result = (|| -> Result<()> {
        let mut f = platform::create_new(&tmp, true)?;
        f.write_all(data)?;
        f.flush()?;
        platform::sync_file(&f, mode)?;
        drop(f);
        platform::atomic_replace(&tmp, path, mode)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub fn empty_schema() -> Val {
    vmap! {
        "contract" => SCHEMA_CONTRACT, "realms" => Val::List(vec![]), "shapes" => Val::List(vec![]),
        "extensions" => Val::List(vec![]), "layouts" => Val::List(vec![]), "indexes" => Val::List(vec![]),
        "math_layouts" => Val::List(vec![]),
    }
}

/// `_system_bootstrap_payload` plus Zelle v9 (feste Mathematik).
#[allow(clippy::too_many_arguments)]
pub fn bootstrap_record(db_id: &str, db_name: &str, plan_name: &str, key_name: &str, generation: u64, digest: &str, created_ns: u64) -> Val {
    vmap! {
        "contract" => R0_CONTRACT,
        "cells" => vmap! {
            "1" => db_id, "2" => db_name, "3" => plan_name, "4" => key_name,
            "5" => generation as i64, "6" => digest, "7" => Val::int(created_ns as i128),
            "8" => CRYPTO_CONTRACT, MATH_CONSTANTS_CELL => math_constants_cell(),
        },
    }
}

/// Nächster R0-Datensatz für eine neue Generation (unveränderliche Zellen
/// werden übernommen). v9 steht nur im ersten SYSTEM-Block der Datei; die
/// Folgegenerationen sind damit bytegleich zu Python-v0.27 aufgebaut.
pub fn next_record(cur: &Val, generation: u64, digest: &str) -> Val {
    let c = |k: &str| cur.get("cells").and_then(|x| x.get(k)).cloned().unwrap_or(Val::None);
    let mut rec = vmap! {"contract" => R0_CONTRACT, "cells" => Val::map()};
    let cells = rec.get_mut("cells").unwrap();
    for k in ["1", "2", "3", "4"] {
        cells.set(k, c(k));
    }
    cells.set("5", Val::int(generation as i128));
    cells.set("6", Val::str(digest));
    cells.set("7", c("7"));
    cells.set("8", c("8"));
    rec
}

/// Prüft einen SYSTEM-Datensatz und seine Verkettung mit dem Vorgänger.
pub fn check_record(item: &Val, prev: Option<&Val>) -> Result<()> {
    if item.get("contract").and_then(|v| v.as_str()) != Some(R0_CONTRACT) {
        return Err(Error::Corruption("R0,u1 Bootstrap-Contract ungueltig".into()));
    }
    let cells = item.get("cells").filter(|c| c.as_map().is_some()).ok_or_else(|| Error::Corruption("R0,u1 Zellen fehlen".into()))?;
    if let Some(v9) = cells.get(MATH_CONSTANTS_CELL) {
        if *v9 != math_constants_cell() || v9.get("realm_span").and_then(|r| r.as_nat()).as_ref() != Some(realm_span()) {
            return Err(Error::Corruption(
                "R0,u1,v9: feste Systemvorgaben verletzt (REALM_SPAN=10**100, NL=0, NR=1)".into(),
            ));
        }
    }
    for k in ["1", "2", "3", "4", "5", "6", "7", "8"] {
        if cells.get(k).is_none() {
            return Err(Error::Corruption(format!("R0,u1,v{k} fehlt")));
        }
    }
    if let Some(prev) = prev {
        let pc = prev.get("cells").unwrap();
        if IMMUTABLE_CELLS.iter().any(|k| cells.get(k) != pc.get(k)) {
            return Err(Error::Corruption("R0,u1 unveraenderliche Systemzellen verletzt".into()));
        }
        let g = cells.get("5").and_then(|v| v.as_i64());
        let pg = pc.get("5").and_then(|v| v.as_i64());
        match (g, pg) {
            (Some(g), Some(pg)) if g == pg + 1 => {}
            _ => return Err(Error::Corruption("R0,u1 Layoutgeneration uebersprungen".into())),
        }
    }
    Ok(())
}

/// `_write_initial_system_db`: neue DB-Datei mit Superblöcken und dem ersten
/// SYSTEM-Block (Block-ID 1).
pub fn write_initial_db(path: &Path, record: &Val, mode: Durability) -> Result<()> {
    let payload = record.to_json()?.into_bytes();
    let end = align8(DATA_START + BLOCK_HEADER_SIZE as u64 + payload.len() as u64);
    let t = super::now_ns();
    let st = SuperState { generation: 0, checkpoint_offset: 0, last_valid_end: end, next_block_id: 2, created_ns: t, updated_ns: t, flags: 0 };
    let sb = pack_super(&st);
    let mut f = platform::create_new(path, false)?;
    f.write_all(&sb)?;
    f.write_all(&sb)?;
    f.write_all(&pack_block_header(BT_SYSTEM, 1, &payload, 0))?;
    f.write_all(&payload)?;
    let pad = (end - DATA_START - BLOCK_HEADER_SIZE as u64 - payload.len() as u64) as usize;
    f.write_all(&vec![0u8; pad])?;
    f.flush()?;
    platform::sync_file(&f, mode)?;
    drop(f);
    platform::sync_dir(&platform::parent_dir(path), mode)
}

/// Erster SYSTEM-Datensatz einer neuen Datei (Kompaktierung): v9 wird
/// übernommen, falls die Quelle sie gespeichert hatte.
pub fn initial_record_from(cur: &Val, with_constants: bool) -> Val {
    let mut rec = cur.clone();
    if let Some(cells) = rec.get_mut("cells") {
        if with_constants {
            cells.set(MATH_CONSTANTS_CELL, math_constants_cell());
        } else {
            cells.remove(MATH_CONSTANTS_CELL);
        }
    }
    rec
}

pub fn nat_of(v: &Val) -> Option<Nat> {
    v.as_nat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_roundtrip_and_tamper() {
        let d = std::env::temp_dir().join(format!("nhydb_sys_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let kp = d.join("a.nhydb.layout.key");
        let key = key_create(&kp, "id-1", "a.nhydb", Durability::Fsync).unwrap();
        assert_eq!(key_load(&kp, "id-1", "a.nhydb").unwrap(), key);
        assert!(key_load(&kp, "id-2", "a.nhydb").is_err());
        assert!(key_create(&kp, "id-1", "a.nhydb", Durability::Fsync).is_err());
        let snap = empty_schema().to_json().unwrap();
        let e0 = seal_entry("id-1", 0, snap.as_bytes(), &key).unwrap();
        let lp = d.join("a.nhydb.layout.enc");
        atomic_write(&lp, &envelope_bytes("id-1", std::slice::from_ref(&e0), &key).unwrap(), Durability::Fsync).unwrap();
        let entries = read_envelope(&lp, "id-1", &key).unwrap();
        assert_eq!(open_entry("id-1", &entries[0], &key).unwrap(), empty_schema());
        let mut raw = std::fs::read(&lp).unwrap();
        let i = raw.len() / 2;
        raw[i] ^= 1;
        std::fs::write(&lp, &raw).unwrap();
        assert!(read_envelope(&lp, "id-1", &key).is_err());
        assert!(read_envelope(&lp, "id-1", &key).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn record_chain_rules() {
        let r0 = bootstrap_record("i", "a", "a.enc", "a.key", 0, "h0", 5);
        check_record(&r0, None).unwrap();
        let r1 = next_record(&r0, 1, "h1");
        assert!(r1.get("cells").unwrap().get("9").is_none());
        check_record(&r1, Some(&r0)).unwrap();
        assert!(check_record(&next_record(&r0, 2, "h2"), Some(&r0)).is_err());
        let mut bad = r1.clone();
        bad.get_mut("cells").unwrap().set("2", Val::str("b"));
        assert!(check_record(&bad, Some(&r0)).is_err());
        let mut v9 = r1.clone();
        assert!(initial_record_from(&r1, true).get("cells").unwrap().get("9").is_some());
        v9.get_mut("cells").unwrap().set("9", vmap! {"lane_NL" => 0i64, "lane_NR" => 1i64, "realm_span" => 10i64});
        assert!(check_record(&v9, Some(&r0)).is_err());
        // Python-v0.27-Datensatz ohne v9 ist gültig
        let mut py = r1.clone();
        py.get_mut("cells").unwrap().remove("9");
        check_record(&py, Some(&r0)).unwrap();
    }
}
