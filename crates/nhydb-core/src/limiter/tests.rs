//! Tests of the limiter core (AP10.1): bounds, zones, ticks, counters,
//! configuration, monitor thread (start, park, panic, fork detection).

use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use crate::error::Error;

const GIB: u64 = 1 << 30;

/// A footprint source per test: a static level and a function reading it.
macro_rules! fake {
    ($lvl:ident, $f:ident) => {
        static $lvl: AtomicU64 = AtomicU64::new(0);
        fn $f() -> Option<u64> {
            Some($lvl.load(Ordering::Relaxed))
        }
    };
}

fn ram64() -> Option<u64> {
    Some(64 * GIB)
}
fn no_cap() -> Option<u64> {
    None
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < end, "timeout: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn bounds_follow_reserve_warn_and_cap() {
    let cfg = LimiterConfig::default();
    let b = Bounds::new(&cfg, 64 * GIB, None);
    assert_eq!(b.limit, 64 * GIB / 100 * 90 + (64 * GIB % 100) * 90 / 100);
    assert_eq!(b.limit - b.yellow_at, (64 * GIB as u128 * 5 / 100) as u64);
    assert_eq!(b.hysteresis, (64 * GIB as u128 * 5 / 100) as u64 / 4);
    // a cgroup limit below the RAM is the base
    let c = Bounds::new(&cfg, 64 * GIB, Some(4 * GIB));
    assert_eq!(c.limit, (4 * GIB as u128 * 90 / 100) as u64);
    // a cgroup "limit" above the RAM changes nothing
    assert_eq!(Bounds::new(&cfg, 64 * GIB, Some(128 * GIB)).limit, b.limit);
}

#[test]
fn zone_table_with_hysteresis() {
    let b = Bounds { ram: 1000, ram_cap: None, limit: 900, yellow_at: 850, hysteresis: 12 };
    use Zone::*;
    let cases = [
        // (current, level, expected)
        (Green, 0, Green),
        (Green, 849, Green),
        (Green, 850, Yellow),
        (Green, 899, Yellow),
        (Green, 900, Red),
        (Green, u64::MAX, Red),
        (Yellow, 899, Yellow),
        (Yellow, 850, Yellow),
        (Yellow, 838, Yellow), // within the hysteresis
        (Yellow, 837, Green),
        (Yellow, 900, Red),
        (Red, 900, Red),
        (Red, 888, Red), // within the hysteresis
        (Red, 887, Yellow),
        (Red, 850, Yellow),
        (Red, 838, Yellow),
        (Red, 837, Green),
        (Red, 0, Green),
    ];
    for (cur, lvl, want) in cases {
        assert_eq!(b.next_zone(cur, lvl), want, "{cur:?} at {lvl}");
    }
}

#[test]
fn config_ranges() {
    let ok = LimiterConfig::default();
    assert!(ok.check().is_ok());
    for bad in [
        LimiterConfig { reserve_percent: 0, ..ok.clone() },
        LimiterConfig { reserve_percent: 91, ..ok.clone() },
        LimiterConfig { warn_percent: 0, ..ok.clone() },
        LimiterConfig { reserve_percent: 90, warn_percent: 10, ..ok.clone() },
        LimiterConfig { tick_ms: 4, ..ok.clone() },
        LimiterConfig { tick_ms: 1001, ..ok.clone() },
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
}

#[test]
fn ticks_measure_zones_and_count() {
    fake!(L1, src1);
    let lim = Limiter::new(src1, ram64, no_cap);
    let b = lim.bounds();
    let t0 = Instant::now();
    let ms = |m: u64| t0 + Duration::from_millis(m);
    assert_eq!(lim.tick(ms(0)), Zone::Green);
    L1.store(b.yellow_at, Ordering::Relaxed);
    assert_eq!(lim.tick(ms(20)), Zone::Yellow); // 20 ms counted as green
    L1.store(b.limit + 5, Ordering::Relaxed);
    assert_eq!(lim.tick(ms(40)), Zone::Red); // 20 ms yellow
    L1.store(0, Ordering::Relaxed);
    assert_eq!(lim.tick(ms(60)), Zone::Green); // 20 ms red
    assert_eq!(lim.tick(ms(5000)), Zone::Green); // long gap: at most 10 ticks counted
    let c = &lim.counters;
    assert_eq!(c.ticks.load(Ordering::Relaxed), 5);
    assert_eq!(c.green_ms.load(Ordering::Relaxed), 20 + 200);
    assert_eq!(c.yellow_ms.load(Ordering::Relaxed), 20);
    assert_eq!(c.red_ms.load(Ordering::Relaxed), 20);
    assert_eq!(c.zone_changes.load(Ordering::Relaxed), 3);
    assert_eq!(c.peak_level.load(Ordering::Relaxed), b.limit + 5);
    assert_eq!(c.peak_over_limit.load(Ordering::Relaxed), 5);
    assert_eq!(lim.level(), 0);
}

#[test]
fn failed_sample_keeps_the_zone() {
    fn none() -> Option<u64> {
        None
    }
    let lim = Limiter::new(none, ram64, no_cap);
    lim.zone.store(Zone::Yellow as u8, Ordering::Relaxed);
    assert_eq!(lim.tick(Instant::now()), Zone::Yellow);
    assert_eq!(lim.counters.sample_errors.load(Ordering::Relaxed), 1);
}

#[test]
fn unknown_ram_keeps_the_limit_far_away() {
    fake!(L2, src2);
    fn none() -> Option<u64> {
        None
    }
    let lim = Limiter::new(src2, none, no_cap);
    L2.store(1 << 50, Ordering::Relaxed);
    assert_eq!(lim.tick(Instant::now()), Zone::Green);
}

#[test]
fn first_handle_defines_the_settings() {
    fake!(L3, src3);
    let lim = Limiter::new(src3, ram64, no_cap);
    let a = LimiterConfig { reserve_percent: 20, ..Default::default() };
    assert_eq!(lim.configure(&a, Some(PathBuf::from("/x/a.toml"))), None);
    assert_eq!(lim.bounds().limit, (64 * GIB as u128 * 80 / 100) as u64);
    assert_eq!(lim.configure(&a, None), None, "same values: no warning");
    let w = lim.configure(&LimiterConfig::default(), None).expect("warning");
    assert!(w.contains("/x/a.toml"), "{w}");
    assert_eq!(lim.config(), a);
    let s = lim.status();
    assert_eq!(s.get("hints").and_then(|h| h.as_list()).map(|l| l.len()), Some(1));
    assert_eq!(s.get("reserve_percent"), Some(&Val::Int(20)));
}

/// Test limiter with a fast tick; N5 off (its own tests switch it on).
fn leak(src: fn() -> Option<u64>) -> &'static Limiter {
    leak_with(src, LimiterConfig { tick_ms: 5, cancel_over_limit: false, ..Default::default() })
}

