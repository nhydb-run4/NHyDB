//! The native NHyDB engine (the `NHyDB` class).
//!
//! The implementation is split across several files:
//! `mod.rs` (state, open, commit, low-level I/O), `schema_ops.rs`,
//! `layout.rs`, `sparse.rs`, `dense.rs`, `read.rs`, `index.rs`,
//! `maint.rs`.

pub mod batch;
pub mod blockcache;
pub mod columns;
pub mod cursor;
pub mod delete;
pub mod dense;
pub mod directory;
pub mod idxspill;
pub mod index;
pub mod interval;
pub mod layout;
pub mod maint;
pub mod materialize;
pub mod math;
pub mod mempot;
pub mod merge;
pub mod read;
pub mod schema_ops;
pub mod secindex;
pub mod segindex;
pub mod sparse;
pub mod system;
pub mod vindex;
pub mod winners;

#[cfg(all(test, nhydb_internal_tests))]
mod complexity_tests;
#[cfg(all(test, nhydb_internal_tests))]
mod directory_tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::address::{lane_from_flags, Lane};
use crate::config::Config;
use crate::codec::{FieldDef, TypeCode};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, get_vu_u64, Nat};
use crate::platform::{self, DbLock, LockOverride};
use crate::plugin::LayoutPlugin;
use crate::schema::{ExtensionDef, IndexDef, LayoutDef, RealmDef, ShapeDef};
use crate::storage::{Storage, SyncMode};
use system::SystemState;
use crate::val::{Val, ValKey};
use crate::vmap;

use directory::{DelIndex, DirState};
use segindex::SegIndex;

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

/// Dense segments of one `(realm, lane)`: directory runs + in-memory tail
/// (`segindex.rs`) and aggregates that are kept without the segments.
#[derive(Default)]
pub struct DenseKeyIndex {
    pub(crate) idx: SegIndex<DenseSegment>,
    /// sum of `count` over all segments
    pub records: u64,
    /// shapes referenced by the segments
    pub shapes: std::collections::BTreeSet<u64>,
}

/// Sparse segments of one `(realm, lane)`.
#[derive(Default)]
pub struct SparseKeyIndex {
    pub(crate) idx: SegIndex<SparseSegment>,
    /// sums of `group_count` / `cell_count` over all segments
    pub groups: u64,
    pub cells: u64,
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

/// Merged sparse state of a u group: v → (newest state, id of the block it
/// comes from). The block id decides against a DENSE record (F1, see
/// [`dense_hides`]).
pub type SparseMap = BTreeMap<Nat, (SVal, u64)>;

/// F1 (0.28.2/delete-api-f1): the newest write of a cell wins, whatever
/// layer it went to. A DENSE record written in block `dense_bid` with
/// `shape` holds every field of that shape; a SPARSE state of one of those
/// fields written in an *older* block (`sparse_bid < dense_bid`) is hidden by
/// the record. States of v outside the shape (extensions, reserved BASE
/// positions, fields of other shapes) are never hidden: a `put` writes only
/// the fields of its shape. DELETE blocks hide the DENSE record only; a full
/// delete (`delete_record`) also tombstones every SPARSE state of the u.
#[inline]
pub(crate) fn dense_hides(shape: &ShapeDef, dense_bid: u64, v: &Nat, sparse_bid: u64) -> bool {
    sparse_bid < dense_bid && shape.by_v(v).is_some()
}

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

/// Key of the posting pool (`[cache] posting_cache_bytes`): decoded index
/// payloads of all kinds.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum PostKey {
    /// document-u posting of a key
    Us(u64, Lane, ValKey),
    /// FULLTEXT term posting with positions
    Ft(u64, Lane, String),
    /// FULLTEXT document lengths: one segment (by its encoded key) or, for
    /// indexes of 0.28.0/0.28.1, the whole list (empty key)
    DocLens(u64, Lane, Vec<u8>),
    /// AP9: base posting \ change mask ∪ overlay of a stale index, valid
    /// for one overlay version (`db/index/ixmaint.rs`)
    Ov(u64, Lane, ValKey, u64),
}

#[derive(Clone)]
pub(crate) enum PostVal {
    Us(Arc<Vec<Nat>>),
    Ft(Arc<Vec<crate::textindex::FtPosting>>),
    DocLens(Arc<crate::textindex::DocLenSeg>),
}

#[derive(Default)]
pub(crate) struct Caches {
    /// Sparse decode caches, one byte budget for the three
    /// (`[cache] sparse_cache_bytes`, `trim_sparse`): group directory of a
    /// sparse block (by block offset); decoded states per `(block_id, u)`
    /// (blocks are immutable, so writes never invalidate entries, P019;
    /// schema changes clear them, `clear_decode_caches`; rollbacks reset all
    /// caches); merged state of a u group `(realm, lane, u)`.
    pub(crate) sparse_block_index: mempot::BudgetMap<u64, Arc<SparseBlockIndex>>,
    pub(crate) sparse_record: mempot::BudgetMap<(u64, Nat), Arc<SparseStates>>,
    pub(crate) sparse_merged: mempot::BudgetMap<(Nat, Lane, Nat), Arc<SparseMap>>,
    /// v indexes per `(realm, lane, u)` (see `vindex.rs`)
    /// cached v indexes with their last use (LRU tick)
    pub sparse_uindex: HashMap<(Nat, Lane, Nat), (Arc<vindex::UIndex>, u64)>,
    /// estimated heap bytes of `sparse_uindex`
    pub uindex_bytes: usize,
    pub uindex_tick: u64,
    /// v index builds (complete) and catch-ups (later blocks only)
    pub uindex_builds: u64,
    pub uindex_catchups: u64,
    /// declared type per address and the tick of its last use (RUN8: LRU half)
    pub field_meta: HashMap<(Nat, Nat, Nat), (Option<FieldDef>, u64)>,
    pub field_meta_tick: u64,
    /// Block cache + per-block "verified" registry (`blockcache.rs`).
    pub block_cache: blockcache::BlockCache,
    /// Decoded index payloads (postings, FULLTEXT positions and document
    /// lengths), one byte budget.
    pub(crate) postings: mempot::BudgetMap<PostKey, PostVal>,
    /// Decoded pages of the SECONDARY key trees by block offset (LRU,
    /// budget `[cache] index_page_cache_bytes`).
    pub(crate) sec_pages: mempot::BudgetMap<u64, Arc<secindex::SecPage>>,
    /// pages read from the file
    pub(crate) sec_reads: u64,
    /// Decoded block directory pages with their last use (LRU, budget in
    /// entries, `directory.rs`).
    pub(crate) dir_pages: HashMap<u64, (directory::AnyPage, u64)>,
    pub(crate) dir_pages_used: usize,
    /// estimated heap bytes of `dir_pages`
    pub(crate) dir_pages_bytes: usize,
    pub(crate) dir_tick: u64,
    /// bytes of this handle booked in the process pot (`mempot.rs`)
    pub(crate) charged: usize,
    /// 0.28.2/scan: winner views built / not built (budget, compact form,
    /// metadata; `stats()["overlay_view"]`, since open or the last rollback)
    pub(crate) overlay_view_built: u64,
    pub(crate) overlay_view_declined: u64,
}

/// Estimated bytes of one `field_meta` entry: key, value, map slot and the
/// heap of the field definition (RB3-04).
const FIELD_META_COST: usize = std::mem::size_of::<((Nat, Nat, Nat), (Option<FieldDef>, u64))>() + 96;

/// Estimated heap bytes of a value (strings, bytes, containers).
pub fn val_heap(v: &Val) -> usize {
    match v {
        Val::Str(s) => s.capacity(),
        Val::Bytes(b) => b.capacity(),
        Val::Big(b) => b.bits() as usize / 8 + 16,
        Val::List(l) => l.capacity() * std::mem::size_of::<Val>() + l.iter().map(val_heap).sum::<usize>(),
        Val::Map(m) => m.capacity() * std::mem::size_of::<(String, Val)>() + m.iter().map(|(k, v)| k.capacity() + val_heap(v)).sum::<usize>(),
        _ => 0,
    }
}

pub(crate) fn nat_heap(n: &Nat) -> usize {
    match n {
        Nat::S(_) => 0,
        Nat::B(b) => b.bits() as usize / 8 + 32,
    }
}

fn sval_heap(s: &SVal) -> usize {
    match s {
        SVal::Tomb => 0,
        SVal::V(v) => val_heap(v),
    }
}

/// Estimated heap bytes of a group directory.
pub(crate) fn block_index_cost(v: &SparseBlockIndex) -> usize {
    v.capacity() * std::mem::size_of::<(Nat, u64, u32)>() + v.iter().map(|e| nat_heap(&e.0)).sum::<usize>() + 48
}

/// Estimated heap bytes of decoded record states.
pub(crate) fn states_cost(v: &SparseStates) -> usize {
    v.capacity() * std::mem::size_of::<(Nat, SVal)>() + v.iter().map(|(n, s)| nat_heap(n) + sval_heap(s)).sum::<usize>() + 48
}

