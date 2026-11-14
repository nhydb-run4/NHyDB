//! RUN¹⁰: the central RAM limiter of the process (exactly one per process).
//!
//! It watches the footprint of the process (the *water level*,
//! `footprint.rs`) and, under memory pressure, steers the large jobs that
//! registered with it; the jobs react with their own means at their
//! checkpoints. Single operations never register.
//!
//! * **Limit:** `limit = (100 − reserve_percent) % × RAM` (Linux: RAM at
//!   most the cgroup v2 `memory.max`). The yellow zone starts
//!   `warn_percent` % of RAM below the limit; leaving a zone needs a
//!   hysteresis of a quarter of the warning band.
//! * **Monitor thread:** started at the first need, ticks every `tick_ms`
//!   with `thread::sleep` (precise on Windows, where a timed condition-
//!   variable wait rounds up to the 15.6 ms timer; AP10.0 M-T2) while jobs
//!   run or the zone is not green, and otherwise parks on a condition
//!   variable without timeout (0 wake-ups). A panic in a tick is caught: the
//!   limiter then only observes and reports the error.
//! * **Settings** come from the first handle opened in the process
//!   (`[limiter]`); other values of later handles are reported as a hint.

pub mod footprint;
pub mod job;
pub mod log;
pub mod policy;

pub use job::{CancelReason, Job, JobKind, JobPath, Meter, Slot, SlotState};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::val::Val;
use crate::vmap;

/// `[limiter]` of `nhydb_config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimiterConfig {
    /// `false`: the limiter only measures and counts, it never steers.
    pub enabled: bool,
    /// Share of the RAM kept free for the system and other processes.
    pub reserve_percent: u32,
    /// Width of the yellow zone below the limit, in % of the RAM.
    pub warn_percent: u32,
    /// Tick of the monitor thread while it is active.
    pub tick_ms: u32,
    /// A job waiting longer than this is marked as waiting long.
    pub notify_wait_s: u64,
    /// 0 = waiting never ends with an error (default).
    pub wait_timeout_s: u64,
    /// RAM the limiter assumes (0 = the physical RAM; Linux: at most the
    /// cgroup limit). Lets NHyDB use only a part of a large machine; also
    /// the test switch of the stress measurements.
    pub ram_bytes: u64,
    /// Diagnosis (AP10.3): decisions are computed and counted (`dry_*`)
    /// but never sent to the jobs.
    pub dry_run: bool,
    /// AP10.5 `TEST_IT`: growth a waiting job may try while the level is
    /// high but stable (0 = no probes).
    pub probe_bytes: u64,
    /// N5: last resort – cancel the single running job that keeps the
    /// process above its limit (`ResourceLimitError`, reason
    /// `cancelled_by_limiter`); `false`: only reported.
    pub cancel_over_limit: bool,
    /// AP10.7: statistics file `limiter.txt` (`[limiter_log]`).
    pub log: log::LogConfig,
}

pub const DEFAULT_PROBE_BYTES: u64 = 256 << 20;

pub const DEFAULT_RESERVE_PERCENT: u32 = 10;
pub const DEFAULT_WARN_PERCENT: u32 = 5;
pub const DEFAULT_TICK_MS: u32 = 20;
pub const DEFAULT_NOTIFY_WAIT_S: u64 = 300;

impl Default for LimiterConfig {
    fn default() -> Self {
        LimiterConfig {
            enabled: true,
            reserve_percent: DEFAULT_RESERVE_PERCENT,
            warn_percent: DEFAULT_WARN_PERCENT,
            tick_ms: DEFAULT_TICK_MS,
            notify_wait_s: DEFAULT_NOTIFY_WAIT_S,
            wait_timeout_s: 0,
            ram_bytes: 0,
            dry_run: false,
            probe_bytes: DEFAULT_PROBE_BYTES,
            cancel_over_limit: true,
            log: log::LogConfig::default(),
        }
    }
}

impl LimiterConfig {
    /// Checks the value ranges (message without the file name).
    pub fn check(&self) -> std::result::Result<(), String> {
        if !(1..=90).contains(&self.reserve_percent) {
            return Err("'limiter.reserve_percent' must be between 1 and 90".into());
        }
        if self.warn_percent == 0 || self.reserve_percent + self.warn_percent >= 100 {
            return Err("'limiter.warn_percent' must be at least 1 and reserve_percent + warn_percent below 100".into());
        }
        if !(5..=1000).contains(&self.tick_ms) {
            return Err("'limiter.tick_ms' must be between 5 and 1000".into());
        }
        if self.ram_bytes != 0 && self.ram_bytes < (64 << 20) {
            return Err("'limiter.ram_bytes' must be 0 (physical RAM) or at least 64 MiB".into());
        }
        self.log.check()
    }
}

/// Zone of the water level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Zone {
    Green = 0,
    Yellow = 1,
    Red = 2,
}