fn leak_with(src: fn() -> Option<u64>, cfg: LimiterConfig) -> &'static Limiter {
    let lim: &'static Limiter = Box::leak(Box::new(Limiter::new(src, ram64, no_cap)));
    lim.configure(&cfg, None);
    lim
}

fn ticks(l: &Limiter) -> u64 {
    l.counters.ticks.load(Ordering::Relaxed)
}

/// Waits until the tick counter stands still (the thread parked).
fn parked(l: &Limiter) -> bool {
    let a = ticks(l);
    std::thread::sleep(Duration::from_millis(60));
    ticks(l) == a
}

#[test]
fn thread_starts_lazily_ticks_with_jobs_and_parks_without() {
    fake!(L4, src4);
    let lim = leak(src4);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(ticks(lim), 0, "no thread before the first job");
    assert_eq!(lim.status().get("thread_running"), Some(&Val::Bool(false)));
    lim.activity_begin();
    wait_until("ticks with a job", || ticks(lim) >= 3);
    assert_eq!(lim.status().get("thread_running"), Some(&Val::Bool(true)));
    lim.activity_end();
    wait_until("parks without jobs", || parked(lim));
    // red without jobs: it keeps ticking until the level falls
    lim.activity_begin();
    L4.store(u64::MAX / 4, Ordering::Relaxed);
    wait_until("red", || lim.zone() == Zone::Red);
    lim.activity_end();
    let a = ticks(lim);
    std::thread::sleep(Duration::from_millis(60));
    assert!(ticks(lim) > a, "red keeps the monitor awake");
    L4.store(0, Ordering::Relaxed);
    wait_until("green and parked", || lim.zone() == Zone::Green && parked(lim));
    assert_eq!(lim.counters.thread_starts.load(Ordering::Relaxed), 1);
}

#[test]
fn panic_in_a_tick_is_caught_and_reported() {
    static BOOM: AtomicBool = AtomicBool::new(true);
    fn src() -> Option<u64> {
        if BOOM.swap(false, Ordering::Relaxed) {
            panic!("source exploded");
        }
        Some(1)
    }
    let lim = leak(src);
    lim.activity_begin();
    wait_until("panic counted and ticks go on", || {
        lim.counters.tick_panics.load(Ordering::Relaxed) == 1 && lim.level() == 1
    });
    let err = lim.status().get("error").cloned();
    assert!(matches!(&err, Some(Val::Str(s)) if s.contains("source exploded")), "{err:?}");
    lim.activity_end();
}

#[test]
fn fork_child_restarts_the_thread_and_resets() {
    fake!(L5, src5);
    let lim = leak(src5);
    lim.activity_begin();
    wait_until("ticks", || ticks(lim) >= 2);
    lim.activity_end();
    wait_until("parked", || parked(lim));
    // as seen by a fork child: the thread belongs to another process
    lock(&lim.inner).thread_pid = Some(std::process::id().wrapping_add(1));
    lim.active.store(7, Ordering::Relaxed); // jobs of the "parent"
    lim.ensure_thread();
    assert_eq!(lim.active.load(Ordering::Relaxed), 0, "parent jobs dropped");
    assert_eq!(lim.counters.thread_starts.load(Ordering::Relaxed), 1, "counters reset, new start counted");
    assert_eq!(ticks(lim), 0);
}

#[test]
fn status_has_the_documented_fields() {
    fake!(L6, src6);
    let lim = Limiter::new(src6, ram64, no_cap);
    L6.store(123, Ordering::Relaxed);
    let s = lim.status();
    for k in [
        "enabled", "mode", "zone", "level_bytes", "limit_bytes", "yellow_bytes", "ram_bytes", "ram_cap_bytes",
        "reserve_percent", "warn_percent", "tick_ms", "source", "config_source", "thread_running", "active_jobs",
        "counters", "hints", "error",
    ] {
        assert!(s.get(k).is_some(), "missing {k}");
    }
    assert_eq!(s.get("level_bytes"), Some(&Val::Int(123)));
    assert_eq!(s.get("zone"), Some(&Val::Str("green".into())));
}

#[test]
fn global_footprint_is_live() {
    let s = global().status();
    match s.get("level_bytes") {
        Some(Val::Int(v)) => assert!(*v > 0),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------
// AP10.2: register, signal slots, checkpoints
// ---------------------------------------------------------------------

fn reg(l: &'static Limiter) -> Job {
    l.register(JobKind::Rigid, JobPath::Collect)
}

#[test]
fn register_and_drop_unregister() {
    fake!(L7, src7);
    let lim = leak(src7);
    let a = reg(lim);
    let b = lim.register(JobKind::Elastic, JobPath::Sort);
    assert!(b.id() > a.id(), "id = age");
    assert_eq!(lim.jobs().len(), 2);
    assert_eq!(lim.active.load(Ordering::Relaxed), 2);
    let st = lim.status();
    let jobs = st.get("jobs").and_then(|j| j.as_list()).unwrap();
    assert_eq!(jobs.len(), 2);
    drop(a);
    assert_eq!(lim.jobs().len(), 1);
    // a panic inside a job unregisters it (Drop during unwinding)
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _j = reg(lim);
        panic!("job failed");
    }));
    assert!(r.is_err());
    drop(b);
    assert!(lim.jobs().is_empty());
    assert_eq!(lim.active.load(Ordering::Relaxed), 0);
    assert_eq!(lim.path_counters.registered[JobPath::Collect as usize].load(Ordering::Relaxed), 2);
}

#[test]
fn signal_priorities() {
    use SlotState::*;
    fake!(L8, src8);
    let lim = leak(src8);
    let j = reg(lim);
    let s = &j.slot;
    assert!(!s.signal(Go, None), "Go on Go: no change");
    assert!(s.signal(Shrink, None));
    assert!(s.signal(Wait, None), "Wait beats Shrink");
    assert!(!s.signal(Shrink, None), "Shrink does not replace Wait");
    assert!(s.signal(Go, None), "RESUME");
    assert!(s.signal(Cancel, Some(CancelReason::Limiter)));
    assert!(!s.signal(Go, None) && !s.signal(Wait, None), "Cancel is final");
    assert_eq!(s.state(), Cancel);
}

#[test]
fn checkpoint_go_shrink_cancel() {
    fake!(L9, src9);
    let lim = leak(src9);
    let j = reg(lim);
    assert!(!j.checkpoint(100).unwrap());
    j.slot.signal(SlotState::Shrink, None);
    assert!(j.checkpoint(0).unwrap(), "shrink requested");
    assert_eq!(j.slot.state(), SlotState::Go, "acknowledged");
    assert!(!j.checkpoint(0).unwrap());
    assert!(lim.cancel_job(j.id()));
    match j.checkpoint(0) {
        Err(Error::ResourceLimit(b)) => assert_eq!(b.0, "cancelled_by_app"),
        other => panic!("{other:?}"),
    }
    assert_eq!(j.slot.grown.load(Ordering::Relaxed), 100);
    assert_eq!(j.slot.checkpoints.load(Ordering::Relaxed), 4);
    assert!(!lim.cancel_job(9_999_999));
}

