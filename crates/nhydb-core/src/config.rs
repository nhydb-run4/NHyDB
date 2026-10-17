//! Zentrale Betriebskonfiguration `nhydb_config.toml`.
//!
//! Suchreihenfolge (erste gefundene Quelle gilt):
//! 1. expliziter Pfad (API-Parameter `config=`),
//! 2. Umgebungsvariable `NHYDB_CONFIG`,
//! 3. `nhydb_config.toml` im Verzeichnis der Datenbank,
//! 4. eingebaute Standardwerte (= Python-v0.27).
//!
//! In der Datenbank persistierte Einstellungen (`DB_CONFIG`-Block,
//! `realm_auto_limit` eines MATH-Layouts) werden beim Öffnen nie durch die
//! Konfiguration überschrieben; die Werte wirken nur beim Anlegen neuer
//! Datenbanken bzw. Objekte. `REALM_SPAN`, `NL` und `NR` sind feste
//! Systemvorgaben und nicht konfigurierbar. Geheimschlüssel gehören nie in
//! diese Datei.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::platform::{Durability, LockConfig};

pub const CONFIG_FILE_NAME: &str = "nhydb_config.toml";
pub const CONFIG_ENV: &str = "NHYDB_CONFIG";

pub const DEFAULT_DB_MAX_SIZE_BYTES: u64 = 50 * 1024 * 1024 * 1024;
pub const DEFAULT_REALM_AUTO_LIMIT: u64 = 4096;
pub const DEFAULT_MATH_MAX_INTERMEDIATE_BITS: u64 = 1 << 20;
pub const DEFAULT_BLOCK_CACHE_BYTES: usize = 64 << 20;
pub const DEFAULT_VINDEX_MAX_MARKS: usize = 4_000_000;
pub const DEFAULT_VINDEX_MAX_INDEXES: usize = 4096;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Startwert der DB-Größengrenze für neue Datenbanken.
    pub db_max_size_bytes: u64,
    pub durability: Durability,
    /// Startwert für neue MATH-Layouts.
    pub realm_auto_limit: u64,
    /// Schutzgrenze für Zwischenergebnisse der MATH-Ausdruckssprache.
    pub math_max_intermediate_bits: u64,
    pub lock: LockConfig,
    /// Optionale Pfadvorlagen (`{db}` = Dateiname der Datenbank).
    pub layout_path: Option<String>,
    pub key_path: Option<String>,
    pub block_cache_bytes: usize,
    pub vindex_max_marks: usize,
    pub vindex_max_indexes: usize,
    /// Herkunft (None = eingebaute Standardwerte).
    pub source: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            db_max_size_bytes: DEFAULT_DB_MAX_SIZE_BYTES,
            durability: Durability::Fsync,
            realm_auto_limit: DEFAULT_REALM_AUTO_LIMIT,
            math_max_intermediate_bits: DEFAULT_MATH_MAX_INTERMEDIATE_BITS,
            lock: LockConfig::default(),
            layout_path: None,
            key_path: None,
            block_cache_bytes: DEFAULT_BLOCK_CACHE_BYTES,
            vindex_max_marks: DEFAULT_VINDEX_MAX_MARKS,
            vindex_max_indexes: DEFAULT_VINDEX_MAX_INDEXES,
            source: None,
        }
    }
}

const FIXED_KEYS: [&str; 3] = ["realm_span", "nl", "nr"];

fn err(msg: impl Into<String>) -> Error {
    Error::Value(format!("nhydb_config.toml: {}", msg.into()))
}

fn as_pos_u64(v: &toml::Value, key: &str) -> Result<u64> {
    match v {
        toml::Value::Integer(i) if *i > 0 => Ok(*i as u64),
        _ => Err(err(format!("'{key}' muss eine positive Ganzzahl sein"))),
    }
}

fn as_u64(v: &toml::Value, key: &str) -> Result<u64> {
    match v {
        toml::Value::Integer(i) if *i >= 0 => Ok(*i as u64),
        _ => Err(err(format!("'{key}' muss eine nichtnegative Ganzzahl sein"))),
    }
}

fn as_bool(v: &toml::Value, key: &str) -> Result<bool> {
    v.as_bool().ok_or_else(|| err(format!("'{key}' muss true oder false sein")))
}

fn as_string(v: &toml::Value, key: &str) -> Result<String> {
    v.as_str().map(str::to_string).ok_or_else(|| err(format!("'{key}' muss eine Zeichenkette sein")))
}

