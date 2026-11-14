//! RUN¹⁰ AP10.2: registration of large jobs, signal slots, checkpoints.
//!
//! * [`Limiter::register`] enters a job (id = age, kind, path) and never
//!   blocks; dropping the [`Job`] leaves (also on error or panic).
//! * Each job has a signal slot: an atomic for the fast path and a mutex +
//!   condition variable for waiting. Signals are changed only under the
//!   mutex (the atomic is written there too), so a wake-up cannot be lost:
//!   the job checks and sleeps under the same mutex (`while` loop).
//!   Priority: `Cancel` > `Wait` > `Shrink` > `Go`; `Cancel` is final.
//! * [`Job::checkpoint`] costs one atomic load while the slot says `Go`.
//! * [`Meter`] is what the paths use: it adds up the growth of a job and
//!   calls a checkpoint once per step (default 1 MiB); it registers only at
//!   its first checkpoint, so small operations never touch the register.
//! * One job per thread (2026-11-03): a meter whose thread already has a
//!   registered meter job joins that job instead of registering a second
//!   one (its growth and checkpoints go to the thread's job). The red rule
//!   "only the oldest job runs" assumes this: two jobs of one thread
//!   deadlocked (the outer one may run, the thread sleeps in the inner one;
//!   e.g. `build_indexes` spill + `persist_secondary`).

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use super::{lock, Limiter};
use crate::error::{Error, Result};

/// Can the job give memory back when asked (`Shrink`)?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// can spill or shrink (sort runs, index build)
    Elastic,
    /// keeps what it has in memory
    Rigid,
}

impl JobKind {
    pub fn name(self) -> &'static str {
        match self {
            JobKind::Elastic => "elastic",
            JobKind::Rigid => "rigid",
        }
    }
}

macro_rules! paths {
    ($($v:ident => $s:literal),+ $(,)?) => {
        /// Where a job comes from (one entry per path of plan §2.5).
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum JobPath { $($v),+ }
        impl JobPath {
            pub const ALL: &'static [JobPath] = &[$(JobPath::$v),+];
            pub fn name(self) -> &'static str {
                match self { $(JobPath::$v => $s),+ }
            }
        }
    };
}

paths!(
    Collect => "collect",
    PyConvert => "py_convert",
    Sort => "sort",
    Candidates => "candidates",
    Postings => "postings",
    TextVerify => "text_verify",
    IndexBuild => "index_build",
    Write => "write",
    Compact => "compact",
);

/// Registrations and checkpoints per path (statistics, completeness test).
pub struct PathCounters {
    pub registered: [AtomicU64; 9],
    pub checkpoints: [AtomicU64; 9],
    /// largest growth reported at one checkpoint (bytes between two
    /// checkpoints; AP10.3 measurement)
    pub max_step: [AtomicU64; 9],
}

impl Default for PathCounters {
    fn default() -> Self {
        PathCounters {
            registered: std::array::from_fn(|_| AtomicU64::new(0)),
            checkpoints: std::array::from_fn(|_| AtomicU64::new(0)),
            max_step: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl PathCounters {
    pub(crate) fn reset(&self) {
        for a in self.registered.iter().chain(self.checkpoints.iter()).chain(self.max_step.iter()) {
            a.store(0, Ordering::Relaxed);
        }
    }
}

/// State of a signal slot (priority: higher value wins, `Cancel` is final).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum SlotState {
    Go = 0,
    /// AP10.5 `TEST_IT`: a waiting job may grow by a bounded probe and
    /// then holds by itself
    Probe = 1,
    Shrink = 2,
    Wait = 3,
    Cancel = 4,
}

/// Why a job was cancelled (`ResourceLimitError.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    App,
    Limiter,
    WaitTimeout,
}

impl CancelReason {
    pub fn name(self) -> &'static str {
        match self {
            CancelReason::App => "cancelled_by_app",
            CancelReason::Limiter => "cancelled_by_limiter",
            CancelReason::WaitTimeout => "wait_timeout",
        }
    }
}