#[test]
fn wait_blocks_until_resume_and_cancel_wakes() {
    fake!(L10, src10);
    let lim = leak(src10);
    let j = reg(lim);
    let slot = j.slot.clone();
    slot.signal(SlotState::Wait, None);
    let h = std::thread::spawn(move || j.checkpoint(1).map(|_| ()));
    wait_until("job waits", || lock(&slot.inner).waiting_since.is_some());
    let st = lim.status();
    let state = st.get("jobs").and_then(|l| l.as_list()).and_then(|l| l.first().cloned()).and_then(|j| j.get("state").cloned());
    assert_eq!(state, Some(Val::Str("waiting".into())));
    std::thread::sleep(Duration::from_millis(20));
    assert!(!h.is_finished());
    slot.signal(SlotState::Go, None);
    h.join().unwrap().unwrap();
    assert!(lock(&slot.inner).waiting_since.is_none());
    // a waiting job is woken by a cancel
    let j = reg(lim);
    let slot = j.slot.clone();
    slot.signal(SlotState::Wait, None);
    let id = j.id();
    let h = std::thread::spawn(move || j.checkpoint(0));
    wait_until("job waits", || lock(&slot.inner).waiting_since.is_some());
    assert!(lim.cancel_job(id));
    assert!(matches!(h.join().unwrap(), Err(Error::ResourceLimit(_))));
}