impl Config {
    /// Parst den Inhalt einer TOML-Datei. Unbekannte Schlüssel sind Fehler.
    pub fn from_toml_str(text: &str) -> Result<Config> {
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| err(e.to_string()))?;
        let mut c = Config::default();
        for (section, val) in &table {
            if FIXED_KEYS.contains(&section.to_ascii_lowercase().as_str()) {
                return Err(fixed_key_error(section));
            }
            let sub = val.as_table().ok_or_else(|| err(format!("'{section}' muss ein Abschnitt [{section}] sein")))?;
            for (key, v) in sub {
                if FIXED_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                    return Err(fixed_key_error(key));
                }
                let full = format!("{section}.{key}");
                match (section.as_str(), key.as_str()) {
                    ("database", "db_max_size_bytes") => c.db_max_size_bytes = as_pos_u64(v, &full)?,
                    ("database", "durability") => c.durability = Durability::parse(&as_string(v, &full)?)?,
                    ("math", "realm_auto_limit") => c.realm_auto_limit = as_pos_u64(v, &full)?,
                    ("math", "math_max_intermediate_bits") => c.math_max_intermediate_bits = as_pos_u64(v, &full)?,
                    ("lock", "enabled") => c.lock.enabled = as_bool(v, &full)?,
                    ("lock", "timeout_ms") => c.lock.timeout_ms = as_u64(v, &full)?,
                    ("paths", "layout_path") => c.layout_path = Some(as_string(v, &full)?),
                    ("paths", "key_path") => c.key_path = Some(as_string(v, &full)?),
                    ("cache", "block_cache_bytes") => c.block_cache_bytes = as_u64(v, &full)? as usize,
                    ("cache", "vindex_max_marks") => c.vindex_max_marks = as_u64(v, &full)? as usize,
                    ("cache", "vindex_max_indexes") => c.vindex_max_indexes = as_u64(v, &full)? as usize,
                    _ => return Err(err(format!("unbekannter Schluessel '{full}'"))),
                }
            }
        }
        Ok(c)
    }

    pub fn from_file(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Value(format!("nhydb_config.toml '{}' nicht lesbar: {e}", path.display())))?;
        let mut c = Config::from_toml_str(&text)
            .map_err(|e| Error::Value(format!("{e} (Datei {})", path.display())))?;
        c.source = Some(path.to_path_buf());
        Ok(c)
    }

    /// Lädt die Konfiguration nach der dokumentierten Suchreihenfolge.
    pub fn load(explicit: Option<&Path>, db_path: &Path) -> Result<Config> {
        if let Some(p) = explicit {
            return Config::from_file(p);
        }
        if let Some(p) = std::env::var_os(CONFIG_ENV).filter(|p| !p.is_empty()) {
            return Config::from_file(Path::new(&p));
        }
        let beside = crate::platform::parent_dir(db_path).join(CONFIG_FILE_NAME);
        if beside.is_file() {
            return Config::from_file(&beside);
        }
        Ok(Config::default())
    }

    /// Setzt eine Pfadvorlage für eine DB ein (`{db}` → Dateiname).
    /// Relative Pfade beziehen sich auf das Verzeichnis der Datenbank.
    pub fn resolve_template(template: &str, db_path: &Path) -> PathBuf {
        let name = db_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let p = PathBuf::from(template.replace("{db}", &name));
        if p.is_absolute() { p } else { crate::platform::parent_dir(db_path).join(p) }
    }
}

fn fixed_key_error(key: &str) -> Error {
    err(format!(
        "'{key}' ist eine feste mathematische Systemvorgabe (REALM_SPAN = 10**100, NL = 0, NR = 1) und nicht konfigurierbar"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python_reference() {
        let c = Config::default();
        assert_eq!(c.db_max_size_bytes, 53_687_091_200);
        assert_eq!(c.realm_auto_limit, 4096);
        assert_eq!(c.math_max_intermediate_bits, 1_048_576);
        assert_eq!(c.durability, Durability::Fsync);
        assert!(c.lock.enabled);
        assert_eq!(Config::from_toml_str("").unwrap(), c);
    }

    #[test]
    fn parse_all_keys() {
        let c = Config::from_toml_str(
            r#"
            [database]
            db_max_size_bytes = 1000000
            durability = "full"
            [math]
            realm_auto_limit = 12
            math_max_intermediate_bits = 4096
            [lock]
            enabled = false
            timeout_ms = 250
            [paths]
            layout_path = "plans/{db}.layout.enc"
            key_path = "/secure/{db}.layout.key"
            [cache]
            block_cache_bytes = 1024
            vindex_max_marks = 10
            vindex_max_indexes = 2
            "#,
        )
        .unwrap();
        assert_eq!(c.db_max_size_bytes, 1_000_000);
        assert_eq!(c.durability, Durability::Full);
        assert_eq!(c.realm_auto_limit, 12);
        assert_eq!(c.math_max_intermediate_bits, 4096);
        assert_eq!(c.lock, LockConfig { enabled: false, timeout_ms: 250 });
        assert_eq!(c.block_cache_bytes, 1024);
        let db = Path::new("/data/x.nhydb");
        assert_eq!(Config::resolve_template(c.layout_path.as_deref().unwrap(), db), PathBuf::from("/data/plans/x.nhydb.layout.enc"));
        assert_eq!(Config::resolve_template(c.key_path.as_deref().unwrap(), db), PathBuf::from("/secure/x.nhydb.layout.key"));
    }

    #[test]
    fn rejects_unknown_fixed_and_invalid() {
        assert!(Config::from_toml_str("[database]\ndb_max_size = 1").is_err());
        assert!(Config::from_toml_str("[math]\nrealm_span = 10").unwrap_err().to_string().contains("feste mathematische"));
        assert!(Config::from_toml_str("NL = 1").is_err());
        assert!(Config::from_toml_str("[database]\ndurability = \"weak\"").is_err());
        assert!(Config::from_toml_str("[math]\nrealm_auto_limit = 0").is_err());
        assert!(Config::from_toml_str("[database\n").is_err());
    }

    #[test]
    fn load_search_order() {
        let d = std::env::temp_dir().join(format!("nhydb_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("a.nhydb");
        assert_eq!(Config::load(None, &db).unwrap().source, None);
        std::fs::write(d.join(CONFIG_FILE_NAME), "[math]\nrealm_auto_limit = 7\n").unwrap();
        let c = Config::load(None, &db).unwrap();
        assert_eq!(c.realm_auto_limit, 7);
        let other = d.join("other.toml");
        std::fs::write(&other, "[math]\nrealm_auto_limit = 9\n").unwrap();
        assert_eq!(Config::load(Some(&other), &db).unwrap().realm_auto_limit, 9);
        let _ = std::fs::remove_dir_all(&d);
    }
}
