//! Block cache and per-block "verified" registry (P001, P003, P005).
//!
//! Every block payload is CRC-checked completely before any byte of it is
//! used. The first access to a block therefore reads it completely (and puts
//! it into the cache if it fits). The offset is then remembered as
//! *verified* together with block ID and payload length; later accesses to a
//! block that is no longer (or never) cached read only the bytes they need
//! (`pread` of the offset table entry and the record) instead of the whole
//! block. Blocks are immutable after they were appended, the writer holds the
//! exclusive lock, and a read-only handle only sees committed bytes, so a
//! verified range keeps its content for the lifetime of the handle.
//!
//! Invariant: an offset can only be reused after the file was cut back
//! (append rollback, MATH rollback, discarded append buffer, truncation on
//! open). Every such path calls [`BlockCache::invalidate_from`] or resets the
//! caches completely (`reset_in_memory_state`, `close`); a reopen starts with
//! an empty registry. `verify(deep)` never uses this cache.
//!
//! The cache budget comes from `[cache] block_cache_bytes` (default 64 MiB).
//! Eviction is least-recently-used in batches (down to 3/4 of the budget)
//! instead of clearing the whole cache.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::BLOCK_HEADER_SIZE;
use crate::val::Val;
use crate::vmap;

use super::Db;

/// Largest single block kept in the cache: at least 8 MiB, at most the budget,
/// 1/8 of larger budgets (a 256 MiB budget caches blocks up to 32 MiB).
const MAX_ITEM_FLOOR: usize = 8 << 20;
/// A verified, cacheable block that is read partially this often since it
/// was last loaded is loaded completely (CRC-checked again) and cached
/// again: many records of the same block are cheaper from the cache than
/// as single `pread`s.
const PROMOTE_AFTER: u32 = 8;

struct CachedBlock {
    data: Arc<Vec<u8>>,
    tick: u64,
}

#[derive(Clone, Copy, Debug)]
struct VerifiedBlock {
    payload_len: u64,
    partial_reads: u32,
}

/// Counters for `stats()["block_cache"]`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlockCacheCounters {
    pub hits: u64,
    pub loads: u64,
    pub partial_reads: u64,
    pub evictions: u64,
}

#[derive(Default)]
pub(crate) struct BlockCache {
    blocks: HashMap<u64, CachedBlock>,
    bytes: usize,
    tick: u64,
    verified: HashMap<u64, VerifiedBlock>,
    pub counters: BlockCacheCounters,
}

impl BlockCache {
    fn get(&mut self, off: u64) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        let b = self.blocks.get_mut(&off)?;
        b.tick = tick;
        self.counters.hits += 1;
        Some(b.data.clone())
    }

    fn insert(&mut self, off: u64, data: Arc<Vec<u8>>, budget: usize) {
        let n = data.len();
        if let Some(old) = self.blocks.remove(&off) {
            self.bytes -= old.data.len();
        }
        if self.bytes + n > budget {
            // LRU in batches: evict the least recently used blocks until
            // 3/4 of the budget is free for the new one.
            let limit = budget - budget / 4;
            let mut order: Vec<(u64, u64)> = self.blocks.iter().map(|(o, b)| (b.tick, *o)).collect();
            order.sort_unstable();
            for (_, o) in order {
                if self.bytes + n <= limit {
                    break;
                }
                if let Some(b) = self.blocks.remove(&o) {
                    self.bytes -= b.data.len();
                    self.counters.evictions += 1;
                }
            }
        }
        if self.bytes + n > budget {
            return;
        }
        self.tick += 1;
        self.bytes += n;
        self.blocks.insert(off, CachedBlock { data, tick: self.tick });
    }

    fn mark_verified(&mut self, off: u64, payload_len: u64) {
        self.verified.insert(off, VerifiedBlock { payload_len, partial_reads: 0 });
    }

    /// Payload length if the block at `off` may be read partially now.
    fn partial(&mut self, off: u64, max_item: usize) -> Option<u64> {
        let v = self.verified.get_mut(&off)?;
        if v.payload_len > max_item as u64 || v.partial_reads < PROMOTE_AFTER {
            v.partial_reads = v.partial_reads.saturating_add(1);
            self.counters.partial_reads += 1;
            return Some(v.payload_len);
        }
        None
    }

    /// Forgets every block at or behind `off` (the file was cut back there).
    pub(crate) fn invalidate_from(&mut self, off: u64) {
        let gone: Vec<u64> = self.blocks.keys().copied().filter(|o| *o >= off).collect();
        for o in gone {
            if let Some(b) = self.blocks.remove(&o) {
                self.bytes -= b.data.len();
            }
        }
        self.verified.retain(|o, _| *o < off);
    }

    #[cfg(test)]
    pub(crate) fn is_verified(&self, off: u64) -> bool {
        self.verified.contains_key(&off)
    }

    #[cfg(test)]
    pub(crate) fn is_cached(&self, off: u64) -> bool {
        self.blocks.contains_key(&off)
    }
}