#[test]
fn no_lost_wakeup_under_wait_resume_storm() {
    fake!(L11, src11);
    let lim = leak(src11);
    let j = reg(lim);
    let slot = j.slot.clone();
    let done = Arc::new(AtomicBool::new(false));
    let d2 = done.clone();
    let worker = std::thread::spawn(move || {
        let mut n = 0u64;
        while !d2.load(Ordering::Acquire) {
            j.checkpoint(1).unwrap();
            n += 1;
        }
        j.checkpoint(1).unwrap();
        n
    });
    let rounds = 1_000_000;
    for _ in 0..rounds {
        slot.signal(SlotState::Wait, None);
        slot.signal(SlotState::Go, None);
    }
    done.store(true, Ordering::Release);
    // watchdog: the worker must finish (no wake-up lost)
    let end = Instant::now() + Duration::from_secs(20);
    while !worker.is_finished() {
        assert!(Instant::now() < end, "worker hangs: lost wake-up");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(worker.join().unwrap() > 0);
}

#[test]
fn many_threads_register_signal_and_leave() {
    fake!(L12, src12);
    let lim = leak(src12);
    let stop = Arc::new(AtomicBool::new(false));
    // signal thread: random signals on whatever is registered
    let s2 = stop.clone();
    let signaller = std::thread::spawn(move || {
        let mut x = 0x2545F4914F6CDD1Du64;
        while !s2.load(Ordering::Relaxed) {
            for s in lim.jobs() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let to = [SlotState::Go, SlotState::Shrink, SlotState::Wait, SlotState::Go][(x % 4) as usize];
                s.signal(to, None);
            }
            // never leave anyone waiting for long
            for s in lim.jobs() {
                s.signal(SlotState::Go, None);
            }
        }
    });
    let workers: Vec<_> = (0..64)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..400 {
                    let j = lim.register(if (t + i) % 2 == 0 { JobKind::Rigid } else { JobKind::Elastic }, JobPath::ALL[(t + i) % JobPath::ALL.len()]);
                    for _ in 0..40 {
                        j.checkpoint(4096).unwrap();
                    }
                }
            })
        })
        .collect();
    let end = Instant::now() + Duration::from_secs(60);
    for w in workers {
        while !w.is_finished() {
            assert!(Instant::now() < end, "hang");
            for s in lim.jobs() {
                s.signal(SlotState::Go, None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    signaller.join().unwrap();
    assert!(lim.jobs().is_empty(), "register leak");
    assert_eq!(lim.active.load(Ordering::Relaxed), 0);
    let total: u64 = lim.path_counters.registered.iter().map(|a| a.load(Ordering::Relaxed)).sum();
    assert_eq!(total, 64 * 400);
}

#[test]
fn meter_registers_only_at_its_first_checkpoint() {
    let g = global();
    let before = g.path_counters.registered[JobPath::Compact as usize].load(Ordering::Relaxed);
    let mut m = Meter::new(JobKind::Rigid, JobPath::Compact);
    for _ in 0..1000 {
        assert!(!m.grow(1000).unwrap());
    }
    assert_eq!(m.job_id(), None, "below one step: no registration");
    assert!(!m.grow(job::METER_STEP).unwrap());
    let id = m.job_id().expect("registered");
    assert!(g.jobs().iter().any(|s| s.id == id));
    assert_eq!(g.path_counters.registered[JobPath::Compact as usize].load(Ordering::Relaxed), before + 1);
    drop(m);
    assert!(!g.jobs().iter().any(|s| s.id == id), "meter drop unregisters");
}

/// AP10.2 micro benchmark (release: `cargo test --release -- --ignored --nocapture checkpoint_cost`).
#[test]
#[ignore]
fn checkpoint_cost() {
    fake!(L13, src13);
    let lim = leak(src13);
    let j = reg(lim);
    let n = 100_000_000u64;
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(j.checkpoint(std::hint::black_box(64)).unwrap());
    }
    let job_ns = t.elapsed().as_nanos() as f64 / n as f64;
    let mut m = Meter::new(JobKind::Rigid, JobPath::Collect);
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(m.grow(std::hint::black_box(64)).unwrap());
    }
    let meter_ns = t.elapsed().as_nanos() as f64 / n as f64;
    println!("CHECKPOINT_COST job.checkpoint {job_ns:.2} ns, meter.grow(64 B) {meter_ns:.2} ns");
}

// ---------------------------------------------------------------------
// AP10.3: policy (pure function) and dry run
// ---------------------------------------------------------------------

use super::policy::{policy, Decision, JobView, Situation, SHRINK_MIN_GROWTH};

fn jv(id: u64, kind: JobKind, state: SlotState, grown: u64, new: bool) -> JobView {
    JobView { id, kind, state, grown_since_shrink: grown, new_in_phase: new, grown_since_over: grown }
}

fn sit(zone: Zone, level: u64, cache: u64, ineffective: bool) -> Situation {
    Situation { zone, level, yellow_at: 850, limit: 900, cache_used: cache, relief_ineffective: ineffective, cancel_over_limit: false, over_limit_ticks: 0 }
}

#[test]
fn policy_green_resumes_and_ends_relief() {
    use SlotState::*;
    let jobs = [jv(1, JobKind::Rigid, Wait, 0, false), jv(2, JobKind::Elastic, Shrink, 0, false), jv(3, JobKind::Rigid, Go, 0, false), jv(4, JobKind::Rigid, Cancel, 0, false)];
    let d = policy(&sit(Zone::Green, 100, 500, false), &jobs);
    assert_eq!(d.signals, vec![(1, Go), (2, Go)]);
    assert_eq!(d.cache_target, Some(u64::MAX));
    assert!(!d.over_limit_single);
}

#[test]
fn policy_yellow_relief_shrink_and_new_jobs_wait() {
    use SlotState::*;
    let m = SHRINK_MIN_GROWTH;
    let jobs = [
        jv(1, JobKind::Rigid, Go, 5 * m, false),
        jv(2, JobKind::Elastic, Go, 2 * m, false),
        jv(3, JobKind::Elastic, Go, 3 * m, false), // largest elastic growth
        jv(4, JobKind::Elastic, Go, m - 1, false), // too little to give back
        jv(5, JobKind::Rigid, Go, 0, true),        // registered in this phase
        jv(6, JobKind::Rigid, Wait, 0, false),     // held in red before
    ];
    let d = policy(&sit(Zone::Yellow, 870, 100, false), &jobs);
    assert_eq!(d.cache_target, Some(100 - 20), "relief of the excess over the yellow border");
    assert_eq!(d.signals, vec![(3, Shrink), (5, Wait), (6, Go)]);
    // relief limited to the cache; R9B-02: once a relief was ineffective it
    // ends (the shrunk pot would only cost block reloads), in yellow and red
    assert_eq!(policy(&sit(Zone::Yellow, 899, 10, false), &jobs).cache_target, Some(0));
    assert_eq!(policy(&sit(Zone::Yellow, 899, 10, true), &jobs).cache_target, Some(u64::MAX));
    assert_eq!(policy(&sit(Zone::Red, 950, 10, true), &jobs).cache_target, Some(u64::MAX));
    assert_eq!(policy(&sit(Zone::Red, 950, 100, false), &jobs).cache_target, Some(0));
    assert_eq!(policy(&sit(Zone::Yellow, 870, 0, false), &jobs).cache_target, None);
}

#[test]
fn policy_red_only_the_oldest_runs() {
    use SlotState::*;
    let jobs = [jv(7, JobKind::Rigid, Go, 0, false), jv(3, JobKind::Elastic, Wait, 0, false), jv(9, JobKind::Rigid, Go, 0, true)];
    let d = policy(&sit(Zone::Red, 950, 0, false), &jobs);
    assert_eq!(d.signals, vec![(7, Wait), (3, Go), (9, Wait)]);
    assert!(d.over_limit_single, "above the limit with only one job running: reported, not cancelled");
    // a cancelled oldest job does not count: the next one runs
    let jobs = [jv(1, JobKind::Rigid, Cancel, 0, false), jv(2, JobKind::Rigid, Wait, 0, false)];
    assert_eq!(policy(&sit(Zone::Red, 890, 0, false), &jobs).signals, vec![(2, Go)]);
    assert!(!policy(&sit(Zone::Red, 890, 0, false), &jobs).over_limit_single);
}

/// Applies the signals of a decision to the views.
fn apply(jobs: &[JobView], d: &Decision) -> Vec<SlotState> {
    jobs.iter().map(|j| d.signals.iter().rev().find(|(i, _)| *i == j.id).map(|x| x.1).unwrap_or(j.state)).collect()
}

#[test]
fn policy_never_holds_every_job() {
    use SlotState::*;
    let mut x = 0x9E3779B97F4A7C15u64;
    let mut rnd = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    for _ in 0..200_000 {
        let n = 1 + rnd(6);
        let jobs: Vec<JobView> = (0..n)
            .map(|i| {
                jv(
                    10 + i * 3 + rnd(3), // unique, like real ids
                    if rnd(2) == 0 { JobKind::Rigid } else { JobKind::Elastic },
                    [Go, Shrink, Wait, Cancel, Probe][rnd(5) as usize],
                    rnd(4) * SHRINK_MIN_GROWTH,
                    rnd(2) == 0,
                )
            })
            .collect();
        let zone = [Zone::Green, Zone::Yellow, Zone::Red][rnd(3) as usize];
        let s = sit(zone, 800 + rnd(200), rnd(300), rnd(2) == 0);
        let d = policy(&s, &jobs);
        let after = apply(&jobs, &d);
        let live = after.iter().filter(|st| **st != Cancel).count();
        if live > 0 {
            assert!(after.iter().any(|st| *st != Wait && *st != Cancel), "{s:?} {jobs:?} -> {d:?}");
        }
        assert!(d.signals.iter().all(|(id, _)| jobs.iter().any(|j| j.id == *id && j.state != Cancel)), "no signal to a cancelled job");
        if zone == Zone::Red {
            assert!(after.iter().filter(|st| !matches!(st, Wait | Cancel | Probe)).count() <= 1, "red: at most one runs besides a probe");
        }
    }
}

#[test]
fn dry_run_counts_but_never_signals() {
    fake!(L14, src14);
    // slow monitor: the manual ticks of the test are the only ones
    let lim: &'static Limiter = Box::leak(Box::new(Limiter::new(src14, ram64, no_cap)));
    lim.configure(&LimiterConfig { tick_ms: 1000, dry_run: true, ..Default::default() }, None);
    let b = lim.bounds();
    let a = reg(lim);
    let n = lim.register(JobKind::Elastic, JobPath::Sort);
    let t0 = Instant::now();
    lim.tick(t0);
    L14.store(b.limit + 1, Ordering::Relaxed);
    assert_eq!(lim.tick(t0 + Duration::from_millis(20)), Zone::Red);
    let c = &lim.counters;
    assert_eq!(c.dry_wait.load(Ordering::Relaxed), 1, "the younger job would wait");
    assert_eq!((a.slot.state(), n.slot.state()), (SlotState::Go, SlotState::Go), "nothing sent");
    assert_eq!(c.signals_wait.load(Ordering::Relaxed), 0);
    assert_eq!(lim.status().get("mode"), Some(&Val::Str("dry_run".into())));
    // quiet green: the register is not visited, nothing counted
    L14.store(0, Ordering::Relaxed);
    lim.tick(t0 + Duration::from_millis(40));
    let before = c.dry_resume.load(Ordering::Relaxed);
    lim.tick(t0 + Duration::from_millis(60));
    assert_eq!(c.dry_resume.load(Ordering::Relaxed), before);
    assert!(c.max_rise_per_tick.load(Ordering::Relaxed) >= b.limit);
}

#[test]
fn a_thread_holding_the_gil_never_waits_in_a_checkpoint() {
    fake!(L15, src15);
    let lim = leak(src15);
    let j = reg(lim);
    let slot = j.slot.clone();
    slot.signal(SlotState::Wait, None);
    let h = std::thread::spawn(move || {
        job::TEST_HOLDS_GIL.with(|c| c.set(true));
        let t = Instant::now();
        assert!(!j.checkpoint(1).unwrap(), "continues instead of waiting");
        assert!(t.elapsed() < Duration::from_millis(500));
        let w = job::take_owed_wait().expect("the wait is owed");
        assert!(job::take_owed_wait().is_none());
        // "GIL released": sit it out
        job::TEST_HOLDS_GIL.with(|c| c.set(false));
        w.wait().unwrap();
        // an owed wait lapses with its job
        job::TEST_HOLDS_GIL.with(|c| c.set(true));
        j.slot.signal(SlotState::Wait, None);
        j.checkpoint(1).unwrap();
        drop(j);
        assert!(job::take_owed_wait().is_none());
    });
    wait_until("owed wait is sat out", || lock(&slot.inner).waiting_since.is_some());
    slot.signal(SlotState::Go, None);
    h.join().unwrap();
    assert!(lim.counters.waits_deferred.load(Ordering::Relaxed) >= 2);
}

// ---------------------------------------------------------------------
// AP10.4: steering
// ---------------------------------------------------------------------

#[test]
fn red_holds_the_younger_job_within_two_ticks_and_green_resumes() {
    fake!(L16, src16);
    let lim = leak(src16);
    let b = lim.bounds();
    let old = reg(lim);
    let young = reg(lim);
    wait_until("ticking", || ticks(lim) >= 2);
    let t0 = ticks(lim);
    L16.store(b.limit + 1, Ordering::Relaxed);
    wait_until("younger waits", || young.slot.state() == SlotState::Wait);
    assert!(ticks(lim) - t0 <= 2, "reaction took {} ticks", ticks(lim) - t0);
    assert_eq!(old.slot.state(), SlotState::Go, "the oldest keeps running");
    // the counter follows the signal in the same tick (the state can be
    // seen a moment earlier): wait for it
    wait_until("wait counted", || lim.counters.signals_wait.load(Ordering::Relaxed) >= 1);
    L16.store(0, Ordering::Relaxed);
    wait_until("resumed", || young.slot.state() == SlotState::Go);
    wait_until("resume counted", || lim.counters.signals_resume.load(Ordering::Relaxed) >= 1);
    assert_eq!(lim.status().get("mode"), Some(&Val::Str("active".into())));
}

#[test]
fn yellow_asks_an_elastic_job_to_shrink() {
    fake!(L17, src17);
    let lim = leak(src17);
    let b = lim.bounds();
    let e = lim.register(JobKind::Elastic, JobPath::Sort);
    e.checkpoint(policy::SHRINK_MIN_GROWTH).unwrap();
    L17.store(b.yellow_at + 1, Ordering::Relaxed);
    wait_until("shrink asked", || e.slot.state() == SlotState::Shrink);
    assert!(e.checkpoint(0).unwrap(), "the job sees SHRINK once");
    assert!(!e.checkpoint(0).unwrap());
    // no new SHRINK before it grew again by the minimum
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(e.slot.state(), SlotState::Go);
    L17.store(0, Ordering::Relaxed);
}

#[test]
fn cancel_beats_a_concurrent_resume() {
    fake!(L18, src18);
    let lim = leak(src18);
    for _ in 0..2000 {
        let j = reg(lim);
        j.slot.signal(SlotState::Wait, None);
        let (a, b) = (j.slot.clone(), j.slot.clone());
        let h1 = std::thread::spawn(move || a.signal(SlotState::Cancel, Some(CancelReason::App)));
        let h2 = std::thread::spawn(move || b.signal(SlotState::Go, None));
        h1.join().unwrap();
        h2.join().unwrap();
        assert_eq!(j.slot.state(), SlotState::Cancel);
        assert!(matches!(j.checkpoint(0), Err(Error::ResourceLimit(_))));
    }
}

#[test]
fn liveness_with_a_level_stuck_in_red() {
    fake!(L19, src19);
    // without TEST_IT probes: the plain order rule of red
    let lim: &'static Limiter = Box::leak(Box::new(Limiter::new(src19, ram64, no_cap)));
    lim.configure(&LimiterConfig { tick_ms: 5, probe_bytes: 0, cancel_over_limit: false, ..Default::default() }, None);
    L19.store(lim.bounds().limit * 2, Ordering::Relaxed);
    let order = Arc::new(Mutex::new(Vec::new()));
    let jobs: Vec<Job> = (0..10).map(|_| reg(lim)).collect();
    let ids: Vec<u64> = jobs.iter().map(|j| j.id()).collect();
    let hs: Vec<_> = jobs
        .into_iter()
        .map(|j| {
            let order = order.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    j.checkpoint(1 << 20).unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                }
                order.lock().unwrap().push(j.id());
            })
        })
        .collect();
    let end = Instant::now() + Duration::from_secs(60);
    for h in hs {
        while !h.is_finished() {
            assert!(Instant::now() < end, "stall: liveness broken");
            std::thread::sleep(Duration::from_millis(5));
        }
        h.join().unwrap();
    }
    // every job finished; in red they ran one after the other, oldest first
    assert_eq!(*order.lock().unwrap(), ids);
    assert!(lim.counters.over_limit_single_ticks.load(Ordering::Relaxed) > 0, "reported, not cancelled");
    L19.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------
