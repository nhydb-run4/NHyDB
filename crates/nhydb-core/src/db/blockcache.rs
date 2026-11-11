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

use super::{BlockInfo, Db};

/// Largest single block kept in the cache: at least 8 MiB, at most the budget,
/// 1/8 of larger budgets (a 256 MiB budget caches blocks up to 32 MiB).
const MAX_ITEM_FLOOR: usize = 8 << 20;
/// A verified, cacheable block that is read partially this often since it
/// was last loaded is loaded completely (CRC-checked again) and cached
/// again: many records of the same block are cheaper from the cache than
/// as single `pread`s.
const PROMOTE_AFTER: u32 = 8;
/// Per-block details (payload length, partial-read counter) are kept for
/// up to this many recently verified blocks; at this size the least recently
/// used half goes (RUN8; before: 65 536 and a full reset). Older verified
/// blocks get them back from their header. The bound stays below half of the
/// map's bucket capacity, so the tombstones of the eviction never force a
/// larger table (see `FIELD_META_MAX` in layout.rs).
const RECENT_EVICT_AT: usize = 49_152;
/// Estimated heap bytes of one entry of the recent block details.
const RECENT_COST: usize = 64;
/// Memory bound of the verified set (bits/positions of block offsets).
const VERIFIED_BYTES_MAX: usize = 64 << 20;
/// One chunk of the verified set covers 1 MiB of the file (8-byte slots).
const CHUNK_SHIFT: u32 = 20;
const CHUNK_SLOTS: usize = 1 << (CHUNK_SHIFT - 3);
/// A chunk becomes a bitmap (16 KiB) when its sorted positions would be larger.
const DENSE_AT: usize = CHUNK_SLOTS / 32;

enum Chunk {
    Sparse(Vec<u32>),
    Dense(Box<[u64]>),
}

impl Chunk {
    fn bytes(&self) -> usize {
        match self {
            Chunk::Sparse(v) => v.capacity() * 4 + 48,
            Chunk::Dense(b) => b.len() * 8 + 48,
        }
    }
}

/// Set of verified block offsets (RC-02): sorted slot positions per MiB of
/// file, a bitmap where blocks are dense – a few bits per block instead of a
/// map entry, so the registry never has to be cleared because of many
/// blocks (which made every query re-check all blocks above 2^18 blocks).
/// Bounded in memory by dropping the oldest chunks (only re-checks).
#[derive(Default)]
struct VerifiedSet {
    chunks: HashMap<u64, Chunk>,
    order: std::collections::VecDeque<u64>,
    n: usize,
    bytes: usize,
}

impl VerifiedSet {
    fn split(off: u64) -> Option<(u64, u32)> {
        off.is_multiple_of(8).then_some((off >> CHUNK_SHIFT, ((off & ((1 << CHUNK_SHIFT) - 1)) >> 3) as u32))
    }

    fn contains(&self, off: u64) -> bool {
        let Some((c, slot)) = Self::split(off) else { return false };
        match self.chunks.get(&c) {
            Some(Chunk::Sparse(v)) => v.binary_search(&slot).is_ok(),
            Some(Chunk::Dense(b)) => b[slot as usize / 64] & (1 << (slot % 64)) != 0,
            None => false,
        }
    }

    fn insert(&mut self, off: u64) {
        let Some((c, slot)) = Self::split(off) else { return };
        if !self.chunks.contains_key(&c) {
            // RB3-01: the fixed part of a new chunk is booked as well
            // (`drop_chunk` subtracts the whole chunk)
            let ch = Chunk::Sparse(Vec::new());
            self.bytes += ch.bytes();
            self.order.push_back(c);
            self.chunks.insert(c, ch);
        }
        let ch = self.chunks.get_mut(&c).expect("chunk present");
        let before = ch.bytes();
        let added = match ch {
            Chunk::Sparse(v) => match v.binary_search(&slot) {
                Ok(_) => false,
                Err(i) => {
                    v.insert(i, slot);
                    if v.len() > DENSE_AT {
                        let mut b = vec![0u64; CHUNK_SLOTS / 64].into_boxed_slice();
                        for s in v.iter() {
                            b[*s as usize / 64] |= 1 << (s % 64);
                        }
                        *ch = Chunk::Dense(b);
                    }
                    true
                }
            },
            Chunk::Dense(b) => {
                let w = &mut b[slot as usize / 64];
                let bit = 1u64 << (slot % 64);
                let new = *w & bit == 0;
                *w |= bit;
                new
            }
        };
        self.bytes = self.bytes + ch.bytes() - before;
        if added {
            self.n += 1;
        }
        while self.bytes > VERIFIED_BYTES_MAX && self.order.len() > 1 {
            let old = self.order.pop_front().expect("not empty");
            self.drop_chunk(old);
        }
        self.debug_check();
    }