impl Zone {
    fn from_u8(v: u8) -> Zone {
        match v {
            1 => Zone::Yellow,
            2 => Zone::Red,
            _ => Zone::Green,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Zone::Green => "green",
            Zone::Yellow => "yellow",
            Zone::Red => "red",
        }
    }
}

/// Limit, yellow threshold and hysteresis in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub ram: u64,
    pub ram_cap: Option<u64>,
    pub limit: u64,
    pub yellow_at: u64,
    pub hysteresis: u64,
}

impl Bounds {
    pub fn new(cfg: &LimiterConfig, ram: u64, ram_cap: Option<u64>) -> Bounds {
        let mut base = ram_cap.map_or(ram, |c| c.min(ram));
        if cfg.ram_bytes > 0 {
            base = base.min(cfg.ram_bytes);
        }
        let pct = |p: u32| (base as u128 * p as u128 / 100) as u64;
        let limit = pct(100 - cfg.reserve_percent);
        let warn = pct(cfg.warn_percent);
        Bounds { ram, ram_cap, limit, yellow_at: limit.saturating_sub(warn), hysteresis: warn / 4 }
    }

    /// Next zone for a water level (pure function). Entering a zone is
    /// immediate; leaving it needs the level a hysteresis below its border.
    pub fn next_zone(&self, cur: Zone, level: u64) -> Zone {
        let near = |border: u64| level.saturating_add(self.hysteresis) >= border;
        if level >= self.limit || (cur == Zone::Red && near(self.limit)) {
            Zone::Red
        } else if level >= self.yellow_at || (cur != Zone::Green && near(self.yellow_at)) {
            Zone::Yellow
        } else {
            Zone::Green
        }
    }
}

macro_rules! counters {
    ($($name:ident),+ $(,)?) => {
        /// Statistics of the limiter (one `fetch_add` per event).
        #[derive(Default)]
        pub struct Counters { $(pub $name: AtomicU64),+ }
        impl Counters {
            pub fn to_val(&self) -> Val {
                vmap! { $(stringify!($name) => self.$name.load(Ordering::Relaxed) as i64),+ }
            }
            fn reset(&self) {
                $(self.$name.store(0, Ordering::Relaxed);)+
            }
            /// All counters (name, value), in declaration order.
            pub fn values(&self) -> Vec<(&'static str, u64)> {
                vec![$((stringify!($name), self.$name.load(Ordering::Relaxed))),+]
            }
        }
    };
}

counters!(
    ticks,
    green_ms,
    yellow_ms,
    red_ms,
    zone_changes,
    peak_level,
    peak_over_limit,
    sample_errors,
    thread_starts,
    tick_panics,
    max_rise_per_tick,
    signals_shrink,
    signals_wait,
    signals_resume,
    dry_shrink,
    dry_wait,
    dry_resume,
    dry_relief_bytes,
    cache_relief_requests,
    cache_relief_bytes,
    cache_relief_ineffective,
    over_limit_single_ticks,
    cancel_held_ticks,
    signals_cancel,
    waits_deferred,
    long_waits,
    wait_timeouts,
    wait_ms_total,
    wait_ms_max,
    jobs_registered,
    jobs_finished,
    jobs_joined,
    trims,
    trim_ms_total,
    probes,
    probes_reused,
    probes_grew,
    probes_invalid,
);

/// AP10.7: maxima of the current log interval (start anew with each record).
#[derive(Default)]
pub struct IntervalMax {
    pub peak_level: AtomicU64,
    pub peak_over_limit: AtomicU64,
    pub max_rise_per_tick: AtomicU64,
    pub wait_ms_max: AtomicU64,
}

/// Counters that are maxima: the log takes them from [`IntervalMax`].
const MAX_COUNTERS: [&str; 4] = ["peak_level", "peak_over_limit", "max_rise_per_tick", "wait_ms_max"];

struct Inner {
    cfg: LimiterConfig,
    cfg_source: Option<PathBuf>,
    configured: bool,
    bounds: Bounds,
    hints: Vec<String>,
    /// process whose monitor thread runs (fork detection)
    thread_pid: Option<u32>,
    last_tick: Option<Instant>,
    error: Option<String>,
    /// registered jobs (AP10.2), unordered; id = age
    jobs: Vec<Arc<Slot>>,
    next_job_id: u64,
    /// start of the current yellow/red phase (jobs registered later are
    /// "new" for the policy)
    phase_since: Option<Instant>,
    /// level when the last cache relief was requested and bytes it freed
    /// (feedback at the next tick)
    relief_pending: Option<(u64, u64)>,
    /// a relief of this phase did not lower the level: no further ones
    relief_ineffective: bool,
    /// a relief target is set in the cache pot
    relief_on: bool,
    /// AP10.5: the running `TEST_IT` probe and the stability of the level
    probe: Option<ProbeRun>,
    stable_ticks: u32,
    last_level: u64,
    /// after real growth was found: no probe while the level is above this
    probe_block_above: Option<u64>,
    /// N5: ticks above the limit since the phase began or the last cancel
    over_ticks: u32,
    /// N5: level at the last cancel of the limiter; the next cancel needs
    /// real growth above it (memory of a cancelled job comes back slowly,
    /// e.g. the Windows heap); `None` again once the level is at the limit
    cancel_level: Option<u64>,
    /// AP10.7: the statistics file
    logger: Option<log::Logger>,
    log_next: Option<Instant>,
    log_last: Vec<u64>,
    log_last_paths: Vec<u64>,
    log_since: Option<std::time::SystemTime>,
    db: Option<PathBuf>,
}

