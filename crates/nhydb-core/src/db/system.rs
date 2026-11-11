//! System address space R0,u1 and external, encrypted layout plan.
//!
//! Three files per database: `X.nhydb`, `X.nhydb.layout.enc` (AES-256-GCM-
//! sealed schema snapshots, at most two generations, HMAC over the
//! envelope) and `X.nhydb.layout.key` (32-byte key, permissions 0600).
//! R0,u1 is a DB-internal `SYSTEM` block per schema generation; it binds
//! the generation and SHA-256 of the active snapshot. Without a matching key,
//! plan, authentication or generation, opening is refused
//! (fail closed). The JSON contracts and bytes are fixed.

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
/// Immutable R0 cells (checked: 1, 2, 3, 4, 7, 8).
pub const IMMUTABLE_CELLS: [&str; 6] = ["1", "2", "3", "4", "7", "8"];
/// R0,u1,v9: fixed mathematical system constants of this database.
pub const MATH_CONSTANTS_CELL: &str = "9";

/// Value of cell v9 (`{"lane_NL":0,"lane_NR":1,"realm_span":10**100}`).
pub fn math_constants_cell() -> Val {
    vmap! {"lane_NL" => 0i64, "lane_NR" => 1i64, "realm_span" => realm_span()}
}

/// Layout key material: overwritten with zeros on drop, never printed by
/// `Debug`.
#[derive(Clone, Default)]
pub struct SecretKey(Vec<u8>);

impl SecretKey {
    pub fn new(key: Vec<u8>) -> SecretKey {
        SecretKey(key)
    }
}

impl std::ops::Deref for SecretKey {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        crypto::wipe(&mut self.0);
    }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretKey(<{} bytes redacted>)", self.0.len())
    }
}

/// Runtime state of the system address space of an open database.
#[derive(Debug, Clone)]
pub struct SystemState {
    /// Current R0 record `{"contract":…, "cells":{…}}`.
    pub record: Val,
    pub key: SecretKey,
    pub layout_path: PathBuf,
    pub key_path: PathBuf,
    pub entries: Vec<Val>,
    pub active_entry: Val,
    /// Schema changed since the last commit.
    pub layout_dirty: bool,
    /// Error during a schema commit: further writes are blocked.
    pub poisoned: bool,
    /// A SYSTEM block of the chain contains v9 (fixed system constants).
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
            .ok_or_else(|| Error::Corruption("R0,u1 layout generation invalid".into()))
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
            Error::IoPath(e, path.into())
        }
    })?;
    let s = std::str::from_utf8(&raw).map_err(|_| Error::Corruption(format!("{}: not UTF-8", path.display())))?;
    Val::from_json(s).map_err(|e| Error::Corruption(format!("{}: invalid JSON ({e})", path.display())))
}

// ----------------------------------------------------------------------
// Key file
// ----------------------------------------------------------------------

pub fn key_create(path: &Path, db_id: &str, db_name: &str, mode: Durability) -> Result<SecretKey> {
    let key = SecretKey::new(crypto::random_bytes(crypto::KEY_LEN)?);
    let raw = vmap! {
        "contract" => KEY_CONTRACT, "db_id" => db_id, "db_name" => db_name,
        "key_b64" => crypto::b64encode(&key),
    }
    .to_json()?;
    let mut raw = raw.into_bytes();
    let dir = platform::parent_dir(path);
    // F048: OS errors name the key file
    std::fs::create_dir_all(&dir).map_err(|e| Error::IoPath(e, path.into()))?;
    let mut f = platform::create_new(path, true).map_err(|e| e.at_path(path))?;
    let res = (|| -> Result<()> {
        f.write_all(&raw)?;
        f.flush()?;
        platform::sync_file(&f, mode)?;
        platform::sync_dir(&dir, mode)
    })()
    .map_err(|e| e.at_path(path));
    drop(f);
    crypto::wipe(&mut raw);
    if res.is_err() {
        // only the file created here is removed
        let _ = std::fs::remove_file(path);
    }
    res?;
    Ok(key)
}

