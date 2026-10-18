//! The native NHyDB engine (the `NHyDB` class).
//!
//! The implementation is split across several files:
//! `mod.rs` (state, open, commit, low-level I/O), `schema_ops.rs`,
//! `layout.rs`, `sparse.rs`, `dense.rs`, `read.rs`, `index.rs`,
//! `maint.rs`.

pub mod columns;
pub mod cursor;
pub mod dense;
pub mod index;
pub mod interval;
pub mod layout;
pub mod maint;
pub mod materialize;
pub mod math;
pub mod read;
pub mod schema_ops;
pub mod sparse;
pub mod system;
pub mod vindex;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::address::{lane_from_flags, Lane};
use crate::config::Config;
use crate::codec::{FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, get_vu_u64, Nat};
use crate::platform::{self, DbLock, LockConfig};
use crate::plugin::LayoutPlugin;
use crate::schema::{ExtensionDef, IndexDef, LayoutDef, RealmDef, ShapeDef};
use crate::storage::{Storage, SyncMode};
use system::SystemState;
use crate::val::{Val, ValKey};
use crate::vmap;

use interval::IntervalIndex;

/// Key `(realm, lane)`.
pub type RL = (Nat, Lane);

pub fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct DenseSegment {
    pub realm_id: Nat,
    pub lane: Lane,
    pub shape_id: u64,
    pub u_start: Nat,
    pub u_end: Nat,
    pub count: u64,
    pub block_id: u64,
    pub block_offset: u64,
    pub payload_offset: u64,
    pub offsets_offset: u64,
    pub records_offset: u64,
    pub payload_len: u64,
}

#[derive(Clone, Debug)]
pub struct SparseSegment {
    pub realm_id: Nat,
    pub lane: Lane,
    pub u_start: Nat,
    pub u_end: Nat,
    pub group_count: u64,
    pub cell_count: u64,
    pub block_id: u64,
    pub block_offset: u64,
    pub payload_offset: u64,
    pub entries_offset: u64,
    pub payload_len: u64,
}

#[derive(Default)]
pub struct DenseKeyIndex {
    /// Segments in registration order (stable indices).
    pub segs: Vec<DenseSegment>,
    pub iv: IntervalIndex,
}

impl DenseKeyIndex {
    /// Segments sorted by `(u_start, block_id)` (canonical list order).
    pub fn sorted(&self) -> Vec<&DenseSegment> {
        self.iv.ids_by_start().iter().map(|&i| &self.segs[i]).collect()
    }
}

#[derive(Default)]
pub struct SparseKeyIndex {
    /// Segments in block order.
    pub segs: Vec<SparseSegment>,
    pub iv: IntervalIndex,
}

/// State of a sparse cell.
#[derive(Clone, Debug, PartialEq)]
pub enum SVal {
    Tomb,
    V(Val),
}

impl SVal {
    pub fn is_tomb(&self) -> bool {
        matches!(self, SVal::Tomb)
    }
    pub fn val(&self) -> Option<&Val> {
        match self {
            SVal::V(v) => Some(v),
            SVal::Tomb => None,
        }
    }
}

/// Merged sparse state of a u group: v → state.
pub type SparseMap = BTreeMap<Nat, SVal>;

/// Decoded sparse cell with type information.
#[derive(Clone, Debug)]
pub struct SparseCellMeta {
    pub value: SVal,
    pub typ: Option<TypeCode>,
    pub selftyped: bool,
}

/// An entry of the persisted secondary index directory.
#[derive(Clone, Debug)]
pub struct PostingRef {
    pub value: Val,
    pub abs_offset: u64,
    pub compressed_len: u64,
    pub raw_count: u64,
    pub width: u8,
}

#[derive(Default, Clone, Debug)]
pub struct SecondaryDir {
    pub entries: Vec<PostingRef>,
    pub by_key: HashMap<ValKey, usize>,
}

/// Group directory of a sparse block: `(u, absolute offset, length)`.
pub type SparseBlockIndex = Vec<(Nat, u64, u32)>;
/// States `(v, value/tombstone)` of a u group in a sparse block.
pub type SparseStates = Vec<(Nat, SVal)>;

#[derive(Default)]
pub(crate) struct Caches {
    pub sparse_block_index: HashMap<u64, Arc<SparseBlockIndex>>,
    pub sparse_record: HashMap<(u64, Nat), Arc<SparseStates>>,
    pub sparse_merged: HashMap<(Nat, Lane, Nat), Arc<SparseMap>>,
    /// v indexes per `(realm, lane, u)` (see `vindex.rs`)
    pub sparse_uindex: HashMap<(Nat, Lane, Nat), Arc<vindex::UIndex>>,
    /// Number of jump marks in `sparse_uindex`
    pub uindex_ck: usize,
    pub field_meta: HashMap<(Nat, Nat, Nat), Option<FieldDef>>,
    pub blocks: HashMap<u64, Arc<Vec<u8>>>,
    pub block_bytes: usize,
    pub postings: HashMap<(u64, Lane, ValKey), Arc<Vec<Nat>>>,
    /// Lazily loaded SECONDARY directories and sorted keys.
    pub sec_dirs: HashMap<(u64, Lane), Arc<SecondaryDir>>,
    pub sec_sorted: HashMap<(u64, Lane), Arc<Vec<Val>>>,
    /// FULLTEXT payloads (positions per term, corpus).
    pub ft_postings: HashMap<(u64, Lane, String), Arc<Vec<crate::textindex::FtPosting>>>,
    pub ft_corpus: HashMap<(u64, Lane), Arc<crate::textindex::FtCorpus>>,
}

const BLOCK_CACHE_BUDGET: usize = 64 << 20;
const BLOCK_CACHE_MAX_ITEM: usize = 8 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynMode {
    Off,
    Once,
    Auto,
}

impl DynMode {
    pub fn parse(s: &str) -> Result<DynMode> {
        match s.to_lowercase().as_str() {
            "off" => Ok(DynMode::Off),
            "once" => Ok(DynMode::Once),
            "auto" => Ok(DynMode::Auto),
            _ => Err(Error::Value("dyn-address mode must be off, once or auto".into())),
        }
    }
}

/// Open options.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    pub strict: bool,
    /// Explicit durability mode (takes precedence over the configuration).
    pub sync_mode: Option<SyncMode>,
    /// Explicit paths of the sidecar files (take precedence over the configuration).
    pub layout_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    /// Already loaded configuration; otherwise the search order applies
    /// (`config_path`, `NHYDB_CONFIG`, file next to the DB, defaults).
    pub config: Option<Config>,
    pub config_path: Option<PathBuf>,
    /// Locking behaviour (takes precedence over `[lock]` of the configuration).
    pub lock: Option<LockConfig>,
    /// Internal compaction staging file (file name check is skipped).
    pub internal_compaction: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            strict: true,
            sync_mode: None,
            layout_path: None,
            key_path: None,
            config: None,
            config_path: None,
            lock: None,
            internal_compaction: false,
        }
    }
}