    /// Debug builds: the byte count equals the sum of the chunks.
    fn debug_check(&self) {
        debug_assert_eq!(self.bytes, self.chunks.values().map(Chunk::bytes).sum::<usize>(), "verified set byte count drifted");
    }

    fn drop_chunk(&mut self, c: u64) {
        if let Some(ch) = self.chunks.remove(&c) {
            self.bytes -= ch.bytes();
            self.n -= match &ch {
                Chunk::Sparse(v) => v.len(),
                Chunk::Dense(b) => b.iter().map(|w| w.count_ones() as usize).sum(),
            };
        }
    }

    fn remove(&mut self, off: u64) {
        let Some((c, slot)) = Self::split(off) else { return };
        let removed = match self.chunks.get_mut(&c) {
            Some(Chunk::Sparse(v)) => match v.binary_search(&slot) {
                Ok(i) => {
                    v.remove(i);
                    true
                }
                Err(_) => false,
            },
            Some(Chunk::Dense(b)) => {
                let w = &mut b[slot as usize / 64];
                let bit = 1u64 << (slot % 64);
                let had = *w & bit != 0;
                *w &= !bit;
                had
            }
            None => false,
        };
        if removed {
            self.n -= 1;
        }
    }

    /// Keeps only offsets `< off`.
    fn retain_below(&mut self, off: u64) {
        let first = off >> CHUNK_SHIFT;
        let gone: Vec<u64> = self.chunks.keys().copied().filter(|c| *c > first).collect();
        for c in gone {
            self.drop_chunk(c);
        }
        self.order.retain(|c| self.chunks.contains_key(c));
        self.debug_check();
        let slots: Vec<u64> = match self.chunks.get(&first) {
            Some(Chunk::Sparse(v)) => v.iter().map(|s| (first << CHUNK_SHIFT) + ((*s as u64) << 3)).filter(|o| *o >= off).collect(),
            Some(Chunk::Dense(_)) => ((off & !7)..((first + 1) << CHUNK_SHIFT)).step_by(8).filter(|o| self.contains(*o)).collect(),
            None => Vec::new(),
        };
        for o in slots {
            self.remove(o);
        }
    }

    fn len(&self) -> usize {
        self.n
    }
}

struct CachedBlock {
    data: Arc<Vec<u8>>,
    tick: u64,
    /// cache hits since it was loaded
    hits: u32,
    /// loaded again by a promotion (after partial reads)
    promoted: bool,
}

/// Hits after which a promotion has paid for its complete read (about one
/// partial read per 16 KiB of block, at least 64).
fn promotion_payoff(len: usize) -> u32 {
    (len / (16 << 10)).max(64).min(u32::MAX as usize) as u32
}

/// Highest doubling of the promotion threshold (8 << 16 partial reads).
const PROMOTE_SHIFT_MAX: u32 = 16;

/// Memory of a cache entry besides its payload (map slot, `Arc`, `Vec`
/// header): counted against the budget, so that very many tiny blocks
/// cannot exceed it.
pub(crate) const ENTRY_OVERHEAD: usize = 112;

impl CachedBlock {
    fn cost(&self) -> usize {
        self.data.len() + ENTRY_OVERHEAD
    }
}

#[derive(Clone, Copy, Debug)]
struct VerifiedBlock {
    payload_len: u64,
    partial_reads: u32,
    block_type: u8,
    block_id: u64,
    flags: u16,
    /// tick of the last use (RUN8: the older half is evicted when full)
    used: u64,
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
    verified: VerifiedSet,
    /// payload length and partial-read counter of recently verified blocks
    recent: HashMap<u64, VerifiedBlock>,
    pub counters: BlockCacheCounters,
    /// RR6-08: the promotion threshold is `PROMOTE_AFTER << promote_shift`;
    /// it doubles whenever a promoted block is evicted before it paid off
    /// (random access over more blocks than fit: thrashing) and halves when
    /// promotions pay off again
    promote_shift: u32,
    /// the block `partial` just sent to a complete load as a promotion
    pending_promotion: Option<u64>,
}