/// Warning if the key file is readable or writable by group/others (Unix);
/// the key is still used (no abort), the caller reports the warning.
pub fn key_permission_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode();
        if mode & 0o077 != 0 {
            return Some(format!(
                "layout key file {} is accessible by group/others (mode {:o}); restrict it with chmod 600",
                path.display(),
                mode & 0o777
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    None
}

/// `db_id` stored in plain text in a key or plan file (`None` if the file is
/// not a readable NHyDB key/plan file).
pub fn sidecar_db_id(path: &Path) -> Option<String> {
    let obj = read_json(path).ok()?;
    let c = obj.get("contract").and_then(|v| v.as_str())?;
    if c != KEY_CONTRACT && c != PLAN_CONTRACT {
        return None;
    }
    obj.get("db_id").and_then(|v| v.as_str()).map(str::to_string)
}

/// Identity `(db_id, db_name)` from the first R0,u1 block of a DB file
/// (read-only, CRC-checked; `None` if not readable).
pub fn read_db_identity(path: &Path) -> Option<(String, String)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(DATA_START)).ok()?;
    let mut raw = [0u8; BLOCK_HEADER_SIZE];
    f.read_exact(&mut raw).ok()?;
    let h = unpack_block_header(&raw).ok()?;
    if h.block_type != BT_SYSTEM || h.payload_len > (1 << 20) {
        return None;
    }
    let mut payload = vec![0u8; h.payload_len as usize];
    f.read_exact(&mut payload).ok()?;
    if crc32(&payload) != h.payload_crc {
        return None;
    }
    let rec = Val::from_json(std::str::from_utf8(&payload).ok()?).ok()?;
    let cell = |k: &str| rec.get("cells").and_then(|c| c.get(k)).and_then(|v| v.as_str()).map(str::to_string);
    Some((cell("1")?, cell("2")?))
}

pub fn key_load(path: &Path, db_id: &str, db_name: &str) -> Result<SecretKey> {
    let obj = read_json(path)?;
    let ok = obj.get("contract").and_then(|v| v.as_str()) == Some(KEY_CONTRACT)
        && obj.get("db_id").and_then(|v| v.as_str()) == Some(db_id)
        && obj.get("db_name").and_then(|v| v.as_str()) == Some(db_name);
    if !ok {
        return Err(Error::Corruption("Wrong layout key file (database ID or name does not match)".into()));
    }
    let b64 = obj.get("key_b64").and_then(|v| v.as_str()).ok_or_else(|| Error::Corruption("Layout key missing".into()))?;
    let key = SecretKey::new(crypto::b64decode(b64)?);
    if key.len() != crypto::KEY_LEN {
        return Err(Error::Corruption("Layout key length invalid".into()));
    }
    Ok(key)
}

// ----------------------------------------------------------------------
// Plan file (envelope with 1–2 sealed generations)
// ----------------------------------------------------------------------

fn entry_aad(db_id: &str, generation: u64, digest: &str) -> Vec<u8> {
    format!("NHYDB027:{db_id}:{generation}:{digest}").into_bytes()
}