// AP10.5: TEST_IT probes
// ---------------------------------------------------------------------

/// Red, stable level; `old` runs, `young` waits and gets a probe. The
/// young job then reports `report` bytes in 4 steps while the level rises
/// by `rise`; the old one reports `other` bytes meanwhile.
fn probe_case(level: &'static AtomicU64, lim: &'static Limiter, rise: u64, other: u64) -> (SlotState, u64, u64, u64) {
    let probe = lim.config().probe_bytes;
    let old = reg(lim);
    let young = reg(lim);
    level.store(lim.bounds().limit + (64 << 20), Ordering::Relaxed);
    wait_until("probe started", || young.slot.state() == SlotState::Probe);
    let base = level.load(Ordering::Relaxed);
    let y = young.slot.clone();
    let h = std::thread::spawn(move || {
        for _ in 0..4 {
            young.checkpoint(probe / 4).unwrap();
        }
        young
    });
    if other > 0 {
        old.checkpoint(other).unwrap();
    }
    level.store(base + rise, Ordering::Relaxed);
    let c = &lim.counters;
    wait_until("verdict", || {
        c.probes_reused.load(Ordering::Relaxed) + c.probes_grew.load(Ordering::Relaxed) + c.probes_invalid.load(Ordering::Relaxed) > 0
    });
    let st = y.state();
    let r = (st, c.probes_reused.load(Ordering::Relaxed), c.probes_grew.load(Ordering::Relaxed), c.probes_invalid.load(Ordering::Relaxed));
    // let everything end
    level.store(0, Ordering::Relaxed);
    let young = h.join().unwrap();
    drop((old, young));
    r
}

