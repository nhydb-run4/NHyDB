//! Central runtime configuration `nhydb_config.toml`.
//!
//! Search order (the first source found wins):
//! 1. explicit path (API parameter `config=`),
//! 2. environment variable `NHYDB_CONFIG`,
//! 3. `nhydb_config.toml` in the database's directory,
//! 4. built-in defaults.
//!
//! Settings persisted in the database (`DB_CONFIG` block, `realm_auto_limit`
//! of a MATH layout) are never overridden by the configuration on open; the
//! values only take effect when new databases or objects are created.
//! `REALM_SPAN`, `NL` and `NR` are fixed system constants and not
//! configurable. Secret keys never belong in this file.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::platform::{Durability, LockConfig};

pub const CONFIG_FILE_NAME: &str = "nhydb_config.toml";
pub const CONFIG_ENV: &str = "NHYDB_CONFIG";

pub const DEFAULT_DB_MAX_SIZE_BYTES: u64 = 50 * 1024 * 1024 * 1024;
/// Smallest accepted DB size limit for new databases (superblocks, R0,u1 and
/// DB_CONFIG need about 9 KiB; smaller limits make the database unusable).
pub const MIN_DB_MAX_SIZE_BYTES: u64 = 64 * 1024;

/// Checks the initial DB size limit of a new database.
pub fn check_db_max_size(n: u64) -> Result<()> {
    if n < MIN_DB_MAX_SIZE_BYTES {
        return Err(Error::Value(format!(
            "db_max_size_bytes must be at least {MIN_DB_MAX_SIZE_BYTES} bytes (got {n})"
        )));
    }
    Ok(())
}
pub const DEFAULT_REALM_AUTO_LIMIT: u64 = 4096;
pub const DEFAULT_MATH_MAX_INTERMEDIATE_BITS: u64 = 1 << 20;
pub const DEFAULT_BLOCK_CACHE_BYTES: usize = 64 << 20;
pub const DEFAULT_VINDEX_MAX_MARKS: usize = 4_000_000;
pub const DEFAULT_VINDEX_MAX_INDEXES: usize = 4096;
/// Memory of the index build before it spills sorted runs (bytes).
pub const DEFAULT_INDEX_BUILD_MEMORY_BYTES: usize = 512 << 20;
/// Decoded SECONDARY index pages held in memory (bytes).
pub const DEFAULT_INDEX_PAGE_CACHE_BYTES: usize = 32 << 20;
/// Decoded index payloads (postings, FULLTEXT positions and document
/// lengths) per handle (bytes).
pub const DEFAULT_POSTING_CACHE_BYTES: usize = 256 << 20;
/// Sparse decode caches (group directories, record states, merged groups)
/// per handle (bytes).
pub const DEFAULT_SPARSE_CACHE_BYTES: usize = 128 << 20;
/// Sort keys of a streaming `ORDER BY` held in memory before spilling.
/// 0.28.2/scan: default of `[query] overlay_view_max_bytes` (256 MiB).
pub const DEFAULT_OVERLAY_VIEW_MAX_BYTES: u64 = 256 << 20;
/// RUN6: default of `[query] max_result_bytes` (4 GiB).
pub const DEFAULT_MAX_RESULT_BYTES: u64 = 4 << 30;
pub const DEFAULT_SORT_MEMORY_BYTES: usize = 256 << 20;
/// RUN9 V1: largest DATA block a dense write creates (`[write]
/// max_block_bytes`); larger calls are split into consecutive blocks. Equals
/// the smallest largest-cacheable block of the block cache, so every new
/// block can be cached and its first access never loads more.
pub const DEFAULT_MAX_BLOCK_BYTES: u64 = 8 << 20;
/// Smallest allowed `[write] max_block_bytes` other than 0.
pub const MIN_MAX_BLOCK_BYTES: u64 = 64 << 10;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Initial DB size limit for new databases.
    pub db_max_size_bytes: u64,
    pub durability: Durability,
    /// Initial value for new MATH layouts.
    pub realm_auto_limit: u64,
    /// Safety limit for intermediate results of the MATH expression language.
    pub math_max_intermediate_bits: u64,
    pub lock: LockConfig,
    /// Optional path templates (`{db}` = file name of the database).
    pub layout_path: Option<String>,
    pub key_path: Option<String>,
    pub block_cache_bytes: usize,
    pub vindex_max_marks: usize,
    pub vindex_max_indexes: usize,
    /// Budget of the decoded SECONDARY index pages (`secindex.rs`).
    pub index_page_cache_bytes: usize,
    /// Budget of the decoded index payloads (`[cache] posting_cache_bytes`).
    pub posting_cache_bytes: usize,
    /// Budget of the sparse decode caches (`[cache] sparse_cache_bytes`).
    pub sparse_cache_bytes: usize,
    /// Cache budget of the whole process, shared by all handles
    /// (`[cache] total_bytes`, `db/mempot.rs`); set by the first handle.
    pub cache_total_bytes: u64,
    /// Memory budget of `build_indexes` (`db/idxspill.rs`).
    pub index_build_memory_bytes: usize,
    /// AP9: automatic index maintenance (`[index]`, `db/index/ixmaint.rs`).
    pub index_maint: IndexMaintConfig,
    /// Block directory (0.17): a commit writes a directory run once this
    /// many data/DELETE blocks or payload bytes were added since the last
    /// one; decoded directory pages held in memory (entries).
    pub directory_flush_blocks: u64,
    pub directory_flush_bytes: u64,
    pub directory_cache_entries: usize,
    /// R4-A6: optional upper bound of the estimated result rows of a
    /// query (`[query] max_result_rows`); 0 = no bound (default). Larger
    /// queries are rejected with `QueryLimitError` before they run.
    pub query_max_result_rows: u64,
    /// RUN6: bound of the estimated bytes of the rows a collecting call
    /// (`sql()`, `execute()`, `search()`, `sql_numpy`) holds in memory
    /// (`[query] max_result_bytes`, default 4 GiB, 0 = none); beyond it the
    /// call ends with `QueryLimitError` while it collects.
    pub query_max_result_bytes: u64,
    /// 0.28.2/scan: the metadata fast paths (record universe, COUNT, NOT,
    /// field scan, ORDER-BY window) also serve a realm/lane with overwritten
    /// DENSE segments or DELETE entries through the exact winner view
    /// (`db/winners.rs`) instead of the cell cursor (`[query] overlay_view`,
    /// default on; off = the general path as before).
    pub query_overlay_view: bool,
    /// 0.28.2/scan: memory bound of one winner view (segment list, runs,
    /// DELETE list; `[query] overlay_view_max_bytes`, default 256 MiB).
    /// A larger view is not built: the query takes the general path.
    pub query_overlay_view_max_bytes: u64,
    /// R4-C: directory of result files (`[results] temp_dir`); `None` =
    /// `<system temp>/nhydb-results`. Independent of the database location.
    pub results_temp_dir: Option<String>,
    /// R4-C: byte budget of the managed files in the result directory per
    /// process (`[results] budget_bytes`); 0 = none (default).
    pub results_budget_bytes: u64,
    /// R4-C: zlib-compressed result chunks by default (`[results] compress`).
    pub results_compress: bool,
    /// R4-C: sort keys held in memory by a streaming `ORDER BY` without
    /// LIMIT before sorted runs go to the result directory
    /// (`[results] sort_memory_bytes`).
    pub results_sort_memory_bytes: usize,
    /// RUN9 V1: largest DATA block of a dense write (`[write]
    /// max_block_bytes`, default 8 MiB, 0 = one block per call as before
    /// 0.28.2/run9); a single record larger than it gets a block of its own.
    pub write_max_block_bytes: u64,
    /// RUN¹⁰: central RAM limiter of the process (`[limiter]`); the first
    /// handle opened in the process defines it (`limiter/mod.rs`).
    pub limiter: crate::limiter::LimiterConfig,
    /// Origin (None = built-in defaults).
    pub source: Option<PathBuf>,
}

