//! Complexity regression tests (T017): count positional file reads (test
//! counter `storage::FILE_READS`) instead of measuring time. A database
//! with `n` data blocks per realm is built for two sizes; point lookups,
//! sparse cell lookups and indexed searches must need the same number of
//! reads for both sizes, opening and full scans grow at most linearly.

use super::*;
use crate::address::NL;
use crate::codec::FieldDef;
use crate::db::dense::DenseRows;
use crate::db::index::Cond;
use crate::db::sparse::CellInput;
use crate::storage::FILE_READS;

const ROWS_PER_BLOCK: u64 = 10;

fn reads() -> (u64, u64) {
    FILE_READS.with(|c| c.get())
}

fn reset() {
    FILE_READS.with(|c| c.set((0, 0)));
}

/// Reads (count, bytes) caused by `f`.
fn count<T>(f: impl FnOnce() -> T) -> ((u64, u64), T) {
    reset();
    let out = f();
    (reads(), out)
}

fn build(blocks: u64) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nhydb_cx_{blocks}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join("cx.nhydb");
    let r = Nat::S(1);
    let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
    db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
    db.create_shape(
        1,
        &r,
        "s",
        vec![FieldDef::new(Nat::S(1), "k", TypeCode::VarUInt), FieldDef::new(Nat::S(2), "s", TypeCode::Utf8)],
        "",
    )
    .unwrap();
    db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
    db.create_realm(&Nat::S(2), "sp", "", None, &Nat::ZERO).unwrap();
    db.create_index(10, &r, "k", &Val::Int(1), "").unwrap();
    for b in 0..blocks {
        let u0 = b * ROWS_PER_BLOCK + 1;
        let rows: Vec<Vec<Val>> =
            (u0..u0 + ROWS_PER_BLOCK).map(|u| vec![Val::Int((u % 97) as i64), Val::Str(format!("row {u}"))]).collect();
        db.put_dense_batch(&r, &Nat::S(u0), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
        let cells = (u0..u0 + ROWS_PER_BLOCK)
            .map(|u| CellInput::typed(Nat::S(u), Nat::S(1), Val::Str(format!("cell {u}")), Some(TypeCode::Utf8)))
            .collect();
        db.put_points(&Nat::S(2), cells, NL, true, 10_000).unwrap();
    }
    db.build_indexes(None, true).unwrap();
    db.close(true).unwrap();
    p
}

struct Measured {
    blocks: u64,
    open: (u64, u64),
    get: (u64, u64),
    get_again: (u64, u64),
    cell: (u64, u64),
    find: (u64, u64),
    find_again: (u64, u64),
    range: (u64, u64),
}

fn measure(blocks: u64) -> Measured {
    let p = build(blocks);
    let (open, mut db) = count(|| Db::open(&p).unwrap());
    let mid = Nat::S(blocks / 2 * ROWS_PER_BLOCK + 3);
    let (get, rec) = count(|| db.get(&Nat::S(1), &mid, NL, None, None, true).unwrap());
    assert!(rec.is_some());
    let (get_again, _) = count(|| db.get(&Nat::S(1), &mid, NL, None, None, true).unwrap());
    let (cell, c) = count(|| db.get_cell(&Nat::S(2), &mid, &Val::Int(1), NL, false).unwrap());
    assert!(c.is_some());
    let filt = vec![("k".to_string(), Cond::Eq(Val::Int(5)))];
    let (find, us) = count(|| db.find_u(&Nat::S(1), &filt, NL, None, None).unwrap());
    assert!(!us.is_empty());
    let (find_again, _) = count(|| db.find_u(&Nat::S(1), &filt, NL, None, None).unwrap());
    let last = Nat::S(blocks * ROWS_PER_BLOCK);
    db.close(false).unwrap();
    let db2 = Db::open(&p).unwrap();
    let (range, rows) = count(|| db2.range(&Nat::S(1), &Nat::S(1), &last, NL, None, None, false).unwrap());
    assert_eq!(rows.len() as u64, blocks * ROWS_PER_BLOCK);
    let _ = std::fs::remove_dir_all(p.parent().unwrap());
    Measured { blocks, open, get, get_again, cell, find, find_again, range }
}

#[test]
fn read_counts_scale_as_expected() {
    let small = measure(8);
    let large = measure(64);
    for m in [&small, &large] {
        eprintln!(
            "blocks={} open={:?} get={:?} get_again={:?} cell={:?} find={:?} find_again={:?} range={:?}",
            m.blocks, m.open, m.get, m.get_again, m.cell, m.find, m.find_again, m.range
        );
    }
    // point lookups: constant number of reads, independent of the size
    assert_eq!(small.get.0, large.get.0, "dense point lookup must not depend on the number of blocks");
    assert!(large.get.0 <= 2, "dense point lookup: {:?}", large.get);
    assert_eq!(large.get_again.0, 0, "repeated lookup is served from the cache");
    assert_eq!(small.cell.0, large.cell.0, "sparse cell lookup must not depend on the number of blocks");
    assert!(large.cell.0 <= 2, "sparse cell lookup: {:?}", large.cell);
    // indexed search: directory + posting, independent of the number of data blocks
    assert_eq!(small.find.0, large.find.0, "indexed search must not scan data blocks");
    assert!(large.find.0 <= 3, "indexed search: {:?}", large.find);
    assert_eq!(large.find_again.0, 0, "repeated indexed search is served from the cache");
    // opening: one scan, at most a few reads per block (header + plausibility)
    let per_block = |m: &Measured| m.open.0 as f64 / (2 * m.blocks) as f64;
    assert!(
        large.open.0 - small.open.0 <= 3 * 2 * (large.blocks - small.blocks),
        "open grows faster than linearly: {:?} vs {:?}",
        small.open,
        large.open
    );
    assert!(per_block(&large) <= 3.0, "open: {} reads per block", per_block(&large));
    // P015: small consecutive blocks share one read-ahead window
    assert!(per_block(&large) <= 0.25, "open of small blocks: {} reads per block", per_block(&large));
    // full range scan: each data block loaded once (header + payload), no re-reads per record
    assert!(large.range.0 <= 2 * large.blocks + 2, "range scan reads blocks more than once: {:?}", large.range);
    assert!(large.range.1 <= 2 * small.range.1 * large.blocks / small.blocks, "range scan bytes: {:?}", large.range);
}