/// A running `TEST_IT` probe.
struct ProbeRun {
    id: u64,
    level0: u64,
    grown0: u64,
    /// growth reported by all other jobs at the start
    others0: u64,
    ticks: u32,
}

/// The level counts as stable while it moves less than this per tick.
const STABLE_DELTA: u64 = 16 << 20;
/// Stable ticks before a probe starts.
const STABLE_TICKS: u32 = 5;
/// A probe ends after this many ticks even if the job grew less.
const PROBE_MAX_TICKS: u32 = 100;

/// The limiter. The process uses [`global`]; tests build their own with a
/// footprint source of their choice.
pub struct Limiter {
    source: fn() -> Option<u64>,
    ram: fn() -> Option<u64>,
    ram_cap: fn() -> Option<u64>,
    inner: Mutex<Inner>,
    /// only the process limiter relieves the process-wide cache pot (test
    /// instances with simulated levels must not evict real caches)
    owns_pot: bool,
    wake: Condvar,
    level: AtomicU64,
    zone: AtomicU8,
    /// registered jobs (AP10.2); the monitor ticks while > 0
    active: AtomicUsize,
    pub counters: Counters,
    pub path_counters: job::PathCounters,
    pub interval_max: IntervalMax,
    /// records the log buffer could not take
    pub log_dropped: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// RAM if the platform does not answer: the limit stays far away.
const UNKNOWN_RAM: u64 = u64::MAX / 2;

static GLOBAL: OnceLock<Limiter> = OnceLock::new();


/// The limiter of the process.
pub fn global() -> &'static Limiter {
    GLOBAL.get_or_init(|| Limiter { owns_pot: true, ..Limiter::new(footprint::footprint, footprint::phys_ram, footprint::ram_cap) })
}

impl Limiter {
    pub fn new(source: fn() -> Option<u64>, ram: fn() -> Option<u64>, ram_cap: fn() -> Option<u64>) -> Limiter {
        let cfg = LimiterConfig::default();
        let bounds = Bounds::new(&cfg, ram().unwrap_or(UNKNOWN_RAM), ram_cap());
        Limiter {
            source,
            ram,
            ram_cap,
            owns_pot: false,
            inner: Mutex::new(Inner {
                cfg,
                cfg_source: None,
                configured: false,
                bounds,
                hints: Vec::new(),
                thread_pid: None,
                last_tick: None,
                error: None,
                jobs: Vec::new(),
                next_job_id: 0,
                phase_since: None,
                relief_pending: None,
                relief_ineffective: false,
                relief_on: false,
                probe: None,
                stable_ticks: 0,
                last_level: 0,
                probe_block_above: None,
                over_ticks: 0,
                cancel_level: None,
                logger: None,
                log_next: None,
                log_last: Vec::new(),
                log_last_paths: Vec::new(),
                log_since: None,
                db: None,
            }),
            wake: Condvar::new(),
            level: AtomicU64::new(0),
            zone: AtomicU8::new(Zone::Green as u8),
            active: AtomicUsize::new(0),
            counters: Counters::default(),
            path_counters: job::PathCounters::default(),
            interval_max: IntervalMax::default(),
            log_dropped: AtomicU64::new(0),
        }
    }

    /// Settings of a newly opened handle. The first one defines the limiter
    /// of the process; a later one with other values gets a warning (also
    /// kept as a hint in the status).
    pub fn configure(&self, cfg: &LimiterConfig, source: Option<PathBuf>) -> Option<String> {
        self.configure_for(cfg, source, None)
    }