/// AP9: automatic index maintenance (`[index]`). Shares relate to the
/// records of the realm lane (`N`, block directory).
#[derive(Clone, Debug, PartialEq)]
pub struct IndexMaintConfig {
    /// `true` (`maintenance = "auto"`, default): a writer renews stale
    /// indexes itself after a commit when it is economical and the RAM
    /// limiter allows it; `false` (`"manual"`): only `build_indexes()`.
    pub auto: bool,
    /// A rebuild becomes economical once this share of `N` changed since
    /// the index was built (default 2 %).
    pub rebuild_ratio: f64,
    /// Upper bound of the in-memory overlay of changed records per handle
    /// and index group; beyond it queries scan as before (default 256 MiB).
    pub overlay_max_bytes: u64,
    /// A commit that alone brings at least this share of `N` counts as a
    /// bulk commit: the rebuild waits for the next smaller commit
    /// (default 0.2 %).
    pub bulk_commit_ratio: f64,
    /// Changes beyond this share of `N` force the rebuild even during bulk
    /// commits, other jobs or a yellow limiter zone (never in red; the
    /// limiter still governs the build) (default 25 %). The rebuild is also
    /// forced once the overlay would reach 90 % of `overlay_max_bytes`
    /// (queries would scan otherwise).
    pub crisis_ratio: f64,
    /// Smallest number of changed records that makes a rebuild worthwhile
    /// (small realms would otherwise rebuild after a few writes; default 1024).
    pub rebuild_min_changes: u64,
}

