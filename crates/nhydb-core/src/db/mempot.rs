//! Process-wide cache budget (0.28.2, round 3) and the byte-budgeted LRU
//! map of the decode caches.
//!
//! Every handle keeps its caches in its own `Mutex<Caches>` with per-pool
//! budgets from its configuration (`[cache] block_cache_bytes`,
//! `index_page_cache_bytes`, `posting_cache_bytes`, `sparse_cache_bytes`,
//! v index and directory pages). On top of that all handles of the process
//! share one pot (`[cache] total_bytes`, default 4 GiB):
//!
//! * the pot is defined by the configuration of the first handle opened
//!   while no other handle is open; a handle with a different value keeps
//!   the pot as it is and gets a warning (`stats()["cache"]`, `health`);
//! * every change of a handle's caches is booked when its cache lock is
//!   released ([`CacheGuard`]); if the pot is above its limit, the least
//!   recently used entries of the handles that are not busy are evicted
//!   (the booking handle included) until it is at 7/8 of the limit;
//! * a busy handle (lock held by another thread) is skipped and trimmed by
//!   the next booking; the pot can therefore exceed its limit by at most
//!   the entries inserted while all other handles were busy;
//! * nothing is shared between processes: each process has its own pot.
//!
//! Evicted entries are only re-read from the file (CRC-checked again):
//! eviction never changes a result.

use std::collections::HashMap;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::Caches;

/// Default of `[cache] total_bytes`.
pub const DEFAULT_TOTAL_BYTES: u64 = 4 << 30;

// ----------------------------------------------------------------------
// Byte-budgeted LRU map
// ----------------------------------------------------------------------

struct Slot<V> {
    val: V,
    cost: usize,
    tick: u64,
}

/// HashMap with exact byte accounting and least-recently-used eviction in
/// batches (down to 3/4 of the budget). An entry larger than the budget is
/// not cached.
pub(crate) struct BudgetMap<K, V> {
    map: HashMap<K, Slot<V>>,
    bytes: usize,
    tick: u64,
    pub evictions: u64,
}

impl<K, V> Default for BudgetMap<K, V> {
    fn default() -> Self {
        BudgetMap { map: HashMap::new(), bytes: 0, tick: 0, evictions: 0 }
    }
}


impl<K: Hash + Eq + Clone, V: Clone> BudgetMap<K, V> {
    /// Memory of one entry besides its value: the hash table slot (key,
    /// value handle, cost, tick) one and a half times – a table is between
    /// 7/16 and 7/8 full – plus its control bytes (measured against the
    /// allocator in `tests/memory_bounds.rs`, case 8).
    pub(crate) fn entry_overhead() -> usize {
        3 * std::mem::size_of::<(K, Slot<V>)>() / 2 + 8
    }

    pub(crate) fn get(&mut self, k: &K) -> Option<V> {
        self.tick += 1;
        let t = self.tick;
        let s = self.map.get_mut(k)?;
        s.tick = t;
        Some(s.val.clone())
    }

    pub(crate) fn contains(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }

    /// Inserts `val` (estimated heap bytes `cost`) within `budget`.
    pub(crate) fn insert(&mut self, k: K, val: V, cost: usize, budget: usize) {
        let cost = cost + Self::entry_overhead();
        if cost > budget {
            self.remove(&k);
            return;
        }
        if self.bytes + cost > budget {
            let limit = (budget - budget / 4).saturating_sub(cost);
            self.evict_to(limit);
        }
        self.tick += 1;
        self.bytes += cost;
        if let Some(old) = self.map.insert(k, Slot { val, cost, tick: self.tick }) {
            self.bytes -= old.cost;
        }
    }

    /// Evicts least recently used entries until at most `limit` bytes are
    /// held; returns the bytes freed. The cut-off tick is found by linear
    /// selection (no sort of all entries), estimated from the average entry
    /// cost and repeated if that freed too little.
    pub(crate) fn evict_to(&mut self, limit: usize) -> usize {
        let before = self.bytes;
        while self.bytes > limit && !self.map.is_empty() {
            let n = self.map.len();
            let need = self.bytes - limit;
            // oldest k entries by tick; k from the share of bytes to free
            let k = ((n as u128 * need as u128).div_ceil(self.bytes.max(1) as u128) as usize).clamp(1, n);
            let mut ticks: Vec<u64> = self.map.values().map(|s| s.tick).collect();
            let cut = *ticks.select_nth_unstable(k - 1).1;
            let mut freed = 0usize;
            let mut gone = 0u64;
            self.map.retain(|_, s| {
                let keep = s.tick > cut;
                if !keep {
                    freed += s.cost;
                    gone += 1;
                }
                keep
            });
            self.bytes -= freed;
            self.evictions += gone;
        }
        before - self.bytes
    }