    /// [`configure`](Self::configure) by the handle of the database `db`:
    /// the statistics file goes next to it (AP10.7).
    pub fn configure_for(&self, cfg: &LimiterConfig, source: Option<PathBuf>, db: Option<&std::path::Path>) -> Option<String> {
        let mut st = lock(&self.inner);
        if !st.configured {
            st.bounds = Bounds::new(cfg, (self.ram)().unwrap_or(UNKNOWN_RAM), (self.ram_cap)());
            st.cfg = cfg.clone();
            st.cfg_source = source;
            st.configured = true;
            st.db = db.map(|p| p.to_path_buf());
            if cfg.log.enabled {
                if let Some(f) = cfg.log.file_for(db) {
                    st.logger = log::Logger::start(f, &cfg.log);
                    st.log_next = Some(Instant::now() + Duration::from_secs(cfg.log.interval_min.saturating_mul(60)));
                    st.log_since = Some(std::time::SystemTime::now());
                    st.log_last = self.counters.values().iter().map(|x| x.1).collect();
                    st.log_last_paths = self.path_values();
                }
            }
            return None;
        }
        if st.cfg == *cfg {
            return None;
        }
        let w = format!(
            "[limiter] settings of this handle ignored: the limiter of the process uses those of the first handle{} (they apply from the next process start)",
            st.cfg_source.as_ref().map(|p| format!(" from {}", p.display())).unwrap_or_default()
        );
        if !st.hints.contains(&w) {
            st.hints.push(w.clone());
        }
        Some(w)
    }

    pub fn config(&self) -> LimiterConfig {
        lock(&self.inner).cfg.clone()
    }

    pub fn bounds(&self) -> Bounds {
        lock(&self.inner).bounds
    }

    pub fn zone(&self) -> Zone {
        Zone::from_u8(self.zone.load(Ordering::Acquire))
    }

    /// Last measured water level.
    pub fn level(&self) -> u64 {
        self.level.load(Ordering::Relaxed)
    }

    /// One tick: measure, zone, counters. Returns the new zone.
    pub fn tick(&self, now: Instant) -> Zone {
        let c = &self.counters;
        c.ticks.fetch_add(1, Ordering::Relaxed);
        let (bounds, dt_ms, tick_ms) = {
            let mut st = lock(&self.inner);
            let dt = st.last_tick.map(|t| now.saturating_duration_since(t).as_millis() as u64).unwrap_or(0);
            st.last_tick = Some(now);
            (st.bounds, dt, st.cfg.tick_ms as u64)
        };
        let cur = self.zone();
        // time of a parked phase is not counted as zone time
        let dt_ms = dt_ms.min(tick_ms.saturating_mul(10));
        match cur {
            Zone::Green => &c.green_ms,
            Zone::Yellow => &c.yellow_ms,
            Zone::Red => &c.red_ms,
        }
        .fetch_add(dt_ms, Ordering::Relaxed);
        let Some(level) = (self.source)() else {
            c.sample_errors.fetch_add(1, Ordering::Relaxed);
            return cur;
        };
        let prev_level = self.level.swap(level, Ordering::Relaxed);
        let iv = &self.interval_max;
        if dt_ms > 0 {
            let rise = level.saturating_sub(prev_level);
            c.max_rise_per_tick.fetch_max(rise, Ordering::Relaxed);
            iv.max_rise_per_tick.fetch_max(rise, Ordering::Relaxed);
        }
        c.peak_level.fetch_max(level, Ordering::Relaxed);
        iv.peak_level.fetch_max(level, Ordering::Relaxed);
        c.peak_over_limit.fetch_max(level.saturating_sub(bounds.limit), Ordering::Relaxed);
        iv.peak_over_limit.fetch_max(level.saturating_sub(bounds.limit), Ordering::Relaxed);
        let next = bounds.next_zone(cur, level);
        if next != cur {
            c.zone_changes.fetch_add(1, Ordering::Relaxed);
            self.zone.store(next as u8, Ordering::Release);
        }
        self.decide(cur, next, level, &bounds, now);
        next
    }