impl OpenOptions {
    pub fn with_sync(mut self, m: SyncMode) -> Self {
        self.sync_mode = Some(m);
        self
    }

    pub(crate) fn resolve_config(&self, db_path: &Path) -> Result<Config> {
        let mut cfg = match &self.config {
            Some(c) => c.clone(),
            None => Config::load(self.config_path.as_deref(), db_path)?,
        };
        if let Some(m) = self.sync_mode {
            cfg.durability = m;
        }
        if let Some(l) = self.lock {
            cfg.lock = l;
        }
        Ok(cfg)
    }

    /// Paths of layout plan and key: explicit > configuration > default.
    pub(crate) fn sidecars(&self, db_path: &Path, cfg: &Config) -> (PathBuf, PathBuf) {
        let (dl, dk) = system::default_sidecars(db_path);
        let lp = self
            .layout_path
            .clone()
            .or_else(|| cfg.layout_path.as_deref().map(|t| Config::resolve_template(t, db_path)))
            .unwrap_or(dl);
        let kp = self
            .key_path
            .clone()
            .or_else(|| cfg.key_path.as_deref().map(|t| Config::resolve_template(t, db_path)))
            .unwrap_or(dk);
        (lp, kp)
    }
}

pub struct Db {
    pub path: PathBuf,
    pub strict: bool,
    pub(crate) store: Storage,
    pub closed: bool,
    active_slot: usize,
    pub sup: SuperState,
    pub checkpoint_offset: u64,
    pub last_valid_end: u64,
    pub next_block_id: u64,
    pub recovered_tail_bytes: u64,

    pub realms: BTreeMap<Nat, RealmDef>,
    pub shapes: BTreeMap<u64, ShapeDef>,
    pub extension_defs: BTreeMap<(Nat, Nat), ExtensionDef>,
    pub(crate) extension_name_map: HashMap<(Nat, String), Nat>,
    pub layouts: BTreeMap<u64, LayoutDef>,
    pub(crate) layouts_by_realm: HashMap<Nat, Vec<u64>>,
    pub(crate) plugins: HashMap<String, Arc<dyn LayoutPlugin>>,
    pub(crate) sparse: HashMap<RL, SparseKeyIndex>,
    pub index_defs: BTreeMap<u64, IndexDef>,
    pub(crate) index_name_map: HashMap<(Nat, String), u64>,
    pub(crate) dense: HashMap<RL, DenseKeyIndex>,
    pub(crate) deletes: HashMap<(Nat, Lane, Nat), u64>,
    pub(crate) secondary_blocks: HashMap<(u64, Lane), index::SecondaryMeta>,
    pub(crate) secondary_source_block: HashMap<(u64, Lane), u64>,
    pub(crate) max_u: HashMap<RL, Nat>,
    pub data_max_block_id: u64,
    pub(crate) data_max_block_id_by_lane: HashMap<RL, u64>,
    pub(crate) overlay_keys: HashSet<RL>,
    /// Keys with actually overlapping dense segments.
    pub(crate) dense_overlap_keys: HashSet<RL>,
    pub has_overlays: bool,
    /// Non-strict open scan ended before end of file → no writes allowed.
    pub scan_incomplete: bool,
    pub last_checkpoint_obj: Val,
    pub(crate) dyn_mode: DynMode,
    pub(crate) dyn_v: Option<Nat>,
    pub(crate) caches: Mutex<Caches>,
    /// Effective runtime configuration.
    pub config: Config,
    /// Process write lock (`<db>.lock`).
    pub(crate) lock: DbLock,
    /// R0,u1 and external layout plan.
    pub(crate) sys: SystemState,
    /// Physical size limit (persisted in the `DB_CONFIG` block).
    pub max_db_size_bytes: u64,
    /// MATH layout states (from the layout plan).
    pub math_layouts: BTreeMap<u64, math::MathLayoutState>,
    /// Internal copy (compaction) may write into exclusive MATH realms.
    pub(crate) internal_math_writes: bool,
}

/// Low-level block information (`BlockInfo`).
#[derive(Clone, Copy, Debug)]
pub struct BlockInfo {
    pub offset: u64,
    pub block_type: u8,
    pub block_id: u64,
    pub flags: u16,
    pub payload_len: u64,
    pub payload_crc: u32,
    pub next_offset: u64,
}

impl Db {
    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    /// Creates a new database with layout plan and key
    /// (`NHyDB.create`).
    pub fn create(path: impl AsRef<Path>, overwrite: bool, opts: OpenOptions) -> Result<Db> {
        let p = path.as_ref();
        let cfg = opts.resolve_config(p)?;
        cfg.durability.check_supported()?;
        let (lp, kp) = opts.sidecars(p, &cfg);
        let dir = platform::parent_dir(p);
        std::fs::create_dir_all(&dir)?;
        let lock = DbLock::acquire(p, cfg.lock)?;
        if !overwrite && (p.exists() || lp.exists() || kp.exists()) {
            return Err(Error::Exists("DB or associated external file already exists".into()));
        }
        if overwrite {
            for x in [p, lp.as_path(), kp.as_path()] {
                if x.exists() {
                    std::fs::remove_file(x)?;
                }
            }
        }
        let mode = cfg.durability;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut created: Vec<PathBuf> = Vec::new();
        let result = (|| -> Result<Db> {
            let db_id = crate::crypto::uuid4()?;
            let key = system::key_create(&kp, &db_id, &name, mode)?;
            created.push(kp.clone());
            let snapshot = system::empty_schema().to_json()?;
            let entry = system::seal_entry(&db_id, 0, snapshot.as_bytes(), &key)?;
            system::atomic_write(&lp, &system::envelope_bytes(&db_id, std::slice::from_ref(&entry), &key)?, mode)?;
            created.push(lp.clone());
            let fname = |q: &Path| q.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let digest = system::entry_digest(&entry)?.to_string();
            let record = system::bootstrap_record(&db_id, &name, &fname(&lp), &fname(&kp), 0, &digest, now_ns());
            system::write_initial_db(p, &record, mode)?;
            created.push(p.to_path_buf());
            let mut o = opts.clone();
            o.layout_path = Some(lp.clone());
            o.key_path = Some(kp.clone());
            let mut db = Db::open_locked(p, &o, cfg.clone(), lock)?;
            db.set_max_db_size(cfg.db_max_size_bytes, true)?;
            Ok(db)
        })();
        if result.is_err() {
            for x in created.iter().rev() {
                let _ = std::fs::remove_file(x);
            }
        }
        result
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        Db::open_with(path, OpenOptions::default())
    }

