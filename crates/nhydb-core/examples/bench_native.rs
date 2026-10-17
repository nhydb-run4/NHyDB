//! Pure Rust benchmark of the engine (without Python): dense KQ records with
//! optional reserve, extension definitions and sparse extension cells.
//!
//! `cargo run --release -p nhydb-core --example bench_native [records] [dir]`

use std::time::Instant;

use nhydb_core::SyncMode;
use nhydb_core::*;

const REALM: u64 = 500;
const BATCH: usize = 10_000;
const COMMIT_EVERY: usize = 10;

fn isqrt(x: i64) -> i64 {
    let mut r = (x as f64).sqrt() as i64;
    while r * r > x {
        r -= 1;
    }
    while (r + 1) * (r + 1) <= x {
        r += 1;
    }
    r
}

struct Cols {
    v: [Vec<i64>; 11],
    lage: Vec<u8>,
}

fn kq(n0: i64, count: usize) -> Cols {
    let mut c = Cols { v: Default::default(), lage: Vec::with_capacity(count) };
    for k in 0..count {
        let n = n0 + k as i64;
        let tq = (isqrt(4 * n + 1) + 1) / 2;
        let tp = tq - 1;
        let gp = n - tq * tp;
        let sp = (gp - tq).abs();
        let lage = if gp == 0 { 0 } else if gp < tq { 1 } else if gp == tq { 2 } else { 3 };
        let g2 = tq * tq;
        let dyn_v = if n <= g2 { n - tq * (tq - 1) } else { n - g2 };
        let dyn_u = tq - dyn_v;
        let station = dyn_v - 1;
        let width = dyn_u + 2;
        let row = [n, tp, tq, gp, sp, 0, dyn_u, dyn_v, station.div_euclid(width) + 1, station, station.rem_euclid(width)];
        for (i, x) in row.iter().enumerate() {
            c.v[i].push(*x);
        }
        c.lage.push(lage);
    }
    c
}

fn setup(path: &std::path::Path, reserve: u64, define: bool, mode: SyncMode) -> Db {
    let opts = OpenOptions { lock: Some(nhydb_core::LockConfig::default()), ..OpenOptions::default() }.with_sync(mode);
    let mut db = Db::create(path, true, opts).unwrap();
    let r = Nat::S(REALM);
    db.create_realm(&r, "bench", "", None, &Nat::ZERO).unwrap();
    let names = ["N", "tp", "tq", "gp", "sp", "lage", "dyn_u", "dyn_v", "stufe", "station", "stufenstation"];
    let fields: Vec<FieldDef> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let t = match *n {
                "lage" => TypeCode::U8,
                "station" => TypeCode::VarInt,
                _ => TypeCode::VarUInt,
            };
            let mut f = FieldDef::new(Nat::S(i as u64 + 1), *n, t);
            if *n == "lage" {
                f.enum_map = Some(vec![("N0".into(), 0), ("NL".into(), 1), ("Gz".into(), 2), ("NR".into(), 3)]);
            }
            f
        })
        .collect();
    db.create_shape(1, &r, "KQ", fields, "").unwrap();
    db.create_classic_layout(1, &r, "KQ", 1, &Nat::S(reserve), &Nat::ONE, None, 0, "").unwrap();
    if define {
        for (v, n) in [(Nat::S(1001), "e1"), (Nat::S(5000), "e2"), (Nat::S(1_000_000), "e3"), (Nat::S(1_000_000_000_000), "e4"), (address::realm_span().clone(), "e5")] {
            db.define_extension(&r, &v, "varuint", n, &Val::None, "", false).unwrap();
        }
    }
    db
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let total: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let dir = args.get(2).map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let mode = SyncMode::parse(args.get(3).map(String::as_str).unwrap_or("fsync")).unwrap();
    println!(
        "NHyDB native Rust benchmark: {total} records, batch {BATCH}, commit every {COMMIT_EVERY} batches, durability={}",
        mode.name()
    );
    println!("{:<20}{:>14}{:>10}{:>14}{:>12}", "Scenario", "bytes", "write s", "rec/s", "verify s");
    for (name, reserve, define, populate) in [
        ("BASE_ONLY", 0u64, false, false),
        ("RESERVE_ONLY", 1000, false, false),
        ("DEFINITIONS_EMPTY", 1000, true, false),
        ("SPARSE_MIX", 1000, true, true),
    ] {
        let path = dir.join(format!("bench_native_{}.nhydb", name.to_lowercase()));
        let mut db = setup(&path, reserve, define, mode);
        let r = Nat::S(REALM);
        let mut write = 0.0;
        let mut done = 0usize;
        let mut batches = 0usize;
        while done < total {
            let count = BATCH.min(total - done);
            let c = kq(done as i64 + 1, count);
            let t = Instant::now();
            let cols: Vec<ColData> = (0..11)
                .map(|i| if i == 5 { ColData::U8(&c.lage) } else { ColData::I64(&c.v[i]) })
                .collect();
            db.put_dense_batch(&r, &Nat::S(done as u64 + 1), &DenseRows::Columns { cols: &cols, count }, None, &[], NL, false)
                .unwrap();
            if populate {
                let mut cells = Vec::new();
                for k in 0..count as u64 {
                    let u = done as u64 + 1 + k;
                    for (v, hit) in [(1001u64, u.is_multiple_of(100)), (5000, u.is_multiple_of(10)), (1_000_000, u.is_multiple_of(2)), (1_000_000_000_000, u % 10 == 5)] {
                        if hit {
                            cells.push(CellInput::new(Nat::S(u), Nat::S(v), Val::int(u)));
                        }
                    }
                }
                db.put_points(&r, cells, NL, false, 10_000).unwrap();
            }
            batches += 1;
            if batches.is_multiple_of(COMMIT_EVERY) {
                db.commit().unwrap();
            }
            write += t.elapsed().as_secs_f64();
            done += count;
        }
        let t = Instant::now();
        db.commit().unwrap();
        write += t.elapsed().as_secs_f64();
        let bytes = db.file_len();
        db.close(true).unwrap();
        let db = Db::open(&path).unwrap();
        let t = Instant::now();
        let v = db.verify(false).unwrap();
        let vs = t.elapsed().as_secs_f64();
        assert_eq!(v.get("ok"), Some(&Val::Bool(true)));
        println!("{name:<20}{bytes:>14}{write:>10.3}{:>14.0}{vs:>12.3}", total as f64 / write);
    }
}