pub const DEFAULT_REBUILD_RATIO: f64 = 0.02;
pub const DEFAULT_OVERLAY_MAX_BYTES: u64 = 256 << 20;
pub const DEFAULT_BULK_COMMIT_RATIO: f64 = 0.002;
pub const DEFAULT_CRISIS_RATIO: f64 = 0.25;
pub const DEFAULT_REBUILD_MIN_CHANGES: u64 = 1024;

impl Default for IndexMaintConfig {
    fn default() -> Self {
        IndexMaintConfig {
            auto: true,
            rebuild_ratio: DEFAULT_REBUILD_RATIO,
            overlay_max_bytes: DEFAULT_OVERLAY_MAX_BYTES,
            bulk_commit_ratio: DEFAULT_BULK_COMMIT_RATIO,
            crisis_ratio: DEFAULT_CRISIS_RATIO,
            rebuild_min_changes: DEFAULT_REBUILD_MIN_CHANGES,
        }
    }
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
            index_page_cache_bytes: DEFAULT_INDEX_PAGE_CACHE_BYTES,
            posting_cache_bytes: DEFAULT_POSTING_CACHE_BYTES,
            sparse_cache_bytes: DEFAULT_SPARSE_CACHE_BYTES,
            cache_total_bytes: crate::db::mempot::DEFAULT_TOTAL_BYTES,
            index_build_memory_bytes: DEFAULT_INDEX_BUILD_MEMORY_BYTES,
            index_maint: IndexMaintConfig::default(),
            directory_flush_blocks: crate::db::directory::DEFAULT_FLUSH_BLOCKS,
            directory_flush_bytes: crate::db::directory::DEFAULT_FLUSH_BYTES,
            directory_cache_entries: crate::db::directory::DEFAULT_CACHE_ENTRIES,
            query_max_result_rows: 0,
            query_max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            query_overlay_view: true,
            query_overlay_view_max_bytes: DEFAULT_OVERLAY_VIEW_MAX_BYTES,
            results_temp_dir: None,
            results_budget_bytes: 0,
            results_compress: false,
            results_sort_memory_bytes: DEFAULT_SORT_MEMORY_BYTES,
            write_max_block_bytes: DEFAULT_MAX_BLOCK_BYTES,
            limiter: crate::limiter::LimiterConfig::default(),
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
        _ => Err(err(format!("'{key}' must be a positive integer"))),
    }
}