pub(crate) struct SlotInner {
    pub state: SlotState,
    pub reason: Option<CancelReason>,
    pub waiting_since: Option<Instant>,
    /// `Probe`: the job holds again once `grown` reaches this value
    pub probe_until: u64,
}

/// Signal slot of one job; the limiter keeps an `Arc` copy, so a signal
/// after the job ended goes into a slot nobody reads (harmless).
pub struct Slot {
    pub id: u64,
    pub kind: JobKind,
    pub path: JobPath,
    pub started: Instant,
    fast: AtomicU8,
    pub(crate) inner: Mutex<SlotInner>,
    cv: Condvar,
    /// growth reported by the job (bytes, cumulative)
    pub grown: AtomicU64,
    /// `grown` when the job last acknowledged a `SHRINK`
    pub grown_at_shrink: AtomicU64,
    /// `grown` when the level last went above the limit (N5)
    pub grown_at_over: AtomicU64,
    /// time this job spent waiting (ms, all waits) and its longest wait
    pub waited_ms: AtomicU64,
    pub waited_max_ms: AtomicU64,
    /// the current wait passed `notify_wait_s` (counted once)
    pub long_noted: std::sync::atomic::AtomicBool,
    pub checkpoints: AtomicU64,
}

impl Slot {
    fn new(id: u64, kind: JobKind, path: JobPath) -> Slot {
        Slot {
            id,
            kind,
            path,
            started: Instant::now(),
            fast: AtomicU8::new(SlotState::Go as u8),
            inner: Mutex::new(SlotInner { state: SlotState::Go, reason: None, waiting_since: None, probe_until: 0 }),
            cv: Condvar::new(),
            grown: AtomicU64::new(0),
            grown_at_shrink: AtomicU64::new(0),
            grown_at_over: AtomicU64::new(0),
            waited_ms: AtomicU64::new(0),
            waited_max_ms: AtomicU64::new(0),
            long_noted: std::sync::atomic::AtomicBool::new(false),
            checkpoints: AtomicU64::new(0),
        }
    }

    pub fn state(&self) -> SlotState {
        lock(&self.inner).state
    }

    /// Sets a signal (limiter or application). A lower-priority signal
    /// does not replace a higher one, except `Go` lifting `Wait`/`Shrink`
    /// (`RESUME`); nothing replaces `Cancel`. Returns whether it changed.
    pub fn signal(&self, to: SlotState, reason: Option<CancelReason>) -> bool {
        let mut g = lock(&self.inner);
        let from = g.state;
        let ok = match (from, to) {
            (SlotState::Cancel, _) => false,
            (_, SlotState::Go) => from != SlotState::Go,
            (f, t) => t > f,
        };
        if ok {
            g.state = to;
            if to == SlotState::Cancel {
                g.reason = reason;
            }
            self.fast.store(to as u8, Ordering::Release);
            drop(g);
            self.cv.notify_all();
        }
        ok
    }