    pub(crate) fn remove(&mut self, k: &K) {
        if let Some(s) = self.map.remove(k) {
            self.bytes -= s.cost;
        }
    }

    pub(crate) fn retain(&mut self, mut f: impl FnMut(&K) -> bool) {
        let mut freed = 0usize;
        self.map.retain(|k, s| {
            let keep = f(k);
            if !keep {
                freed += s.cost;
            }
            keep
        });
        self.bytes -= freed;
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

// ----------------------------------------------------------------------
// Process pot
// ----------------------------------------------------------------------

struct PotState {
    limit: u64,
    /// configuration file that defined the limit (None = default/explicit)
    source: Option<PathBuf>,
    members: Vec<Weak<Mutex<Caches>>>,
}

pub(crate) struct Pot {
    state: Mutex<PotState>,
    used: AtomicUsize,
    limit: AtomicU64,
    reclaims: AtomicU64,
    reclaimed: AtomicU64,
    peak: AtomicUsize,
    /// RUN¹⁰: temporary relief target set by the RAM limiter (`u64::MAX` =
    /// none); the configured limit stays as it is
    relief: AtomicU64,
}

pub(crate) static POT: Pot = Pot {
    state: Mutex::new(PotState { limit: DEFAULT_TOTAL_BYTES, source: None, members: Vec::new() }),
    used: AtomicUsize::new(0),
    limit: AtomicU64::new(DEFAULT_TOTAL_BYTES),
    reclaims: AtomicU64::new(0),
    reclaimed: AtomicU64::new(0),
    peak: AtomicUsize::new(0),
    relief: AtomicU64::new(u64::MAX),
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `try_lock` that, like `lock`, also takes a poisoned mutex: a panic under
/// a cache lock must not exclude the handle from the eviction (RB3-03).
fn try_lock<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

impl Pot {
    /// Registers the caches of a new handle. Returns a warning if the
    /// handle asks for another limit than the one in force.
    pub(crate) fn register(&self, caches: &Arc<Mutex<Caches>>, limit: u64, source: Option<PathBuf>) -> Option<String> {
        let mut st = lock(&self.state);
        st.members.retain(|w| w.strong_count() > 0);
        let warn = if st.members.is_empty() {
            st.limit = limit;
            st.source = source;
            self.limit.store(limit, Ordering::Relaxed);
            None
        } else if st.limit != limit {
            Some(format!(
                "cache.total_bytes = {limit} of this handle ignored: the process cache budget is {} bytes{} (set by the first open handle; change it while no handle is open)",
                st.limit,
                st.source.as_ref().map(|p| format!(" from {}", p.display())).unwrap_or_default()
            ))
        } else {
            None
        };
        st.members.push(Arc::downgrade(caches));
        warn
    }

    /// Leaves the pot (closed handle); its bytes were released by resetting
    /// its caches.
    /// `user`: an application handle (not an internal temporary one).
    pub(crate) fn unregister(&self, caches: &Arc<Mutex<Caches>>, user: bool) {
        let mut st = lock(&self.state);
        let me = Arc::downgrade(caches);
        st.members.retain(|w| w.strong_count() > 0 && !w.ptr_eq(&me));
        let last = st.members.is_empty();
        drop(st);
        if last && user {
            // AP10.7: the last handle of the process closed – write the
            // started statistics interval
            crate::limiter::global().log_final();
        }
    }

    /// Limit in force: the configured one, during a relief of the RAM
    /// limiter at most its target.
    pub(crate) fn limit(&self) -> usize {
        let l = self.limit.load(Ordering::Relaxed).min(self.relief.load(Ordering::Relaxed));
        usize::try_from(l).unwrap_or(usize::MAX)
    }

    /// RUN¹⁰: the limiter asks the caches of the process to shrink to
    /// `target` bytes (least recently used entries first, busy handles are
    /// skipped); they stay below it until [`end_relief`](Self::end_relief).
    /// Returns the bytes freed now.
    pub(crate) fn relieve(&self, target: u64) -> u64 {
        self.relief.store(target, Ordering::Relaxed);
        let before = self.used();
        self.reclaim_to(usize::try_from(target).unwrap_or(usize::MAX));
        before.saturating_sub(self.used()) as u64
    }

    /// End of a relief: the caches fill up again by normal use.
    pub(crate) fn end_relief(&self) {
        self.relief.store(u64::MAX, Ordering::Relaxed);
    }

    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    fn book(&self, old: usize, new: usize) {
        if new >= old {
            let now = self.used.fetch_add(new - old, Ordering::Relaxed) + (new - old);
            // statistics only: a racy maximum is good enough (no second RMW)
            if now > self.peak.load(Ordering::Relaxed) {
                self.peak.store(now, Ordering::Relaxed);
            }
        } else {
            self.used.fetch_sub(old - new, Ordering::Relaxed);
        }
    }

    /// Evicts from the registered handles until the pot is at 7/8 of its
    /// limit. Must be called without holding any cache lock.
    fn reclaim(&self) {
        let limit = self.limit();
        if self.used() <= limit {
            return;
        }
        self.reclaim_to(limit - limit / 8);
    }

    /// Evicts until the pot holds at most `target` bytes (as far as the
    /// handles are not busy).
    fn reclaim_to(&self, target: usize) {
        if self.used() <= target {
            return;
        }
        let members: Vec<Arc<Mutex<Caches>>> = {
            let mut st = lock(&self.state);
            st.members.retain(|w| w.strong_count() > 0);
            st.members.iter().filter_map(|w| w.upgrade()).collect()
        };
        self.reclaims.fetch_add(1, Ordering::Relaxed);
        // largest handles first, each trimmed by its share of the excess
        let mut sizes: Vec<(usize, Arc<Mutex<Caches>>)> = members
            .into_iter()
            .filter_map(|m| {
                let n = try_lock(&m)?.charged;
                Some((n, m))
            })
            .collect();
        sizes.sort_by_key(|s| std::cmp::Reverse(s.0));
        let total: usize = sizes.iter().map(|s| s.0).sum::<usize>().max(1);
        for pass in 0..2 {
            for (n, m) in &sizes {
                let used = self.used();
                if used <= target {
                    return;
                }
                let excess = used - target;
                let want = if pass == 0 { ((excess as u128 * *n as u128) / total as u128) as usize + 1 } else { excess };
                if let Some(mut c) = try_lock(m) {
                    let freed = c.shrink(want);
                    self.reclaimed.fetch_add(freed as u64, Ordering::Relaxed);
                    c.sync_pot();
                }
            }
        }
    }

    pub(crate) fn stats(&self) -> PotStats {
        let st = lock(&self.state);
        PotStats {
            limit: st.limit,
            source: st.source.clone(),
            used: self.used(),
            peak: self.peak.load(Ordering::Relaxed),
            handles: st.members.iter().filter(|w| w.strong_count() > 0).count(),
            reclaims: self.reclaims.load(Ordering::Relaxed),
            reclaimed: self.reclaimed.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct PotStats {
    pub limit: u64,
    pub source: Option<PathBuf>,
    pub used: usize,
    pub peak: usize,
    pub handles: usize,
    pub reclaims: u64,
    pub reclaimed: u64,
}

impl Caches {
    /// Books the change of this handle's cache bytes since the last call.
    /// Returns whether the pot is above its limit.
    pub(crate) fn sync_pot(&mut self) -> bool {
        let now = self.tracked_bytes();
        if now != self.charged {
            POT.book(self.charged, now);
            self.charged = now;
        }
        POT.used() > POT.limit()
    }
}

impl Drop for Caches {
    fn drop(&mut self) {
        if self.charged > 0 {
            POT.book(self.charged, 0);
            self.charged = 0;
        }
    }
}

/// Cache lock of a handle: books the change on release and evicts across
/// handles when the pot is full.
pub(crate) struct CacheGuard<'a> {
    inner: Option<MutexGuard<'a, Caches>>,
}

impl<'a> CacheGuard<'a> {
    #[cfg(not(feature = "lock_stats"))]
    pub(crate) fn new(m: &'a Mutex<Caches>) -> CacheGuard<'a> {
        CacheGuard { inner: Some(lock(m)) }
    }

    #[cfg(feature = "lock_stats")]
    pub(crate) fn new(m: &'a Mutex<Caches>) -> CacheGuard<'a> {
        lock_stats::ACQUIRED.fetch_add(1, Ordering::Relaxed);
        if let Some(g) = try_lock(m) {
            return CacheGuard { inner: Some(g) };
        }
        let t = std::time::Instant::now();
        let g = lock(m);
        lock_stats::CONTENDED.fetch_add(1, Ordering::Relaxed);
        lock_stats::WAIT_NS.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        CacheGuard { inner: Some(g) }
    }
}

/// RUN8 AP0 (A7 analysis, feature `lock_stats`): process-wide counters of the
/// cache lock of all handles.
#[cfg(feature = "lock_stats")]
pub mod lock_stats {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) static ACQUIRED: AtomicU64 = AtomicU64::new(0);
    pub(crate) static CONTENDED: AtomicU64 = AtomicU64::new(0);
    pub(crate) static WAIT_NS: AtomicU64 = AtomicU64::new(0);

    /// `(acquisitions, contended acquisitions, total wait in ns)`; resets.
    pub fn take() -> (u64, u64, u64) {
        (ACQUIRED.swap(0, Ordering::Relaxed), CONTENDED.swap(0, Ordering::Relaxed), WAIT_NS.swap(0, Ordering::Relaxed))
    }
}

impl Deref for CacheGuard<'_> {
    type Target = Caches;
    fn deref(&self) -> &Caches {
        self.inner.as_ref().expect("guard alive")
    }
}

impl DerefMut for CacheGuard<'_> {
    fn deref_mut(&mut self) -> &mut Caches {
        self.inner.as_mut().expect("guard alive")
    }
}

impl Drop for CacheGuard<'_> {
    fn drop(&mut self) {
        let over = match self.inner.as_mut() {
            Some(c) => c.sync_pot(),
            None => false,
        };
        drop(self.inner.take());
        // RX-02: no eviction while unwinding (a second panic would abort)
        if over && !std::thread::panicking() {
            POT.reclaim();
        }
    }
}

impl super::Db {
    /// `stats()["cache"]`: process pot and the pools of this handle.
    pub(crate) fn cache_stats(&self) -> crate::val::Val {
        use crate::val::Val;
        use crate::vmap;
        let p = POT.stats();
        let warnings: Vec<Val> = self.open_warnings.iter().filter(|w| w.starts_with("cache.total_bytes")).map(|w| Val::str(w.as_str())).collect();
        let cfg = &self.config;
        let c = self.cache();
        let pool = |budget: usize, bytes: usize, entries: usize, evictions: u64| {
            vmap! {"budget_bytes" => budget as i64, "bytes" => bytes as i64, "entries" => entries as i64, "evictions" => evictions as i64}
        };
        vmap! {
            "process" => vmap! {
                "limit_bytes" => p.limit as i64, "used_bytes" => p.used as i64, "peak_bytes" => p.peak as i64,
                "handles" => p.handles as i64, "reclaims" => p.reclaims as i64, "reclaimed_bytes" => p.reclaimed as i64,
                "source" => p.source.map(|s| Val::Str(s.display().to_string())).unwrap_or(Val::None),
                "warnings" => Val::List(warnings),
            },
            "handle_bytes" => c.tracked_bytes() as i64,
            "pools" => vmap! {
                "block" => pool(cfg.block_cache_bytes, c.block_cache.cached_bytes(), c.block_cache.cached_count(), c.block_cache.counters.evictions),
                "index_pages" => pool(cfg.index_page_cache_bytes, c.sec_pages.bytes(), c.sec_pages.len(), c.sec_pages.evictions),
                "postings" => pool(cfg.posting_cache_bytes, c.postings.bytes(), c.postings.len(), c.postings.evictions),
                "sparse" => pool(cfg.sparse_cache_bytes, c.sparse_bytes(), c.sparse_block_index.len() + c.sparse_record.len() + c.sparse_merged.len(),
                    c.sparse_block_index.evictions + c.sparse_record.evictions + c.sparse_merged.evictions),
                "vindex" => pool(cfg.vindex_max_marks.saturating_mul(super::vindex::MARK_BYTES), c.uindex_bytes, c.sparse_uindex.len(), 0),
                "directory_pages" => vmap! {"budget_entries" => cfg.directory_cache_entries as i64, "bytes" => c.dir_pages_bytes as i64, "entries" => c.dir_pages.len() as i64},
                "field_meta" => vmap! {"bytes" => c.field_meta_bytes() as i64, "entries" => c.field_meta.len() as i64},
                "verified_registry" => vmap! {"bytes" => c.block_cache.registry_bytes() as i64},
            },
        }
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "mempot_tests.rs"]
mod tests;
