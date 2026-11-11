//! RUN8 AP0 (A7 analysis): read throughput of N threads on ONE handle against
//! N threads with one handle each, without Python (no GIL, no conversion).
//!
//! `cargo run --release -p nhydb-core --example a7_scan [--features lock_stats] -- <db> <work> <threads> <shared|own> [seconds]`
//!
//! work: `scan` – `SELECT COUNT(*) AS n FROM KQ WHERE gp = :x` (no index)
//!       `idx`  – `SELECT * FROM KQ WHERE tq = :x` (VALUE index, ~2 000 rows)
//!       `get`  – `get(1, u)` with random u
//! Output: one JSON line (operations/s; with `lock_stats` also cache-lock
//! acquisitions, contended acquisitions and wait time per operation).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nhydb_core::db::{Db, OpenOptions};
use nhydb_core::query::{Providers, QueryEngine};
use nhydb_core::sql::{execute_sql, Params};
use nhydb_core::*;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (path, work, threads, mode) = (a[1].clone(), a[2].clone(), a[3].parse::<usize>().unwrap(), a[4].clone());
    let secs: f64 = a.get(5).map(|s| s.parse().unwrap()).unwrap_or(8.0);
    let open = || Db::open_with(&path, OpenOptions { readonly: true, ..Default::default() }).unwrap();
    let handles: Vec<Arc<Db>> = (0..if mode == "own" { threads } else { 1 }).map(|_| Arc::new(open())).collect();
    let ops = Arc::new(AtomicU64::new(0));
    #[cfg(feature = "lock_stats")]
    let _ = nhydb_core::db::mempot::lock_stats::take();
    let stop = Instant::now() + Duration::from_secs_f64(secs);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for i in 0..threads {
            let db = handles[if mode == "own" { i } else { 0 }].clone();
            let (ops, work) = (ops.clone(), work.clone());
            s.spawn(move || {
                let prov = Providers::default();
                let eng = QueryEngine::new(&db, &prov);
                let mut rng = Rng(1000 + i as u64);
                let mut n = 0u64;
                while Instant::now() < stop {
                    match work.as_str() {
                        "scan" => {
                            let p = Params::Map(vec![("x".into(), Val::Int(1 + rng.below(999) as i64))]);
                            execute_sql(&eng, "SELECT COUNT(*) AS n FROM KQ WHERE gp = :x", &p, NL).unwrap();
                        }
                        "idx" => {
                            let p = Params::Map(vec![("x".into(), Val::Int(100 + rng.below(2900) as i64))]);
                            execute_sql(&eng, "SELECT * FROM KQ WHERE tq = :x", &p, NL).unwrap();
                        }
                        _ => {
                            db.get(&Nat::S(1), &Nat::S(1 + rng.below(9_999_999)), NL, None, None, true).unwrap();
                        }
                    }
                    n += 1;
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
    });
    let wall = t0.elapsed().as_secs_f64();
    let n = ops.load(Ordering::Relaxed);
    #[allow(unused_mut)]
    let mut extra = String::new();
    #[cfg(feature = "lock_stats")]
    {
        let (acq, cont, wait) = nhydb_core::db::mempot::lock_stats::take();
        let per = |x: u64| x as f64 / n.max(1) as f64;
        extra = format!(
            ", \"locks_per_op\": {:.1}, \"contended_per_op\": {:.1}, \"contended_share\": {:.3}, \"wait_ms_per_op\": {:.3}, \"wait_share_of_thread_time\": {:.3}",
            per(acq),
            per(cont),
            cont as f64 / acq.max(1) as f64,
            per(wait) / 1e6,
            wait as f64 / 1e9 / (wall * threads as f64)
        );
    }
    println!(
        "{{\"work\": \"{work}\", \"threads\": {threads}, \"mode\": \"{mode}\", \"s\": {wall:.2}, \"ops\": {n}, \"ops_per_s\": {:.1}{extra}}}",
        n as f64 / wall
    );
}