impl BlockCache {
    fn get(&mut self, off: u64) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        let b = self.blocks.get_mut(&off)?;
        b.tick = tick;
        b.hits = b.hits.saturating_add(1);
        if b.promoted && b.hits == promotion_payoff(b.data.len()).saturating_mul(4) {
            self.promote_shift = self.promote_shift.saturating_sub(1);
        }
        self.counters.hits += 1;
        Some(b.data.clone())
    }

    fn insert(&mut self, off: u64, data: Arc<Vec<u8>>, budget: usize) {
        let promoted = self.pending_promotion.take() == Some(off);
        let n = data.len() + ENTRY_OVERHEAD;
        if let Some(old) = self.blocks.remove(&off) {
            self.bytes -= old.cost();
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
                    self.bytes -= b.cost();
                    self.counters.evictions += 1;
                    if b.promoted && b.hits < promotion_payoff(b.data.len()) {
                        self.promote_shift = (self.promote_shift + 2).min(PROMOTE_SHIFT_MAX);
                    }
                }
            }
        }
        if self.bytes + n > budget {
            return;
        }
        self.tick += 1;
        self.bytes += n;
        self.blocks.insert(off, CachedBlock { data, tick: self.tick, hits: 0, promoted });
    }

    /// Bytes of the cached payloads.
    pub(crate) fn cached_bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn cached_count(&self) -> usize {
        self.blocks.len()
    }

    /// Bytes of the verified registry and the recent block details.
    pub(crate) fn registry_bytes(&self) -> usize {
        self.verified.bytes + self.recent.len() * RECENT_COST
    }

    /// Everything this cache holds (booked in the process pot).
    pub(crate) fn tracked_bytes(&self) -> usize {
        self.cached_bytes() + self.registry_bytes()
    }

    /// Evicts least recently used blocks until at most `limit` payload
    /// bytes are cached; returns the bytes freed.
    pub(crate) fn evict_to(&mut self, limit: usize) -> usize {
        if self.bytes <= limit {
            return 0;
        }
        let before = self.bytes;
        let mut order: Vec<(u64, u64)> = self.blocks.iter().map(|(o, b)| (b.tick, *o)).collect();
        order.sort_unstable();
        for (_, o) in order {
            if self.bytes <= limit {
                break;
            }
            if let Some(b) = self.blocks.remove(&o) {
                self.bytes -= b.cost();
                self.counters.evictions += 1;
            }
        }
        before - self.bytes
    }

    /// Last resort of the process pot: forgets the oldest verified chunks
    /// and the recent details (only costs CRC re-checks); returns the bytes
    /// freed.
    pub(crate) fn shrink_registry(&mut self, want: usize) -> usize {
        let before = self.registry_bytes();
        if !self.recent.is_empty() {
            self.recent.clear();
        }
        while before - self.registry_bytes() < want && self.verified.order.len() > 1 {
            let old = self.verified.order.pop_front().expect("not empty");
            self.verified.drop_chunk(old);
        }
        before.saturating_sub(self.registry_bytes())
    }

    fn mark_verified(&mut self, info: &BlockInfo) {
        self.verified.insert(info.offset);
        self.remember(info);
    }

    fn remember(&mut self, info: &BlockInfo) {
        // bounded details: forgetting them only costs a header read
        if self.recent.len() >= RECENT_EVICT_AT && !self.recent.contains_key(&info.offset) {
            // RUN8: the least recently used half goes (no full reset)
            let mut used: Vec<u64> = self.recent.values().map(|v| v.used).collect();
            let mid = used.len() / 2;
            let cutoff = *used.select_nth_unstable(mid).1;
            self.recent.retain(|_, v| v.used > cutoff);
        }
        self.tick += 1;
        let v = VerifiedBlock { payload_len: info.payload_len, partial_reads: 0, block_type: info.block_type, block_id: info.block_id, flags: info.flags, used: self.tick };
        self.recent.insert(info.offset, v);
    }

    /// Whether the block at `off` may be read partially now: `Some(Some(len))`
    /// yes, `Some(None)` yes but its payload length must be read from the
    /// header, `None` load it completely.
    fn partial(&mut self, off: u64, max_item: usize) -> Option<Option<u64>> {
        if !self.verified.contains(off) {
            return None;
        }
        self.tick += 1;
        let tick = self.tick;
        let Some(v) = self.recent.get_mut(&off) else {
            self.counters.partial_reads += 1;
            return Some(None);
        };
        v.used = tick;
        if v.payload_len > max_item as u64 || v.partial_reads < PROMOTE_AFTER << self.promote_shift {
            v.partial_reads = v.partial_reads.saturating_add(1);
            self.counters.partial_reads += 1;
            return Some(Some(v.payload_len));
        }
        v.partial_reads = 0;
        self.pending_promotion = Some(off);
        None
    }

    /// Forgets every block at or behind `off` (the file was cut back there).
    pub(crate) fn invalidate_from(&mut self, off: u64) {
        let gone: Vec<u64> = self.blocks.keys().copied().filter(|o| *o >= off).collect();
        for o in gone {
            if let Some(b) = self.blocks.remove(&o) {
                self.bytes -= b.cost();
            }
        }
        self.verified.retain_below(off);
        self.recent.retain(|o, _| *o < off);
    }

    #[cfg(test)]
    pub(crate) fn is_verified(&self, off: u64) -> bool {
        self.verified.contains(off)
    }

    pub(crate) fn is_verified_pub(&self, off: u64) -> bool {
        self.verified.contains(off)
    }

    #[cfg(test)]
    pub(crate) fn is_cached(&self, off: u64) -> bool {
        self.blocks.contains_key(&off)
    }
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
        let info = self.read_header_at(block_offset)?;
        self.load_block_info(info)
    }

    /// [`Db::load_block`] with the header already read.
    fn load_block_info(&self, info: BlockInfo) -> Result<Arc<Vec<u8>>> {
        let block_offset = info.offset;
        let payload = self.read_payload_checked(&info)?;
        let arc = Arc::new(payload);
        let (budget, max_item) = self.block_budget();
        let mut c = self.cache();
        let bc = &mut c.block_cache;
        bc.counters.loads += 1;
        bc.mark_verified(&info);
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
        c.block_cache.mark_verified(&info);
        Ok(Arc::new(payload))
    }

    /// Access to a block for range reads: cached payload, or – for a block
    /// verified earlier in this session – direct range reads. An unverified
    /// block is loaded and checked completely first.
    pub(crate) fn block_access(&self, block_offset: u64) -> Result<BlockAccess> {
        Ok(self.block_access_fresh(block_offset)?.0)
    }

    /// [`Db::block_access`] plus whether the block was just loaded and
    /// CRC-checked for the first time in this session.
    pub(crate) fn block_access_fresh(&self, block_offset: u64) -> Result<(BlockAccess, bool)> {
        self.block_access_fresh_len(block_offset, None)
    }

    /// [`Db::block_access_fresh`] for a block whose payload length the
    /// caller knows from its entry (saves the header read of a verified
    /// block whose details were forgotten).
    pub(crate) fn block_access_fresh_len(&self, block_offset: u64, known_len: Option<u64>) -> Result<(BlockAccess, bool)> {
        let (_, max_item) = self.block_budget();
        {
            let mut c = self.cache();
            if let Some(b) = c.block_cache.get(block_offset) {
                return Ok((BlockAccess::Cached(b), false));
            }
            match c.block_cache.partial(block_offset, max_item) {
                Some(Some(payload_len)) => return Ok((BlockAccess::Verified { payload_len }, false)),
                Some(None) if known_len.is_some() => {
                    return Ok((BlockAccess::Verified { payload_len: known_len.unwrap() }, false));
                }
                Some(None) => {
                    drop(c);
                    // verified earlier, details forgotten: the header gives the length
                    let info = self.read_header_at(block_offset)?;
                    self.cache().block_cache.remember(&info);
                    return Ok((BlockAccess::Verified { payload_len: info.payload_len }, false));
                }
                None => {}
            }
        }
        // RUN9 V1: a block larger than the cache takes is CRC-checked in
        // chunks and then read by ranges; it never sits in memory as a whole
        let info = match known_len {
            Some(n) if n <= max_item as u64 => return Ok((BlockAccess::Cached(self.load_block(block_offset)?), true)),
            _ => self.read_header_at(block_offset)?,
        };
        if info.payload_len > max_item as u64 {
            self.verify_streaming_info(&info)?;
            return Ok((BlockAccess::Verified { payload_len: info.payload_len }, true));
        }
        Ok((BlockAccess::Cached(self.load_block_info(info)?), true))
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
            "vindex_indexes" => c.sparse_uindex.len() as i64, "vindex_bytes" => c.uindex_bytes as i64,
            "vindex_builds" => c.uindex_builds as i64, "vindex_catchups" => c.uindex_catchups as i64,
            "vindex_budget_bytes" => self.config.vindex_max_marks.saturating_mul(super::vindex::MARK_BYTES) as i64,
            "index_pages" => c.sec_pages.len() as i64, "index_page_bytes" => c.sec_pages.bytes() as i64,
            "index_page_budget_bytes" => self.config.index_page_cache_bytes as i64,
            "index_page_reads" => c.sec_reads as i64, "index_page_evictions" => c.sec_pages.evictions as i64,
        }
    }

    /// Payload CRC check of a block without loading it into memory as a
    /// whole (1 MiB chunks; small blocks go through the cache). Registers
    /// the block as verified.
    pub(crate) fn verify_block_streaming(&self, block_offset: u64) -> Result<()> {
        if self.cache().block_cache.verified.contains(block_offset) {
            return Ok(());
        }
        let info = self.read_header_at(block_offset)?;
        if info.payload_len <= MAX_ITEM_FLOOR as u64 {
            return self.load_block_info(info).map(|_| ());
        }
        self.verify_streaming_info(&info)
    }

    /// Chunked payload CRC check of the block `info` describes (1 MiB at a
    /// time, whatever its size); registers it as verified.
    fn verify_streaming_info(&self, info: &BlockInfo) -> Result<()> {
        let block_offset = info.offset;
        const CHUNK: u64 = 1 << 20;
        let mut h = crc32fast::Hasher::new();
        let mut buf = vec![0u8; CHUNK as usize];
        let start = block_offset + BLOCK_HEADER_SIZE as u64;
        let end = start + info.payload_len;
        let mut off = start;
        while off < end {
            let n = (end - off).min(CHUNK) as usize;
            self.store.read_exact_at(off, &mut buf[..n])?;
            h.update(&buf[..n]);
            off += n as u64;
        }
        if h.finalize() != info.payload_crc {
            return Err(Error::Corruption(format!("block {} (byte {block_offset}): payload CRC mismatch", info.block_id)));
        }
        self.cache().block_cache.mark_verified(info);
        Ok(())
    }

    /// Header fields `(type, block id, flags, payload length)` of a block:
    /// remembered from its CRC-checked load, else read (header CRC checked).
    pub(crate) fn block_header_of(&self, off: u64) -> Result<(u8, u64, u16, u64)> {
        if let Some(v) = self.cache().block_cache.recent.get(&off) {
            return Ok((v.block_type, v.block_id, v.flags, v.payload_len));
        }
        let h = self.read_header_at(off)?;
        Ok((h.block_type, h.block_id, h.flags, h.payload_len))
    }

    /// Forgets that the block at `off` was checked (its content contradicts
    /// its directory entry; also drops it from the cache).
    pub(crate) fn forget_verified(&self, off: u64) {
        let mut c = self.cache();
        let bc = &mut c.block_cache;
        bc.verified.remove(off);
        bc.recent.remove(&off);
        if let Some(b) = bc.blocks.remove(&off) {
            bc.bytes -= b.cost();
        }
    }

    /// Forgets cached/verified blocks at or behind `off` (file cut back).
    pub(crate) fn invalidate_blocks_from(&mut self, off: u64) {
        let mut c = self.cache();
        let c = &mut *c;
        c.block_cache.invalidate_from(off);
        // offsets at or behind `off` are handed to new blocks
        c.sparse_block_index.retain(|o| *o < off);
        let (mut freed, mut freed_bytes) = (0usize, 0usize);
        c.dir_pages.retain(|o, (p, _)| {
            let keep = *o < off;
            if !keep {
                freed += p.len();
                freed_bytes += p.cost();
            }
            keep
        });
        c.dir_pages_used = c.dir_pages_used.saturating_sub(freed);
        c.dir_pages_bytes = c.dir_pages_bytes.saturating_sub(freed_bytes);
        c.sec_pages.retain(|o| *o < off);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn field_type_cache_keeps_the_newer_half_when_full() {
        // RUN8: no full reset of the field type cache at its bound
        let p = tmp("fieldmeta");
        let r = Nat::S(1);
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        db.create_shape(1, &r, "s", vec![FieldDef::new(Nat::S(1), "b", TypeCode::Bytes)], "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
        for u in 1..=49_153u64 {
            db.field_meta_for_address(&r, &Nat::S(u), &Nat::S(1)).unwrap();
        }
        db.field_meta_for_address(&r, &Nat::S(7), &Nat::S(1)).unwrap(); // used again: newest
        assert_eq!(db.cache().field_meta.len(), 49_153);
        db.field_meta_for_address(&r, &Nat::S(70_000), &Nat::S(1)).unwrap();
        let c = db.cache();
        assert!(c.field_meta.len() > 24_000 && c.field_meta.len() < 25_000, "{}", c.field_meta.len());
        assert!(c.field_meta.contains_key(&(r.clone(), Nat::S(7), Nat::S(1))), "recently used entry evicted");
        assert!(!c.field_meta.contains_key(&(r.clone(), Nat::S(1), Nat::S(1))), "oldest entry kept");
        drop(c);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn recent_table_keeps_the_newer_half_when_full() {
        // RUN8: no full reset when full
        let mut bc = BlockCache::default();
        let info = |off: u64| BlockInfo { offset: off, block_type: 1, block_id: off, flags: 0, payload_len: 64, payload_crc: 0, next_offset: 0 };
        let n = RECENT_EVICT_AT as u64;
        for off in 0..n {
            bc.remember(&info(off * 8));
        }
        bc.verified.insert(0);
        assert!(bc.partial(0, 1 << 20).is_some()); // block 0 used again: now the newest
        bc.remember(&info(n * 8));
        assert!(bc.recent.len() > RECENT_EVICT_AT / 2 - 2 && bc.recent.len() <= RECENT_EVICT_AT / 2 + 1, "{}", bc.recent.len());
        assert!(bc.recent.contains_key(&0), "recently used entry evicted");
        assert!(!bc.recent.contains_key(&8), "oldest entry kept");
        assert!(bc.recent.contains_key(&(n * 8)));
    }
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
        db.dense[&(Nat::S(1), NL)].idx.tail[0].block_offset
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
        let plen = db.dense[&(Nat::S(1), NL)].idx.tail[0].payload_len;
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
        // RUN9 V1: CRC-checked in chunks, never loaded as a whole
        let st = db.block_cache_stats();
        assert_eq!(st.get("loads"), Some(&Val::Int(0)));
        // read_numpy/columns over the whole block: same values as get
        let cols = db.read_columns(&Nat::S(1), &Nat::S(1), 4000, None, NL, false).unwrap();
        assert_eq!(cols.present.iter().filter(|x| **x == 1).count(), 4000);
    }

    /// RUN9 V1: the first access to a block larger than the cache takes
    /// reads it in chunks of at most 1 MiB (CRC check) plus the prefix and
    /// the record, for `get` and for the column path; values are the ones a
    /// handle with a large cache returns.
    #[test]
    fn oversized_block_first_access_reads_at_most_one_mib_at_once() {
        use crate::storage::MAX_FILE_READ;
        let p = tmp("v1chunks");
        build(&p, 60_000, OpenOptions::default()).close(true).unwrap();
        let big = Db::open_with(&p, OpenOptions { readonly: true, ..Default::default() }).unwrap();
        let plen = big.dense[&(Nat::S(1), NL)].idx.tail[0].payload_len;
        assert!(plen > 3 << 20, "block must be several MiB ({plen})");
        for budget in [16usize << 10, 1 << 20] {
            let db = Db::open_with(&p, OpenOptions { readonly: true, ..opts(budget) }).unwrap();
            let want = field(&big, 31_337);
            MAX_FILE_READ.with(|c| c.set(0));
            let got = field(&db, 31_337);
            assert!(MAX_FILE_READ.with(|c| c.get()) <= 1 << 20, "budget {budget} max {}", MAX_FILE_READ.with(|c| c.get()));
            assert_eq!(got, want);
            assert_eq!(db.block_cache_stats().get("loads"), Some(&Val::Int(0)));
            for u in [1u64, 2, 59_999, 60_000] {
                assert_eq!(field(&db, u), field(&big, u));
            }
            // the column path reads the requested records as one range (its
            // size is the request, not the block); the check is chunked
            let db2 = Db::open_with(&p, OpenOptions { readonly: true, ..opts(budget) }).unwrap();
            let a = db2.read_columns(&Nat::S(1), &Nat::S(100), 50, None, NL, false).unwrap();
            let b = big.read_columns(&Nat::S(1), &Nat::S(100), 50, None, NL, false).unwrap();
            assert_eq!(a.present, b.present);
            assert_eq!(db2.block_cache_stats().get("loads"), Some(&Val::Int(0)));
        }
    }

    /// RUN9 V1: an oversized block whose prefix contradicts its directory
    /// entry is rejected on the chunked path as well and stays unverified.
    #[test]
    fn oversized_block_with_wrong_prefix_is_rejected() {
        let p = tmp("v1prefix");
        build(&p, 4000, OpenOptions::default()).close(true).unwrap();
        let db = Db::open_with(&p, OpenOptions { readonly: true, ..opts(16 << 10) }).unwrap();
        let mut seg = db.dense[&(Nat::S(1), NL)].idx.tail[0].clone();
        seg.count += 1;
        assert!(matches!(db.dense_block_access(&seg), Err(Error::Corruption(_))));
        assert!(!db.cache().block_cache.is_verified(seg.block_offset));
        assert!(matches!(db.ensure_verified_dense(&seg), Err(Error::Corruption(_))));
        let ok = db.dense[&(Nat::S(1), NL)].idx.tail[0].clone();
        assert!(db.ensure_verified_dense(&ok).is_ok());
        assert!(db.cache().block_cache.is_verified(ok.block_offset));
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
            let seg = db.dense[&(Nat::S(1), NL)].idx.tail[0].clone();
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
        let own = r.dense[&(Nat::S(1), NL)].idx.tail.last().unwrap().block_offset;
        assert!(r.cache().block_cache.is_verified(own));
        assert!(r.commit().is_err());
        assert!(!r.cache().block_cache.is_verified(own) && !r.cache().block_cache.is_cached(own));
        let _ = r.close(false);
        w.close(true).unwrap();
    }

    #[test]
    fn invalidate_from_and_lru_eviction() {
        let mut bc = BlockCache::default();
        for i in 0..10u64 {
            bc.mark_verified(&BlockInfo { offset: i * 104, block_type: 4, block_id: i, flags: 0, payload_len: 10, payload_crc: 0, next_offset: 0 });
            bc.insert(i * 104, Arc::new(vec![0u8; 10]), 4 * (10 + ENTRY_OVERHEAD));
            if i >= 1 {
                // keep block 0 hot
                assert!(bc.get(0).is_some());
            }
            assert!(bc.bytes <= 4 * (10 + ENTRY_OVERHEAD));
        }
        assert!(bc.is_cached(0), "recently used block survives the eviction");
        assert!(bc.is_cached(936), "newest block is cached");
        assert!(bc.counters.evictions > 0);
        bc.invalidate_from(520);
        assert!(bc.blocks.keys().all(|o| *o < 520) && (0..20u64).all(|i| bc.is_verified(i * 104) == (i * 104 < 520 && i < 10)));
        assert_eq!(bc.bytes, bc.blocks.values().map(|b| b.cost()).sum::<usize>());
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
            c.block_cache.bytes -= b.cost();
        }
        for u in 1..=PROMOTE_AFTER as u64 {
            assert!(field(&db, u).is_some());
            assert!(!db.cache().block_cache.is_cached(off));
        }
        assert!(field(&db, 49).is_some());
        assert!(db.cache().block_cache.is_cached(off));
        assert_eq!(db.block_cache_stats().get("loads"), Some(&Val::Int(2)));
    }

    /// RR6-08 (RUN7): random access over more cacheable blocks than the
    /// cache holds must not thrash (each promotion evicting the blocks the
    /// next promotions need): the promotion threshold backs off, the number
    /// of complete loads stays small and every value stays right. A
    /// promotion that pays off (sequential reading) still happens.
    #[test]
    fn random_access_over_more_blocks_than_cached_does_not_thrash() {
        let p = tmp("thrash");
        let r = Nat::S(1);
        let blocks = 30u64;
        let per = 400u64;
        {
            let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
            db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
            db.create_shape(1, &r, "s", vec![FieldDef::new(Nat::S(1), "b", TypeCode::Bytes)], "").unwrap();
            db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
            for b in 0..blocks {
                let rows: Vec<Vec<Val>> = (1..=per).map(|i| vec![Val::Bytes(rec(b * per + i))]).collect();
                db.put_dense_batch(&r, &Nat::S(b * per + 1), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
            }
            db.close(true).unwrap();
        }
        let block_bytes = std::fs::metadata(&p).unwrap().len() as usize / blocks as usize;
        // room for about a third of the blocks
        let db = Db::open_with(&p, opts(block_bytes * blocks as usize / 3)).unwrap();
        let n = blocks * per;
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..60_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let u = 1 + x % n;
            assert_eq!(field(&db, u), Some(Val::Bytes(rec(u))));
        }
        let loads = db.block_cache_stats().get("loads").and_then(|v| v.as_i64()).unwrap();
        // before RUN7 about one complete load per 9 accesses (> 6 000)
        assert!(loads < 1_000, "{loads} complete loads for 60 000 random accesses over {blocks} blocks");
        // sequential reading of an evicted block still brings it back
        let off = first_seg_offset(&db);
        {
            let mut c = db.cache();
            if let Some(b) = c.block_cache.blocks.remove(&off) {
                c.block_cache.bytes -= b.cost();
            }
            c.block_cache.promote_shift = 0;
        }
        for u in 1..=per {
            assert_eq!(field(&db, u), Some(Val::Bytes(rec(u))));
        }
        assert!(db.cache().block_cache.is_cached(off));
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

    /// RC-02: the verified set against a hash set (sparse and dense chunks,
    /// removal, cut-back); never cleared because of many blocks.
    #[test]
    fn verified_set_against_hashset() {
        let mut vs = VerifiedSet::default();
        let mut hs = std::collections::HashSet::new();
        let mut seed = 5u64;
        for i in 0..200_000u64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            // dense region (tiny blocks) and sparse region (large blocks)
            let off = if i % 2 == 0 { 8192 + (seed >> 40) % 600_000 * 8 } else { (1 << 32) + (seed >> 30) % 4000 * (1 << 21) };
            if i % 7 == 0 {
                vs.remove(off);
                hs.remove(&off);
            } else {
                vs.insert(off);
                hs.insert(off);
            }
        }
        assert_eq!(vs.len(), hs.len());
        assert!(hs.iter().all(|o| vs.contains(*o)));
        assert!(!vs.contains(8193) && !vs.contains(12));
        assert!(vs.bytes < 4 << 20, "{} bytes for {} blocks", vs.bytes, vs.len());
        let cut = 8192 + 300_000 * 8 + 8;
        vs.retain_below(cut);
        hs.retain(|o| *o < cut);
        assert_eq!(vs.len(), hs.len());
        assert!(hs.iter().all(|o| vs.contains(*o)));
        assert!(!vs.contains(1 << 32));
    }

    /// RB3-01: the byte count equals the sum of the chunks after inserts,
    /// chunk drops (registry bound, shrink) and cut-backs; it never drifts
    /// below zero (the fixed part of new chunks was not booked).
    #[test]
    fn verified_set_byte_count_is_exact() {
        let sum = |vs: &VerifiedSet| vs.chunks.values().map(Chunk::bytes).sum::<usize>();
        let mut vs = VerifiedSet::default();
        for c in 0..5000u64 {
            vs.insert((c << CHUNK_SHIFT) + 8 * (c % 7));
            assert_eq!(vs.bytes, sum(&vs));
        }
        let mut freed = 0usize;
        while vs.order.len() > 1 {
            let old = vs.order.pop_front().unwrap();
            let before = vs.bytes;
            vs.drop_chunk(old);
            freed += before - vs.bytes;
            assert_eq!(vs.bytes, sum(&vs));
        }
        assert!(freed >= 4999 * 48);
        vs.retain_below(0);
        assert_eq!(vs.bytes, 0);
        assert_eq!(vs.len(), 0);
        let mut cache = BlockCache::default();
        for c in 0..300u64 {
            cache.verified.insert(c << CHUNK_SHIFT);
        }
        let before = cache.registry_bytes();
        let got = cache.shrink_registry(usize::MAX);
        assert_eq!(cache.registry_bytes(), before - got);
        assert_eq!(cache.verified.bytes, sum(&cache.verified));
    }
}