    /// Policy step of a tick (AP10.3): in a quiet green phase nothing is
    /// done (the register is not even read).
    fn decide(&self, prev: Zone, zone: Zone, level: u64, b: &Bounds, now: Instant) {
        let c = &self.counters;
        let (steer, phase_since, relief_ineffective, jobs, cancel_over_limit, over_limit_ticks, waits) = {
            let mut st = lock(&self.inner);
            if let Some((at, freed)) = st.relief_pending.take() {
                if level.saturating_add(freed / 2) > at {
                    // the freed cache blocks stayed in the footprint (macOS
                    // large cache): further reliefs would only cost speed
                    st.relief_ineffective = true;
                    c.cache_relief_ineffective.fetch_add(1, Ordering::Relaxed);
                }
            }
            if zone == Zone::Green {
                st.phase_since = None;
                st.relief_ineffective = false;
                st.over_ticks = 0;
                st.cancel_level = None;
                if prev == Zone::Green && !st.relief_on {
                    return;
                }
            } else if st.phase_since.is_none() {
                st.phase_since = Some(now);
            }
            // N5: after a cancel the window counts again only once the level
            // rose clearly above the level of that cancel (real growth, not
            // memory of the cancelled job that is still on its way back)
            let held = level > b.limit && st.cancel_level.is_some_and(|l| level <= l.saturating_add(policy::CANCEL_REARM_MARGIN));
            if held {
                st.over_ticks = 0;
                c.cancel_held_ticks.fetch_add(1, Ordering::Relaxed);
            } else if level > b.limit {
                st.cancel_level = None;
                if st.over_ticks == 0 {
                    for j in &st.jobs {
                        j.grown_at_over.store(j.grown.load(Ordering::Relaxed), Ordering::Relaxed);
                    }
                }
                st.over_ticks = st.over_ticks.saturating_add(1);
            } else {
                st.over_ticks = 0;
                st.cancel_level = None;
            }
            (
                st.cfg.enabled && !st.cfg.dry_run && st.error.is_none(),
                st.phase_since,
                st.relief_ineffective,
                st.jobs.clone(),
                st.cfg.cancel_over_limit,
                st.over_ticks,
                (st.cfg.notify_wait_s, st.cfg.wait_timeout_s),
            )
        };
        // long waits: marked once per wait; optional timeout (off by default)
        for j in &jobs {
            if let Some(w) = j.waiting_for() {
                if w.as_secs() >= waits.0 && !j.long_noted.swap(true, Ordering::Relaxed) {
                    c.long_waits.fetch_add(1, Ordering::Relaxed);
                }
                if steer && waits.1 > 0 && w.as_secs() >= waits.1 && j.signal(SlotState::Cancel, Some(job::CancelReason::WaitTimeout)) {
                    c.wait_timeouts.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let views: Vec<policy::JobView> = jobs
            .iter()
            .map(|j| policy::JobView {
                id: j.id,
                kind: j.kind,
                state: j.state(),
                grown_since_shrink: j.grown.load(Ordering::Relaxed).saturating_sub(j.grown_at_shrink.load(Ordering::Relaxed)),
                new_in_phase: phase_since.is_some_and(|p| j.started >= p),
                grown_since_over: j.grown.load(Ordering::Relaxed).saturating_sub(j.grown_at_over.load(Ordering::Relaxed)),
            })
            .collect();
        let cache_used = if self.owns_pot { crate::db::mempot::POT.used() as u64 } else { 0 };
        let sit = policy::Situation {
            zone,
            level,
            yellow_at: b.yellow_at,
            limit: b.limit,
            cache_used,
            relief_ineffective,
            cancel_over_limit,
            over_limit_ticks,
        };
        let d = policy::policy(&sit, &views);
        if d.over_limit_single {
            c.over_limit_single_ticks.fetch_add(1, Ordering::Relaxed);
        }
        let count = |to: SlotState, shrink: &AtomicU64, wait: &AtomicU64, resume: &AtomicU64| match to {
            SlotState::Shrink => shrink.fetch_add(1, Ordering::Relaxed),
            SlotState::Wait => wait.fetch_add(1, Ordering::Relaxed),
            SlotState::Cancel => 0,
            _ => resume.fetch_add(1, Ordering::Relaxed),
        };
        if !steer {
            for (_, to) in &d.signals {
                count(*to, &c.dry_shrink, &c.dry_wait, &c.dry_resume);
            }
            if let Some(t) = d.cache_target.filter(|t| *t < cache_used) {
                c.dry_relief_bytes.fetch_add(cache_used - t, Ordering::Relaxed);
            }
            return;
        }
        for (id, to) in &d.signals {
            if let Some(j) = jobs.iter().find(|j| j.id == *id) {
                if *to == SlotState::Cancel {
                    if j.signal(SlotState::Cancel, Some(job::CancelReason::Limiter)) {
                        c.signals_cancel.fetch_add(1, Ordering::Relaxed);
                        // the next job gets a fresh window, and only after
                        // real growth above this level (N5 cascade)
                        let mut st = lock(&self.inner);
                        st.over_ticks = 0;
                        st.cancel_level = Some(level);
                    }
                } else if j.signal(*to, None) {
                    count(*to, &c.signals_shrink, &c.signals_wait, &c.signals_resume);
                }
            }
        }
        match d.cache_target {
            Some(u64::MAX) => {
                let mut st = lock(&self.inner);
                if st.relief_on {
                    st.relief_on = false;
                    drop(st);
                    crate::db::mempot::POT.end_relief();
                }
            }
            Some(t) if t < cache_used => {
                let freed = crate::db::mempot::POT.relieve(t);
                c.cache_relief_requests.fetch_add(1, Ordering::Relaxed);
                c.cache_relief_bytes.fetch_add(freed, Ordering::Relaxed);
                let mut st = lock(&self.inner);
                st.relief_on = true;
                if freed > 0 {
                    st.relief_pending = Some((level, freed));
                }
            }
            _ => {}
        }
        self.probe_step(zone, level, &jobs);
    }

    /// AP10.5 `TEST_IT`: with a high but stable level, the oldest waiting
    /// job may grow by a bounded probe. If the footprint rose clearly less
    /// than the job reported (memory the allocator kept is reused, e.g. the
    /// macOS large cache), it gets the next probe and so runs on in tested
    /// steps; if it rose about as much, it waits again. A probe during which
    /// other jobs grew is not judged (invalid).
    fn probe_step(&self, zone: Zone, level: u64, jobs: &[Arc<Slot>]) {
        let c = &self.counters;
        let mut st = lock(&self.inner);
        let bytes = st.cfg.probe_bytes;
        st.stable_ticks = if level.abs_diff(st.last_level) <= STABLE_DELTA { st.stable_ticks.saturating_add(1) } else { 0 };
        st.last_level = level;
        let grown_of = |j: &Slot| j.grown.load(Ordering::Relaxed);
        let others = |id: u64| jobs.iter().filter(|j| j.id != id).map(|j| grown_of(j)).sum::<u64>();
        if let Some(mut p) = st.probe.take() {
            let Some(j) = jobs.iter().find(|j| j.id == p.id) else {
                // the job finished on its probe
                c.probes_reused.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let grown = grown_of(j).saturating_sub(p.grown0);
            let state = j.state();
            if state == SlotState::Probe && grown < bytes && p.ticks < PROBE_MAX_TICKS && zone != Zone::Green {
                p.ticks += 1;
                st.probe = Some(p);
                return;
            }
            if zone == Zone::Green || !matches!(state, SlotState::Probe | SlotState::Wait) {
                return; // the policy took over (RESUME in green, CANCEL)
            }
            let rise = level.saturating_sub(p.level0);
            if others(p.id).saturating_sub(p.others0) > grown / 4 {
                c.probes_invalid.fetch_add(1, Ordering::Relaxed);
                j.signal(SlotState::Wait, None);
                st.stable_ticks = 0;
            } else if rise.saturating_mul(2) < grown && j.start_probe(bytes) {
                c.probes_reused.fetch_add(1, Ordering::Relaxed);
                c.probes.fetch_add(1, Ordering::Relaxed);
                st.probe = Some(ProbeRun { id: p.id, level0: level, grown0: grown_of(j), others0: others(p.id), ticks: 0 });
            } else {
                c.probes_grew.fetch_add(1, Ordering::Relaxed);
                j.signal(SlotState::Wait, None);
                // real growth: no further probe before the level fell by a
                // probe (each one would cost real memory)
                st.probe_block_above = Some(level.saturating_sub(bytes));
            }
            return;
        }
        if zone == Zone::Green {
            st.probe_block_above = None;
        }
        if let Some(b) = st.probe_block_above {
            if level > b {
                return;
            }
            st.probe_block_above = None;
        }
        if bytes == 0 || zone == Zone::Green || st.stable_ticks < STABLE_TICKS {
            return;
        }
        let Some(j) = jobs.iter().filter(|j| j.state() == SlotState::Wait).min_by_key(|j| j.id) else { return };
        if j.start_probe(bytes) {
            c.probes.fetch_add(1, Ordering::Relaxed);
            st.probe = Some(ProbeRun { id: j.id, level0: level, grown0: grown_of(j), others0: others(j.id), ticks: 0 });
        }
    }

    /// The monitor has work: jobs are registered or the zone is not green.
    fn busy(&self) -> bool {
        self.active.load(Ordering::Acquire) > 0 || self.zone() != Zone::Green
    }

    /// Starts the monitor thread of this process if it does not run (first
    /// need, or a `fork()` child: a child inherits no threads), and wakes it.
    pub fn ensure_thread(&'static self) {
        let pid = std::process::id();
        let mut st = lock(&self.inner);
        if st.thread_pid != Some(pid) {
            if st.thread_pid.is_some() {
                // fork child: jobs and statistics of the parent are not ours
                self.active.store(0, Ordering::Release);
                st.jobs.clear();
                self.counters.reset();
                self.path_counters.reset();
                self.zone.store(Zone::Green as u8, Ordering::Release);
                st.last_tick = None;
                st.error = None;
                st.probe = None;
                st.over_ticks = 0;
                st.cancel_level = None;
            }
            let spawned = std::thread::Builder::new()
                .name("nhydb-limiter".into())
                // small: only the touched pages count (AP10.0: 16–80 KiB)
                .stack_size(256 << 10)
                .spawn(move || self.run());
            match spawned {
                Ok(_) => {
                    st.thread_pid = Some(pid);
                    self.counters.thread_starts.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => st.error = Some(format!("monitor thread not started: {e}")),
            }
        }
        drop(st);
        self.wake.notify_all();
    }

    /// Loop of the monitor thread (lives as long as the process).
    fn run(&'static self) {
        loop {
            let idle_log = {
                let mut st = lock(&self.inner);
                let mut due = false;
                while !self.busy() {
                    st.last_tick = None;
                    // AP10.7: without work the thread still wakes for the
                    // next statistics record
                    match st.log_next {
                        Some(t) => {
                            let now = Instant::now();
                            if now >= t {
                                due = true;
                                break;
                            }
                            st = self.wake.wait_timeout(st, t - now).unwrap_or_else(|e| e.into_inner()).0;
                        }
                        None => st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner()),
                    }
                }
                due
            };
            if idle_log {
                self.log_if_due(Instant::now());
                continue;
            }
            let tick = Duration::from_millis(lock(&self.inner).cfg.tick_ms as u64);
            std::thread::sleep(tick);
            self.log_if_due(Instant::now());
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.tick(Instant::now())));
            if let Err(p) = r {
                self.counters.tick_panics.fetch_add(1, Ordering::Relaxed);
                let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned());
                // rather unprotected than blocked (R9): every wait is lifted,
                // from now on the limiter only observes
                let jobs = {
                    let mut st = lock(&self.inner);
                    st.error = Some(format!("limiter tick failed: {}", msg.unwrap_or_else(|| "panic".into())));
                    st.jobs.clone()
                };
                for j in jobs {
                    j.signal(SlotState::Go, None);
                }
                if self.owns_pot {
                    crate::db::mempot::POT.end_relief();
                }
            }
        }
    }

    /// Called for each registered job.
    pub(crate) fn activity_begin(&'static self) {
        self.active.fetch_add(1, Ordering::AcqRel);
        self.ensure_thread();
    }

    pub(crate) fn activity_end(&self) {
        let prev = self.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0);
    }