    /// Opens an existing database (`NHyDB.__init__`).
    pub fn open_with(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Db> {
        let p = path.as_ref();
        if !p.exists() {
            return Err(Error::NotFound(p.display().to_string()));
        }
        let cfg = opts.resolve_config(p)?;
        cfg.durability.check_supported()?;
        let lock = DbLock::acquire(p, cfg.lock)?;
        Db::open_locked(p, &opts, cfg, lock)
    }

    /// Opens with the lock already held. Fail closed: nothing in the file is
    /// modified before R0, key and plan have been verified.
    pub(crate) fn open_locked(p: &Path, opts: &OpenOptions, cfg: Config, lock: DbLock) -> Result<Db> {
        let (lp, kp) = opts.sidecars(p, &cfg);
        let store = Storage::open(p, cfg.durability)?;
        let (active_slot, sup) = best_super(&store)?;
        let physical = store.len();
        let committed = sup.last_valid_end;
        if committed < DATA_START || committed > physical {
            return Err(Error::Corruption("invalid commit boundary".into()));
        }
        let mut db = Db {
            path: p.to_path_buf(),
            strict: opts.strict,
            store,
            closed: false,
            active_slot,
            sup,
            checkpoint_offset: sup.checkpoint_offset,
            last_valid_end: sup.last_valid_end,
            next_block_id: sup.next_block_id,
            recovered_tail_bytes: 0,
            realms: BTreeMap::new(),
            shapes: BTreeMap::new(),
            extension_defs: BTreeMap::new(),
            extension_name_map: HashMap::new(),
            layouts: BTreeMap::new(),
            layouts_by_realm: HashMap::new(),
            plugins: HashMap::new(),
            sparse: HashMap::new(),
            index_defs: BTreeMap::new(),
            index_name_map: HashMap::new(),
            dense: HashMap::new(),
            deletes: HashMap::new(),
            secondary_blocks: HashMap::new(),
            secondary_source_block: HashMap::new(),
            max_u: HashMap::new(),
            data_max_block_id: 0,
            data_max_block_id_by_lane: HashMap::new(),
            overlay_keys: HashSet::new(),
            dense_overlap_keys: HashSet::new(),
            has_overlays: false,
            scan_incomplete: false,
            last_checkpoint_obj: Val::map(),
            dyn_mode: DynMode::Off,
            dyn_v: None,
            caches: Mutex::new(Caches::default()),
            config: cfg,
            lock,
            sys: SystemState {
                record: Val::None,
                key: Vec::new(),
                layout_path: lp,
                key_path: kp,
                entries: Vec::new(),
                active_entry: Val::None,
                layout_dirty: false,
                poisoned: false,
                math_constants_stored: false,
            },
            max_db_size_bytes: crate::config::DEFAULT_DB_MAX_SIZE_BYTES,
            math_layouts: BTreeMap::new(),
            internal_math_writes: false,
        };
        // 1) One scan over the committed range: register data blocks,
        //    verify the R0 chain, reject layout metadata inside the DB.
        let record = db.open_scan(committed)?;
        // 2) Identity, key, plan, generation (fail closed).
        let cells_name = record.get("cells").and_then(|c| c.get("2")).map(|v| v.py_str()).unwrap_or_default();
        let fname = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !opts.internal_compaction && fname != cells_name {
            return Err(Error::Corruption("DB file name does not match R0,u1".into()));
        }
        db.sys.record = record;
        let db_id = db.sys.db_id();
        db.sys.key = system::key_load(&db.sys.key_path, &db_id, &cells_name)?;
        db.sys.entries = system::read_envelope(&db.sys.layout_path, &db_id, &db.sys.key)?;
        let generation = db.sys.generation()?;
        let digest = db.sys.cell("6").map(|v| v.py_str()).unwrap_or_default();
        let matches: Vec<&Val> = db
            .sys
            .entries
            .iter()
            .filter(|e| system::entry_generation(e).ok() == Some(generation) && system::entry_digest(e).ok() == Some(digest.as_str()))
            .collect();
        if matches.len() != 1 {
            return Err(Error::Corruption("no layout plan matching R0,u1 generation and hash".into()));
        }
        db.sys.active_entry = matches[0].clone();
        let snapshot = system::open_entry(&db_id, &db.sys.active_entry, &db.sys.key)?;
        // 3) Only now: discard the uncommitted tail.
        if physical > committed {
            db.recovered_tail_bytes = physical - committed;
            db.store.truncate(committed)?;
        }
        db.import_schema(&snapshot)?;
        db.validate_math_layout_state()?;
        Ok(db)
    }

    /// Resets all state derived from the file and schema
    /// (rollback of a failed MATH batch).
    pub(crate) fn reset_in_memory_state(&mut self) {
        self.realms.clear();
        self.shapes.clear();
        self.extension_defs.clear();
        self.extension_name_map.clear();
        self.layouts.clear();
        self.layouts_by_realm.clear();
        self.sparse.clear();
        self.index_defs.clear();
        self.index_name_map.clear();
        self.dense.clear();
        self.deletes.clear();
        self.secondary_blocks.clear();
        self.secondary_source_block.clear();
        self.max_u.clear();
        self.data_max_block_id = 0;
        self.data_max_block_id_by_lane.clear();
        self.overlay_keys.clear();
        self.dense_overlap_keys.clear();
        self.has_overlays = false;
        self.scan_incomplete = false;
        self.math_layouts.clear();
        *self.caches.get_mut().unwrap_or_else(|e| e.into_inner()) = Caches::default();
    }

    pub fn sync_mode(&self) -> SyncMode {
        self.store.sync_mode
    }

    pub fn set_sync_mode(&mut self, m: SyncMode) -> Result<()> {
        m.check_supported()?;
        self.store.sync_mode = m;
        self.config.durability = m;
        Ok(())
    }

    pub(crate) fn check_open(&self) -> Result<()> {
        if self.closed {
            return Err(Error::NHyDB("database is closed".into()));
        }
        Ok(())
    }

    /// `close(commit=True)`
    pub fn close(&mut self, commit: bool) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        if commit {
            // On a failed commit the database stays open (as in the reference
            // implementation): retry, or close(commit=false) to discard.
            self.commit()?;
        }
        // Release the OS file handle immediately (not only on drop), so the
        // file can be replaced by compact_file()/repair or renamed while the
        // closed object is still referenced (Windows). The handle and the
        // lock are released even if writing the buffer fails.
        let res = self.store.close();
        self.closed = true;
        self.lock.release();
        res
    }

    /// `sync(hard=False)`: write buffers, optionally fsync.
    pub fn sync(&mut self, hard: bool) -> Result<()> {
        self.check_open()?;
        self.store.flush()?;
        if hard {
            self.store.fsync()?;
        }
        Ok(())
    }