#[test]
fn probe_reused_memory_lets_the_job_run_on() {
    fake!(L20, src20);
    let lim = leak(src20);
    let (st, reused, grew, invalid) = probe_case(&L20, lim, 8 << 20, 0);
    assert_eq!((reused, grew, invalid), (1, 0, 0));
    assert_eq!(st, SlotState::Probe, "next probe slice");
}

#[test]
fn probe_real_growth_sends_the_job_back_to_wait() {
    fake!(L21, src21);
    let lim = leak(src21);
    let (st, reused, grew, invalid) = probe_case(&L21, lim, 250 << 20, 0);
    assert_eq!((reused, grew, invalid), (0, 1, 0));
    assert_eq!(st, SlotState::Wait);
    // blocked: no new probe while the level stays up
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(lim.counters.probes.load(Ordering::Relaxed), 1);
}

#[test]
fn probe_with_other_jobs_growing_is_not_judged() {
    fake!(L22, src22);
    let lim = leak(src22);
    let (st, reused, grew, invalid) = probe_case(&L22, lim, 8 << 20, 200 << 20);
    assert_eq!((reused, grew, invalid), (0, 0, 1));
    // waits again; a new probe may follow once the level is stable again
    assert!(matches!(st, SlotState::Wait | SlotState::Probe), "{st:?}");
}

// ---------------------------------------------------------------------
// N5: CANCEL as last resort
// ---------------------------------------------------------------------

#[test]
fn policy_cancels_the_single_growing_job_only_as_last_resort() {
    use SlotState::*;
    let m = 1 << 20;
    let s = |ticks: u32, on: bool| Situation { cancel_over_limit: on, over_limit_ticks: ticks, ..sit(Zone::Red, 950, 0, true) };
    let grow = [jv(1, JobKind::Rigid, Go, m, false), jv(2, JobKind::Rigid, Wait, 0, false)];
    // not before the window is over, never when switched off
    assert_eq!(policy(&s(policy::CANCEL_AFTER_TICKS - 1, true), &grow).signals, vec![]);
    assert_eq!(policy(&s(99, false), &grow).signals, vec![]);
    // then: cancel the running one, the next one runs
    assert_eq!(policy(&s(policy::CANCEL_AFTER_TICKS, true), &grow).signals, vec![(1, Cancel), (2, Go)]);
    // a running job that did not grow is not cancelled (the level is held by others)
    let flat = [jv(1, JobKind::Rigid, Go, 0, false), jv(2, JobKind::Rigid, Wait, 0, false)];
    assert_eq!(policy(&s(50, true), &flat).signals, vec![]);
    // below the limit (red by hysteresis): no cancel
    let below = Situation { level: 895, ..s(50, true) };
    assert_eq!(policy(&below, &grow).signals, vec![]);
}