    fn path_values(&self) -> Vec<u64> {
        let pc = &self.path_counters;
        pc.registered.iter().chain(pc.checkpoints.iter()).map(|a| a.load(Ordering::Relaxed)).collect()
    }

    /// Starts the monitor thread for the statistics file (it then wakes
    /// once per interval even without work).
    pub fn start_log_clock(&'static self) {
        if lock(&self.inner).logger.is_some() {
            self.ensure_thread();
        }
    }

    fn log_if_due(&self, now: Instant) {
        let due = lock(&self.inner).log_next.is_some_and(|t| now >= t);
        if due {
            self.log_record(false);
        }
    }

    /// AP10.7: takes one consistent record of the interval and hands it to
    /// the helper (never blocks). Counters: differences to the last record
    /// (each event in exactly one record); peaks: maxima of the interval.
    pub fn log_record(&self, last: bool) -> bool {
        let mut st = lock(&self.inner);
        if st.logger.is_none() {
            return false;
        }
        let now = std::time::SystemTime::now();
        let vals = self.counters.values();
        let iv = &self.interval_max;
        let mut counters = Vec::with_capacity(vals.len());
        for (i, (name, v)) in vals.iter().enumerate() {
            let x = match *name {
                "peak_level" => iv.peak_level.swap(0, Ordering::Relaxed),
                "peak_over_limit" => iv.peak_over_limit.swap(0, Ordering::Relaxed),
                "max_rise_per_tick" => iv.max_rise_per_tick.swap(0, Ordering::Relaxed),
                "wait_ms_max" => iv.wait_ms_max.swap(0, Ordering::Relaxed),
                _ => v.wrapping_sub(st.log_last.get(i).copied().unwrap_or(0)),
            };
            counters.push((name.to_string(), Val::Int(x as i64)));
        }
        debug_assert!(MAX_COUNTERS.iter().all(|m| vals.iter().any(|v| v.0 == *m)));
        let paths = self.path_values();
        let n = job::JobPath::ALL.len();
        let path_val = Val::Map(
            job::JobPath::ALL
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let d = |k: usize| paths[k].wrapping_sub(st.log_last_paths.get(k).copied().unwrap_or(0)) as i64;
                    (p.name().to_string(), vmap! {"registered" => d(i), "checkpoints" => d(n + i)})
                })
                .collect(),
        );
        let b = st.bounds;
        let since = st.log_since.unwrap_or(now);
        let rec = vmap! {
            "ts" => log::utc_stamp(now),
            "from" => log::utc_stamp(since),
            "interval_s" => now.duration_since(since).map(|d| d.as_secs()).unwrap_or(0) as i64,
            "final" => last,
            "pid" => std::process::id() as i64,
            "db" => st.db.as_ref().map(|p| Val::Str(p.display().to_string())).unwrap_or(Val::None),
            "mode" => if !st.cfg.enabled || st.error.is_some() { "observe" } else if st.cfg.dry_run { "dry_run" } else { "active" },
            "source" => footprint::SOURCE,
            "ram_bytes" => b.ram as i64,
            "limit_bytes" => b.limit as i64,
            "yellow_bytes" => b.yellow_at as i64,
            "level_bytes" => self.level() as i64,
            "zone" => self.zone().name(),
            "jobs_now" => st.jobs.len() as i64,
            "log_dropped" => self.log_dropped.load(Ordering::Relaxed) as i64,
            "counters" => Val::Map(counters),
            "paths" => path_val,
        };
        st.log_last = vals.iter().map(|x| x.1).collect();
        st.log_last_paths = paths;
        st.log_since = Some(now);
        let interval = st.cfg.log.interval_min;
        st.log_next = Some(Instant::now() + Duration::from_secs(interval.saturating_mul(60)));
        let line = rec.to_json_unsorted().unwrap_or_default();
        let ok = st.logger.as_ref().is_some_and(|l| l.offer(line));
        if !ok {
            self.log_dropped.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// Writes the started interval and waits (up to 2 s) until the helper
    /// wrote everything: the last handle of the process closed.
    pub fn log_final(&self) {
        if self.log_record(true) {
            let st = lock(&self.inner);
            if let Some(l) = &st.logger {
                let l2 = log::Logger { tx: l.tx_clone(), state: l.state.clone() };
                drop(st);
                l2.flush(Duration::from_secs(2));
            }
        }
    }

    /// `resource_status()`.
    pub fn status(&self) -> Val {
        let st = lock(&self.inner);
        let b = st.bounds;
        let now = (self.source)();
        let level = now.unwrap_or_else(|| self.level());
        let pid = std::process::id();
        vmap! {
            "enabled" => st.cfg.enabled,
            "mode" => if !st.cfg.enabled || st.error.is_some() { "observe" } else if st.cfg.dry_run { "dry_run" } else { "active" },
            "zone" => self.zone().name(),
            "level_bytes" => level as i64,
            "limit_bytes" => b.limit as i64,
            "yellow_bytes" => b.yellow_at as i64,
            "ram_bytes" => b.ram as i64,
            "ram_cap_bytes" => b.ram_cap.map(|c| Val::Int(c as i64)).unwrap_or(Val::None),
            "reserve_percent" => st.cfg.reserve_percent as i64,
            "warn_percent" => st.cfg.warn_percent as i64,
            "tick_ms" => st.cfg.tick_ms as i64,
            "source" => footprint::SOURCE,
            "config_source" => st.cfg_source.as_ref().map(|p| Val::Str(p.display().to_string())).unwrap_or(Val::None),
            "thread_running" => st.thread_pid == Some(pid),
            "active_jobs" => self.active.load(Ordering::Relaxed) as i64,
            "jobs" => Val::List(st.jobs.iter().map(|j| job_status(j, st.cfg.notify_wait_s)).collect()),
            "paths" => Val::Map(job::JobPath::ALL.iter().map(|p| (p.name().to_string(), vmap! {
                "registered" => self.path_counters.registered[*p as usize].load(Ordering::Relaxed) as i64,
                "checkpoints" => self.path_counters.checkpoints[*p as usize].load(Ordering::Relaxed) as i64,
                "max_step_bytes" => self.path_counters.max_step[*p as usize].load(Ordering::Relaxed) as i64,
            })).collect()),
            "counters" => self.counters.to_val(),
            "hints" => Val::List(st.hints.iter().map(|h| Val::Str(h.clone())).collect()),
            "log" => match &st.logger {
                None => Val::None,
                Some(l) => {
                    let ls = log::lock_state(&l.state);
                    vmap! {
                        "file" => ls.file.as_ref().map(|p| Val::Str(p.display().to_string())).unwrap_or(Val::None),
                        "interval_min" => st.cfg.log.interval_min as i64,
                        "max_bytes" => st.cfg.log.max_bytes as i64,
                        "lines_written" => ls.lines_written as i64,
                        "rotations" => ls.rotations as i64,
                        "dropped" => self.log_dropped.load(Ordering::Relaxed) as i64,
                        "last_error" => ls.last_error.clone().map(Val::Str).unwrap_or(Val::None),
                    }
                }
            },
            "error" => st.error.clone().map(Val::Str).unwrap_or(Val::None),
        }
    }
}

fn job_status(j: &Slot, notify_wait_s: u64) -> Val {
    let g = lock(&j.inner);
    vmap! {
        "id" => j.id as i64,
        "path" => j.path.name(),
        "kind" => j.kind.name(),
        "state" => match g.state {
            SlotState::Go => "running",
            SlotState::Shrink => "shrink",
            SlotState::Wait => if g.waiting_since.is_some() { "waiting" } else { "wait" },
            SlotState::Probe => "probe",
            SlotState::Cancel => "cancel",
        },
        "running_s" => j.started.elapsed().as_secs_f64(),
        "waiting_s" => g.waiting_since.map(|t| Val::Float(t.elapsed().as_secs_f64())).unwrap_or(Val::None),
        "waiting_long" => g.waiting_since.is_some_and(|t| t.elapsed().as_secs() >= notify_wait_s),
        "grown_bytes" => j.grown.load(Ordering::Relaxed) as i64,
        "checkpoints" => j.checkpoints.load(Ordering::Relaxed) as i64,
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "limiter_tests.rs"]
mod tests;