    /// Durable commit. Without a schema change: write data + sync, then the
    /// alternating superblock + sync. With a schema change, first publish the
    /// new encrypted generation atomically in the layout plan (active + new
    /// generation), then append the R0 record and write the superblock.
    /// Returns the commit boundary.
    pub fn commit(&mut self) -> Result<u64> {
        if self.closed || self.sys.poisoned {
            return Err(Error::NHyDB("database closed or security state unclear".into()));
        }
        if self.sys.layout_dirty {
            let new_gen = self.sys.generation()? + 1;
            let db_id = self.sys.db_id();
            let snapshot = self.export_schema()?;
            let snap_json = snapshot.to_json()?;
            let next_entry = system::seal_entry(&db_id, new_gen, snap_json.as_bytes(), &self.sys.key)?;
            let env = system::envelope_bytes(&db_id, &[self.sys.active_entry.clone(), next_entry.clone()], &self.sys.key)?;
            system::atomic_write(&self.sys.layout_path, &env, self.store.sync_mode)?;
            let next_record = system::next_record(&self.sys.record, new_gen, system::entry_digest(&next_entry)?);
            self.append_block(BT_SYSTEM, next_record.to_json()?.as_bytes(), 0)?;
            let r = (|| -> Result<()> {
                self.store.flush()?;
                self.store.fsync()?;
                self.commit_super()
            })();
            if let Err(e) = r {
                self.sys.poisoned = true;
                return Err(e);
            }
            let prev_last = self.sys.entries.last().cloned().unwrap_or(Val::None);
            self.sys.record = next_record;
            self.sys.active_entry = next_entry.clone();
            self.sys.entries = vec![prev_last, next_entry];
            self.sys.layout_dirty = false;
        } else {
            self.store.flush()?;
            self.store.fsync()?;
            self.commit_super()?;
        }
        Ok(self.sup.last_valid_end)
    }

    fn commit_super(&mut self) -> Result<()> {
        let slot = 1 - self.active_slot;
        let s = SuperState {
            generation: self.sup.generation + 1,
            checkpoint_offset: self.checkpoint_offset,
            last_valid_end: self.last_valid_end,
            next_block_id: self.next_block_id,
            created_ns: self.sup.created_ns,
            updated_ns: now_ns(),
            flags: self.sup.flags,
        };
        self.store.write_at((slot * SUPERBLOCK_SIZE) as u64, &pack_super(&s))?;
        self.store.fsync()?;
        self.active_slot = slot;
        self.sup = s;
        Ok(())
    }

    // ------------------------------------------------------------------
    // System address space, DB configuration
    // ------------------------------------------------------------------

    /// `get_system_metadata()`: read-only copy of R0,u1.
    pub fn get_system_metadata(&self) -> Val {
        let mut cells = self.sys.record.get("cells").cloned().unwrap_or(Val::map());
        if self.sys.math_constants_stored {
            cells.set(system::MATH_CONSTANTS_CELL, system::math_constants_cell());
        }
        vmap! {"realm" => 0i64, "u" => 1i64, "cells" => cells}
    }

    /// `get_system_cell(v)`; unoccupied cells return `None`.
    pub fn get_system_cell(&self, v: &Nat) -> Result<Val> {
        if v.is_zero() || v > crate::address::realm_span() {
            return Err(Error::Address("system v outside R0,u1".into()));
        }
        let key = v.to_string();
        if key == system::MATH_CONSTANTS_CELL && self.sys.math_constants_stored {
            return Ok(system::math_constants_cell());
        }
        Ok(self.sys.cell(&key).cloned().unwrap_or(Val::None))
    }

    pub fn layout_path(&self) -> &Path {
        &self.sys.layout_path
    }

    pub fn key_path(&self) -> &Path {
        &self.sys.key_path
    }

    pub fn active_layout_entry(&self) -> Val {
        self.sys.active_entry.clone()
    }

    pub fn layout_dirty(&self) -> bool {
        self.sys.layout_dirty
    }

    /// `set_max_db_size(size_bytes, commit=False)`
    pub fn set_max_db_size(&mut self, size_bytes: u64, commit: bool) -> Result<u64> {
        if size_bytes == 0 {
            return Err(Error::Value("size_bytes must be a positive integer".into()));
        }
        let current = self.store.len();
        if size_bytes < current {
            return Err(Error::SizeLimit(format!(
                "new DB maximum {size_bytes} is below the current file size {current}"
            )));
        }
        let payload = vmap! {"contract" => "NHYDB_DB_CONFIG_V015", "max_db_size_bytes" => Val::int(size_bytes as i128)}.to_json()?;
        self.append_block_limit(BT_DB_CONFIG, &[payload.as_bytes()], 0, Some(size_bytes))?;
        self.max_db_size_bytes = size_bytes;
        if commit {
            self.commit()?;
        }
        Ok(self.max_db_size_bytes)
    }

    /// Schema snapshot for the external layout plan (`_export_external_schema`).
    pub fn export_schema(&self) -> Result<Val> {
        let list = |it: Vec<Val>| Val::List(it);
        Ok(vmap! {
            "contract" => system::SCHEMA_CONTRACT,
            "realms" => list(self.realms.values().map(|r| r.to_val()).collect()),
            "shapes" => list(self.shapes.values().map(|x| x.to_val()).collect()),
            "extensions" => list(self.extension_defs.values().map(|x| x.to_val()).collect()),
            "layouts" => list(self.layouts.values().map(|x| x.to_val()).collect()),
            "indexes" => list(self.index_defs.values().map(|x| x.to_val()).collect()),
            "math_layouts" => list(self.math_layouts.values().map(|m| m.to_val()).collect()),
        })
    }

    /// `_import_external_schema`
    pub(crate) fn import_schema(&mut self, snap: &Val) -> Result<()> {
        if snap.get("contract").and_then(|v| v.as_str()) != Some(system::SCHEMA_CONTRACT) {
            return Err(Error::Corruption("invalid layout plan".into()));
        }
        let rows = |k: &str| -> Result<Vec<Val>> {
            snap.get(k)
                .and_then(|v| v.as_list())
                .cloned()
                .ok_or_else(|| Error::Corruption(format!("layout plan: '{k}' missing")))
        };
        for row in rows("realms")? {
            let r = RealmDef::from_val(&row)?;
            self.max_u.entry((r.realm_id.clone(), 0)).or_insert(Nat::ZERO);
            self.max_u.entry((r.realm_id.clone(), 1)).or_insert(Nat::ZERO);
            self.realms.insert(r.realm_id.clone(), r);
        }
        for row in rows("shapes")? {
            let sh = ShapeDef::from_val(&row)?;
            self.shapes.insert(sh.shape_id, sh);
        }
        for row in rows("extensions")? {
            self.install_extension(ExtensionDef::from_val(&row)?);
        }
        for row in rows("layouts")? {
            let lay = LayoutDef::from_val(&row)?;
            let ids = self.layouts_by_realm.entry(lay.realm_id.clone()).or_default();
            if !ids.contains(&lay.layout_id) {
                ids.push(lay.layout_id);
            }
            self.layouts.insert(lay.layout_id, lay);
        }
        for row in rows("indexes")? {
            let idx = IndexDef::from_val(&row)?;
            self.index_name_map.insert((idx.realm_id.clone(), idx.name.clone()), idx.index_id);
            self.index_defs.insert(idx.index_id, idx);
        }
        for row in rows("math_layouts")? {
            let st = math::MathLayoutState::from_val(&row)?;
            self.math_layouts.insert(st.layout_id, st);
        }
        self.clear_decode_caches();
        Ok(())
    }