/// Estimated heap bytes of a merged u group (BTreeMap: about 1.5 slots per
/// entry).
pub(crate) fn merged_cost(m: &SparseMap) -> usize {
    m.len() * std::mem::size_of::<(Nat, (SVal, u64))>() * 3 / 2 + m.iter().map(|(n, (s, _))| nat_heap(n) + sval_heap(s)).sum::<usize>() + 48
}

impl PostVal {
    pub(crate) fn cost(&self) -> usize {
        match self {
            PostVal::Us(v) => 48 + v.capacity() * std::mem::size_of::<Nat>() + v.iter().map(nat_heap).sum::<usize>(),
            PostVal::Ft(v) => {
                48 + v.capacity() * std::mem::size_of::<crate::textindex::FtPosting>()
                    + v.iter().map(|p| p.positions.capacity() * 8 + nat_heap(&p.u)).sum::<usize>()
            }
            PostVal::DocLens(s) => s.cost(),
        }
    }
}

impl Caches {
    pub(crate) fn sparse_bytes(&self) -> usize {
        self.sparse_block_index.bytes() + self.sparse_record.bytes() + self.sparse_merged.bytes()
    }

    /// Evicts from the three sparse caches (largest first) until they hold
    /// at most `limit` bytes together; returns the bytes freed.
    pub(crate) fn evict_sparse_to(&mut self, limit: usize) -> usize {
        let before = self.sparse_bytes();
        while self.sparse_bytes() > limit {
            let excess = self.sparse_bytes() - limit;
            let sizes = [self.sparse_block_index.bytes(), self.sparse_record.bytes(), self.sparse_merged.bytes()];
            let (i, &n) = sizes.iter().enumerate().max_by_key(|(_, b)| **b).expect("three");
            let to = n.saturating_sub(excess);
            let got = match i {
                0 => self.sparse_block_index.evict_to(to),
                1 => self.sparse_record.evict_to(to),
                _ => self.sparse_merged.evict_to(to),
            };
            if got == 0 {
                break;
            }
        }
        before - self.sparse_bytes()
    }

    /// Keeps the three sparse caches within their common budget (down to
    /// 3/4 of it once exceeded).
    pub(crate) fn trim_sparse(&mut self, budget: usize) {
        if self.sparse_bytes() > budget {
            self.evict_sparse_to(budget - budget / 4);
        }
    }

    /// Bytes held by this handle (what is booked in the process pot).
    pub(crate) fn tracked_bytes(&self) -> usize {
        self.sparse_bytes()
            + self.uindex_bytes
            + self.block_cache.tracked_bytes()
            + self.postings.bytes()
            + self.sec_pages.bytes()
            + self.dir_pages_bytes
            + self.field_meta_bytes()
    }

    /// Estimated heap bytes of the field type cache (entry, map slot and
    /// the name of the field definition).
    pub(crate) fn field_meta_bytes(&self) -> usize {
        self.field_meta.len() * FIELD_META_COST
    }

    /// Evicts about `want` bytes, least recently used first, from the pools
    /// in the order of their size. A pool that frees nothing is skipped, not
    /// the end of the eviction (RB3-02). The verified registry comes last:
    /// dropping it costs CRC checks again. Returns the bytes freed.
    pub(crate) fn shrink(&mut self, want: usize) -> usize {
        const POOLS: usize = 7;
        let mut freed = 0usize;
        let mut exhausted = [false; POOLS + 1];
        for _ in 0..(8 + POOLS + 1) {
            if freed >= want {
                break;
            }
            let pools = [
                self.postings.bytes(),
                self.sparse_bytes(),
                self.block_cache.cached_bytes(),
                self.sec_pages.bytes(),
                self.uindex_bytes,
                self.dir_pages_bytes,
                self.field_meta_bytes(),
            ];
            let pick = pools.iter().enumerate().filter(|(i, b)| !exhausted[*i] && **b > 0).max_by_key(|(_, b)| **b).map(|(i, b)| (i, *b));
            let (i, size) = match pick {
                Some(p) => p,
                None if !exhausted[POOLS] && self.block_cache.registry_bytes() > 0 => (POOLS, self.block_cache.registry_bytes()),
                None => break,
            };
            let take = (want - freed).min(size);
            let limit = size - take;
            let got = match i {
                0 => self.postings.evict_to(limit),
                1 => self.evict_sparse_to(limit),
                2 => self.block_cache.evict_to(limit),
                3 => self.sec_pages.evict_to(limit),
                4 => vindex::evict_uindex_to(self, limit),
                5 => directory::evict_dir_pages_to(self, limit),
                6 => {
                    self.field_meta.clear();
                    size
                }
                _ => self.block_cache.shrink_registry(take),
            };
            if got == 0 {
                exhausted[i] = true;
            }
            freed += got;
        }
        freed
    }
}

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

/// Read-ahead window of the open scan (P015). Small blocks are read
/// 64 KiB at a time (many headers/prefixes per `pread`), large blocks with
/// a 512-byte window (header + prefix in one `pread`, offset-table end in a
/// second one). Only bytes inside the logical file are read.
#[derive(Default)]
struct ScanBuf {
    off: u64,
    data: Vec<u8>,
    window: usize,
}

impl ScanBuf {
    const SMALL_WINDOW: usize = 512;
    const LARGE_WINDOW: usize = 64 << 10;
    const SMALL_BLOCK: u64 = 16 << 10;

    /// Window for the following reads, from the size of the current block.
    fn adapt(&mut self, block_bytes: u64) {
        self.window = if block_bytes <= Self::SMALL_BLOCK { Self::LARGE_WINDOW } else { Self::SMALL_WINDOW };
    }

    /// Exactly `len` bytes at `off` (errors like `Storage::read_exact_at`).
    fn get(&mut self, store: &Storage, off: u64, len: usize) -> Result<&[u8]> {
        let end = off.checked_add(len as u64).filter(|e| *e <= store.len()).ok_or_else(|| {
            Error::Corruption(format!("read {off}..{} beyond end of file {}", off.saturating_add(len as u64), store.len()))
        })?;
        let have = self.off + self.data.len() as u64;
        if off < self.off || end > have {
            let n = (self.window.max(Self::SMALL_WINDOW).max(len) as u64).min(store.len() - off) as usize;
            self.data.resize(n, 0);
            store.read_exact_at(off, &mut self.data)?;
            self.off = off;
        }
        let s = (off - self.off) as usize;
        Ok(&self.data[s..s + len])
    }
}