#[test]
fn limiter_cancels_a_job_that_keeps_growing_above_the_limit() {
    fake!(L23, src23);
    let lim = leak_with(src23, LimiterConfig { tick_ms: 5, ..Default::default() });
    assert!(lim.config().cancel_over_limit, "default on (N5)");
    let old = reg(lim);
    let young = reg(lim);
    L23.store(lim.bounds().limit + (100 << 20), Ordering::Relaxed);
    wait_until("young waits", || young.slot.state() == SlotState::Wait);
    // the running job keeps growing
    let end = Instant::now() + Duration::from_secs(10);
    let err = loop {
        match old.checkpoint(1 << 20) {
            Err(e) => break e,
            Ok(_) => {
                assert!(Instant::now() < end, "never cancelled");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    };
    assert!(matches!(&err, Error::ResourceLimit(b) if b.0 == "cancelled_by_limiter"), "{err:?}");
    drop(old);
    wait_until("the next job runs", || young.slot.state() == SlotState::Go);
    assert_eq!(lim.counters.signals_cancel.load(Ordering::Relaxed), 1);
    L23.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------
// AP10.6: long waits, wait timeout, wait statistics
// ---------------------------------------------------------------------

#[test]
fn long_wait_is_marked_and_timeout_cancels() {
    fake!(L24, src24);
    let lim = leak_with(src24, LimiterConfig { tick_ms: 5, cancel_over_limit: false, notify_wait_s: 0, wait_timeout_s: 1, probe_bytes: 0, ..Default::default() });
    let old = reg(lim);
    let young = reg(lim);
    // first checkpoint before red, in this thread (a late thread start,
    // e.g. Windows under load, must not let the wait begin there)
    young.checkpoint(0).unwrap();
    L24.store(lim.bounds().limit + 1, Ordering::Relaxed);
    let t = Instant::now();
    let h = std::thread::spawn(move || {
        loop {
            if let Err(e) = young.checkpoint(0) {
                return e;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let err = h.join().unwrap();
    assert!(matches!(&err, Error::ResourceLimit(b) if b.0 == "wait_timeout"), "{err:?}");
    assert!(t.elapsed() >= Duration::from_secs(1));
    let c = &lim.counters;
    assert!(c.long_waits.load(Ordering::Relaxed) >= 1);
    wait_until("timeout counted", || c.wait_timeouts.load(Ordering::Relaxed) >= 1);
    assert_eq!(c.wait_timeouts.load(Ordering::Relaxed), 1);
    assert!(c.wait_ms_max.load(Ordering::Relaxed) >= 900, "wait recorded when the job left");
    drop(old);
    L24.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------
// AP10.7: limiter.txt
// ---------------------------------------------------------------------

fn log_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nhydb_limlog_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn log_lim(src: fn() -> Option<u64>, dir: &std::path::Path, max_bytes: u64) -> &'static Limiter {
    let lim: &'static Limiter = Box::leak(Box::new(Limiter::new(src, ram64, no_cap)));
    let cfg = LimiterConfig { tick_ms: 5, log: log::LogConfig { max_bytes, ..Default::default() }, ..Default::default() };
    lim.configure_for(&cfg, None, Some(&dir.join("x.nhydb")));
    lim
}

fn lines(p: &std::path::Path) -> Vec<Val> {
    std::fs::read_to_string(p).unwrap_or_default().lines().map(|l| Val::from_json(l).unwrap()).collect()
}

fn num(v: &Val, path: &[&str]) -> i64 {
    let mut x = v;
    for k in path {
        x = x.get(k).unwrap_or_else(|| panic!("missing {k} in {v:?}"));
    }
    match x {
        Val::Int(i) => *i,
        o => panic!("{o:?}"),
    }
}

#[test]
fn log_records_count_every_event_exactly_once() {
    fake!(L25, src25);
    let dir = log_dir("exact");
    let lim = log_lim(src25, &dir, 60_000_000);
    let stop = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let j = lim.register(JobKind::Rigid, JobPath::Collect);
                    j.checkpoint(1).unwrap();
                    j.checkpoint(1).unwrap();
                    n += 1;
                }
                n
            })
        })
        .collect();
    // the helper writes asynchronously; slow file systems (Windows with a
    // virus scanner) must not let the 16-record buffer overflow here: a
    // dropped record is the designed reaction, but this test counts events
    let written = || lock(&lock(&lim.inner).logger.as_ref().unwrap().state.clone()).lines_written;
    for i in 0..200u64 {
        let t = Instant::now();
        while i.saturating_sub(written()) >= 8 && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(lim.log_record(false), "record {i} dropped");
        std::thread::sleep(Duration::from_micros(300));
    }
    stop.store(true, Ordering::Relaxed);
    let total: u64 = workers.into_iter().map(|w| w.join().unwrap()).sum();
    lim.log_final();
    let recs = lines(&dir.join("limiter.txt"));
    assert_eq!(recs.len(), 201);
    let reg: i64 = recs.iter().map(|r| num(r, &["counters", "jobs_registered"])).sum();
    let fin: i64 = recs.iter().map(|r| num(r, &["counters", "jobs_finished"])).sum();
    let cps: i64 = recs.iter().map(|r| num(r, &["paths", "collect", "checkpoints"])).sum();
    assert_eq!((reg as u64, fin as u64, cps as u64), (total, total, 2 * total), "no event lost or counted twice");
    assert_eq!(recs.last().unwrap().get("final"), Some(&Val::Bool(true)));
    for r in &recs {
        for k in ["ts", "from", "interval_s", "pid", "db", "mode", "source", "limit_bytes", "level_bytes", "zone"] {
            assert!(r.get(k).is_some(), "{k}");
        }
    }
}

#[test]
fn log_interval_peaks_start_anew() {
    fake!(L26, src26);
    let dir = log_dir("peaks");
    let lim = log_lim(src26, &dir, 60_000_000);
    let t0 = Instant::now();
    L26.store(5 << 30, Ordering::Relaxed);
    lim.tick(t0);
    lim.log_record(false);
    L26.store(1 << 30, Ordering::Relaxed);
    lim.tick(t0 + Duration::from_millis(20));
    lim.log_final();
    let recs = lines(&dir.join("limiter.txt"));
    assert_eq!(num(&recs[0], &["counters", "peak_level"]), 5 << 30);
    assert_eq!(num(&recs[1], &["counters", "peak_level"]), 1 << 30, "the peak of the interval, not of the process");
    assert_eq!(num(&recs[0], &["counters", "ticks"]) + num(&recs[1], &["counters", "ticks"]), 2);
}

#[test]
fn log_rotates_into_a_zip_after_writing() {
    fake!(L27, src27);
    let dir = log_dir("rotate");
    let lim = log_lim(src27, &dir, 4096);
    let mut written = Vec::new();
    for _ in 0..12 {
        lim.log_record(false);
        lim.log_final(); // also waits for the helper
        written.push(std::fs::metadata(dir.join("limiter.txt")).map(|m| m.len()).unwrap_or(0));
    }
    let zips: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "zip")).collect();
    assert!(!zips.is_empty(), "rotated: {written:?}");
    // the archive holds limiter.txt as it was: complete JSON lines, the last one over the threshold
    let z = std::fs::read(&zips[0]).unwrap();
    assert_eq!(&z[..4], &0x0403_4b50u32.to_le_bytes());
    let name_len = u16::from_le_bytes([z[26], z[27]]) as usize;
    let comp = u32::from_le_bytes([z[18], z[19], z[20], z[21]]) as usize;
    assert_eq!(&z[30..30 + name_len], b"limiter.txt");
    let data = miniz_oxide::inflate::decompress_to_vec(&z[30 + name_len..30 + name_len + comp]).unwrap();
    assert_eq!(crc32fast::hash(&data), u32::from_le_bytes([z[14], z[15], z[16], z[17]]));
    let text = String::from_utf8(data).unwrap();
    assert!(text.len() >= 4096 && text.ends_with('\n'));
    assert!(text.lines().all(|l| Val::from_json(l).is_ok()));
    let st = lim.status();
    assert!(num(&st, &["log", "rotations"]) >= 1);
}

#[test]
fn log_write_errors_never_touch_the_limiter() {
    fake!(L28, src28);
    let dir = log_dir("error");
    let blocker = dir.join("not_a_dir");
    std::fs::write(&blocker, b"x").unwrap();
    let lim: &'static Limiter = Box::leak(Box::new(Limiter::new(src28, ram64, no_cap)));
    let cfg = LimiterConfig {
        tick_ms: 5,
        log: log::LogConfig { path: Some(blocker.join("sub").display().to_string()), ..Default::default() },
        ..Default::default()
    };
    lim.configure_for(&cfg, None, Some(&dir.join("x.nhydb")));
    assert!(lim.log_record(false), "handed over");
    lim.log_final();
    let st = lim.status();
    assert!(matches!(st.get("log").and_then(|l| l.get("last_error")), Some(Val::Str(_))), "{st:?}");
    // the limiter itself goes on
    let j = reg(lim);
    wait_until("ticks", || ticks(lim) >= 3);
    drop(j);
    assert_eq!(st.get("error"), Some(&Val::None));
}

// ---------------------------------------------------------------------
// One job per thread (2026-11-03): a nested meter joins the thread's job
// ---------------------------------------------------------------------

#[test]
fn nested_meter_joins_the_thread_job() {
    fake!(L30, src30);
    let lim = leak(src30);
    let mut outer = Meter::new_in(lim, JobKind::Elastic, JobPath::IndexBuild);
    outer.checkpoint().unwrap();
    let id = outer.job_id().unwrap();
    {
        let mut inner = Meter::new_in(lim, JobKind::Rigid, JobPath::Postings);
        inner.grow(3 << 20).unwrap();
        assert_eq!(inner.job_id(), Some(id), "joined, no second job");
        assert_eq!(lim.jobs().len(), 1);
        // the inner path is counted, its growth goes to the thread's job
        assert_eq!(lim.path_counters.registered[JobPath::Postings as usize].load(Ordering::Relaxed), 1);
        assert_eq!(lim.path_counters.checkpoints[JobPath::Postings as usize].load(Ordering::Relaxed), 1);
        assert_eq!(lim.jobs()[0].grown.load(Ordering::Relaxed), 3 << 20);
        // a SHRINK for the elastic job is left to its own (elastic) meter
        lim.jobs()[0].signal(SlotState::Shrink, None);
        assert!(!inner.checkpoint().unwrap(), "a rigid joined meter does not take SHRINK");
        assert_eq!(lim.jobs()[0].state(), SlotState::Shrink);
    }
    assert!(outer.checkpoint().unwrap(), "the elastic owner gets its SHRINK");
    assert_eq!(lim.counters.jobs_registered.load(Ordering::Relaxed), 1);
    assert_eq!(lim.counters.jobs_joined.load(Ordering::Relaxed), 1);
    drop(outer);
    assert!(lim.jobs().is_empty());
    // the next operation of the thread registers anew
    let mut next = Meter::new_in(lim, JobKind::Rigid, JobPath::Collect);
    next.checkpoint().unwrap();
    assert_ne!(next.job_id(), Some(id));
    assert_eq!(lim.counters.jobs_registered.load(Ordering::Relaxed), 2);
    // other threads have their own job
    let other = std::thread::spawn(move || {
        let mut m = Meter::new_in(lim, JobKind::Rigid, JobPath::Collect);
        m.checkpoint().unwrap();
        m.job_id().unwrap()
    })
    .join()
    .unwrap();
    assert_ne!(Some(other), next.job_id());
}

