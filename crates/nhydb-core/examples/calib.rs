//! RUN8 AP0: calibration of the memory estimates against the real Rust heap
//! (counting global allocator of this binary; no Python).
//!
//! `cargo run --release -p nhydb-core --example calib -- <dir> [docs]`
//!
//! 1. `build_indexes` (FULLTEXT + VALUE) with several `[index] build_memory_bytes`:
//!    peak heap during the build against the budget.
//! 2. collected `sql()` results: the `ResultGuard` estimate
//!    (`size_of::<Val>() + val_heap(row)` per row) against the heap the
//!    result really holds.
//!    Output: JSON lines.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use nhydb_core::db::{val_heap, Db, OpenOptions};
use nhydb_core::query::{Providers, QueryEngine};
use nhydb_core::sql::{execute_sql, Params};
use nhydb_core::*;

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let n = NOW.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(n, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        NOW.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let n = NOW.fetch_add(new - l.size(), Ordering::Relaxed) + new - l.size();
                PEAK.fetch_max(n, Ordering::Relaxed);
            } else {
                NOW.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

fn mib(x: usize) -> f64 {
    x as f64 / 1048576.0
}

fn make(path: &std::path::Path, docs: usize, budget: usize) -> Db {
    let cfg = Config { index_build_memory_bytes: budget, ..Config::default() };
    let mut db = Db::create(path, true, OpenOptions { config: Some(cfg), ..Default::default() }).unwrap();
    let r = Nat::S(1);
    db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
    db.create_shape(
        1,
        &r,
        "s",
        vec![FieldDef::new(Nat::S(1), "a", TypeCode::VarInt), FieldDef::new(Nat::S(2), "t", TypeCode::Utf8)],
        "",
    )
    .unwrap();
    db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
    let words: Vec<String> = (0..5000).map(|i| format!("w{i}")).collect();
    let mut u = 1usize;
    while u <= docs {
        let n = 10_000.min(docs + 1 - u);
        let rows: Vec<Vec<Val>> = (u..u + n)
            .map(|k| {
                let text: Vec<&str> = (0..20).map(|j| words[(k * 31 + j * 977) % words.len()].as_str()).chain(["common"]).collect();
                vec![Val::Int((k % 7) as i64), Val::str(text.join(" "))]
            })
            .collect();
        db.put_dense_batch(&r, &Nat::S(u as u64), &DenseRows::Tuples(&rows), Some(1), &[], NL, false).unwrap();
        u += n;
    }
    db.create_index_ext(1, &r, "ft", &Val::str("t"), &Val::str("FULLTEXT"), &Val::None, None, None, None, "").unwrap();
    db.create_index(2, &r, "ia", &Val::str("a"), "").unwrap();
    db.commit().unwrap();
    db
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&a[1]);
    let docs: usize = a.get(2).map(|s| s.parse().unwrap()).unwrap_or(400_000);
    std::fs::create_dir_all(&dir).unwrap();
    for budget in [8usize << 20, 64 << 20, 512 << 20] {
        let p = dir.join(format!("calib_{budget}.nhydb"));
        let mut db = make(&p, docs, budget);
        let base = NOW.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let r = db.build_indexes(None, true).unwrap();
        let peak = PEAK.load(Ordering::Relaxed) - base;
        println!(
            "{{\"case\": \"build_indexes\", \"docs\": {docs}, \"budget_mib\": {:.0}, \"peak_heap_mib\": {:.1}, \"factor\": {:.2}, \"spill_rounds\": {}}}",
            mib(budget),
            mib(peak),
            peak as f64 / budget as f64,
            r.get("spill_rounds").and_then(|v| v.as_i64()).unwrap_or(-1)
        );
        if budget == 64 << 20 {
            let prov = Providers::default();
            let eng = QueryEngine::new(&db, &prov);
            for (sql, label) in [("SELECT a, t FROM c WHERE a = 3", "fields_2cols"), ("SELECT a FROM c", "fields_1col_int")] {
                let res = execute_sql(&eng, sql, &Params::None, NL).unwrap();
                let rows = res.get("rows").and_then(|r| r.as_list()).unwrap();
                let est: usize = rows.iter().map(|row| std::mem::size_of::<Val>() + val_heap(row)).sum();
                let n_rows = rows.len();
                // the heap the result itself holds: freed when it is dropped
                // (caches filled by the query stay and are not counted)
                let with = NOW.load(Ordering::Relaxed);
                drop(res);
                let held = with - NOW.load(Ordering::Relaxed);
                println!(
                    "{{\"case\": \"sql_collect\", \"query\": \"{label}\", \"rows\": {}, \"guard_estimate_mib\": {:.1}, \"held_heap_mib\": {:.1}, \"factor\": {:.2}}}",
                    n_rows,
                    mib(est),
                    mib(held),
                    held as f64 / est.max(1) as f64
                );
            }
        }
        drop(db);
        for s in ["", ".layout.enc", ".layout.key", ".lock"] {
            let _ = std::fs::remove_file(format!("{}{s}", p.display()));
        }
    }
}