/// Open options.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    /// Read-only mode: `O_RDONLY`, no lock, the file is never modified
    /// (no tail truncation, no commit on close); write APIs fail.
    pub readonly: bool,
    /// Explicit durability mode (takes precedence over the configuration).
    pub sync_mode: Option<SyncMode>,
    /// Explicit paths of the sidecar files (take precedence over the configuration).
    pub layout_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    /// Already loaded configuration; otherwise the search order applies
    /// (`config_path`, `NHYDB_CONFIG`, file next to the DB, defaults).
    pub config: Option<Config>,
    pub config_path: Option<PathBuf>,
    /// Locking behaviour: the given fields override `[lock]` of the
    /// configuration field by field.
    pub lock: LockOverride,
    /// Internal compaction staging file (file name check is skipped).
    pub internal_compaction: bool,
    /// Ignore the block directory and scan every block on open (used by
    /// the compaction of a file whose directory is damaged).
    pub ignore_directory: bool,
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
        cfg.lock = self.lock.apply(cfg.lock);
        Ok(cfg)
    }

    /// Anchors explicit sidecar and configuration paths absolutely (against
    /// the current working directory at open time), so that a later
    /// `chdir` cannot redirect a schema commit into another directory.
    pub(crate) fn absolutized(&self) -> Result<OpenOptions> {
        let abs = |p: &Option<PathBuf>, what: &str| -> Result<Option<PathBuf>> {
            match p {
                None => Ok(None),
                Some(x) if x.as_os_str().is_empty() => Err(Error::Value(format!("{what} must not be empty"))),
                Some(x) => Ok(Some(platform::absolute(x)?)),
            }
        };
        let mut o = self.clone();
        o.layout_path = abs(&self.layout_path, "layout_path")?;
        o.key_path = abs(&self.key_path, "key_path")?;
        o.config_path = abs(&self.config_path, "config")?;
        Ok(o)
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
    /// Opened with `readonly=True` (no writes, no commit, no lock).
    pub readonly: bool,
    pub(crate) store: Storage,
    pub closed: bool,
    active_slot: usize,
    pub sup: SuperState,
    /// Content of the other (inactive) superblock slot, if valid.
    other_super: Option<SuperState>,
    /// G2-1: this handle wrote the active superblock itself (own commit in
    /// this session). Only then is the other slot mirrored on close (N2).
    own_super: bool,
    /// Warnings of the open (superblock fallback, key file permissions, …).
    pub open_warnings: Vec<String>,
    /// RUN¹⁰: an internal handle (temporary file of create/compaction); it
    /// neither defines the RAM limiter nor ends its statistics interval
    internal: bool,
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
    /// DELETE entries per `(realm, lane)` (directory runs + tail).
    pub(crate) deletes: HashMap<RL, DelIndex>,
    pub(crate) secondary_blocks: HashMap<(u64, Lane), index::SecondaryMeta>,
    pub(crate) secondary_source_block: HashMap<(u64, Lane), u64>,
    pub(crate) max_u: HashMap<RL, Nat>,
    /// Memory assumed for planning `build_indexes` instead of the physical
    /// memory (tests of the pass planning, I001).
    pub(crate) index_build_memory_override: Option<u64>,
    pub data_max_block_id: u64,
    pub(crate) data_max_block_id_by_lane: HashMap<RL, u64>,
    pub(crate) overlay_keys: HashSet<RL>,
    /// Keys with actually overlapping dense segments.
    pub(crate) dense_overlap_keys: HashSet<RL>,
    pub has_overlays: bool,
    pub last_checkpoint_obj: Val,
    pub(crate) dyn_mode: DynMode,
    pub(crate) dyn_v: Option<Nat>,
    pub(crate) caches: Arc<Mutex<Caches>>,
    /// AP9: overlays of stale indexes per `(realm, lane, index source)`
    /// (`db/index/ixmaint.rs`); derived, rebuilt on demand.
    pub(crate) overlays: Mutex<index::ixmaint::OverlayMap>,
    /// AP9: maintenance state of the writer per `(realm, lane)`.
    pub(crate) ixm: HashMap<RL, index::ixmaint::MaintState>,
    /// AP9: a maintenance step is running (its own commits do not start another).
    pub(crate) ixm_busy: bool,
    /// AP9: indexes defined since the last commit (built by `build_indexes`,
    /// not by the maintenance of the commit that publishes them).
    pub(crate) ixm_new_defs: HashSet<u64>,
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
    /// IT1: counts the events that discard data of this handle (rollback,
    /// dropped append buffer). Cursors remember it on their first batch and
    /// stop instead of reading block offsets that may be reused by new
    /// writes ([`Db::check_cursor_epoch`]).
    pub(crate) cursor_epoch: u64,
    /// Persistent block directory state (`directory.rs`).
    pub(crate) dir: DirState,
    /// flush mode of the next commit (`checkpoint`, compaction target)
    pub(crate) dir_force: Option<directory::FlushMode>,
    /// open without the block directory (`OpenOptions::ignore_directory`)
    pub(crate) ignore_directory: bool,
    /// Write path fills the sparse group-directory cache with the blocks it
    /// writes (off for the compaction target: nothing reads them there).
    pub(crate) write_cache: bool,
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
    ///
    /// The new triad (DB, plan, key) is first built under temporary names
    /// next to the targets; existing files are replaced only after that has
    /// succeeded (`overwrite=True`). Plan or key files that belong to
    /// another database are never deleted.
    pub fn create(path: impl AsRef<Path>, overwrite: bool, opts: OpenOptions) -> Result<Db> {
        let p = platform::absolute(path.as_ref())?;
        let p = p.as_path();
        if opts.readonly {
            return Err(Error::Value("create() cannot be combined with readonly=True".into()));
        }
        let opts = opts.absolutized()?;
        let cfg = opts.resolve_config(p)?;
        cfg.durability.check_supported()?;
        crate::config::check_db_max_size(cfg.db_max_size_bytes)?;
        let (lp, kp) = opts.sidecars(p, &cfg);
        validate_paths(p, &lp, &kp)?;
        let dir = platform::parent_dir(p);
        std::fs::create_dir_all(&dir)?;
        // G2-6: a lock file created by this call is removed again (while the
        // lock is still held) if the create fails and no database exists –
        // a pre-existing lock file is never deleted.
        let lock_file = DbLock::lock_path(p);
        let had_lock_file = std::fs::symlink_metadata(&lock_file).is_ok();
        let mut lock = Some(DbLock::acquire(p, cfg.lock)?);
        let res = Self::create_locked(p, overwrite, opts, cfg, lp, kp, &mut lock);
        if res.is_err() && lock.as_ref().is_some_and(|l| l.is_held()) && !had_lock_file && !p.exists() {
            let _ = std::fs::remove_file(&lock_file);
        }
        drop(lock);
        res
    }

    #[allow(clippy::too_many_arguments)]
    fn create_locked(
        p: &Path,
        overwrite: bool,
        opts: OpenOptions,
        cfg: Config,
        lp: PathBuf,
        kp: PathBuf,
        lock: &mut Option<DbLock>,
    ) -> Result<Db> {
        let existing: Vec<&Path> = [p, lp.as_path(), kp.as_path()].into_iter().filter(|x| x.exists()).collect();
        if !overwrite && !existing.is_empty() {
            let names: Vec<String> = existing.iter().map(|x| format!("'{}'", x.display())).collect();
            return Err(Error::Exists(format!("DB or associated external file already exists: {}", names.join(", "))));
        }
        if overwrite {
            check_sidecars_owned(p, &lp, &kp)?;
        }
        let mode = cfg.durability;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let fname = |q: &Path| q.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // 1) New triad under temporary names (same directories as the targets).
        let tdb = sibling_temp(p, "creating")?;
        let tlp = sibling_temp(&lp, "creating")?;
        let tkp = sibling_temp(&kp, "creating")?;
        let built = (|| -> Result<()> {
            let db_id = crate::crypto::uuid4()?;
            // F048: OS errors on the temporary side files name the target files
            let key = system::key_create(&tkp, &db_id, &name, mode).map_err(|e| e.at_path(&kp))?;
            let snapshot = system::empty_schema().to_json()?;
            let entry = system::seal_entry(&db_id, 0, snapshot.as_bytes(), &key)?;
            system::atomic_write(&tlp, &system::envelope_bytes(&db_id, std::slice::from_ref(&entry), &key)?, mode)
                .map_err(|e| e.at_path(&lp))?;
            let digest = system::entry_digest(&entry)?.to_string();
            let record = system::bootstrap_record(&db_id, &name, &fname(&lp), &fname(&kp), 0, &digest, now_ns());
            system::write_initial_db(&tdb, &record, mode)?;
            let o = OpenOptions {
                layout_path: Some(tlp.clone()),
                key_path: Some(tkp.clone()),
                internal_compaction: true,
                ..opts.clone()
            };
            let mut db = Db::open_locked(&tdb, &o, cfg.clone(), DbLock::held_elsewhere(&tdb))?;
            db.set_max_db_size(cfg.db_max_size_bytes, true)?;
            db.close(false)
        })();
        let temps = [tdb.clone(), tlp.clone(), tkp.clone()];
        if let Err(e) = built {
            for t in &temps {
                let _ = std::fs::remove_file(t);
            }
            return Err(e);
        }
        // 2) Swap: move existing files aside, put the new ones in place, and
        //    only then delete the old ones (roll back on error).
        let mut aside: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut placed: Vec<PathBuf> = Vec::new();
        let swap = (|| -> Result<()> {
            for old in [p, lp.as_path(), kp.as_path()] {
                if old.exists() {
                    let a = sibling_temp(old, "replaced")?;
                    std::fs::rename(old, &a).map_err(|e| Error::IoPath(e, old.into()))?;
                    aside.push((old.to_path_buf(), a));
                }
            }
            for (t, fin) in [(&tkp, &kp), (&tlp, &lp), (&tdb, &p.to_path_buf())] {
                std::fs::rename(t, fin).map_err(|e| Error::IoPath(e, fin.as_path().into()))?;
                placed.push(fin.clone());
                platform::sync_renamed(fin, mode).map_err(|e| e.at_path(fin))?; // R005 (Windows)
            }
            for d in [platform::parent_dir(p), platform::parent_dir(&lp), platform::parent_dir(&kp)] {
                platform::sync_dir(&d, mode)?;
            }
            Ok(())
        })();
        if let Err(e) = swap {
            for f in &placed {
                let _ = std::fs::remove_file(f);
            }
            for (old, a) in &aside {
                let _ = std::fs::rename(a, old);
            }
            for t in &temps {
                let _ = std::fs::remove_file(t);
            }
            return Err(e);
        }
        for (_, a) in &aside {
            let _ = std::fs::remove_file(a);
        }
        let o = OpenOptions { layout_path: Some(lp), key_path: Some(kp), ..opts };
        let lock = lock.take().unwrap_or_else(|| DbLock::none(p));
        Db::open_locked(p, &o, cfg, lock)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        Db::open_with(path, OpenOptions::default())
    }

    /// Opens an existing database (`NHyDB.__init__`).
    pub fn open_with(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Db> {
        let p = platform::absolute(path.as_ref())?;
        if !p.exists() {
            return Err(Error::NotFound(p.display().to_string()));
        }
        let opts = opts.absolutized()?;
        let cfg = opts.resolve_config(&p)?;
        cfg.durability.check_supported()?;
        let lock = if opts.readonly { DbLock::none(&p) } else { DbLock::acquire(&p, cfg.lock)? };
        let db = Db::open_locked(&p, &opts, cfg, lock)?;
        // E6 (R9B-04): a staging file of an in-place compaction that was
        // killed. A compaction holds the write lock of the database for its
        // whole run, so while this handle holds its own lock none can run:
        // the file is a leftover and removed (best effort; `health` reports
        // one that stays).
        if db.lock.is_held() {
            let _ = db.remove_compaction_leftover_unchecked();
        }
        Ok(db)
    }

    /// Opens with the lock already held. Fail closed: nothing in the file is
    /// modified before R0, key and plan have been verified, and the
    /// uncommitted tail is only discarded if the commit boundary is
    /// unambiguous (both superblocks valid and consistent) and the lock
    /// protects the file.
    pub(crate) fn open_locked(p: &Path, opts: &OpenOptions, cfg: Config, lock: DbLock) -> Result<Db> {
        if lock.guards() {
            return Self::open_attempt(p, opts, cfg, lock, &mut None);
        }
        // N1: without the lock (readonly=True or lock disabled) a writer may
        // publish schema generations while this handle reads superblock, R0
        // chain and layout plan one after another. The plan keeps only the
        // active and the newest generation, so after two concurrent schema
        // commits no plan entry matches the R0 generation read before. This
        // race is recognised (plan only newer than R0) and the open is
        // repeated as long as the superblock keeps moving; an unchanged
        // superblock with a non-matching plan stays a CorruptionError.
        let mut lock = Some(lock);
        let mut previous: Option<(u64, u64)> = None;
        let mut attempt = 0u32;
        loop {
            let l = lock.take().unwrap_or_else(|| DbLock::none(p));
            let mut race = None;
            let err = match Self::open_attempt(p, opts, cfg.clone(), l, &mut race) {
                Ok(db) => return Ok(db),
                Err(e) => e,
            };
            let Some(state) = race else { return Err(err) };
            if previous == Some(state) {
                return Err(err); // superblock unchanged: not a concurrent commit
            }
            attempt += 1;
            if attempt >= OPEN_RACE_ATTEMPTS {
                return Err(Error::Locked(format!(
                    "a concurrent writer published new schema generations during each of {OPEN_RACE_ATTEMPTS} attempts \
                     to open the database without lock (readonly or lock disabled); retry the open later"
                )));
            }
            previous = Some(state);
            std::thread::sleep(std::time::Duration::from_millis(1u64 << attempt.min(6)));
        }
    }

    /// One open attempt (see `open_locked`). `race` is set to the superblock
    /// state (generation, commit boundary) when the open failed only because
    /// the layout plan is newer than the R0 record read (unguarded open).
    fn open_attempt(p: &Path, opts: &OpenOptions, cfg: Config, lock: DbLock, race: &mut Option<(u64, u64)>) -> Result<Db> {
        let (lp, kp) = opts.sidecars(p, &cfg);
        validate_paths(p, &lp, &kp)?;
        let mut store = Storage::open_mode(p, cfg.durability, opts.readonly)?;
        let physical = store.len();
        if physical < DATA_START {
            return Err(Error::Corruption(format!(
                "file is shorter than the superblock area ({physical} < {DATA_START} bytes): not an NHyDB {FILE_FORMAT_VERSION} database or truncated"
            )));
        }
        let slots = read_slots(&store)?;
        let j = judge_superblocks(&slots, physical);
        let mut warnings = Vec::new();
        let Some((active_slot, sup)) = j.chosen else {
            return Err(Error::Corruption(format!(
                "{}; no usable superblock. Nothing was changed; inspect with NHyDB.inspect_tail() and run repair_file()",
                j.problem.unwrap_or_default()
            )));
        };
        let mut committed = sup.last_valid_end;
        let mut tail = physical - committed;
        if !j.clean {
            let problem = j.problem.clone().unwrap_or_default();
            let harmless = j.newest && tail == 0;
            if !harmless && !opts.readonly {
                return Err(Error::Corruption(format!(
                    "{problem}. The commit boundary is ambiguous: the {tail} bytes after the boundary {committed} of the \
                     usable superblock (generation {}) may contain committed data. Nothing was changed. Open with \
                     readonly=True to read the state of the usable superblock, or run repair_file() with \
                     keep_uncommitted=True (keep these blocks) or allow_data_loss=True (discard them)",
                    sup.generation
                )));
            }
            warnings.push(format!("{problem}; opened at superblock generation {} (commit boundary {committed})", sup.generation));
        }
        if tail > 0 {
            if opts.readonly {
                // uncommitted tail: invisible, reported as recovered_tail_bytes
                store.limit_view(committed);
            } else if !lock.guards() {
                return Err(Error::Locked(format!(
                    "{tail} uncommitted bytes after the commit boundary {committed} (possibly from a concurrent writer); \
                     with the lock disabled they cannot be discarded safely. Open with the lock enabled or readonly=True"
                )));
            }
        }
        let other_super = slots[1 - active_slot].sup;
        let mut db = Db {
            path: p.to_path_buf(),
            readonly: opts.readonly,
            store,
            closed: false,
            active_slot,
            sup,
            other_super,
            own_super: false,
            open_warnings: Vec::new(),
            internal: opts.internal_compaction,
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
            index_build_memory_override: None,
            data_max_block_id: 0,
            data_max_block_id_by_lane: HashMap::new(),
            overlay_keys: HashSet::new(),
            dense_overlap_keys: HashSet::new(),
            has_overlays: false,
            last_checkpoint_obj: Val::map(),
            dyn_mode: DynMode::Off,
            dyn_v: None,
            caches: Arc::new(Mutex::new(Caches::default())),
            overlays: Mutex::new(HashMap::new()),
            ixm: HashMap::new(),
            ixm_busy: false,
            ixm_new_defs: HashSet::new(),
            config: cfg,
            lock,
            sys: SystemState {
                record: Val::None,
                key: Default::default(),
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
            cursor_epoch: 0,
            dir: DirState::default(),
            dir_force: None,
            ignore_directory: opts.ignore_directory,
            write_cache: true,
        };
        // 1) Block directory of the superblock plus one scan over the
        //    committed blocks behind it (or the full scan): register data
        //    blocks, verify the R0 chain, reject layout metadata inside the DB.
        let record = db.load_storage_state(committed)?;
        #[cfg(test)]
        AFTER_OPEN_SCAN.with(|h| {
            if let Some(f) = h.borrow_mut().as_mut() {
                f();
            }
        });
        // 2) Identity, key, plan, generation (fail closed).
        let cells_name = record.get("cells").and_then(|c| c.get("2")).map(|v| v.py_str()).unwrap_or_default();
        if !opts.internal_compaction && !platform::stored_name_is(p, &cells_name) {
            let fname = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Err(Error::Corruption(format!(
                "DB file name '{fname}' does not match the name '{cells_name}' bound in R0,u1: the database file must keep \
                 the name it was created with (rename it back, or create a copy under a new name with \
                 compact_file(output=...))"
            )));
        }
        db.sys.record = record;
        let db_id = db.sys.db_id();
        db.sys.key = system::key_load(&db.sys.key_path, &db_id, &cells_name)?;
        if let Some(w) = system::key_permission_warning(&db.sys.key_path) {
            warnings.push(w);
        }
        let mut catch_ups = 0u32;
        let active_entry = loop {
            db.sys.entries = system::read_envelope(&db.sys.layout_path, &db_id, &db.sys.key)?;
            let generation = db.sys.generation()?;
            let digest = db.sys.cell("6").map(|v| v.py_str()).unwrap_or_default();
            let matches: Vec<&Val> = db
                .sys
                .entries
                .iter()
                .filter(|e| {
                    system::entry_generation(e).ok() == Some(generation) && system::entry_digest(e).ok() == Some(digest.as_str())
                })
                .collect();
            if matches.len() == 1 {
                break matches[0].clone();
            }
            // N1: plan only newer than the R0 record read – a writer
            // published schema generations during this unguarded open.
            let plan_newer = !db.lock.guards()
                && matches.is_empty()
                && !db.sys.entries.is_empty()
                && db.sys.entries.iter().all(|e| system::entry_generation(e).is_ok_and(|g| g > generation));
            if plan_newer && opts.readonly && catch_ups < OPEN_CATCH_UP_LIMIT {
                // Append-only file: continue the scan from the old commit
                // boundary to the boundary of the newer superblock instead of
                // reading the whole file again.
                if let Some((slot, newer, other, new_tail)) = db.newer_clean_superblock(committed)? {
                    catch_ups += 1;
                    db.store.extend_view(newer.last_valid_end)?;
                    let record = db.open_scan_range(committed, newer.last_valid_end, Some(db.sys.record.clone()))?;
                    db.sys.record = record;
                    db.active_slot = slot;
                    db.other_super = other;
                    db.sup = newer;
                    db.checkpoint_offset = newer.checkpoint_offset;
                    db.next_block_id = db.next_block_id.max(newer.next_block_id);
                    committed = newer.last_valid_end;
                    tail = new_tail;
                    if db.sys.db_id() != db_id {
                        return Err(Error::Corruption("R0,u1 database id changed during the open".into()));
                    }
                    continue;
                }
            }
            if plan_newer {
                *race = Some((db.sup.generation, db.sup.last_valid_end));
            }
            return Err(Error::Corruption("no layout plan matching R0,u1 generation and hash".into()));
        };
        db.sys.active_entry = active_entry;
        let snapshot = system::open_entry(&db_id, &db.sys.active_entry, &db.sys.key)?;
        db.import_schema(&snapshot)?;
        db.validate_math_layout_state()?;
        db.check_segment_plausibility()?;
        // 3) Only now: discard the uncommitted tail (unambiguous boundary,
        //    lock held – checked above).
        if tail > 0 {
            db.recovered_tail_bytes = tail;
            if !opts.readonly {
                db.store.truncate(committed)?;
                db.invalidate_blocks_from(committed);
            }
        }
        warnings.append(&mut db.open_warnings);
        db.open_warnings = warnings;
        db.register_caches();
        Ok(db)
    }

    /// N1 catch-up: re-reads both superblock slots of a read-only handle and
    /// returns the newest one if it is clean (both slots valid and
    /// consistent) and lies beyond the current boundary `committed`:
    /// (slot, state, other slot, bytes after its boundary). `None`: no
    /// usable newer state (the caller reports the plan mismatch).
    #[allow(clippy::type_complexity)]
    fn newer_clean_superblock(&self, committed: u64) -> Result<Option<(usize, SuperState, Option<SuperState>, u64)>> {
        let physical = self.store.file_len()?;
        let slots = read_slots(&self.store)?;
        let j = judge_superblocks(&slots, physical);
        Ok(match j.chosen {
            Some((slot, s)) if j.clean && s.generation > self.sup.generation && s.last_valid_end >= committed => {
                Some((slot, s, slots[1 - slot].sup, physical - s.last_valid_end))
            }
            _ => None,
        })
    }

    /// Plausibility of the DATA routing prefixes registered by the open scan
    /// (read without payload CRC, protected by the prefix checksum, F021):
    /// every DATA segment
    /// must refer to a published shape of its own realm (`put_dense_batch`
    /// only writes a shape of the target realm, and a shape never changes
    /// its realm), so a flipped realm varint is rejected here. A flipped
    /// `u_start` of the same length is caught when a read misses the
    /// displaced segment (`check_uncovered_u`). SPARSE prefixes are covered
    /// by the prefix checksum.
    fn check_segment_plausibility(&self) -> Result<()> {
        // The tail segments carry their block; the persisted ones are
        // represented by the shape set of their key (directory aggregate).
        for ((realm, _lane), idx) in &self.dense {
            for seg in &idx.idx.tail {
                let Some(shape) = self.shapes.get(&seg.shape_id) else {
                    return Err(Error::Corruption(format!(
                        "DATA block {} (byte {}) refers to unknown shape {}",
                        seg.block_id, seg.block_offset, seg.shape_id
                    )));
                };
                if shape.realm_id != seg.realm_id {
                    return Err(Error::Corruption(format!(
                        "DATA block {} (byte {}): realm {} does not match realm {} of shape {}",
                        seg.block_id, seg.block_offset, seg.realm_id, shape.realm_id, seg.shape_id
                    )));
                }
            }
            for sid in &idx.shapes {
                match self.shapes.get(sid) {
                    None => return Err(Error::Corruption(format!("DATA segments of realm {realm} refer to unknown shape {sid}"))),
                    Some(shape) if &shape.realm_id != realm => {
                        return Err(Error::Corruption(format!(
                            "DATA segments of realm {realm} refer to shape {sid} of realm {}",
                            shape.realm_id
                        )))
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Resets all state derived from the file and schema
    /// (rollback of a failed MATH batch).
    pub(crate) fn reset_in_memory_state(&mut self) {
        // AP9: overlays and counters describe the discarded state
        self.ixm_reset();
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
        self.math_layouts.clear();
        *self.cache() = Caches::default();
        let stats = self.dir.stats;
        self.dir = DirState { stats, ..DirState::default() };
    }

    pub fn sync_mode(&self) -> SyncMode {
        self.store.sync_mode
    }

    pub fn set_sync_mode(&mut self, m: SyncMode) -> Result<()> {
        if self.closed {
            return Err(Error::NHyDB("database is closed".into()));
        }
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

    /// Current cursor epoch (IT1): changes whenever this handle discards
    /// data that an open cursor may still refer to.
    pub fn cursor_epoch(&self) -> u64 {
        self.cursor_epoch
    }

    /// IT1: binds a cursor to the epoch of its first batch; afterwards a
    /// changed epoch (rollback, dropped append buffer) ends the cursor with
    /// an error instead of reading offsets of discarded blocks, which a new
    /// write may already have reused.
    pub fn check_cursor_epoch(&self, bound: &mut Option<u64>) -> Result<()> {
        match *bound {
            None => {
                *bound = Some(self.cursor_epoch);
                Ok(())
            }
            Some(e) if e == self.cursor_epoch => Ok(()),
            Some(_) => Err(Error::NHyDB(CURSOR_INVALIDATED.into())),
        }
    }

    /// `close(commit=True)`. A read-only database is closed without
    /// commit.
    pub fn close(&mut self, commit: bool) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        if commit && !self.readonly {
            // On a failed commit the database stays open (as in the reference
            // implementation): retry, or close(commit=false) to discard.
            // Exception: a poisoned database (fsync error or failed schema
            // commit) can never commit again – handle and lock are released
            // and the error is returned.
            if let Err(e) = self.commit().and_then(|_| self.mirror_super()) {
                if !self.sys.poisoned {
                    return Err(e);
                }
                let _ = self.store.close();
                self.closed = true;
                self.lock.release();
                return Err(e);
            }
        }
        // Release the OS file handle immediately (not only on drop), so the
        // file can be replaced by compact_file()/repair or renamed while the
        // closed object is still referenced (Windows). The handle and the
        // lock are released even if writing the buffer fails.
        let res = self.store.close();
        self.closed = true;
        self.lock.release();
        // F028: no read is answered from caches after close (C_index).
        *self.cache() = Caches::default();
        mempot::POT.unregister(&self.caches, !self.internal);
        res
    }

    /// `sync(hard=False)`: write buffers, optionally fsync.
    pub fn sync(&mut self, hard: bool) -> Result<()> {
        self.check_open()?;
        if self.readonly {
            return Ok(());
        }
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
    ///
    /// Every fsync/superblock error poisons the handle (a retried fsync may
    /// report success although the data was lost, "fsyncgate"): the
    /// database must be reopened. A commit without changes writes no
    /// superblock once both slots hold the current state.
    pub fn commit(&mut self) -> Result<u64> {
        let end = self.commit_core()?;
        // AP9: automatic index maintenance after a successful data commit
        // (its own commit; never fails the data commit)
        if self.config.index_maint.auto && !self.ixm_busy {
            self.ixm_after_commit();
        }
        self.ixm_new_defs.clear();
        Ok(end.max(self.sup.last_valid_end))
    }

    fn commit_core(&mut self) -> Result<u64> {
        if self.closed {
            return Err(Error::NHyDB("database is closed".into()));
        }
        if self.readonly {
            return Err(Error::NHyDB("database is opened read-only (readonly=True); commit is not possible".into()));
        }
        if self.sys.poisoned {
            return Err(Error::NHyDB(
                "security/durability state unclear after a failed commit (fsync or schema publication); reopen the database".into(),
            ));
        }
        if self.sys.layout_dirty {
            // G2-1: without the lock, never publish a plan over the state of
            // a concurrent writer.
            self.check_no_concurrent_writer()?;
            let new_gen = self.sys.generation()? + 1;
            let db_id = self.sys.db_id();
            let snapshot = self.export_schema()?;
            let snap_json = snapshot.to_json()?;
            let next_entry = system::seal_entry(&db_id, new_gen, snap_json.as_bytes(), &self.sys.key)?;
            let env = system::envelope_bytes(&db_id, &[self.sys.active_entry.clone(), next_entry.clone()], &self.sys.key)?;
            system::atomic_write(&self.sys.layout_path, &env, self.store.sync_mode)?;
            let next_record = system::next_record(&self.sys.record, new_gen, system::entry_digest(&next_entry)?);
            let (sys_off, _) = self.append_block(BT_SYSTEM, next_record.to_json()?.as_bytes(), 0)?;
            let prev_sys_off = self.dir.last_system_off;
            self.dir.last_system_off = sys_off;
            let mode = self.dir_force.take().unwrap_or(directory::FlushMode::Commit);
            if let Err(e) = self.maybe_flush_directory(mode) {
                self.dir.last_system_off = prev_sys_off;
                self.sys.poisoned = true;
                return Err(e);
            }
            let r = (|| -> Result<()> {
                self.store.flush()?;
                self.store.fsync_barrier()?;
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
            // G2-1/G2-4: no blocks appended since the last commit – no
            // barrier, no superblock (a handle that changed nothing never
            // overwrites the commit boundary of another writer).
            let mode = self.dir_force.take().unwrap_or(directory::FlushMode::Commit);
            if !self.has_pending() && !(mode != directory::FlushMode::Commit && self.directory_checkpoint_pending(mode)) {
                return Ok(self.sup.last_valid_end);
            }
            self.check_no_concurrent_writer()?;
            self.maybe_flush_directory(mode)?;
            // A failed write keeps the buffer (retry possible); a failed
            // fsync or superblock write poisons the handle.
            self.store.flush()?;
            let r = (|| -> Result<()> {
                self.store.fsync_barrier()?;
                self.commit_super()
            })();
            if let Err(e) = r {
                self.sys.poisoned = true;
                return Err(e);
            }
        }
        Ok(self.sup.last_valid_end)
    }

    /// Blocks were appended (or the checkpoint moved) since the last commit
    /// of this handle.
    fn has_pending(&self) -> bool {
        self.store.len() != self.sup.last_valid_end
            || self.last_valid_end != self.sup.last_valid_end
            || self.checkpoint_offset != self.sup.checkpoint_offset
    }

    /// G2-1: with the lock disabled (`lock=False`) another process may have
    /// committed since this handle read the superblock. Before every
    /// superblock or plan write the slots are read again: a newer generation,
    /// a different commit boundary or bytes appended by someone else mean a
    /// concurrent writer – writing would reset its commit boundary. Returns
    /// a description of the conflict, `None` if the file is unchanged.
    fn concurrent_writer(&self) -> Result<Option<String>> {
        if self.lock.guards() {
            return Ok(None);
        }
        let slots = read_slots(&self.store)?;
        let newest = slots.iter().filter_map(|s| s.sup).max_by_key(|s| s.generation);
        let physical = self.store.file_len()?;
        let conflict = match newest {
            Some(s) if s.generation != self.sup.generation || s.last_valid_end != self.sup.last_valid_end => Some(format!(
                "the file has superblock generation {} (commit boundary {}), this handle is based on generation {} \
                 (commit boundary {})",
                s.generation, s.last_valid_end, self.sup.generation, self.sup.last_valid_end
            )),
            Some(_) if physical > self.store.len() => Some(format!(
                "the file has grown to {physical} bytes behind this handle's end {}",
                self.store.len()
            )),
            Some(_) => None,
            None => Some("no valid superblock in the file".into()),
        };
        Ok(conflict)
    }

    fn check_no_concurrent_writer(&mut self) -> Result<()> {
        if let Some(c) = self.concurrent_writer()? {
            // The appended blocks of this handle can never be committed;
            // unwritten ones are dropped so that neither close() nor drop
            // writes them over the other writer's blocks.
            self.sys.poisoned = true;
            self.store.discard_pending();
            self.cursor_epoch += 1;
            let end = self.store.len();
            self.invalidate_blocks_from(end);
            return Err(Error::Locked(format!(
                "concurrent writer detected: {c}. The database was opened with lock=False; nothing was committed and \
                 the commit boundary of the other writer was left unchanged. Use the lock for writers and readonly=True \
                 for readers"
            )));
        }
        Ok(())
    }

    /// N2: after the closing commit, bring the second superblock slot to the
    /// committed state as well, so that a cleanly closed database always has
    /// two current, consistent superblocks (a later bit error in one slot then
    /// falls back transparently instead of making the commit boundary
    /// ambiguous). Writes only when this handle committed itself and the
    /// other slot is stale – an unchanged database writes nothing (F012,
    /// G2-1). The data is already durable (the commit before synchronized
    /// it), so only the superblock is written and synchronized; a torn
    /// mirror write leaves the active slot as the newest valid one with the
    /// same boundary (opens with a warning). Without the lock the mirror is
    /// skipped if another writer has committed in the meantime.
    fn mirror_super(&mut self) -> Result<()> {
        let s = &self.sup;
        let mirrored = self.other_super.as_ref().is_some_and(|o| {
            o.last_valid_end == s.last_valid_end
                && o.checkpoint_offset == s.checkpoint_offset
                && o.next_block_id == s.next_block_id
                && o.dir_root == s.dir_root
        });
        if !self.own_super || mirrored || self.has_pending() || self.concurrent_writer()?.is_some() {
            return Ok(());
        }
        if let Err(e) = self.commit_super() {
            self.sys.poisoned = true;
            return Err(e);
        }
        Ok(())
    }

    fn commit_super(&mut self) -> Result<()> {
        // G2-1: last check directly before the superblock write.
        self.check_no_concurrent_writer()?;
        let slot = 1 - self.active_slot;
        let s = SuperState {
            generation: self.sup.generation + 1,
            checkpoint_offset: self.checkpoint_offset,
            last_valid_end: self.last_valid_end,
            next_block_id: self.next_block_id,
            created_ns: self.sup.created_ns,
            updated_ns: now_ns(),
            flags: self.sup.flags,
            dir_root: self.dir.root_off,
        };
        self.store.write_at((slot * SUPERBLOCK_SIZE) as u64, &pack_super(&s))?;
        self.store.fsync()?;
        self.active_slot = slot;
        self.other_super = Some(self.sup);
        self.sup = s;
        self.own_super = true;
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
        let (off, _) = self.append_block_limit(BT_DB_CONFIG, &[payload.as_bytes()], 0, Some(size_bytes))?;
        self.dir.last_config_off = off;
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
        if self.readonly {
            return Err(Error::NHyDB("database is opened read-only (readonly=True); writing is not possible".into()));
        }
        if self.sys.poisoned {
            return Err(Error::NHyDB("security state unclear; reopen the database".into()));
        }
        if is_layout_meta(bt) {
            self.sys.layout_dirty = true;
            return Ok((self.last_valid_end, self.next_block_id));
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
                self.invalidate_blocks_from(off);
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
        if off < DATA_START || off.checked_add(BLOCK_HEADER_SIZE as u64).is_none_or(|e| e > size) {
            return Err(Error::Corruption(format!("invalid block offset {off}")));
        }
        let mut raw = [0u8; BLOCK_HEADER_SIZE];
        self.store.read_exact_at(off, &mut raw)?;
        Self::header_info(off, &raw, size)
    }

    /// Checks a raw block header read at `off` (file length `size`).
    fn header_info(off: u64, raw: &[u8], size: u64) -> Result<BlockInfo> {
        let h = unpack_block_header(raw)?;
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
        let payload = self.read_payload_checked(&info)?;
        Ok((info, payload))
    }

    /// Payload of the block `info` describes, CRC-checked.
    pub(crate) fn read_payload_checked(&self, info: &BlockInfo) -> Result<Vec<u8>> {
        let payload = self.store.read_vec(info.offset + BLOCK_HEADER_SIZE as u64, info.payload_len as usize)?;
        if crc32(&payload) != info.payload_crc {
            return Err(Error::Corruption(format!("block {}: payload CRC mismatch", info.block_id)));
        }
        Ok(payload)
    }

    #[inline]
    pub(crate) fn cache(&self) -> mempot::CacheGuard<'_> {
        mempot::CacheGuard::new(&self.caches)
    }

    /// Joins the process cache pot (`mempot.rs`); a different
    /// `cache.total_bytes` than the one in force is reported as a warning.
    fn register_caches(&mut self) {
        let src = self.config.source.clone();
        if let Some(w) = mempot::POT.register(&self.caches, self.config.cache_total_bytes, src.clone()) {
            self.open_warnings.push(w);
        }
        // RUN¹⁰: the first handle of the process defines the limiter
        if !self.internal {
            let lim = crate::limiter::global();
            if let Some(w) = lim.configure_for(&self.config.limiter, src, Some(&self.path)) {
                self.open_warnings.push(w);
            }
            lim.start_log_clock();
        }
    }

    pub(crate) fn clear_decode_caches(&mut self) {
        let mut c = self.cache();
        c.field_meta.clear();
        c.sparse_record.clear();
        c.sparse_merged.clear();
        c.sparse_uindex.clear();
        c.uindex_bytes = 0;
    }

    // ------------------------------------------------------------------
    // Open-Scan
    // ------------------------------------------------------------------

    /// Reads and parses the variable-length prefix of a data block.
    ///
    /// Realm and u are unbounded VarUInts (F003), so the prefix has no fixed
    /// size: the window starts small (fast path) and grows up to the payload
    /// length as long as the parser reports a truncated value.
    fn read_block_prefix<T>(&self, sb: &mut ScanBuf, payload_off: u64, payload_len: u64, parse: impl Fn(&[u8]) -> Result<T>) -> Result<T> {
        let total = usize::try_from(payload_len).unwrap_or(usize::MAX);
        let mut window = 96.min(total).max(4);
        loop {
            let avail = usize::try_from(self.store.len().saturating_sub(payload_off)).unwrap_or(usize::MAX);
            let n = window.min(avail);
            let raw = sb.get(&self.store, payload_off, n)?;
            match parse(raw) {
                Err(Error::Corruption(_)) if window < total && n == window => {
                    window = window.saturating_mul(8).min(total);
                }
                r => return r,
            }
        }
    }

    fn parse_dense_prefix(&self, info: &BlockInfo, sb: &mut ScanBuf) -> Result<DenseSegment> {
        let payload_off = info.offset + BLOCK_HEADER_SIZE as u64;
        let (realm, shape, u_start, count, p) = self.read_block_prefix(sb, payload_off, info.payload_len, |raw| {
            if !raw.starts_with(DATA_TAG) {
                return Err(Error::Corruption(format!("DATA block {}: wrong tag", info.block_id)));
            }
            let mut p = 4;
            let realm = get_vu(raw, &mut p)?;
            let shape = get_vu_u64(raw, &mut p, "DATA shape")?;
            let u_start = get_vu(raw, &mut p)?;
            let count = get_vu_u64(raw, &mut p, "DATA count")?;
            // RD-08: the routing prefix is checked on open
            check_prefix_crc(raw, &mut p).map_err(|e| Error::Corruption(format!("DATA block {} (byte {}): {e}", info.block_id, info.offset)))?;
            Ok((realm, shape, u_start, count, p))
        })?;
        if u_start.is_zero() {
            // u = 0 is no valid Dynadron coordinate (F021 plausibility)
            return Err(Error::Corruption(format!("DATA block {}: u_start = 0", info.block_id)));
        }
        let offsets_off = payload_off + p as u64;
        let records_off = count
            .checked_add(1)
            .and_then(|c| c.checked_mul(4))
            .and_then(|t| t.checked_add(offsets_off))
            .unwrap_or(u64::MAX);
        if records_off > payload_off + info.payload_len {
            return Err(Error::Corruption(format!("DATA block {}: offset table too large", info.block_id)));
        }
        // Plausibility of the offset table (not covered by the prefix
        // checksum): its last entry must end exactly at the end of the payload.
        let last: [u8; 4] = sb.get(&self.store, records_off - 4, 4)?.try_into().expect("4 bytes");
        if records_off + u32::from_le_bytes(last) as u64 != payload_off + info.payload_len {
            return Err(Error::Corruption(format!(
                "DATA block {} (byte {}): prefix and offset table contradict each other",
                info.block_id, info.offset
            )));
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

    fn parse_sparse_prefix(&self, info: &BlockInfo, sb: &mut ScanBuf) -> Result<SparseSegment> {
        let payload_off = info.offset + BLOCK_HEADER_SIZE as u64;
        let (realm, u_start, u_end, group_count, cell_count, p) =
            self.read_block_prefix(sb, payload_off, info.payload_len, |raw| {
                if !raw.starts_with(SPARSE_TAG) {
                    return Err(Error::Corruption(format!("SPARSE block {}: wrong tag", info.block_id)));
                }
                let mut p = 4;
                let realm = get_vu(raw, &mut p)?;
                let u_start = get_vu(raw, &mut p)?;
                let u_end = get_vu(raw, &mut p)?;
                let group_count = get_vu_u64(raw, &mut p, "SPARSE groups")?;
                let cell_count = get_vu_u64(raw, &mut p, "SPARSE cells")?;
                check_prefix_crc(raw, &mut p).map_err(|e| Error::Corruption(format!("SPARSE block {} (byte {}): {e}", info.block_id, info.offset)))?;
                Ok((realm, u_start, u_end, group_count, cell_count, p))
            })?;
        if group_count == 0 || cell_count == 0 || u_start.is_zero() || u_end < u_start {
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

    pub(crate) fn register_segment(&mut self, seg: DenseSegment) -> Result<()> {
        let key: RL = (seg.realm_id.clone(), seg.lane);
        let bid = seg.block_id;
        // Overlap with an earlier segment: decided once per key (the flag
        // never goes back); beyond the largest u end no lookup is needed.
        if !self.dense_overlap_keys.contains(&key) {
            let beyond = self.max_u.get(&key).is_none_or(|m| &seg.u_start > m);
            let overlap = !beyond
                && match self.dense.get(&key) {
                    Some(idx) => idx.idx.any_overlap(self, &seg.u_start, &seg.u_end)?,
                    None => false,
                };
            if overlap {
                self.has_overlays = true;
                self.overlay_keys.insert(key.clone());
                self.dense_overlap_keys.insert(key.clone());
            }
        }
        let mu = self.max_u.entry(key.clone()).or_insert(Nat::ZERO);
        if seg.u_end > *mu {
            *mu = seg.u_end.clone();
        }
        self.dir.tail_entries += 1;
        self.dir.tail_bytes += seg.payload_len;
        let idx = self.dense.entry(key.clone()).or_default();
        idx.records += seg.count;
        idx.shapes.insert(seg.shape_id);
        let n = seg.count;
        idx.idx.push(seg);
        self.note_data_block(&key, bid);
        self.ixm_note_written(&key, n);
        Ok(())
    }

    pub(crate) fn register_sparse_segment(&mut self, seg: SparseSegment) {
        let key: RL = (seg.realm_id.clone(), seg.lane);
        let bid = seg.block_id;
        self.dir.tail_entries += 1;
        self.dir.tail_bytes += seg.payload_len;
        let idx = self.sparse.entry(key.clone()).or_default();
        idx.groups += seg.group_count;
        idx.cells += seg.cell_count;
        let n = seg.group_count;
        idx.idx.push(seg);
        self.note_data_block(&key, bid);
        self.ixm_note_written(&key, n);
        self.has_overlays = true;
        self.overlay_keys.insert(key);
    }

    /// Registers a DELETE of `(realm, lane, u)` by block `bid`.
    pub(crate) fn register_delete(&mut self, realm: Nat, lane: Lane, u: Nat, bid: u64) {
        let key: RL = (realm, lane);
        self.dir.tail_entries += 1;
        let d = self.deletes.entry(key.clone()).or_default();
        let e = d.tail.entry(u).or_insert(0);
        *e = (*e).max(bid);
        self.note_data_block(&key, bid);
        self.ixm_note_written(&key, 1);
        self.has_overlays = true;
        self.overlay_keys.insert(key);
    }

    /// One scan over the committed range `[DATA_START, end)`: registers data
    /// blocks, verifies the R0,u1 chain and rejects layout metadata inside the
    /// DB. Returns the current R0 record.
    fn open_scan(&mut self, end: u64) -> Result<Val> {
        self.open_scan_range(DATA_START, end, None)
    }

    /// `open_scan` for `[start, end)`, continuing the R0 chain after
    /// `record` (the state of a scan up to `start`; N1 catch-up of an
    /// unguarded read-only open).
    fn open_scan_range(&mut self, start: u64, end: u64, mut record: Option<Val>) -> Result<Val> {
        let mut off = start;
        let mut maxid = 0u64;
        // RA2-02: a continued scan starts behind a known block (directory
        // root, earlier scan); its first block is checked against that id
        let mut prev_id = if start > DATA_START { self.dir.scan_last_id } else { 0 };
        // P015: headers, prefixes and offset-table ends of consecutive
        // blocks come from one read-ahead window instead of 3 reads per block.
        let mut sb = ScanBuf::default();
        while off < end {
            let size = self.store.len();
            if off < DATA_START || off + BLOCK_HEADER_SIZE as u64 > size {
                return Err(Error::Corruption(format!("invalid block offset {off}")));
            }
            let info = match Self::header_info(off, sb.get(&self.store, off, BLOCK_HEADER_SIZE)?, size) {
                Ok(i) => i,
                Err(e) => {
                    // RA-01: a directory block with a damaged header is
                    // redundant – skip it (the directory falls back anyway)
                    let mut raw = [0u8; BLOCK_HEADER_SIZE];
                    self.store.read_exact_at(off, &mut raw)?;
                    let read_at = |buf: &mut [u8], o: u64| self.store.read_exact_at(o, buf);
                    match directory::damaged_directory_extent(&read_at, off, &raw, end) {
                        Some(next) => {
                            self.open_warnings.push(format!("damaged block directory block at byte {off} skipped ({e})"));
                            self.dir.stats.damaged += 1;
                            off = next;
                            continue;
                        }
                        None => return Err(e),
                    }
                }
            };
            sb.adapt(info.next_offset - off);
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
                    self.dir.last_system_off = off;
                }
                BT_DIRECTORY => {
                    // directory blocks of a later flush (catch-up of a
                    // read-only handle): the blocks they cover are
                    // registered by this scan anyway
                }
                bt if is_layout_meta(bt) => {
                    return Err(Error::Corruption("layout metadata must not be stored inside the DB".into()));
                }
                BT_DELETE | BT_CHECKPOINT | BT_DB_CONFIG => {
                    let payload = if info.payload_len <= ScanBuf::LARGE_WINDOW as u64 {
                        let raw = sb.get(&self.store, off + BLOCK_HEADER_SIZE as u64, info.payload_len as usize)?.to_vec();
                        if crc32(&raw) != info.payload_crc {
                            return Err(Error::Corruption(format!("block {}: payload CRC mismatch", info.block_id)));
                        }
                        raw
                    } else {
                        self.read_block(off)?.1
                    };
                    self.apply_meta_block(&info, &payload)?;
                    match info.block_type {
                        BT_CHECKPOINT => self.dir.last_checkpoint_off = off,
                        BT_DB_CONFIG => self.dir.last_config_off = off,
                        _ => {}
                    }
                }
                BT_DATA_DENSE => {
                    let seg = self.parse_dense_prefix(&info, &mut sb)?;
                    self.register_segment(seg)?;
                }
                BT_SPARSE_CELLS => {
                    let seg = self.parse_sparse_prefix(&info, &mut sb)?;
                    // not registered as verified: the payload CRC is
                    // checked on first use
                    self.register_sparse_segment(seg);
                }
                BT_SECONDARY => {
                    self.parse_secondary_directory(&info)?;
                }
                BT_SECONDARY_PAGE => self.dir.unlisted_blocks += 1,
                _ => {}
            }
            // RD-05: block ids grow with the file position (a copied or
            // swapped block with a foreign id is corruption)
            if info.block_id <= prev_id && (off > start || start > DATA_START) {
                return Err(Error::Corruption(format!("block {} at byte {off}: block ids not ascending (previous {prev_id})", info.block_id)));
            }
            prev_id = info.block_id;
            maxid = maxid.max(info.block_id);
            off = info.next_offset;
        }
        if off != end {
            return Err(Error::Corruption("R0,u1 bootstrap missing or invalid end of file".into()));
        }
        let record = record.ok_or_else(|| Error::Corruption("R0,u1 bootstrap missing or invalid end of file".into()))?;
        self.last_valid_end = end;
        self.next_block_id = self.next_block_id.max(maxid + 1);
        self.dir.scan_last_id = prev_id;
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
                self.register_delete(r, lane, u, info.block_id);
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

/// Content of one superblock slot.
#[derive(Clone, Debug, Default)]
pub struct SlotState {
    pub sup: Option<SuperState>,
    pub error: Option<String>,
}

/// Decodes both superblock slots from the first `DATA_START` bytes.
pub(crate) fn slots_from_bytes(raw: &[u8]) -> [SlotState; 2] {
    let one = |slot: usize| {
        let b = raw.get(slot * SUPERBLOCK_SIZE..(slot + 1) * SUPERBLOCK_SIZE).unwrap_or(&[]);
        if b.len() == SUPERBLOCK_SIZE && !b.starts_with(b"NHYDB") {
            return SlotState { sup: None, error: Some("unknown file signature".into()) };
        }
        if b.len() == SUPERBLOCK_SIZE && &b[..8] != SUPER_MAGIC {
            return SlotState {
                sup: None,
                error: Some(format!("unsupported file format {} (expected NHYDB {FILE_FORMAT_VERSION})", format_name(&b[..8]))),
            };
        }
        match unpack_super(b) {
            Ok(s) => SlotState { sup: Some(s), error: None },
            Err(e) => SlotState { sup: None, error: Some(e.to_string()) },
        }
    };
    [one(0), one(1)]
}

pub(crate) fn read_slots(store: &Storage) -> Result<[SlotState; 2]> {
    Ok(slots_from_bytes(&store.read_upto(0, DATA_START as usize)?))
}

/// Assessment of the two superblocks for a file of `physical` bytes.
#[derive(Clone, Debug)]
pub(crate) struct SuperJudgement {
    /// Usable superblock (slot, state): the newest valid one whose commit
    /// boundary lies inside the file, otherwise an older valid one.
    pub chosen: Option<(usize, SuperState)>,
    /// `chosen` is the newest valid superblock.
    pub newest: bool,
    /// Both slots valid and consistent, boundary inside the file: the bytes
    /// after the boundary are certainly uncommitted.
    pub clean: bool,
    pub problem: Option<String>,
}

#[cfg(test)]
thread_local! {
    /// Test-only (N1): runs between the open scan and reading the layout
    /// plan, to interleave a concurrent writer deterministically.
    pub(crate) static AFTER_OPEN_SCAN: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

/// Maximum number of open attempts without lock while a concurrent writer
/// keeps publishing schema generations (N1).
const OPEN_RACE_ATTEMPTS: u32 = 12;

/// Maximum number of incremental catch-ups within one read-only open attempt
/// (N1): each one scans only the bytes committed since the previous one.
const OPEN_CATCH_UP_LIMIT: u32 = 64;

/// Error of a cursor whose handle discarded data after its first batch (IT1).
pub const CURSOR_INVALIDATED: &str =
    "iterator invalidated: rollback() (or a dropped append buffer) discarded data of this database object after the \
     iteration started; already returned and buffered results may belong to the discarded state. Restart the query";

pub(crate) fn judge_superblocks(slots: &[SlotState; 2], physical: u64) -> SuperJudgement {
    let mut problems: Vec<String> = Vec::new();
    for (i, s) in slots.iter().enumerate() {
        if let Some(e) = &s.error {
            problems.push(format!("superblock slot {i} damaged ({e})"));
        }
    }
    let mut valid: Vec<(usize, SuperState)> = slots.iter().enumerate().filter_map(|(i, s)| s.sup.map(|x| (i, x))).collect();
    valid.sort_by(|a, b| b.1.generation.cmp(&a.1.generation).then(a.0.cmp(&b.0)));
    let join = |p: &[String]| if p.is_empty() { None } else { Some(p.join("; ")) };
    let Some(&(nslot, newest)) = valid.first() else {
        let foreign = slots
            .iter()
            .all(|s| s.error.as_deref().is_some_and(|e| e.starts_with("unknown file signature") || e.starts_with("unsupported file format")));
        problems.insert(0, if foreign { format!("not an NHyDB {FILE_FORMAT_VERSION} database file") } else { "both superblocks damaged".into() });
        return SuperJudgement { chosen: None, newest: false, clean: false, problem: join(&problems) };
    };
    let fits = |s: &SuperState| s.last_valid_end >= DATA_START && s.last_valid_end <= physical;
    if fits(&newest) {
        let mut clean = valid.len() == 2;
        if let Some(&(oslot, other)) = valid.get(1) {
            let consistent = other.last_valid_end <= newest.last_valid_end
                && (other.generation < newest.generation || other.last_valid_end == newest.last_valid_end);
            if !consistent {
                clean = false;
                problems.push(format!(
                    "superblocks inconsistent: slot {nslot} (generation {}) ends at byte {}, slot {oslot} (generation {}) at byte {}",
                    newest.generation, newest.last_valid_end, other.generation, other.last_valid_end
                ));
            }
        }
        return SuperJudgement { chosen: Some((nslot, newest)), newest: true, clean, problem: join(&problems) };
    }
    problems.push(format!(
        "newest superblock (slot {nslot}, generation {}) points to commit boundary {} outside the file ({physical} bytes; file truncated?)",
        newest.generation, newest.last_valid_end
    ));
    let fallback = valid.iter().skip(1).find(|x| fits(&x.1)).copied();
    SuperJudgement { chosen: fallback, newest: false, clean: false, problem: join(&problems) }
}

/// Checks the sidecar paths: not empty, not a directory, all distinct from
/// each other, from the DB and from the lock file.
/// A directory where a file is expected: `IsADirectoryError` (errno EISDIR)
/// on the Python side (F051/F048).
/// The error names the affected path (`filename`).
fn is_a_directory(_what: &str, p: &Path) -> Error {
    #[cfg(unix)]
    return Error::IoPath(std::io::Error::from_raw_os_error(libc::EISDIR), p.into());
    #[cfg(not(unix))]
    return Error::IoPath(
        std::io::Error::new(std::io::ErrorKind::IsADirectory, format!("{_what} '{}' is a directory", p.display())),
        p.into(),
    );
}

pub(crate) fn validate_paths(db: &Path, layout: &Path, key: &Path) -> Result<()> {
    for (what, x) in [("layout_path", layout), ("key_path", key)] {
        if x.as_os_str().is_empty() {
            return Err(Error::Value(format!("{what} must not be empty")));
        }
        if x.is_dir() {
            return Err(is_a_directory(what, x));
        }
    }
    if db.is_dir() {
        return Err(is_a_directory("database path", db));
    }
    let lock = DbLock::lock_path(db);
    let all = [("database", db), ("layout_path", layout), ("key_path", key), ("lock file", lock.as_path())];
    for i in 0..all.len() {
        for j in i + 1..all.len() {
            if all[i].1 == all[j].1 {
                return Err(Error::Value(format!(
                    "{} and {} must be different files ('{}')",
                    all[i].0,
                    all[j].0,
                    all[i].1.display()
                )));
            }
        }
    }
    Ok(())
}

/// `create(overwrite=True)`: an existing plan/key file may only be replaced
/// if it provably belongs to the database being overwritten (same `db_id`
/// as R0,u1 of the existing DB file).
fn check_sidecars_owned(db: &Path, layout: &Path, key: &Path) -> Result<()> {
    let owner = if db.exists() { system::read_db_identity(db).map(|x| x.0) } else { None };
    for x in [layout, key] {
        if !x.exists() {
            continue;
        }
        let id = system::sidecar_db_id(x);
        if id.is_none() || id != owner {
            return Err(Error::Exists(format!(
                "refusing to overwrite '{}': it does not belong to the database '{}' being overwritten{} – remove it manually or choose another layout_path/key_path",
                x.display(),
                db.display(),
                id.map(|i| format!(" (it belongs to db_id {i})")).unwrap_or_default()
            )));
        }
    }
    Ok(())
}

/// Unused temporary name next to `p` (same directory, hidden).
pub(crate) fn sibling_temp(p: &Path, tag: &str) -> Result<PathBuf> {
    let dir = platform::parent_dir(p);
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    loop {
        let suffix = crate::crypto::hex(&crate::crypto::random_bytes(6)?);
        let t = dir.join(format!(".{name}.{tag}-{suffix}"));
        if !t.exists() {
            return Ok(t);
        }
    }
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

#[cfg(all(test, nhydb_internal_tests))]
#[path = "db_file_handle_tests.rs"]
mod file_handle_tests;