fn as_u64(v: &toml::Value, key: &str) -> Result<u64> {
    match v {
        toml::Value::Integer(i) if *i >= 0 => Ok(*i as u64),
        _ => Err(err(format!("'{key}' must be a non-negative integer"))),
    }
}

fn as_u32(v: &toml::Value, key: &str) -> Result<u32> {
    u32::try_from(as_u64(v, key)?).map_err(|_| err(format!("'{key}' is too large")))
}

fn as_usize(v: &toml::Value, key: &str) -> Result<usize> {
    usize::try_from(as_u64(v, key)?).map_err(|_| err(format!("'{key}' is too large for this platform")))
}

fn non_empty(s: String, key: &str) -> Result<String> {
    if s.trim().is_empty() {
        return Err(err(format!("'{key}' must not be empty")));
    }
    Ok(s)
}

/// A share in (0, 1] (float or integer 1).
fn as_ratio(v: &toml::Value, key: &str) -> Result<f64> {
    let x = match v {
        toml::Value::Float(f) => *f,
        toml::Value::Integer(i) => *i as f64,
        _ => f64::NAN,
    };
    if x.is_finite() && x > 0.0 && x <= 1.0 {
        Ok(x)
    } else {
        Err(err(format!("'{key}' must be a number in (0, 1]")))
    }
}

fn as_bool(v: &toml::Value, key: &str) -> Result<bool> {
    v.as_bool().ok_or_else(|| err(format!("'{key}' must be true or false")))
}

fn as_string(v: &toml::Value, key: &str) -> Result<String> {
    v.as_str().map(str::to_string).ok_or_else(|| err(format!("'{key}' must be a string")))
}