    /// Waits while the slot says `Wait` (under its mutex, no lost wake-up).
    fn wait_out(&self, mut g: std::sync::MutexGuard<'_, SlotInner>) -> Result<()> {
        loop {
            match g.state {
                SlotState::Wait => {
                    if g.waiting_since.is_none() {
                        g.waiting_since = Some(Instant::now());
                    }
                    g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                st => {
                    if let Some(t) = g.waiting_since.take() {
                        let ms = t.elapsed().as_millis() as u64;
                        self.waited_ms.fetch_add(ms, Ordering::Relaxed);
                        self.waited_max_ms.fetch_max(ms, Ordering::Relaxed);
                        self.long_noted.store(false, Ordering::Relaxed);
                    }
                    return if st == SlotState::Cancel { Err(self.cancelled_error(g.reason)) } else { Ok(()) };
                }
            }
        }
    }

    /// How long the job has been waiting now (`None`: it does not wait).
    pub fn waiting_for(&self) -> Option<std::time::Duration> {
        lock(&self.inner).waiting_since.map(|t| t.elapsed())
    }

    /// `TEST_IT`: a waiting job (or one whose probe is renewed) may grow by
    /// `bytes` (reported growth) and then holds by itself.
    pub fn start_probe(&self, bytes: u64) -> bool {
        let mut g = lock(&self.inner);
        if !matches!(g.state, SlotState::Wait | SlotState::Probe) {
            return false;
        }
        g.state = SlotState::Probe;
        g.probe_until = self.grown.load(Ordering::Relaxed).saturating_add(bytes);
        self.fast.store(SlotState::Probe as u8, Ordering::Release);
        drop(g);
        self.cv.notify_all();
        true
    }

    fn cancelled_error(&self, reason: Option<CancelReason>) -> Error {
        let r = reason.unwrap_or(CancelReason::App);
        Error::ResourceLimit(Box::new((r.name(), format!("job {} ({}) {}", self.id, self.path.name(), match r {
            CancelReason::App => "was cancelled by the application (cancel_job)",
            CancelReason::Limiter => "was cancelled by the RAM limiter: it alone kept the process above its memory limit",
            CancelReason::WaitTimeout => "waited longer than [limiter] wait_timeout_s for memory",
        }))))
    }
}

/// A registered job; leaves the register when dropped.
pub struct Job {
    lim: &'static Limiter,
    pub(crate) slot: Arc<Slot>,
}

impl Limiter {
    /// Enters a job. Never blocks (a short lock of the register). The job
    /// must call [`Job::checkpoint`] before its first large allocation.
    pub fn register(&'static self, kind: JobKind, path: JobPath) -> Job {
        let slot = {
            let mut st = lock(&self.inner);
            st.next_job_id += 1;
            let slot = Arc::new(Slot::new(st.next_job_id, kind, path));
            st.jobs.push(slot.clone());
            slot
        };
        self.path_counters.registered[path as usize].fetch_add(1, Ordering::Relaxed);
        self.counters.jobs_registered.fetch_add(1, Ordering::Relaxed);
        self.activity_begin();
        Job { lim: self, slot }
    }

    fn unregister(&self, id: u64) {
        let mut st = lock(&self.inner);
        if let Some(i) = st.jobs.iter().position(|s| s.id == id) {
            let s = st.jobs.swap_remove(i);
            let c = &self.counters;
            c.wait_ms_total.fetch_add(s.waited_ms.load(Ordering::Relaxed), Ordering::Relaxed);
            c.wait_ms_max.fetch_max(s.waited_max_ms.load(Ordering::Relaxed), Ordering::Relaxed);
            self.interval_max.wait_ms_max.fetch_max(s.waited_max_ms.load(Ordering::Relaxed), Ordering::Relaxed);
            c.jobs_finished.fetch_add(1, Ordering::Relaxed);
            drop(st);
            self.activity_end();
        }
        // not found: the register was reset (fork child) – nothing to do
    }

    /// Copies of the slots of all registered jobs (the caller works on them
    /// without the register lock).
    pub fn jobs(&self) -> Vec<Arc<Slot>> {
        lock(&self.inner).jobs.clone()
    }

    /// `cancel_job`: cancels a job at its next checkpoint (a waiting job is
    /// woken). Returns whether the job exists.
    pub fn cancel_job(&self, id: u64) -> bool {
        let slot = lock(&self.inner).jobs.iter().find(|s| s.id == id).cloned();
        match slot {
            Some(s) => {
                s.signal(SlotState::Cancel, Some(CancelReason::App));
                true
            }
            None => false,
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // the thread's meter job ends (one job per thread)
        let _ = CURRENT.try_with(|c| {
            let mut c = c.borrow_mut();
            if c.as_ref().is_some_and(|x| Arc::ptr_eq(x, &self.slot)) {
                *c = None;
            }
        });
        // a wait owed by this job lapses with it
        let _ = OWED.try_with(|o| {
            let mut o = o.borrow_mut();
            if o.as_ref().is_some_and(|x| Arc::ptr_eq(x, &self.slot)) {
                *o = None;
            }
        });
        self.lim.unregister(self.slot.id);
        // U2 (2026-11-03): a cancelled job has freed its memory, but glibc
        // keeps it in the process (Linux RSS stayed at 5–6 GiB after all
        // results were freed; `malloc_trim` brought it to 0.1–0.3 GiB).
        // Given back here: in the job's own thread, after its data and its
        // registration are gone, outside every limiter lock.
        if self.slot.state() == SlotState::Cancel {
            release_freed_memory(self.lim);
        }
    }
}

/// Gives memory the allocator keeps after a cancel back to the system
/// (Linux/glibc `malloc_trim(0)`; elsewhere nothing to do: the Windows heap
/// and macOS return it by themselves or reuse it).
fn release_freed_memory(lim: &Limiter) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::os::raw::c_int;
        }
        let t = Instant::now();
        // SAFETY: plain libc call without pointers; glibc locks its arenas
        unsafe {
            malloc_trim(0);
        }
        lim.counters.trims.fetch_add(1, Ordering::Relaxed);
        lim.counters.trim_ms_total.fetch_add(t.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let _ = lim;
}

impl Job {
    pub fn id(&self) -> u64 {
        self.slot.id
    }