/// R016: item budgets of the decode caches that used to grow without bound
/// over a session (one item = one posting / sparse group / sparse cell).
pub(crate) const POSTINGS_BUDGET: usize = 8 << 20;
pub(crate) const FT_POSTINGS_BUDGET: usize = 8 << 20;
pub(crate) const SPARSE_INDEX_BUDGET: usize = 4 << 20;
pub(crate) const SPARSE_CELLS_BUDGET: usize = 1 << 20;

/// Items held per budgeted cache (may overestimate after targeted
/// removals; recomputed before anything is evicted).
#[derive(Default)]
pub(crate) struct CacheUse {
    pub postings: usize,
    pub ft_postings: usize,
    pub sparse_block_index: usize,
    pub sparse_record: usize,
    pub sparse_merged: usize,
}

/// Inserts into a budgeted cache map (R016). An entry larger than the
/// budget is not cached; when the budget or `max_entries` would be
/// exceeded, the map is cleared first (after recomputing the real use).
pub(crate) fn bounded_insert<K: std::hash::Hash + Eq, V>(
    map: &mut HashMap<K, Arc<V>>,
    used: &mut usize,
    key: K,
    val: Arc<V>,
    size: impl Fn(&V) -> usize,
    budget: usize,
    max_entries: usize,
) {
    let n = size(&val);
    if n > budget {
        return;
    }
    if *used + n > budget || map.len() >= max_entries {
        *used = map.values().map(|v| size(v)).sum();
        if *used + n > budget || map.len() >= max_entries {
            map.clear();
            *used = 0;
        }
    }
    if let Some(old) = map.insert(key, val) {
        *used = used.saturating_sub(size(&old));
    }
    *used += n;
}

/// Bytes of a payload range: shared with the block cache or read on demand.
pub(crate) enum BlockBytes {
    Shared(Arc<Vec<u8>>, usize, usize),
    Owned(Vec<u8>),
}

impl Deref for BlockBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            BlockBytes::Shared(a, s, n) => &a[*s..*s + *n],
            BlockBytes::Owned(v) => v,
        }
    }
}

/// How a block can be accessed right now.
pub(crate) enum BlockAccess {
    /// Complete, CRC-checked payload (from the cache or just loaded).
    Cached(Arc<Vec<u8>>),
    /// CRC-checked earlier in this session, not cached: read ranges directly.
    Verified { payload_len: u64 },
}

impl BlockAccess {
    pub(crate) fn payload_len(&self) -> u64 {
        match self {
            BlockAccess::Cached(a) => a.len() as u64,
            BlockAccess::Verified { payload_len } => *payload_len,
        }
    }
}

impl Db {
    fn block_budget(&self) -> (usize, usize) {
        let budget = self.config.block_cache_bytes;
        (budget, budget.min(MAX_ITEM_FLOOR.max(budget / 8)))
    }

    /// Reads a block completely, checks the payload CRC, registers it as
    /// verified and caches it if it fits.
    fn load_block(&self, block_offset: u64) -> Result<Arc<Vec<u8>>> {
        let (info, payload) = self.read_block(block_offset)?;
        let arc = Arc::new(payload);
        let (budget, max_item) = self.block_budget();
        let mut c = self.cache();
        let bc = &mut c.block_cache;
        bc.counters.loads += 1;
        bc.mark_verified(block_offset, info.payload_len);
        if arc.len() <= max_item {
            bc.insert(block_offset, arc.clone(), budget);
        }
        Ok(arc)
    }