/// `_layout_seal_entry`: plaintext = canonical JSON of the snapshot.
/// The field `sha256` holds the keyed schema digest (HMAC-SHA256,
/// `crypto::schema_digest`, R013) – deliberate format change against v0.27
/// (there: plain SHA-256 of the snapshot); field name kept.
pub fn seal_entry(db_id: &str, generation: u64, snapshot_json: &[u8], key: &[u8]) -> Result<Val> {
    let digest = crypto::schema_digest(key, snapshot_json);
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

/// `_layout_read_envelope`: check integrity (HMAC) and ownership.
pub fn read_envelope(path: &Path, db_id: &str, key: &[u8]) -> Result<Vec<Val>> {
    let mut env = read_json(path)?;
    let mac = env.remove("mac");
    if env.get("contract").and_then(|v| v.as_str()) != Some(PLAN_CONTRACT) || env.get("db_id").and_then(|v| v.as_str()) != Some(db_id) {
        return Err(Error::Corruption("External layout plan does not belong to the database".into()));
    }
    let expected = crypto::layout_mac(key, env.to_json()?.as_bytes());
    match mac.as_ref().and_then(|m| m.as_str()) {
        Some(m) if crypto::ct_eq(m.as_bytes(), expected.as_bytes()) => {}
        _ => return Err(Error::Corruption("Layout plan: integrity check failed".into())),
    }
    match env.get("entries").and_then(|e| e.as_list()) {
        Some(l) if (1..=2).contains(&l.len()) => Ok(l.clone()),
        _ => Err(Error::Corruption("Layout plan: invalid generation list".into())),
    }
}

pub fn entry_generation(e: &Val) -> Result<u64> {
    e.get("generation")
        .and_then(|g| g.as_i64())
        .filter(|g| *g >= 0)
        .map(|g| g as u64)
        .ok_or_else(|| Error::Corruption("Layout plan: generation invalid".into()))
}

pub fn entry_digest(e: &Val) -> Result<&str> {
    e.get("sha256").and_then(|g| g.as_str()).ok_or_else(|| Error::Corruption("Layout plan: schema digest missing".into()))
}

/// Message for layout plans with the unkeyed schema digest of NHyDB 0.27.1
/// and earlier (G2-2).
pub const LEGACY_DIGEST_MESSAGE: &str = "database was created by NHyDB 0.27.1 or earlier (unkeyed schema digest); this \
     version cannot open it - export the data with NHyDB 0.27.1 and re-import it (there is no migration)";

/// `_layout_open_entry`: decrypt, check keyed digest and contract.
pub fn open_entry(db_id: &str, entry: &Val, key: &[u8]) -> Result<Val> {
    let generation = entry_generation(entry)?;
    let digest = entry_digest(entry)?;
    let nonce = entry.get("nonce").and_then(|v| v.as_str()).ok_or_else(|| Error::Corruption("Layout plan: nonce missing".into()))?;
    let ct = entry.get("ciphertext").and_then(|v| v.as_str()).ok_or_else(|| Error::Corruption("Layout plan: ciphertext missing".into()))?;
    let raw = crypto::layout_open(key, nonce, ct, &entry_aad(db_id, generation, digest))?;
    if !crypto::ct_eq(crypto::schema_digest(key, &raw).as_bytes(), digest.as_bytes()) {
        // G2-2: AES-GCM (with the digest in the AAD) and the plan MAC are
        // valid, the digest is the plain SHA-256 of the snapshot: a plan
        // written by NHyDB 0.27.1 or earlier (no migration, R013).
        if crypto::ct_eq(crypto::sha256_hex(&raw).as_bytes(), digest.as_bytes()) {
            return Err(Error::Corruption(LEGACY_DIGEST_MESSAGE.into()));
        }
        return Err(Error::Corruption("Layout plan snapshot digest mismatch".into()));
    }
    let text = std::str::from_utf8(&raw).map_err(|_| Error::Corruption("Layout plan snapshot is not UTF-8".into()))?;
    let snap = Val::from_json(text)?;
    if snap.get("contract").and_then(|v| v.as_str()) != Some(SCHEMA_CONTRACT) {
        return Err(Error::Corruption("Layout plan snapshot contract mismatch".into()));
    }
    Ok(snap)
}

/// `_atomic_layout_replace`: temporary file in the target directory (0600), write,
/// sync, swap atomically, sync the directory.
pub fn atomic_write(path: &Path, data: &[u8], mode: Durability) -> Result<()> {
    atomic_write_inner(path, data, mode).map_err(|e| e.at_path(path)) // F048: OS errors name the target file
}

fn atomic_write_inner(path: &Path, data: &[u8], mode: Durability) -> Result<()> {
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

/// `_system_bootstrap_payload` plus cell v9 (fixed mathematics).
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

/// Next R0 record for a new generation (immutable cells
/// are carried over). v9 is stored only in the first SYSTEM block of the file;
/// subsequent generations contain only cells 1–8.
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

/// Checks a SYSTEM record and its chaining with the predecessor.
pub fn check_record(item: &Val, prev: Option<&Val>) -> Result<()> {
    if item.get("contract").and_then(|v| v.as_str()) != Some(R0_CONTRACT) {
        return Err(Error::Corruption("R0,u1 bootstrap contract invalid".into()));
    }
    let cells = item.get("cells").filter(|c| c.as_map().is_some()).ok_or_else(|| Error::Corruption("R0,u1 cells missing".into()))?;
    if let Some(v9) = cells.get(MATH_CONSTANTS_CELL) {
        if *v9 != math_constants_cell() || v9.get("realm_span").and_then(|r| r.as_nat()).as_ref() != Some(realm_span()) {
            return Err(Error::Corruption(
                "R0,u1,v9: fixed system constants violated (REALM_SPAN=10**100, NL=0, NR=1)".into(),
            ));
        }
    }
    for k in ["1", "2", "3", "4", "5", "6", "7", "8"] {
        if cells.get(k).is_none() {
            return Err(Error::Corruption(format!("R0,u1,v{k} missing")));
        }
    }
    if let Some(prev) = prev {
        let pc = prev.get("cells").unwrap();
        if IMMUTABLE_CELLS.iter().any(|k| cells.get(k) != pc.get(k)) {
            return Err(Error::Corruption("R0,u1 immutable system cells violated".into()));
        }
        let g = cells.get("5").and_then(|v| v.as_i64());
        let pg = pc.get("5").and_then(|v| v.as_i64());
        match (g, pg) {
            (Some(g), Some(pg)) if g == pg + 1 => {}
            _ => return Err(Error::Corruption("R0,u1 layout generation skipped".into())),
        }
    }
    Ok(())
}

/// Binds a SYSTEM record taken from the block directory to the first record
/// of the file (block directory, 0.17): the chain between them was checked
/// when the directory was written; here the immutable cells must still be
/// those of the bootstrap record and the generation must not go backwards.
pub fn check_record_bound(item: &Val, first: &Val) -> Result<()> {
    check_record(first, None)?;
    let (c, f) = (item.get("cells").unwrap(), first.get("cells").unwrap());
    if IMMUTABLE_CELLS.iter().any(|k| c.get(k) != f.get(k)) {
        return Err(Error::Corruption("R0,u1 immutable system cells violated".into()));
    }
    let g = c.get("5").and_then(|v| v.as_i64());
    let fg = f.get("5").and_then(|v| v.as_i64());
    match (g, fg) {
        (Some(g), Some(fg)) if g >= fg => Ok(()),
        _ => Err(Error::Corruption("R0,u1 layout generation goes backwards".into())),
    }
}

/// `_write_initial_system_db`: new DB file with superblocks and the first
/// SYSTEM block (block ID 1).
pub fn write_initial_db(path: &Path, record: &Val, mode: Durability) -> Result<()> {
    let payload = record.to_json()?.into_bytes();
    let end = align8(DATA_START + BLOCK_HEADER_SIZE as u64 + payload.len() as u64);
    let t = super::now_ns();
    let st = SuperState { generation: 0, checkpoint_offset: 0, last_valid_end: end, next_block_id: 2, created_ns: t, updated_ns: t, flags: 0, dir_root: 0 };
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

/// First SYSTEM record of a new file (compaction): v9 is
/// carried over if the source had stored it.
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

    /// G2-2: an entry with the plain SHA-256 of NHyDB 0.27.1 (valid AEAD)
    /// gets a message naming the old version; a manipulated digest keeps the
    /// generic message.
    #[test]
    fn legacy_plain_digest_is_named() {
        let key = [7u8; 32];
        let snap = format!("{{\"contract\":\"{SCHEMA_CONTRACT}\"}}");
        let raw = snap.as_bytes();
        let seal = |digest: &str| {
            let (nonce, ct) = crypto::layout_seal(&key, raw, &entry_aad("id", 3, digest)).unwrap();
            vmap! {"generation" => 3i64, "sha256" => digest, "nonce" => nonce, "ciphertext" => ct}
        };
        assert!(open_entry("id", &seal_entry("id", 3, raw, &key).unwrap(), &key).is_ok());
        let legacy = seal(&crypto::sha256_hex(raw));
        assert!(matches!(open_entry("id", &legacy, &key), Err(Error::Corruption(m)) if m == LEGACY_DIGEST_MESSAGE));
        let forged = seal(&"0".repeat(64));
        assert!(matches!(open_entry("id", &forged, &key), Err(Error::Corruption(m)) if m == "Layout plan snapshot digest mismatch"));
    }

    #[test]
    fn plan_roundtrip_and_tamper() {
        let d = std::env::temp_dir().join(format!("nhydb_sys_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let kp = d.join("a.nhydb.layout.key");
        let key = key_create(&kp, "id-1", "a.nhydb", Durability::Fsync).unwrap();
        assert_eq!(&*key_load(&kp, "id-1", "a.nhydb").unwrap(), &*key);
        assert!(!format!("{key:?}").contains(&crypto::b64encode(&key)), "Debug must not print the key");
        assert!(format!("{key:?}").contains("redacted"));
        assert_eq!(sidecar_db_id(&kp).as_deref(), Some("id-1"));
        assert!(key_permission_warning(&kp).is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(key_permission_warning(&kp).unwrap().contains("chmod 600"));
            std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
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

    /// R013: the schema digest in plan entry / R0,u1,v6 is keyed – it can
    /// not be recomputed (or a guessed schema confirmed) from the snapshot
    /// alone; manipulating digest or snapshot fails closed.
    #[test]
    fn schema_digest_is_keyed_and_checked() {
        let key = vec![7u8; crypto::KEY_LEN];
        let snap = empty_schema().to_json().unwrap();
        let e = seal_entry("id-1", 3, snap.as_bytes(), &key).unwrap();
        let d = entry_digest(&e).unwrap().to_string();
        assert_ne!(d, crypto::sha256_hex(snap.as_bytes()), "digest must not be a plain SHA-256");
        assert_eq!(d, crypto::schema_digest(&key, snap.as_bytes()));
        assert_ne!(d, crypto::schema_digest(&[8u8; crypto::KEY_LEN], snap.as_bytes()));
        assert_ne!(d, crypto::layout_mac(&key, snap.as_bytes()), "domain separation from the plan MAC");
        assert_eq!(open_entry("id-1", &e, &key).unwrap(), empty_schema());
        // manipulated digest field (also bound via AAD) -> fail closed
        let mut bad = e.clone();
        bad.set("sha256", Val::str(crypto::sha256_hex(snap.as_bytes())));
        assert!(matches!(open_entry("id-1", &bad, &key), Err(Error::Corruption(_))));
        let mut missing = e.clone();
        missing.remove("sha256");
        assert!(matches!(open_entry("id-1", &missing, &key), Err(Error::Corruption(_))));
        // validly encrypted snapshot whose digest does not match it (a
        // v0.27-style plain SHA-256 entry / other snapshot) -> fail closed;
        // the plain SHA-256 is named as NHyDB 0.27.1 file (G2-2)
        for wrong in [crypto::sha256_hex(snap.as_bytes()), crypto::schema_digest(&key, b"{}")] {
            let nonce = [1u8; crypto::NONCE_LEN];
            let ct = crypto::aes_gcm_seal_with_nonce(&key, &nonce, snap.as_bytes(), &entry_aad("id-1", 3, &wrong)).unwrap();
            let forged = vmap! {"generation" => 3i64, "sha256" => wrong.as_str(),
                "nonce" => crypto::b64encode(&nonce), "ciphertext" => crypto::b64encode(&ct)};
            match open_entry("id-1", &forged, &key) {
                Err(Error::Corruption(m)) => assert!(m.contains("digest mismatch") || m == LEGACY_DIGEST_MESSAGE, "{m}"),
                other => panic!("expected digest mismatch, got {other:?}"),
            }
        }
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
        // a record without v9 is valid
        let mut py = r1.clone();
        py.get_mut("cells").unwrap().remove("9");
        check_record(&py, Some(&r0)).unwrap();
    }
}