    /// Checkpoint: reports `grown` further bytes. Normally one atomic load.
    /// `Ok(true)`: the limiter asks the job to give memory back (`Shrink`;
    /// the signal is acknowledged). Waits inside while the slot says
    /// `Wait`. `Err(ResourceLimitError)`: the job was cancelled.
    #[inline]
    pub fn checkpoint(&self, grown: u64) -> Result<bool> {
        checkpoint_at(self.lim, &self.slot, self.slot.path, true, grown)
    }

    /// Like [`checkpoint`](Self::checkpoint), but never waits: `Ok(None)`
    /// means the slot says `Wait` – the caller releases what it must not
    /// hold while waiting (the Python GIL) and then calls
    /// `checkpoint(0)`.
    #[inline]
    pub fn try_checkpoint(&self, grown: u64) -> Result<Option<bool>> {
        try_checkpoint_at(self.lim, &self.slot, self.slot.path, true, grown)
    }
}

/// Books `grown` on the slot and the counters of `path` (the path of the
/// meter that reports; for a joined meter not the path of the job).
#[inline]
fn book(lim: &Limiter, s: &Slot, path: JobPath, grown: u64) {
    // single writer (the thread of the job): load + store, no read-modify-write
    s.grown.store(s.grown.load(Ordering::Relaxed).wrapping_add(grown), Ordering::Relaxed);
    s.checkpoints.store(s.checkpoints.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    let pc = &lim.path_counters;
    pc.checkpoints[path as usize].fetch_add(1, Ordering::Relaxed);
    if grown > pc.max_step[path as usize].load(Ordering::Relaxed) {
        pc.max_step[path as usize].fetch_max(grown, Ordering::Relaxed);
    }
}

/// `may_shrink`: the reporting meter can give memory back; a rigid meter
/// that joined an elastic job leaves a `Shrink` for the job's own meter.
#[inline]
fn checkpoint_at(lim: &Limiter, s: &Arc<Slot>, path: JobPath, may_shrink: bool, grown: u64) -> Result<bool> {
    book(lim, s, path, grown);
    if s.fast.load(Ordering::Acquire) == SlotState::Go as u8 {
        return Ok(false);
    }
    slow(lim, s, may_shrink)
}

#[inline]
fn try_checkpoint_at(lim: &Limiter, s: &Arc<Slot>, path: JobPath, may_shrink: bool, grown: u64) -> Result<Option<bool>> {
    book(lim, s, path, grown);
    if s.fast.load(Ordering::Acquire) == SlotState::Go as u8 {
        return Ok(Some(false));
    }
    {
        let mut g = lock(&s.inner);
        if g.state == SlotState::Probe && s.grown.load(Ordering::Relaxed) >= g.probe_until {
            g.state = SlotState::Wait;
            s.fast.store(SlotState::Wait as u8, Ordering::Release);
        }
        if g.state == SlotState::Wait {
            return Ok(None);
        }
    }
    slow(lim, s, may_shrink).map(Some)
}

#[cold]
#[inline(never)]
fn slow(lim: &Limiter, slot: &Arc<Slot>, may_shrink: bool) -> Result<bool> {
    let s = &**slot;
    let mut g = lock(&s.inner);
    match g.state {
        SlotState::Go => Ok(false),
        SlotState::Cancel => Err(s.cancelled_error(g.reason)),
        SlotState::Shrink if !may_shrink => Ok(false),
        SlotState::Shrink => {
            g.state = SlotState::Go;
            s.fast.store(SlotState::Go as u8, Ordering::Release);
            s.grown_at_shrink.store(s.grown.load(Ordering::Relaxed), Ordering::Relaxed);
            Ok(true)
        }
        SlotState::Probe if s.grown.load(Ordering::Relaxed) < g.probe_until => Ok(false),
        SlotState::Probe => {
            // probe used up: hold by itself, the limiter decides
            g.state = SlotState::Wait;
            s.fast.store(SlotState::Wait as u8, Ordering::Release);
            if !may_wait() {
                drop(g);
                OWED.with(|o| *o.borrow_mut() = Some(slot.clone()));
                lim.counters.waits_deferred.fetch_add(1, Ordering::Relaxed);
                return Ok(false);
            }
            s.wait_out(g).map(|_| false)
        }
        SlotState::Wait if !may_wait() => {
            // R4: this thread holds the Python GIL – never wait here;
            // the binding sits the wait out after releasing it
            // (`take_owed_wait`)
            drop(g);
            OWED.with(|o| *o.borrow_mut() = Some(slot.clone()));
            lim.counters.waits_deferred.fetch_add(1, Ordering::Relaxed);
            Ok(false)
        }
        SlotState::Wait => s.wait_out(g).map(|_| false),
    }
}

/// Default step of a [`Meter`]: one checkpoint per MiB of growth.
pub const METER_STEP: u64 = 1 << 20;

/// The job a [`Meter`] reports to.
enum MeterJob {
    /// registered by this meter (the thread's job while it lives)
    Own(Job),
    /// joined: the thread already had a meter job (one job per thread)
    Joined(Arc<Slot>),
}

/// Growth meter of one operation: registers at its first checkpoint, or
/// joins the job of its thread when that has one.
pub struct Meter {
    kind: JobKind,
    path: JobPath,
    step: u64,
    pending: u64,
    job: Option<MeterJob>,
    /// tests: a limiter of their own (`None` = the process limiter)
    lim: Option<&'static Limiter>,
}

impl Meter {
    pub fn new(kind: JobKind, path: JobPath) -> Meter {
        Meter { kind, path, step: METER_STEP, pending: 0, job: None, lim: None }
    }

    #[cfg(all(test, nhydb_internal_tests))]
    pub(crate) fn new_in(lim: &'static Limiter, kind: JobKind, path: JobPath) -> Meter {
        Meter { lim: Some(lim), ..Meter::new(kind, path) }
    }

    fn lim(&self) -> &'static Limiter {
        self.lim.unwrap_or_else(super::global)
    }

    /// Adds growth; a checkpoint once `step` bytes are pending.
    #[inline]
    pub fn grow(&mut self, bytes: u64) -> Result<bool> {
        self.pending = self.pending.wrapping_add(bytes);
        if self.pending < self.step {
            return Ok(false);
        }
        self.checkpoint()
    }

    /// Adds growth without a checkpoint; `true` when one is due.
    #[inline]
    pub fn due(&mut self, bytes: u64) -> bool {
        self.pending = self.pending.wrapping_add(bytes);
        self.pending >= self.step
    }

    /// The slot this meter reports to: its own job, the thread's job it
    /// joined, or – at the first checkpoint – a new registration (or a
    /// join when the thread already has a job). A joined job that ended
    /// meanwhile is left; the meter then resolves anew.
    fn slot(&mut self) -> (Arc<Slot>, bool) {
        if let Some(MeterJob::Joined(s)) = &self.job {
            let live = CURRENT.with(|c| c.borrow().as_ref().is_some_and(|x| Arc::ptr_eq(x, s)));
            if !live {
                self.job = None;
            }
        }
        if self.job.is_none() {
            let lim = self.lim();
            let cur = CURRENT.with(|c| c.borrow().clone());
            self.job = Some(match cur {
                Some(s) => {
                    // one job per thread: join it (the path is still counted)
                    lim.path_counters.registered[self.path as usize].fetch_add(1, Ordering::Relaxed);
                    lim.counters.jobs_joined.fetch_add(1, Ordering::Relaxed);
                    MeterJob::Joined(s)
                }
                None => {
                    let j = lim.register(self.kind, self.path);
                    CURRENT.with(|c| *c.borrow_mut() = Some(j.slot.clone()));
                    MeterJob::Own(j)
                }
            });
        }
        match self.job.as_ref().unwrap() {
            MeterJob::Own(j) => (j.slot.clone(), true),
            // a rigid meter in an elastic job does not take its SHRINK
            MeterJob::Joined(s) => (s.clone(), self.kind == JobKind::Elastic || s.kind == JobKind::Rigid),
        }
    }

    /// Checkpoint that never waits (see [`Job::try_checkpoint`]).
    pub fn try_checkpoint(&mut self) -> Result<Option<bool>> {
        let grown = std::mem::take(&mut self.pending);
        let (s, may_shrink) = self.slot();
        try_checkpoint_at(self.lim(), &s, self.path, may_shrink, grown)
    }

    /// Checkpoint now (chunk boundary, before a large allocation).
    pub fn checkpoint(&mut self) -> Result<bool> {
        let grown = std::mem::take(&mut self.pending);
        let (s, may_shrink) = self.slot();
        checkpoint_at(self.lim(), &s, self.path, may_shrink, grown)
    }

    /// Id of the job this meter reports to (own or joined).
    pub fn job_id(&self) -> Option<u64> {
        self.job.as_ref().map(|j| match j {
            MeterJob::Own(j) => j.id(),
            MeterJob::Joined(s) => s.id,
        })
    }
}

/// One checkpoint after a large single step (a candidate set, a merged
/// posting list): steps below one meter step never register; a larger one
/// registers briefly, so the limiter can hold the operation here before its
/// next step.
pub fn step(kind: JobKind, path: JobPath, bytes: u64) -> Result<bool> {
    if bytes < METER_STEP {
        return Ok(false);
    }
    let mut m = Meter::new(kind, path);
    m.grow(bytes)
}

/// R4: answers whether the current thread may block in a checkpoint. The
/// Python binding installs a check of the GIL; without one every thread may.
static MAY_WAIT: OnceLock<fn() -> bool> = OnceLock::new();

thread_local! {
    /// A `WAIT` this thread could not sit out (it held the GIL).
    static OWED: RefCell<Option<Arc<Slot>>> = const { RefCell::new(None) };
    /// The meter job of this thread (one job per thread).
    static CURRENT: RefCell<Option<Arc<Slot>>> = const { RefCell::new(None) };
}

/// Installs the check (once per process; later calls are ignored).
pub fn set_may_wait(f: fn() -> bool) {
    let _ = MAY_WAIT.set(f);
}

#[cfg(test)]
thread_local! {
    /// Tests: this thread behaves as if it held the GIL.
    pub(crate) static TEST_HOLDS_GIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn may_wait() -> bool {
    #[cfg(test)]
    if TEST_HOLDS_GIL.with(|c| c.get()) {
        return false;
    }
    MAY_WAIT.get().is_none_or(|f| f())
}

/// A `WAIT` signal a job received while its thread held the GIL.
pub struct OwedWait(Arc<Slot>);

impl OwedWait {
    /// Sits the wait out (call it without the GIL). A cancel during the
    /// wait is the job's error.
    pub fn wait(self) -> Result<()> {
        let g = lock(&self.0.inner);
        self.0.wait_out(g)
    }
}

/// Takes the wait this thread owes, if any.
pub fn take_owed_wait() -> Option<OwedWait> {
    OWED.with(|o| o.borrow_mut().take()).map(OwedWait)
}
