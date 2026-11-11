//! RUN¹⁰ AP10.4: integrity under the RAM limiter on real databases.
//!
//! * `CANCEL` between two blocks of a write call: the whole call is rolled
//!   back (`write_scope`), the file verifies, the data is the state before.
//! * `WAIT` in the middle of an index build and of a compaction, then
//!   `RESUME`: the results equal those of an undisturbed run.
//!
//! One test function (own process, scenarios one after the other): the jobs
//! are caught in the register of the process limiter, parallel tests would
//! add their own jobs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nhydb_core::db::dense::DenseRows;
use nhydb_core::db::maint::CompactOptions;
use nhydb_core::limiter::{self, JobPath, Slot, SlotState};
use nhydb_core::{Db, Error, FieldDef, Nat, OpenOptions, TypeCode, Val, NL};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nhydb_limint_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join(format!("{name}.nhydb"))
}

fn setup(p: &Path) -> Db {
    let mut db = Db::create(p, true, OpenOptions::default()).unwrap();
    let r = Nat::S(1);
    db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
    db.create_shape(1, &r, "s", vec![FieldDef::new(Nat::S(1), "k", TypeCode::VarUInt), FieldDef::new(Nat::S(2), "s", TypeCode::Utf8)], "").unwrap();
    db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
    db.create_index(10, &r, "k", &Val::Int(1), "").unwrap();
    db.commit().unwrap();
    db
}

fn rows(from: u64, n: u64, width: usize) -> Vec<Vec<Val>> {
    (from..from + n).map(|u| vec![Val::Int((u * 7919 % 100_003) as i64), Val::Str(format!("{u:08}{}", "x".repeat(width)))]).collect()
}

fn put(db: &mut Db, from: u64, rs: &[Vec<Val>], commit: bool) -> nhydb_core::Result<Val> {
    db.put_dense_batch(&Nat::S(1), &Nat::S(from), &DenseRows::Tuples(rs), Some(1), &[], NL, commit)
}

fn ok(db: &Db) {
    let v = db.verify(true).unwrap();
    assert_eq!(v.get("ok"), Some(&Val::Bool(true)), "{v:?}");
}