#[test]
fn joined_meter_of_an_ended_job_resolves_anew() {
    fake!(L31, src31);
    let lim = leak(src31);
    let mut outer = Meter::new_in(lim, JobKind::Rigid, JobPath::Collect);
    outer.checkpoint().unwrap();
    let mut inner = Meter::new_in(lim, JobKind::Rigid, JobPath::PyConvert);
    inner.checkpoint().unwrap();
    let first = inner.job_id();
    drop(outer);
    // the joined job ended: the meter registers a job of its own
    inner.checkpoint().unwrap();
    assert_ne!(inner.job_id(), first);
    assert_eq!(lim.jobs().len(), 1);
    assert_eq!(lim.jobs()[0].id, inner.job_id().unwrap());
}

/// The hang of 2026-11-02 (e6ecdca, Windows): `build_indexes` held its spill
/// meter while `persist_secondary` registered a second job in the same
/// thread; in red the outer job was allowed to run while the thread slept in
/// the inner one. With one job per thread the operation runs on.
#[test]
fn nested_meters_never_deadlock_in_red() {
    for cancel in [false, true] {
        fake!(L32, src32);
        let lim = leak_with(src32, LimiterConfig { tick_ms: 5, probe_bytes: 0, cancel_over_limit: cancel, ..Default::default() });
        L32.store(lim.bounds().limit + (100 << 20), Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let mut spill = Meter::new_in(lim, JobKind::Elastic, JobPath::IndexBuild);
            spill.checkpoint().unwrap();
            tx.send(()).unwrap();
            let r = (|| -> crate::error::Result<()> {
                let mut persist = Meter::new_in(lim, JobKind::Rigid, JobPath::IndexBuild);
                for _ in 0..20 {
                    persist.grow(1 << 20)?;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            })();
            r.and_then(|_| spill.checkpoint().map(|_| ()))
        });
        // a younger job of another thread holds its memory and waits
        rx.recv().unwrap();
        let other = std::thread::spawn(move || {
            let mut m = Meter::new_in(lim, JobKind::Rigid, JobPath::Collect);
            for _ in 0..20 {
                if m.grow(1 << 20).is_err() {
                    return;
                }
            }
        });
        let end = Instant::now() + Duration::from_secs(20);
        while !h.is_finished() {
            assert!(Instant::now() < end, "deadlock (cancel_over_limit = {cancel}): {:?}", lim.jobs().iter().map(|s| (s.id, s.path, s.state())).collect::<Vec<_>>());
            std::thread::sleep(Duration::from_millis(5));
        }
        // finished, or (N5 on) cancelled as a whole – never stuck
        let r = h.join().unwrap();
        assert!(r.is_ok() || matches!(&r, Err(Error::ResourceLimit(b)) if b.0 == "cancelled_by_limiter"), "{r:?}");
        L32.store(0, Ordering::Relaxed);
        other.join().unwrap();
    }
}

/// N5 cascade (Windows 2026-11-02: all 8 queries cancelled within 0.8 s):
/// after a cancel the level stays up while the memory of the cancelled job
/// comes back slowly; the next job must not be cancelled for that. It is
/// cancelled only after real growth above the level of the last cancel.
#[test]
fn n5_cancels_the_next_job_only_after_real_growth() {
    fake!(L33, src33);
    let lim = leak_with(src33, LimiterConfig { tick_ms: 5, probe_bytes: 0, ..Default::default() });
    let over = lim.bounds().limit + (100 << 20);
    L33.store(over, Ordering::Relaxed);
    let first = reg(lim);
    let next = reg(lim);
    wait_until("next waits", || next.slot.state() == SlotState::Wait);
    // the first job keeps growing: cancelled (last resort)
    let end = Instant::now() + Duration::from_secs(10);
    while first.checkpoint(1 << 20).is_ok() {
        assert!(Instant::now() < end, "never cancelled");
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(first);
    wait_until("the next job runs", || next.slot.state() == SlotState::Go);
    // the level stays where it was (memory comes back slowly) and the next
    // job grows: no second cancel
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        next.checkpoint(1 << 20).expect("not cancelled while the level only stays up");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(lim.counters.signals_cancel.load(Ordering::Relaxed), 1);
    assert!(lim.counters.cancel_held_ticks.load(Ordering::Relaxed) > 0);
    // still within the margin: held
    L33.store(over + policy::CANCEL_REARM_MARGIN / 2, Ordering::Relaxed);
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(200) {
        next.checkpoint(1 << 20).expect("within the margin: held");
        std::thread::sleep(Duration::from_millis(2));
    }
    // real growth above the margin: the last resort applies again
    L33.store(over + 2 * policy::CANCEL_REARM_MARGIN, Ordering::Relaxed);
    let end = Instant::now() + Duration::from_secs(10);
    let err = loop {
        match next.checkpoint(1 << 20) {
            Err(e) => break e,
            Ok(_) => {
                assert!(Instant::now() < end, "never cancelled after real growth");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    };
    assert!(matches!(&err, Error::ResourceLimit(b) if b.0 == "cancelled_by_limiter"), "{err:?}");
    wait_until("second cancel counted", || lim.counters.signals_cancel.load(Ordering::Relaxed) >= 2);
    assert_eq!(lim.counters.signals_cancel.load(Ordering::Relaxed), 2);
    drop(next);
    // at the limit the hold ends
    L33.store(0, Ordering::Relaxed);
}

/// U2 (2026-11-03): a cancelled job gives the memory the allocator keeps
/// back when it leaves (Linux/glibc `malloc_trim`); a job that ends normally
/// does not.
#[test]
fn a_cancelled_job_releases_freed_memory_when_it_leaves() {
    fake!(L35, src35);
    let lim = leak(src35);
    let trims = || lim.counters.trims.load(Ordering::Relaxed);
    drop(reg(lim));
    assert_eq!(trims(), 0, "normal end: no trim");
    let j = reg(lim);
    assert!(lim.cancel_job(j.id()));
    assert!(j.checkpoint(0).is_err());
    drop(j);
    let expect = if cfg!(all(target_os = "linux", target_env = "gnu")) { 1 } else { 0 };
    assert_eq!(trims(), expect);
}