    /// Complete block payload via the block cache (CRC-checked on load).
    /// For whole-block consumers (scans, index builds, group directories).
    pub(crate) fn block_payload(&self, block_offset: u64) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.cache().block_cache.get(block_offset) {
            return Ok(b);
        }
        self.load_block(block_offset)
    }

    /// Complete block payload for a single sequential pass (index build,
    /// I001): from the cache if it is there, otherwise read and CRC-checked
    /// like `load_block` and registered as verified, but not inserted into
    /// the cache – a one-time scan over all data blocks would only evict
    /// blocks that later reads need, and keep up to the whole budget alive.
    pub(crate) fn block_payload_once(&self, block_offset: u64) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.cache().block_cache.get(block_offset) {
            return Ok(b);
        }
        let (info, payload) = self.read_block(block_offset)?;
        let mut c = self.cache();
        c.block_cache.counters.loads += 1;
        c.block_cache.mark_verified(block_offset, info.payload_len);
        Ok(Arc::new(payload))
    }

    /// Access to a block for range reads: cached payload, or – for a block
    /// verified earlier in this session – direct range reads. An unverified
    /// block is loaded and checked completely first.
    pub(crate) fn block_access(&self, block_offset: u64) -> Result<BlockAccess> {
        let (_, max_item) = self.block_budget();
        {
            let mut c = self.cache();
            if let Some(b) = c.block_cache.get(block_offset) {
                return Ok(BlockAccess::Cached(b));
            }
            if let Some(payload_len) = c.block_cache.partial(block_offset, max_item) {
                return Ok(BlockAccess::Verified { payload_len });
            }
        }
        Ok(BlockAccess::Cached(self.load_block(block_offset)?))
    }

    /// `len` payload bytes from `rel` on, through an access obtained from
    /// [`Db::block_access`]. A range outside the payload is corruption.
    pub(crate) fn access_range(&self, acc: &BlockAccess, block_offset: u64, rel: usize, len: usize) -> Result<BlockBytes> {
        let end = rel.checked_add(len).map(|e| e as u64);
        if end.is_none_or(|e| e > acc.payload_len()) {
            return Err(Error::Corruption(format!(
                "block at byte {block_offset}: range {rel}+{len} outside the payload ({} bytes)",
                acc.payload_len()
            )));
        }
        Ok(match acc {
            BlockAccess::Cached(a) => BlockBytes::Shared(a.clone(), rel, len),
            BlockAccess::Verified { .. } => {
                BlockBytes::Owned(self.store.read_vec(block_offset + BLOCK_HEADER_SIZE as u64 + rel as u64, len)?)
            }
        })
    }

    /// `len` payload bytes from `rel` on (single range of a block).
    pub(crate) fn block_range(&self, block_offset: u64, rel: usize, len: usize) -> Result<BlockBytes> {
        let acc = self.block_access(block_offset)?;
        self.access_range(&acc, block_offset, rel, len)
    }

    /// `stats()["block_cache"]`: budget, occupancy and counters of this handle.
    pub(crate) fn block_cache_stats(&self) -> Val {
        let (budget, max_item) = self.block_budget();
        let c = self.cache();
        let bc = &c.block_cache;
        vmap! {
            "budget_bytes" => budget as i64, "max_item_bytes" => max_item as i64,
            "cached_bytes" => bc.bytes as i64, "cached_blocks" => bc.blocks.len() as i64,
            "verified_blocks" => bc.verified.len() as i64, "hits" => bc.counters.hits as i64,
            "loads" => bc.counters.loads as i64, "partial_reads" => bc.counters.partial_reads as i64,
            "evictions" => bc.counters.evictions as i64,
        }
    }

    /// Makes sure the block at `block_offset` was CRC-checked in this
    /// session (loads and checks it otherwise).
    pub(crate) fn ensure_verified(&self, block_offset: u64) -> Result<()> {
        if self.cache().block_cache.verified.contains_key(&block_offset) {
            return Ok(());
        }
        self.load_block(block_offset).map(|_| ())
    }

    /// Records that the payload of the block at `block_offset` passed its
    /// CRC check outside the cache (open scan of SPARSE blocks, F021).
    pub(crate) fn note_verified(&mut self, block_offset: u64, payload_len: u64) {
        self.caches.get_mut().unwrap_or_else(|e| e.into_inner()).block_cache.mark_verified(block_offset, payload_len);
    }

    /// Forgets cached/verified blocks at or behind `off` (file cut back).
    pub(crate) fn invalidate_blocks_from(&mut self, off: u64) {
        self.caches.get_mut().unwrap_or_else(|e| e.into_inner()).block_cache.invalidate_from(off);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::address::NL;
    use crate::codec::{FieldDef, TypeCode};
    use crate::config::Config;
    use crate::db::dense::DenseRows;
    use crate::db::sparse::CellInput;
    use crate::db::OpenOptions;
    use crate::nat::Nat;
    use crate::platform::LockOverride;
    use crate::storage::FILE_READS;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nhydb_bc_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join(format!("{name}.nhydb"))
    }

    fn opts(cache_bytes: usize) -> OpenOptions {
        OpenOptions { config: Some(Config { block_cache_bytes: cache_bytes, ..Config::default() }), ..Default::default() }
    }

    fn rec(u: u64) -> Vec<u8> {
        format!("record-{u}-").repeat(1 + (u % 7) as usize).into_bytes()
    }

    /// One DATA block with `n` bytes records (u = 1..=n).
    fn build(p: &Path, n: u64, o: OpenOptions) -> Db {
        let r = Nat::S(1);
        let mut db = Db::create(p, true, o).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        db.create_shape(1, &r, "s", vec![FieldDef::new(Nat::S(1), "b", TypeCode::Bytes)], "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
        let rows: Vec<Vec<Val>> = (1..=n).map(|u| vec![Val::Bytes(rec(u))]).collect();
        db.put_dense_batch(&r, &Nat::S(1), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
        db
    }

    fn field(db: &Db, u: u64) -> Option<Val> {
        db.get(&Nat::S(1), &Nat::S(u), NL, None, None, true).unwrap().and_then(|r| r.get("b").cloned())
    }

    fn first_seg_offset(db: &Db) -> u64 {
        db.dense[&(Nat::S(1), NL)].segs[0].block_offset
    }

    /// P001: a block larger than the largest cacheable item is CRC-checked
    /// once completely; afterwards a record costs two small positional reads
    /// instead of reading the whole block again. Results are unchanged.
    #[test]
    fn big_block_is_verified_once_then_read_partially() {
        let p = tmp("big");
        build(&p, 4000, opts(16 << 10)).close(true).unwrap();
        let db = Db::open_with(&p, opts(16 << 10)).unwrap();
        let off = first_seg_offset(&db);
        let plen = db.dense[&(Nat::S(1), NL)].segs[0].payload_len;
        assert!(plen > 16 << 10, "block must exceed the cache budget");
        assert_eq!(field(&db, 1), Some(Val::Bytes(rec(1))));
        {
            let c = db.cache();
            assert!(c.block_cache.is_verified(off) && !c.block_cache.is_cached(off));
        }
        FILE_READS.with(|c| c.set((0, 0)));
        for u in (2..=4000).step_by(37) {
            assert_eq!(field(&db, u), Some(Val::Bytes(rec(u))));
        }
        let (n, bytes) = FILE_READS.with(|c| c.get());
        let gets = (2..=4000).step_by(37).count() as u64;
        assert_eq!(n, 2 * gets, "offset table entry + record per get");
        assert!(bytes < gets * 200, "only record bytes are read ({bytes} bytes for {gets} gets)");
        let st = db.block_cache_stats();
        assert_eq!(st.get("loads"), Some(&Val::Int(1)));
        // read_numpy/columns over the whole block: same values as get
        let cols = db.read_columns(&Nat::S(1), &Nat::S(1), 4000, None, NL, false).unwrap();
        assert_eq!(cols.present.iter().filter(|x| **x == 1).count(), 4000);
    }

    /// The budget 0 disables the cache: every block is still CRC-checked on
    /// its first use, then only read partially; values equal the cached path.
    #[test]
    fn zero_budget_equals_default_cache() {
        let p = tmp("zero");
        build(&p, 600, OpenOptions::default()).close(true).unwrap();
        let a = Db::open_with(&p, opts(0)).unwrap();
        let b = Db::open_with(&p, OpenOptions { readonly: true, ..Default::default() }).unwrap();
        for u in 1..=601 {
            assert_eq!(field(&a, u), field(&b, u), "u={u}");
        }
        let ca = a.read_columns(&Nat::S(1), &Nat::S(100), 450, None, NL, false).unwrap();
        let cb = b.read_columns(&Nat::S(1), &Nat::S(100), 450, None, NL, false).unwrap();
        assert_eq!(ca.present, cb.present);
        assert_eq!(a.block_cache_stats().get("cached_blocks"), Some(&Val::Int(0)));
    }

    /// CRC check before use: a flipped payload byte inside a record (the
    /// prefix stays plausible, so the open succeeds) is reported on the
    /// first access – for cacheable and for oversized blocks – and the
    /// block is never registered as verified.
    #[test]
    fn corrupted_payload_is_never_served() {
        for budget in [64usize << 20, 1 << 10] {
            let p = tmp(&format!("crc{budget}"));
            let db = build(&p, 300, OpenOptions::default());
            let seg = db.dense[&(Nat::S(1), NL)].segs[0].clone();
            drop(db);
            let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            // a byte in the middle of the records area
            let pos = seg.records_offset + (seg.payload_offset + seg.payload_len - seg.records_offset) / 2;
            f.seek(SeekFrom::Start(pos)).unwrap();
            f.write_all(&[0xA5]).unwrap();
            drop(f);
            let db = Db::open_with(&p, OpenOptions { readonly: true, ..opts(budget) }).unwrap();
            for u in [1u64, 150, 300] {
                let e = db.get(&Nat::S(1), &Nat::S(u), NL, None, None, true).unwrap_err();
                assert!(matches!(e, Error::Corruption(_)), "{e}");
            }
            assert!(db.read_columns(&Nat::S(1), &Nat::S(1), 300, None, NL, false).is_err());
            assert!(!db.cache().block_cache.is_verified(seg.block_offset));
        }
    }

    /// Cutting the file back forgets verified/cached blocks at and behind
    /// the cut: a handle that read its own unwritten block and then
    /// discards it (lock=False handle next to a writer) keeps no registry
    /// entry for that offset.
    #[test]
    fn discarded_blocks_are_forgotten() {
        let p = tmp("discard");
        let mut w = build(&p, 10, OpenOptions::default());
        let nolock = OpenOptions { lock: LockOverride { enabled: Some(false), timeout_ms: None }, ..Default::default() };
        let mut r = Db::open_with(&p, nolock).unwrap();
        let rows = vec![vec![Val::Bytes(b"writer".to_vec())]];
        w.put_dense_batch(&Nat::S(1), &Nat::S(20), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
        let rows = vec![vec![Val::Bytes(b"mine".to_vec())]];
        r.put_dense_batch(&Nat::S(1), &Nat::S(30), &DenseRows::Tuples(&rows), Some(1), &[], NL, false).unwrap();
        assert_eq!(field(&r, 30), Some(Val::Bytes(b"mine".to_vec())));
        let own = r.dense[&(Nat::S(1), NL)].segs.last().unwrap().block_offset;
        assert!(r.cache().block_cache.is_verified(own));
        assert!(r.commit().is_err());
        assert!(!r.cache().block_cache.is_verified(own) && !r.cache().block_cache.is_cached(own));
        let _ = r.close(false);
        w.close(true).unwrap();
    }

    /// R016: budgeted caches never hold more items than their budget and
    /// never cache a single oversized entry; targeted removals (which do not
    /// update the counter) do not cause needless clearing.
    #[test]
    fn bounded_insert_respects_budget() {
        let mut m: HashMap<u64, Arc<Vec<u8>>> = HashMap::new();
        let mut used = 0usize;
        for k in 0..100u64 {
            bounded_insert(&mut m, &mut used, k, Arc::new(vec![0; 7]), |v| v.len(), 50, usize::MAX);
            assert!(m.values().map(|v| v.len()).sum::<usize>() <= 50);
            assert_eq!(used, m.values().map(|v| v.len()).sum::<usize>());
        }
        bounded_insert(&mut m, &mut used, 1000, Arc::new(vec![0; 51]), |v| v.len(), 50, usize::MAX);
        assert!(!m.contains_key(&1000), "oversized entry is not cached");
        // drift: entries removed behind the counter's back
        let keys: Vec<u64> = m.keys().copied().collect();
        for k in &keys[1..] {
            m.remove(k);
        }
        let keep = keys[0];
        bounded_insert(&mut m, &mut used, 2000, Arc::new(vec![0; 40]), |v| v.len(), 50, usize::MAX);
        assert!(m.contains_key(&keep) && m.contains_key(&2000), "recomputed use avoids clearing");
        // entry limit
        let mut e: HashMap<u64, Arc<Vec<u8>>> = HashMap::new();
        let mut eu = 0usize;
        for k in 0..10u64 {
            bounded_insert(&mut e, &mut eu, k, Arc::new(vec![]), |v| v.len(), 50, 4);
            assert!(e.len() <= 4);
        }
    }

    #[test]
    fn invalidate_from_and_lru_eviction() {
        let mut bc = BlockCache::default();
        for i in 0..10u64 {
            bc.mark_verified(i * 100, 10);
            bc.insert(i * 100, Arc::new(vec![0u8; 10]), 40);
            if i >= 1 {
                // keep block 0 hot
                assert!(bc.get(0).is_some());
            }
            assert!(bc.bytes <= 40);
        }
        assert!(bc.is_cached(0), "recently used block survives the eviction");
        assert!(bc.is_cached(900), "newest block is cached");
        assert!(bc.counters.evictions > 0);
        bc.invalidate_from(500);
        assert!(bc.blocks.keys().all(|o| *o < 500) && bc.verified.keys().all(|o| *o < 500));
        assert_eq!(bc.bytes, bc.blocks.values().map(|b| b.data.len()).sum::<usize>());
        // a block larger than the budget is never cached
        bc.insert(5000, Arc::new(vec![0u8; 50]), 40);
        assert!(!bc.is_cached(5000));
    }

    /// Cacheable verified blocks that are no longer cached are read
    /// partially a few times and then loaded (and checked) again, so that
    /// sequential reading of many records of one block uses the cache.
    #[test]
    fn evicted_block_is_promoted_back() {
        let p = tmp("promote");
        build(&p, 50, OpenOptions::default()).close(true).unwrap();
        let db = Db::open_with(&p, opts(64 << 20)).unwrap();
        let off = first_seg_offset(&db);
        assert!(field(&db, 1).is_some());
        {
            let mut c = db.cache();
            let b = c.block_cache.blocks.remove(&off).unwrap();
            c.block_cache.bytes -= b.data.len();
        }
        for u in 1..=PROMOTE_AFTER as u64 {
            assert!(field(&db, u).is_some());
            assert!(!db.cache().block_cache.is_cached(off));
        }
        assert!(field(&db, 49).is_some());
        assert!(db.cache().block_cache.is_cached(off));
        assert_eq!(db.block_cache_stats().get("loads"), Some(&Val::Int(2)));
    }

    /// P019: a cached v index of a u group is brought up to date lazily
    /// when later sparse blocks (overrides, tombstones, other u groups)
    /// were written; results equal a fresh handle.
    #[test]
    fn sparse_vindex_catches_up_after_writes() {
        let p = tmp("vidx");
        let r = Nat::S(2);
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "sp", "", None, &Nat::ZERO).unwrap();
        let cell = |u: u64, v: u64, x: i64| CellInput::typed(Nat::S(u), Nat::S(v), Val::Int(x), Some(TypeCode::VarInt));
        db.put_points(&r, (1..=500).map(|v| cell(5, v, v as i64)).collect(), NL, true, 10_000).unwrap();
        let get = |db: &Db, u: u64, v: u64| db.get_cell_value(&r, &Nat::S(u), &Nat::S(v), NL).unwrap();
        assert_eq!(get(&db, 5, 7), Some(Val::Int(7)));
        // other u groups, overlapping u range, overrides and a tombstone
        db.put_points(&r, vec![cell(4, 1, -1), cell(6, 1, -1)], NL, false, 10_000).unwrap();
        db.put_points(&r, vec![cell(5, 7, 700), cell(5, 501, 501), cell(9, 3, 3)], NL, false, 10_000).unwrap();
        db.delete_point(&r, &Nat::S(5), &Nat::S(8), NL, false).unwrap();
        assert_eq!(get(&db, 5, 7), Some(Val::Int(700)));
        assert_eq!(get(&db, 5, 8), None);
        assert_eq!(get(&db, 5, 501), Some(Val::Int(501)));
        assert_eq!(get(&db, 5, 499), Some(Val::Int(499)));
        assert_eq!(get(&db, 4, 1), Some(Val::Int(-1)));
        db.commit().unwrap();
        let fresh = Db::open_with(&p, OpenOptions { readonly: true, ..Default::default() }).unwrap();
        for v in 1..=502 {
            assert_eq!(get(&db, 5, v), get(&fresh, 5, v), "v={v}");
        }
        let a = db.sparse_range(&r, &Nat::S(5), &Nat::S(1), &Nat::S(600), NL).unwrap();
        let b = fresh.sparse_range(&r, &Nat::S(5), &Nat::S(1), &Nat::S(600), NL).unwrap();
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }
}