    pub fn active_slot(&self) -> usize {
        self.active_slot
    }

    /// Current (logical) file size including unwritten bytes.
    pub fn file_len(&self) -> u64 {
        self.store.len()
    }

    // ------------------------------------------------------------------
    // Blocks
    // ------------------------------------------------------------------

    /// Appends a block (`_append`). Returns (offset, block ID).
    pub(crate) fn append_block(&mut self, bt: u8, payload: &[u8], flags: u16) -> Result<(u64, u64)> {
        self.append_block_parts(bt, &[payload], flags)
    }

    /// Like `append_block`, with a payload made of several parts (avoids copies).
    pub(crate) fn append_block_parts(&mut self, bt: u8, parts: &[&[u8]], flags: u16) -> Result<(u64, u64)> {
        self.append_block_limit(bt, parts, flags, None)
    }

    /// `_append(..., _size_limit_override=…)`. Layout metadata is not written
    /// to the DB; instead it marks the schema as changed for the next commit
    /// (external layout plan).
    pub(crate) fn append_block_limit(&mut self, bt: u8, parts: &[&[u8]], flags: u16, limit: Option<u64>) -> Result<(u64, u64)> {
        self.check_open()?;
        if self.sys.poisoned {
            return Err(Error::NHyDB("security state unclear; reopen the database".into()));
        }
        if is_layout_meta(bt) {
            self.sys.layout_dirty = true;
            return Ok((self.last_valid_end, self.next_block_id));
        }
        if self.scan_incomplete {
            return Err(Error::NHyDB(format!(
                "file was opened non-strictly and is damaged from byte {}; run repair_file() first",
                self.last_valid_end
            )));
        }
        let off = self.store.len();
        let plen: u64 = parts.iter().map(|p| p.len() as u64).sum();
        let nxt = align8(off + BLOCK_HEADER_SIZE as u64 + plen);
        let lim = limit.unwrap_or(self.max_db_size_bytes);
        if nxt > lim {
            return Err(Error::SizeLimit(format!("DB maximum exceeded: current={off}, next={nxt}, limit={lim} bytes")));
        }
        match self.append_raw(bt, parts, flags, off) {
            Ok(r) => Ok(r),
            Err(e) => {
                // Discard partially written bytes so that no half-written
                // block ends up behind the next commit boundary.
                let _ = self.store.rollback_to(off);
                Err(e)
            }
        }
    }

    fn append_raw(&mut self, bt: u8, parts: &[&[u8]], flags: u16, off: u64) -> Result<(u64, u64)> {
        let bid = self.next_block_id;
        let plen: usize = parts.iter().map(|p| p.len()).sum();
        let mut hasher = crc32fast::Hasher::new();
        for p in parts {
            hasher.update(p);
        }
        let pcrc = hasher.finalize();
        let header = pack_header_with_crc(bt, bid, plen as u64, pcrc, flags);
        self.store.append(&header)?;
        for p in parts {
            self.store.append(p)?;
        }
        let end = off + BLOCK_HEADER_SIZE as u64 + plen as u64;
        let nxt = align8(end);
        if nxt > end {
            self.store.append_zeros((nxt - end) as usize)?;
        }
        self.next_block_id += 1;
        self.last_valid_end = nxt;
        Ok((off, bid))
    }

    pub(crate) fn read_header_at(&self, off: u64) -> Result<BlockInfo> {
        let size = self.store.len();
        if off < DATA_START || off + BLOCK_HEADER_SIZE as u64 > size {
            return Err(Error::Corruption(format!("invalid block offset {off}")));
        }
        let mut raw = [0u8; BLOCK_HEADER_SIZE];
        self.store.read_exact_at(off, &mut raw)?;
        let h = unpack_block_header(&raw)?;
        let nxt = (off + BLOCK_HEADER_SIZE as u64)
            .checked_add(h.payload_len)
            .and_then(|e| e.checked_add(7))
            .map(|e| e & !7)
            .unwrap_or(u64::MAX);
        if nxt > size {
            return Err(Error::Corruption(format!("block {}: end incomplete", h.block_id)));
        }
        Ok(BlockInfo {
            offset: off,
            block_type: h.block_type,
            block_id: h.block_id,
            flags: h.flags,
            payload_len: h.payload_len,
            payload_crc: h.payload_crc,
            next_offset: nxt,
        })
    }

    /// Reads header and payload including CRC check (`_read_block`).
    pub(crate) fn read_block(&self, off: u64) -> Result<(BlockInfo, Vec<u8>)> {
        let info = self.read_header_at(off)?;
        let payload = self.store.read_vec(off + BLOCK_HEADER_SIZE as u64, info.payload_len as usize)?;
        if crc32(&payload) != info.payload_crc {
            return Err(Error::Corruption(format!("block {}: payload CRC mismatch", info.block_id)));
        }
        Ok((info, payload))
    }