impl Config {
    /// Parses the contents of a TOML file. Unknown keys are errors.
    pub fn from_toml_str(text: &str) -> Result<Config> {
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| err(e.to_string()))?;
        let mut c = Config::default();
        for (section, val) in &table {
            if FIXED_KEYS.contains(&section.to_ascii_lowercase().as_str()) {
                return Err(fixed_key_error(section));
            }
            let sub = val.as_table().ok_or_else(|| err(format!("'{section}' must be a section [{section}]")))?;
            for (key, v) in sub {
                if FIXED_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                    return Err(fixed_key_error(key));
                }
                let full = format!("{section}.{key}");
                match (section.as_str(), key.as_str()) {
                    ("database", "db_max_size_bytes") => {
                        c.db_max_size_bytes = as_pos_u64(v, &full)?;
                        check_db_max_size(c.db_max_size_bytes).map_err(|e| err(e.to_string()))?;
                    }
                    ("database", "durability") => c.durability = Durability::parse(&as_string(v, &full)?)?,
                    ("math", "realm_auto_limit") => c.realm_auto_limit = as_pos_u64(v, &full)?,
                    ("math", "math_max_intermediate_bits") => c.math_max_intermediate_bits = as_pos_u64(v, &full)?,
                    ("lock", "enabled") => c.lock.enabled = as_bool(v, &full)?,
                    ("lock", "timeout_ms") => c.lock.timeout_ms = as_u64(v, &full)?,
                    ("paths", "layout_path") => c.layout_path = Some(non_empty(as_string(v, &full)?, &full)?),
                    ("paths", "key_path") => c.key_path = Some(non_empty(as_string(v, &full)?, &full)?),
                    ("cache", "block_cache_bytes") => c.block_cache_bytes = as_u64(v, &full)? as usize,
                    ("cache", "vindex_max_marks") => c.vindex_max_marks = as_u64(v, &full)? as usize,
                    ("cache", "vindex_max_indexes") => c.vindex_max_indexes = as_u64(v, &full)? as usize,
                    ("cache", "index_page_cache_bytes") => c.index_page_cache_bytes = as_u64(v, &full)? as usize,
                    ("cache", "posting_cache_bytes") => c.posting_cache_bytes = as_usize(v, &full)?,
                    ("cache", "sparse_cache_bytes") => c.sparse_cache_bytes = as_usize(v, &full)?,
                    ("cache", "total_bytes") => c.cache_total_bytes = as_u64(v, &full)?,
                    ("index", "build_memory_bytes") => c.index_build_memory_bytes = as_pos_u64(v, &full)? as usize,
                    ("index", "maintenance") => c.index_maint.auto = match as_string(v, &full)?.to_ascii_lowercase().as_str() {
                        "auto" => true,
                        "manual" => false,
                        _ => return Err(err(format!("'{full}' must be \"auto\" or \"manual\""))),
                    },
                    ("index", "rebuild_ratio") => c.index_maint.rebuild_ratio = as_ratio(v, &full)?,
                    ("index", "overlay_max_bytes") => c.index_maint.overlay_max_bytes = as_pos_u64(v, &full)?,
                    ("index", "bulk_commit_ratio") => c.index_maint.bulk_commit_ratio = as_ratio(v, &full)?,
                    ("index", "crisis_ratio") => c.index_maint.crisis_ratio = as_ratio(v, &full)?,
                    ("index", "rebuild_min_changes") => c.index_maint.rebuild_min_changes = as_pos_u64(v, &full)?,
                    ("directory", "flush_blocks") => c.directory_flush_blocks = as_pos_u64(v, &full)?,
                    ("directory", "flush_bytes") => c.directory_flush_bytes = as_pos_u64(v, &full)?,
                    ("directory", "cache_entries") => c.directory_cache_entries = as_pos_u64(v, &full)? as usize,
                    ("query", "max_result_rows") => c.query_max_result_rows = as_u64(v, &full)?,
                    ("query", "max_result_bytes") => c.query_max_result_bytes = as_u64(v, &full)?,
                    ("query", "overlay_view") => c.query_overlay_view = as_bool(v, &full)?,
                    ("query", "overlay_view_max_bytes") => c.query_overlay_view_max_bytes = as_pos_u64(v, &full)?,
                    ("results", "temp_dir") => c.results_temp_dir = Some(non_empty(as_string(v, &full)?, &full)?),
                    ("results", "budget_bytes") => c.results_budget_bytes = as_u64(v, &full)?,
                    ("results", "compress") => c.results_compress = as_bool(v, &full)?,
                    ("results", "sort_memory_bytes") => c.results_sort_memory_bytes = as_pos_u64(v, &full)? as usize,
                    ("write", "max_block_bytes") => {
                        c.write_max_block_bytes = as_u64(v, &full)?;
                        if c.write_max_block_bytes != 0 && c.write_max_block_bytes < MIN_MAX_BLOCK_BYTES {
                            return Err(err(format!("'{full}' must be 0 or at least {MIN_MAX_BLOCK_BYTES}")));
                        }
                    }
                    ("limiter", "enabled") => c.limiter.enabled = as_bool(v, &full)?,
                    ("limiter", "reserve_percent") => c.limiter.reserve_percent = as_u32(v, &full)?,
                    ("limiter", "warn_percent") => c.limiter.warn_percent = as_u32(v, &full)?,
                    ("limiter", "tick_ms") => c.limiter.tick_ms = as_u32(v, &full)?,
                    ("limiter", "notify_wait_s") => c.limiter.notify_wait_s = as_u64(v, &full)?,
                    ("limiter", "wait_timeout_s") => c.limiter.wait_timeout_s = as_u64(v, &full)?,
                    ("limiter", "ram_bytes") => c.limiter.ram_bytes = as_u64(v, &full)?,
                    ("limiter", "dry_run") => c.limiter.dry_run = as_bool(v, &full)?,
                    ("limiter", "probe_bytes") => c.limiter.probe_bytes = as_u64(v, &full)?,
                    ("limiter", "cancel_over_limit") => c.limiter.cancel_over_limit = as_bool(v, &full)?,
                    ("limiter_log", "enabled") => c.limiter.log.enabled = as_bool(v, &full)?,
                    ("limiter_log", "path") => c.limiter.log.path = Some(non_empty(as_string(v, &full)?, &full)?),
                    ("limiter_log", "interval_min") => c.limiter.log.interval_min = as_u64(v, &full)?,
                    ("limiter_log", "max_bytes") => c.limiter.log.max_bytes = as_u64(v, &full)?,
                    ("limiter_log", "buffer_records") => c.limiter.log.buffer_records = as_u64(v, &full)?,
                    _ => return Err(err(format!("unknown key '{full}'"))),
                }
            }
        }
        c.limiter.check().map_err(err)?;
        if c.index_maint.crisis_ratio < c.index_maint.rebuild_ratio {
            return Err(err("'index.crisis_ratio' must not be smaller than 'index.rebuild_ratio'"));
        }
        Ok(c)
    }

    pub fn from_file(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Value(format!("nhydb_config.toml '{}' not readable: {e}", path.display())))?;
        let mut c = Config::from_toml_str(&text)
            .map_err(|e| Error::Value(format!("{e} (file {})", path.display())))?;
        c.source = Some(path.to_path_buf());
        Ok(c)
    }

    /// Loads the configuration following the documented search order.
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

    /// Expands a path template for a DB (`{db}` → file name,
    /// `{db_path_hash}` → first 16 hex digits of the SHA-256 of the absolute
    /// DB path, unique per database location – use it when several
    /// databases with the same file name share one key directory).
    /// Relative paths are resolved against the database's directory.
    pub fn resolve_template(template: &str, db_path: &Path) -> PathBuf {
        let name = db_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut t = template.replace("{db}", &name);
        if t.contains("{db_path_hash}") {
            let abs = std::path::absolute(db_path).unwrap_or_else(|_| db_path.to_path_buf());
            let h = crate::crypto::sha256_hex(abs.to_string_lossy().as_bytes());
            t = t.replace("{db_path_hash}", &h[..16]);
        }
        let p = PathBuf::from(t);
        if p.is_absolute() { p } else { crate::platform::parent_dir(db_path).join(p) }
    }
}