/// Waits until a job of `path` with an id above `after` is registered.
fn catch(path: JobPath, after: u64) -> Arc<Slot> {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(s) = limiter::global().jobs().into_iter().find(|s| s.path == path && s.id > after) {
            return s;
        }
        assert!(Instant::now() < end, "no {path:?} job registered");
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// Holds a job in `WAIT` until it really waits, keeps it there for a
/// moment and lets it go on.
fn hold(s: &Slot, for_ms: u64) {
    assert!(s.signal(SlotState::Wait, None));
    let end = Instant::now() + Duration::from_secs(60);
    while limiter::global().status().get("jobs").and_then(|j| j.as_list()).is_some_and(|l| {
        !l.iter().any(|j| j.get("id") == Some(&Val::Int(s.id as i64)) && j.get("state") == Some(&Val::Str("waiting".into())))
    }) {
        assert!(Instant::now() < end, "job does not wait");
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(for_ms));
    assert!(s.signal(SlotState::Go, None));
}

fn last_id() -> u64 {
    limiter::global().jobs().iter().map(|s| s.id).max().unwrap_or(0)
}

fn get(db: &Db, u: u64) -> Option<Val> {
    db.get(&Nat::S(1), &Nat::S(u), NL, None, None, false).unwrap()
}

#[test]
fn limiter_integrity_scenarios() {
    cancel_between_blocks_rolls_the_write_back();
    wait_in_index_build_gives_the_same_index();
    wait_in_compaction_gives_the_same_data();
}

fn cancel_between_blocks_rolls_the_write_back() {
    let p = tmp("cancel_write");
    let mut db = setup(&p);
    put(&mut db, 1, &rows(1, 1000, 100), true).unwrap();
    let before = std::fs::metadata(&p).unwrap().len();
    let after_id = last_id();
    // ~200 MB in one call: blocks of at most 8 MiB, a checkpoint between them
    let h = std::thread::spawn(move || {
        let big = rows(1001, 100_000, 2000);
        let r = put(&mut db, 1001, &big, false);
        (db, r)
    });
    let s = catch(JobPath::Write, after_id);
    hold(&s, 20);
    // the job runs again; cancel it at its next checkpoint
    let s = catch(JobPath::Write, after_id);
    assert!(limiter::global().cancel_job(s.id));
    let (mut db, r) = h.join().unwrap();
    match r {
        Err(Error::ResourceLimit(b)) => assert_eq!(b.0, "cancelled_by_app"),
        other => panic!("expected ResourceLimitError, got {other:?}"),
    }
    assert!(get(&db, 1000).is_some());
    assert!(get(&db, 1001).is_none() && get(&db, 50_000).is_none(), "nothing of the cancelled call is visible");
    ok(&db);
    db.commit().unwrap();
    db.close(true).unwrap();
    let db = Db::open(&p).unwrap();
    ok(&db);
    assert!(get(&db, 1001).is_none());
    assert_eq!(get(&db, 7).and_then(|r| r.get("s").cloned()), Some(Val::Str(format!("{:08}{}", 7, "x".repeat(100)))));
    drop(db);
    let _ = before;
}

fn build(p: &Path, hold_ms: Option<u64>) -> (Db, Val) {
    let mut db = setup(p);
    put(&mut db, 1, &rows(1, 300_000, 20), true).unwrap();
    let after_id = last_id();
    let h = std::thread::spawn(move || {
        let r = db.build_indexes(None, true).unwrap();
        (db, r)
    });
    if let Some(ms) = hold_ms {
        let s = catch(JobPath::IndexBuild, after_id);
        hold(&s, ms);
    }
    h.join().unwrap()
}

fn wait_in_index_build_gives_the_same_index() {
    // equal name lengths: the file stores its name
    let (a, _) = build(&tmp("ib_aaaa"), None);
    let (b, _) = build(&tmp("ib_bbbb"), Some(100));
    ok(&b);
    for k in [0i64, 1, 7919, 50_000, 99_999, 100_002] {
        let ca = a.index_candidates(10, "eq", &Val::Int(k), &Val::None, NL, &Val::None).unwrap();
        let cb = b.index_candidates(10, "eq", &Val::Int(k), &Val::None, NL, &Val::None).unwrap();
        assert_eq!(ca, cb, "key {k}");
    }
    let ra = a.index_candidates(10, "between", &Val::Int(1000), &Val::Int(60_000), NL, &Val::None).unwrap();
    let rb = b.index_candidates(10, "between", &Val::Int(1000), &Val::Int(60_000), NL, &Val::None).unwrap();
    assert_eq!(ra, rb);
    assert!(ra.len() > 100_000);
    let size = |db: &Db| std::fs::metadata(&db.path).unwrap().len();
    assert_eq!(size(&a), size(&b), "same file size after the build");
}

fn wait_in_compaction_gives_the_same_data() {
    let mk = |name: &str| {
        let p = tmp(name);
        let mut db = setup(&p);
        put(&mut db, 1, &rows(1, 200_000, 60), true).unwrap();
        put(&mut db, 50_000, &rows(50_000, 1000, 70), true).unwrap(); // overwritten records
        db.close(true).unwrap();
        p
    };
    let (pa, pb) = (mk("cp_aaaa"), mk("cp_bbbb"));
    let opts = CompactOptions { make_backup: false, ..CompactOptions::default() };
    Db::compact_file(&pa, &opts).unwrap();
    let after_id = last_id();
    let o2 = opts.clone();
    let pb2 = pb.clone();
    let h = std::thread::spawn(move || Db::compact_file(&pb2, &o2).unwrap());
    let s = catch(JobPath::Compact, after_id);
    hold(&s, 100);
    h.join().unwrap();
    let (a, b) = (Db::open(&pa).unwrap(), Db::open(&pb).unwrap());
    ok(&b);
    for u in [1u64, 2, 49_999, 50_000, 50_500, 50_999, 51_000, 123_456, 200_000, 200_001] {
        assert_eq!(get(&a, u), get(&b, u), "u {u}");
    }
    assert_eq!(std::fs::metadata(&pa).unwrap().len(), std::fs::metadata(&pb).unwrap().len());
}