    /// Block payload via the block cache (CRC-checked on load).
    pub(crate) fn block_payload(&self, block_offset: u64) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.cache().blocks.get(&block_offset) {
            return Ok(b.clone());
        }
        let (_info, payload) = self.read_block(block_offset)?;
        let arc = Arc::new(payload);
        if arc.len() <= BLOCK_CACHE_MAX_ITEM {
            let mut c = self.cache();
            if c.block_bytes + arc.len() > BLOCK_CACHE_BUDGET {
                c.blocks.clear();
                c.block_bytes = 0;
            }
            c.block_bytes += arc.len();
            c.blocks.insert(block_offset, arc.clone());
        }
        Ok(arc)
    }

    #[inline]
    pub(crate) fn cache(&self) -> MutexGuard<'_, Caches> {
        self.caches.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn clear_decode_caches(&mut self) {
        let c = self.caches.get_mut().unwrap_or_else(|e| e.into_inner());
        c.field_meta.clear();
        c.sparse_record.clear();
        c.sparse_merged.clear();
        c.sparse_uindex.clear();
        c.uindex_ck = 0;
    }

    // ------------------------------------------------------------------
    // Open-Scan
    // ------------------------------------------------------------------

    fn parse_dense_prefix(&self, info: &BlockInfo) -> Result<DenseSegment> {
        let payload_off = info.offset + BLOCK_HEADER_SIZE as u64;
        let raw = self.store.read_upto(payload_off, 96.min(info.payload_len as usize).max(4))?;
        if !raw.starts_with(DATA_TAG) {
            return Err(Error::Corruption(format!("DATA block {}: wrong tag", info.block_id)));
        }
        let mut p = 4;
        let realm = get_vu(&raw, &mut p)?;
        let shape = get_vu_u64(&raw, &mut p, "DATA shape")?;
        let u_start = get_vu(&raw, &mut p)?;
        let count = get_vu_u64(&raw, &mut p, "DATA count")?;
        let offsets_off = payload_off + p as u64;
        let records_off = count
            .checked_add(1)
            .and_then(|c| c.checked_mul(4))
            .and_then(|t| t.checked_add(offsets_off))
            .unwrap_or(u64::MAX);
        if records_off > payload_off + info.payload_len {
            return Err(Error::Corruption(format!("DATA block {}: offset table too large", info.block_id)));
        }
        let u_end = if count == 0 { u_start.clone() } else { u_start.add_u64(count - 1) };
        Ok(DenseSegment {
            realm_id: realm,
            lane: lane_from_flags(info.flags),
            shape_id: shape,
            u_start,
            u_end,
            count,
            block_id: info.block_id,
            block_offset: info.offset,
            payload_offset: payload_off,
            offsets_offset: offsets_off,
            records_offset: records_off,
            payload_len: info.payload_len,
        })
    }

    fn parse_sparse_prefix(&self, info: &BlockInfo) -> Result<SparseSegment> {
        let payload_off = info.offset + BLOCK_HEADER_SIZE as u64;
        let raw = self.store.read_upto(payload_off, 96.min(info.payload_len as usize).max(4))?;
        if !raw.starts_with(SPARSE_TAG) {
            return Err(Error::Corruption(format!("SPARSE block {}: wrong tag", info.block_id)));
        }
        // The prefix can exceed 96 bytes for huge u.
        let raw = if raw.len() < info.payload_len as usize && raw.len() == 96 {
            self.store.read_upto(payload_off, (info.payload_len as usize).min(16 << 10))?
        } else {
            raw
        };
        let mut p = 4;
        let realm = get_vu(&raw, &mut p)?;
        let u_start = get_vu(&raw, &mut p)?;
        let u_end = get_vu(&raw, &mut p)?;
        let group_count = get_vu_u64(&raw, &mut p, "SPARSE groups")?;
        let cell_count = get_vu_u64(&raw, &mut p, "SPARSE cells")?;
        if group_count == 0 || cell_count == 0 || u_end < u_start {
            return Err(Error::Corruption(format!("SPARSE block {}: invalid range/group", info.block_id)));
        }
        let entries_off = payload_off + p as u64;
        if entries_off > payload_off + info.payload_len {
            return Err(Error::Corruption(format!("SPARSE block {}: prefix too large", info.block_id)));
        }
        Ok(SparseSegment {
            realm_id: realm,
            lane: lane_from_flags(info.flags),
            u_start,
            u_end,
            group_count,
            cell_count,
            block_id: info.block_id,
            block_offset: info.offset,
            payload_offset: payload_off,
            entries_offset: entries_off,
            payload_len: info.payload_len,
        })
    }

    pub(crate) fn note_data_block(&mut self, key: &RL, bid: u64) {
        self.data_max_block_id = self.data_max_block_id.max(bid);
        let e = self.data_max_block_id_by_lane.entry(key.clone()).or_insert(0);
        *e = (*e).max(bid);
    }

    pub(crate) fn register_segment(&mut self, seg: DenseSegment) {
        let key: RL = (seg.realm_id.clone(), seg.lane);
        let bid = seg.block_id;
        let idx = self.dense.entry(key.clone()).or_default();
        if idx.iv.any_overlap(&seg.u_start, &seg.u_end) {
            self.has_overlays = true;
            self.overlay_keys.insert(key.clone());
            self.dense_overlap_keys.insert(key.clone());
        }
        let mu = self.max_u.entry(key.clone()).or_insert(Nat::ZERO);
        if seg.u_end > *mu {
            *mu = seg.u_end.clone();
        }
        let i = idx.segs.len();
        idx.iv.insert(seg.u_start.clone(), seg.u_end.clone(), i);
        idx.segs.push(seg);
        self.note_data_block(&key, bid);
    }

    pub(crate) fn register_sparse_segment(&mut self, seg: SparseSegment) {
        let key: RL = (seg.realm_id.clone(), seg.lane);
        let bid = seg.block_id;
        let idx = self.sparse.entry(key.clone()).or_default();
        let i = idx.segs.len();
        idx.iv.insert(seg.u_start.clone(), seg.u_end.clone(), i);
        idx.segs.push(seg);
        self.note_data_block(&key, bid);
        self.has_overlays = true;
        self.overlay_keys.insert(key);
    }

    /// One scan over the committed range `[DATA_START, end)`: registers data
    /// blocks, verifies the R0,u1 chain and rejects layout metadata inside the
    /// DB. Returns the current R0 record.
    fn open_scan(&mut self, end: u64) -> Result<Val> {
        let mut off = DATA_START;
        let mut maxid = 0u64;
        let mut record: Option<Val> = None;
        while off < end {
            let info = self.read_header_at(off)?;
            if info.next_offset > end {
                return Err(Error::Corruption("system bootstrap: incomplete commit block".into()));
            }
            match info.block_type {
                BT_SYSTEM => {
                    let (_i, payload) = self.read_block(off)?;
                    let item = Self::json_payload(&payload)?;
                    system::check_record(&item, record.as_ref())?;
                    if item.get("cells").and_then(|c| c.get(system::MATH_CONSTANTS_CELL)).is_some() {
                        self.sys.math_constants_stored = true;
                    }
                    record = Some(item);
                }
                bt if is_layout_meta(bt) => {
                    return Err(Error::Corruption("layout metadata must not be stored inside the DB".into()));
                }
                BT_DELETE | BT_CHECKPOINT | BT_DB_CONFIG => {
                    let (_i, payload) = self.read_block(off)?;
                    self.apply_meta_block(&info, &payload)?;
                }
                BT_DATA_DENSE => {
                    let seg = self.parse_dense_prefix(&info)?;
                    self.register_segment(seg);
                }
                BT_SPARSE_CELLS => {
                    let seg = self.parse_sparse_prefix(&info)?;
                    self.register_sparse_segment(seg);
                }
                BT_SECONDARY => {
                    self.parse_secondary_directory(&info)?;
                }
                _ => {}
            }
            maxid = maxid.max(info.block_id);
            off = info.next_offset;
        }
        if off != end {
            return Err(Error::Corruption("R0,u1 bootstrap missing or invalid end of file".into()));
        }
        let record = record.ok_or_else(|| Error::Corruption("R0,u1 bootstrap missing or invalid end of file".into()))?;
        self.last_valid_end = end;
        self.scan_incomplete = false;
        self.next_block_id = self.next_block_id.max(maxid + 1);
        Ok(record)
    }

    fn json_payload(payload: &[u8]) -> Result<Val> {
        let s = std::str::from_utf8(payload).map_err(|e| Error::Corruption(format!("metadata UTF-8: {e}")))?;
        Val::from_json(s)
    }

    fn apply_meta_block(&mut self, info: &BlockInfo, payload: &[u8]) -> Result<()> {
        match info.block_type {
            BT_DB_CONFIG => {
                let cfg = Self::json_payload(payload)?;
                if cfg.get("contract").and_then(|v| v.as_str()) != Some("NHYDB_DB_CONFIG_V015") {
                    return Err(Error::Corruption("unknown DB_CONFIG contract".into()));
                }
                let limit = match cfg.get("max_db_size_bytes") {
                    None => crate::config::DEFAULT_DB_MAX_SIZE_BYTES as i128,
                    Some(v) => v.as_bigint().and_then(|b| num_traits::ToPrimitive::to_i128(&b)).unwrap_or(0),
                };
                if limit <= 0 {
                    return Err(Error::Corruption("DB_CONFIG max_db_size_bytes must be >0".into()));
                }
                self.max_db_size_bytes = limit.min(u64::MAX as i128) as u64;
            }
            BT_DELETE => {
                let lane = lane_from_flags(info.flags);
                let mut p = 0;
                let r = get_vu(payload, &mut p)?;
                let u = get_vu(payload, &mut p)?;
                if p != payload.len() {
                    return Err(Error::Corruption("DELETE trailing bytes".into()));
                }
                let key: RL = (r.clone(), lane);
                self.deletes.insert((r, lane, u), info.block_id);
                self.note_data_block(&key, info.block_id);
                self.has_overlays = true;
                self.overlay_keys.insert(key);
            }
            BT_CHECKPOINT => {
                if payload.starts_with(CHECKPOINT_TAG) {
                    self.last_checkpoint_obj = std::str::from_utf8(&payload[4..])
                        .ok()
                        .and_then(|s| Val::from_json(s).ok())
                        .unwrap_or(Val::map());
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn install_extension(&mut self, ext: ExtensionDef) {
        let rid = ext.realm_id.clone();
        let v = ext.v.clone();
        self.extension_name_map.retain(|k, vv| !(k.0 == rid && *vv == v));
        if !ext.name.is_empty() {
            self.extension_name_map.insert((rid.clone(), ext.name.clone()), v.clone());
        }
        self.extension_defs.insert((rid, v), ext);
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    pub(crate) fn require_realm(&self, realm: &Nat) -> Result<&RealmDef> {
        self.realms.get(realm).ok_or_else(|| Error::Schema("realm missing".into()))
    }

    /// Shape of a stored segment (missing → CorruptionError).
    pub(crate) fn shape_of(&self, shape_id: u64) -> Result<&ShapeDef> {
        self.shapes
            .get(&shape_id)
            .ok_or_else(|| Error::Corruption(format!("shape {shape_id} of the DATA segment missing")))
    }

    pub fn has_realm(&self, realm: &Nat) -> bool {
        self.realms.contains_key(realm)
    }

    pub fn superblock(&self) -> SuperState {
        self.sup
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // Like a Python file object on release: write buffers,
        // but NO commit (the commit boundary stays unchanged).
        if !self.closed {
            let _ = self.store.flush();
        }
    }
}

pub(crate) fn best_super(store: &Storage) -> Result<(usize, SuperState)> {
    let mut best: Option<(usize, SuperState)> = None;
    for slot in 0..2 {
        let raw = store.read_upto((slot * SUPERBLOCK_SIZE) as u64, SUPERBLOCK_SIZE)?;
        if let Ok(s) = unpack_super(&raw) {
            match &best {
                Some((_, b)) if b.generation >= s.generation => {}
                _ => best = Some((slot, s)),
            }
        }
    }
    best.ok_or_else(|| Error::Corruption("both superblocks damaged".into()))
}

fn pack_header_with_crc(bt: u8, bid: u64, plen: u64, pcrc: u32, flags: u16) -> [u8; BLOCK_HEADER_SIZE] {
    let mut b = [0u8; BLOCK_HEADER_SIZE];
    b[0..4].copy_from_slice(BLOCK_MAGIC);
    b[4] = BLOCK_VERSION;
    b[5] = bt;
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    b[8..16].copy_from_slice(&bid.to_le_bytes());
    b[16..24].copy_from_slice(&plen.to_le_bytes());
    b[24..28].copy_from_slice(&pcrc.to_le_bytes());
    let hcrc = crc32(&b);
    b[28..32].copy_from_slice(&hcrc.to_le_bytes());
    b
}

#[cfg(test)]
mod file_handle_tests {
    use super::*;
    use crate::db::dense::DenseRows;
    use crate::db::maint::CompactOptions;
    use crate::codec::FieldDef;
    use crate::address::NL;
    use crate::db::sparse::CellInput;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nhydb_fh_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join(format!("{name}.nhydb"))
    }

    fn bytes_db(p: &Path) -> Db {
        let r = Nat::S(1);
        let mut db = Db::create(p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        db.create_shape(1, &r, "s", vec![FieldDef::new(Nat::S(1), "b", TypeCode::Bytes)], "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
        db
    }

    fn put(db: &mut Db, u: u64, b: Vec<u8>, commit: bool) -> Result<Val> {
        let rows = vec![vec![Val::Bytes(b)]];
        db.put_dense_batch(&Nat::S(1), &Nat::S(u), &DenseRows::Tuples(&rows), Some(1), &[], NL, commit)
    }

    /// Platform-independent counterpart of the POSIX-only Python test
    /// `test_failed_append_is_rolled_back`: a write error in the middle of a
    /// large block (here injected like a file size limit) must not leave a
    /// partial block behind that a later commit would make durable.
    #[test]
    fn failed_append_is_rolled_back() {
        let p = tmp("efbig");
        let mut db = bytes_db(&p);
        put(&mut db, 1, b"ok".to_vec(), true).unwrap();
        let len_before = db.store.len();
        db.store.fail_writes_beyond = Some(1 << 20);
        let err = put(&mut db, 2, vec![b'x'; 9 << 20], false).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err}");
        db.store.fail_writes_beyond = None;
        assert_eq!(db.store.len(), len_before, "logical length rolled back");
        assert!(std::fs::metadata(&p).unwrap().len() <= len_before, "partially written bytes removed from the file");
        put(&mut db, 3, b"drei".to_vec(), true).unwrap();
        db.close(true).unwrap();
        let db = Db::open(&p).unwrap();
        assert_eq!(db.recovered_tail_bytes, 0);
        let field = |u: u64| db.get(&Nat::S(1), &Nat::S(u), NL, None, None, true).unwrap();
        assert_eq!(field(1).and_then(|r| r.get("b").cloned()), Some(Val::Bytes(b"ok".to_vec())));
        assert_eq!(field(3).and_then(|r| r.get("b").cloned()), Some(Val::Bytes(b"drei".to_vec())));
        assert!(field(2).is_none());
        assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
    }

    /// `close()` releases the OS file handle: the file can be replaced
    /// (compact_file) and renamed while the closed object still exists.
    /// On Windows an open handle makes MoveFileExW fail with os error 5.
    #[test]
    fn close_releases_file_handle() {
        let p = tmp("close");
        let mut db = bytes_db(&p);
        for u in 1..=5 {
            put(&mut db, u, vec![u as u8; 10], true).unwrap();
        }
        db.close(true).unwrap();
        assert!(matches!(db.get(&Nat::S(1), &Nat::S(1), NL, None, None, true), Err(Error::NHyDB(_))));
        let res = Db::compact_file(&p, &CompactOptions::default()).unwrap();
        assert_eq!(res.get("replaced_original"), Some(&Val::Bool(true)));
        let q = p.with_extension("renamed");
        std::fs::rename(&p, &q).unwrap();
        std::fs::rename(&q, &p).unwrap();
        drop(db);
        let db = Db::open(&p).unwrap();
        assert!(db.get(&Nat::S(1), &Nat::S(5), NL, None, None, true).unwrap().is_some());
    }

    /// Repeated close() is a no-op; every file operation afterwards fails
    /// with "database is closed" instead of touching the file.
    #[test]
    fn close_is_idempotent_and_blocks_file_access() {
        let p = tmp("twice");
        let mut db = bytes_db(&p);
        put(&mut db, 1, b"a".to_vec(), true).unwrap();
        db.close(true).unwrap();
        db.close(true).unwrap();
        db.close(false).unwrap();
        let closed = |r: Result<()>| matches!(r, Err(Error::NHyDB(m)) if m.contains("closed"));
        assert!(closed(put(&mut db, 2, b"b".to_vec(), true).map(|_| ())));
        assert!(closed(db.commit().map(|_| ())));
        assert!(closed(db.sync(true)));
        assert!(closed(db.store.append(b"x")));
        assert!(closed(db.store.read_vec(0, 8).map(|_| ())));
        assert!(closed(db.store.fsync()));
        assert!(closed(db.store.write_at(0, b"x")));
        assert!(closed(db.store.truncate(0)));
        assert!(closed(db.store.rollback_to(0)));
        drop(db); // Drop after close() does not flush
        let db = Db::open(&p).unwrap();
        assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
    }

    /// close(commit=false) releases handle and lock even if writing the
    /// buffer fails; the error is returned, the commit boundary is intact.
    #[test]
    fn close_releases_handle_on_write_error() {
        let p = tmp("closeerr");
        let mut db = bytes_db(&p);
        put(&mut db, 1, b"ok".to_vec(), true).unwrap();
        put(&mut db, 2, b"pending".to_vec(), false).unwrap(); // stays in the buffer
        db.store.fail_writes_beyond = Some(std::fs::metadata(&p).unwrap().len()); // nothing more can be written
        assert!(matches!(db.close(false), Err(Error::Io(_))));
        assert!(db.closed && !db.lock.is_held());
        let q = p.with_extension("moved");
        std::fs::rename(&p, &q).unwrap();
        std::fs::rename(&q, &p).unwrap();
        let other = Db::open(&p).unwrap(); // lock released
        assert!(other.get(&Nat::S(1), &Nat::S(1), NL, None, None, true).unwrap().is_some());
        assert!(other.get(&Nat::S(1), &Nat::S(2), NL, None, None, true).unwrap().is_none());
        drop(other);
        drop(db);
    }

    /// close(commit=false) never publishes uncommitted data: neither records
    /// still in the buffer, nor records already written to the file (sync,
    /// large direct writes), nor sparse cells - not after reopening, not
    /// after a later commit, not after reopening again.
    #[test]
    fn close_without_commit_hides_uncommitted_data() {
        let p = tmp("nocommit");
        let r = Nat::S(1);
        let r2 = Nat::S(2);
        let mut db = bytes_db(&p);
        db.create_realm(&r2, "sparse", "", None, &Nat::ZERO).unwrap();
        put(&mut db, 1, b"committed".to_vec(), true).unwrap();
        let boundary = db.superblock().last_valid_end;
        put(&mut db, 2, b"flushed".to_vec(), false).unwrap();
        db.sync(true).unwrap(); // physically in the file, but behind the commit boundary
        put(&mut db, 3, vec![b'L'; 9 << 20], false).unwrap(); // written directly (> buffer threshold)
        put(&mut db, 4, b"buffered".to_vec(), false).unwrap(); // only in the buffer
        let cell = CellInput::typed(Nat::S(5), Nat::S(9), Val::str("sparse"), Some(TypeCode::Utf8));
        db.put_points(&r2, vec![cell], NL, false, 10_000).unwrap();
        assert!(db.get(&r, &Nat::S(4), NL, None, None, true).unwrap().is_some()); // visible before close
        assert_eq!(db.get_cell_value(&r2, &Nat::S(5), &Nat::S(9), NL).unwrap(), Some(Val::str("sparse")));
        db.close(false).unwrap();
        assert!(std::fs::metadata(&p).unwrap().len() > boundary, "uncommitted bytes reached the file");
        let invisible = |db: &Db| {
            for u in 2..=4 {
                assert!(db.get(&r, &Nat::S(u), NL, None, None, true).unwrap().is_none(), "record u={u} visible");
            }
            assert_eq!(db.get_cell_value(&r2, &Nat::S(5), &Nat::S(9), NL).unwrap(), None, "sparse cell visible");
            assert!(db.get(&r, &Nat::S(1), NL, None, None, true).unwrap().is_some());
            assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
        };
        drop(db);
        let mut db = Db::open(&p).unwrap();
        assert_eq!(db.superblock().last_valid_end, boundary);
        assert!(db.recovered_tail_bytes > 0);
        invisible(&db);
        // a later commit must not resurrect the discarded tail
        put(&mut db, 6, b"later".to_vec(), true).unwrap();
        invisible(&db);
        db.close(true).unwrap();
        let db = Db::open(&p).unwrap();
        invisible(&db);
        assert!(db.get(&r, &Nat::S(6), NL, None, None, true).unwrap().is_some());
    }

    /// A failed commit in close(commit=true) keeps the database open (as
    /// in the reference implementation) so it can be retried.
    #[test]
    fn failed_commit_on_close_keeps_database_open() {
        let p = tmp("commiterr");
        let mut db = bytes_db(&p);
        put(&mut db, 1, b"ok".to_vec(), true).unwrap();
        put(&mut db, 2, b"zwei".to_vec(), false).unwrap();
        db.store.fail_writes_beyond = Some(std::fs::metadata(&p).unwrap().len()); // nothing more can be written
        assert!(db.close(true).is_err());
        assert!(!db.closed && db.lock.is_held());
        db.store.fail_writes_beyond = None;
        db.close(true).unwrap(); // retry succeeds
        let db = Db::open(&p).unwrap();
        assert!(db.get(&Nat::S(1), &Nat::S(2), NL, None, None, true).unwrap().is_some());
        assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
    }
}