fn fixed_key_error(key: &str) -> Error {
    err(format!(
        "'{key}' is a fixed mathematical system constant (REALM_SPAN = 10**100, NL = 0, NR = 1) and not configurable"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values() {
        let c = Config::default();
        assert_eq!(c.db_max_size_bytes, 53_687_091_200);
        assert_eq!(c.realm_auto_limit, 4096);
        assert_eq!(c.math_max_intermediate_bits, 1_048_576);
        assert_eq!(c.durability, Durability::Fsync);
        assert!(c.lock.enabled);
        // RUN6: sql()/execute()/search()/sql_numpy() bounded by default
        assert_eq!(c.query_max_result_bytes, 4 << 30);
        assert_eq!(Config::from_toml_str("[query]\nmax_result_bytes = 0\n").unwrap().query_max_result_bytes, 0);
        assert!(c.query_overlay_view);
        assert_eq!(c.query_overlay_view_max_bytes, 256 << 20);
        let o = Config::from_toml_str("[query]\noverlay_view = false\noverlay_view_max_bytes = 4096\n").unwrap();
        assert!(!o.query_overlay_view);
        assert_eq!(o.query_overlay_view_max_bytes, 4096);
        assert!(Config::from_toml_str("[query]\noverlay_view_max_bytes = 0\n").is_err());
        assert!(Config::from_toml_str("[query]\noverlay_view = 1\n").is_err());
        assert_eq!(c.write_max_block_bytes, 8 << 20);
        assert_eq!(Config::from_toml_str("[write]\nmax_block_bytes = 0\n").unwrap().write_max_block_bytes, 0);
        assert_eq!(Config::from_toml_str("[write]\nmax_block_bytes = 65536\n").unwrap().write_max_block_bytes, 65536);
        assert!(Config::from_toml_str("[write]\nmax_block_bytes = 65535\n").is_err());
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
            [directory]
            flush_blocks = 7
            flush_bytes = 4096
            cache_entries = 99
            "#,
        )
        .unwrap();
        assert_eq!((c.directory_flush_blocks, c.directory_flush_bytes, c.directory_cache_entries), (7, 4096, 99));
        assert_eq!(c.db_max_size_bytes, 1_000_000);
        assert_eq!(c.durability, Durability::Full);
        assert_eq!(c.realm_auto_limit, 12);
        assert_eq!(c.math_max_intermediate_bits, 4096);
        assert_eq!(c.lock, LockConfig { enabled: false, timeout_ms: 250 });
        assert_eq!(c.block_cache_bytes, 1024);
        let db = Path::new("/data/x.nhydb");
        assert_eq!(Config::resolve_template(c.layout_path.as_deref().unwrap(), db), PathBuf::from("/data/plans/x.nhydb.layout.enc"));
        assert_eq!(Config::resolve_template(c.key_path.as_deref().unwrap(), db), PathBuf::from("/secure/x.nhydb.layout.key"));
        // F005: {db_path_hash} separates equally named databases in different directories
        let a = Config::resolve_template("/secure/{db}.{db_path_hash}.key", Path::new("/a/kunden.nhydb"));
        let b = Config::resolve_template("/secure/{db}.{db_path_hash}.key", Path::new("/b/kunden.nhydb"));
        assert_ne!(a, b);
        assert!(a.to_string_lossy().starts_with("/secure/kunden.nhydb."));
    }

    #[test]
    fn limiter_section() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.limiter, crate::limiter::LimiterConfig::default());
        let c = Config::from_toml_str(
            "[limiter]\nenabled = false\nreserve_percent = 20\nwarn_percent = 3\ntick_ms = 10\nnotify_wait_s = 60\nwait_timeout_s = 600\n",
        )
        .unwrap();
        let l = &c.limiter;
        assert_eq!((l.enabled, l.reserve_percent, l.warn_percent, l.tick_ms, l.notify_wait_s, l.wait_timeout_s), (false, 20, 3, 10, 60, 600));
        let c = Config::from_toml_str("[limiter]\nram_bytes = 8589934592\ndry_run = true\n").unwrap();
        assert_eq!((c.limiter.ram_bytes, c.limiter.dry_run), (8 << 30, true));
        assert!(Config::from_toml_str("[limiter]\nram_bytes = 1000\n").is_err());
        for bad in ["reserve_percent = 0", "reserve_percent = 95", "warn_percent = 0", "tick_ms = 1", "enabled = 1", "reserve_percent = -1", "x = 1"] {
            assert!(Config::from_toml_str(&format!("[limiter]\n{bad}\n")).is_err(), "{bad}");
        }
        assert!(Config::from_toml_str("[limiter]\nreserve_percent = 50\nwarn_percent = 50\n").is_err());
    }

    #[test]
    fn rejects_unknown_fixed_and_invalid() {
        assert!(Config::from_toml_str("[database]\ndb_max_size = 1").is_err());
        assert!(Config::from_toml_str("[math]\nrealm_span = 10").unwrap_err().to_string().contains("fixed mathematical"));
        assert!(Config::from_toml_str("NL = 1").is_err());
        assert!(Config::from_toml_str("[database]\ndurability = \"weak\"").is_err());
        assert!(Config::from_toml_str("[math]\nrealm_auto_limit = 0").is_err());
        assert!(Config::from_toml_str("[database\n").is_err());
        // F051: minimum DB size, empty sidecar paths
        assert!(Config::from_toml_str("[database]\ndb_max_size_bytes = 1").unwrap_err().to_string().contains("at least"));
        assert!(Config::from_toml_str("[paths]\nkey_path = \"\"").unwrap_err().to_string().contains("must not be empty"));
        assert!(Config::from_toml_str("[directory]\nflush_blocks = 0").is_err());
        assert!(Config::from_toml_str("[directory]\nunknown = 1").is_err());
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
