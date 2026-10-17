//! MATH-Benchmark im Kern: Zielberechnung und `put_math_batch` ohne Python.
//! Aufruf: cargo run --release --example bench_math -- [rows] [dir]
use std::time::Instant;

use nhydb_core::db::{Db, OpenOptions};
use nhydb_core::mathexpr::{MathMap, MathMapLayoutSpec};
use nhydb_core::{address, Val};
use num_bigint::BigInt;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let dir = args.get(2).cloned().unwrap_or_else(|| std::env::temp_dir().join("nhydb_bench_math").to_string_lossy().into());
    std::fs::create_dir_all(&dir).unwrap();
    let span = address::realm_span().to_bigint();
    let m = MathMap::compile("bench", &["x".into(), "k".into()], "x // 1000 + 1", "k*10**100 + x % 1000 + 1", &Val::str("x % 2"), &span).unwrap();
    let t = Instant::now();
    let mut acc = 0u64;
    for x in 0..n {
        let tg = m.target(&[Val::Int(x), Val::Int(x % 8)], 1 << 20).unwrap();
        acc += tg.side as u64;
    }
    let dt = t.elapsed().as_secs_f64();
    println!("target            {:>10.1} k/s  ({acc})", n as f64 / dt / 1e3);

    let p = std::path::Path::new(&dir).join("m.nhydb");
    for s in ["", ".layout.enc", ".layout.key", ".lock"] {
        let _ = std::fs::remove_file(format!("{}{s}", p.display()));
    }
    let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
    let spec = MathMapLayoutSpec::new("bench", m, BigInt::from(64), "").unwrap();
    db.create_math_layout(spec, Some(1), "varint", false).unwrap();
    let rows: Vec<(Vec<Val>, Val)> = (0..n).map(|x| (vec![Val::Int(x), Val::Int(x % 8)], Val::Int(x))).collect();
    let t = Instant::now();
    db.put_math_batch(1, rows, false, true).unwrap();
    let dt = t.elapsed().as_secs_f64();
    println!("put_math_batch    {:>10.1} k/s", n as f64 / dt / 1e3);
    let t = Instant::now();
    for x in (0..n).step_by(10) {
        let v = db.get_math(1, &[Val::Int(x), Val::Int(x % 8)]).unwrap();
        assert_eq!(v, Val::Int(x));
    }
    let dt = t.elapsed().as_secs_f64();
    println!("get_math          {:>10.1} k/s", (n / 10) as f64 / dt / 1e3);
}
