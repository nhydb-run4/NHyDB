//! Tests of the block directory (0.17) and of the memory-bounded sparse
//! merge / compaction: a database written with tiny directory thresholds
//! (many flushes and merges) must answer every read exactly like a twin
//! written without directory – before and after reopen, rollback,
//! compaction – and both must match a model of the sparse cell states.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::address::{NL, NR};
use crate::codec::FieldDef;
use crate::db::dense::DenseRows;
use crate::db::maint::CompactOptions;
use crate::db::sparse::CellInput;
use crate::db::directory::{damaged_directory_extent, root_run_offsets, DirCodec, ROOT_TAG};
use crate::db::segindex::PageSource;
use crate::nat::put_vu64;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nhydb_dir_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join(format!("{name}.nhydb"))
}

fn opts(flush_blocks: u64) -> OpenOptions {
    let cfg = Config { directory_flush_blocks: flush_blocks, directory_cache_entries: 300, ..Config::default() };
    OpenOptions { config: Some(cfg), ..OpenOptions::default() }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const R_DENSE: u64 = 1;
const R_SPARSE: u64 = 2;

fn setup(p: &Path, o: OpenOptions) -> Db {
    let mut db = Db::create(p, true, o).unwrap();
    let r = Nat::S(R_DENSE);
    db.create_realm(&r, "dense", "", None, &Nat::ZERO).unwrap();
    db.create_shape(1, &r, "s", vec![FieldDef::new(Nat::S(1), "k", TypeCode::VarUInt), FieldDef::new(Nat::S(2), "s", TypeCode::Utf8)], "").unwrap();
    db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
    db.define_extension(&r, &Nat::S(50), "utf8", "note", &Val::None, "", false).unwrap();
    db.create_index(10, &r, "k", &Val::Int(1), "").unwrap();
    db.create_realm(&Nat::S(R_SPARSE), "sparse", "", None, &Nat::ZERO).unwrap();
    db.commit().unwrap();
    db
}

/// Model of the sparse cell states written to realm 2: `None` = tombstone.
type Model = BTreeMap<(u64, Nat, Nat), Option<Val>>;

fn big_u(i: u64) -> Nat {
    Nat::from(num_bigint::BigUint::from(10u32).pow(40) + num_bigint::BigUint::from(i))
}

/// Applies one random operation to both databases (and the model).
fn random_op(rng: &mut Rng, dbs: &mut [&mut Db], model: &mut Model) {
    random_op_c(rng, dbs, model, true)
}

fn random_op_c(rng: &mut Rng, dbs: &mut [&mut Db], model: &mut Model, allow_commit: bool) {
    let lane = if rng.below(4) == 0 { NR } else { NL };
    let pick = if allow_commit { rng.below(100) } else { rng.below(92) };
    match pick {
        0..=24 => {
            let u0 = rng.below(300) + 1;
            let n = rng.below(20) + 1;
            let rows: Vec<Vec<Val>> = (0..n).map(|i| vec![Val::Int(((u0 + i) % 13) as i64), Val::str(format!("r{u0}-{i}-{}", rng.next()))]).collect();
            for db in dbs.iter_mut() {
                db.put_dense_batch(&Nat::S(R_DENSE), &Nat::S(u0), &DenseRows::Tuples(&rows), Some(1), &[], lane, false).unwrap();
            }
        }
        25..=59 => {
            let k = rng.below(30) + 1;
            let mut cells = Vec::new();
            for _ in 0..k {
                let u = if rng.below(10) == 0 { big_u(rng.below(5)) } else { Nat::S(rng.below(400) + 1) };
                let v = Nat::S(rng.below(30) + 1);
                let val = if rng.below(50) == 0 {
                    Val::str("x".repeat(70_000 + rng.below(3000) as usize))
                } else if rng.below(2) == 0 {
                    Val::Int(rng.next() as i64)
                } else {
                    Val::str(format!("v{}", rng.next()))
                };
                let typ = if matches!(val, Val::Int(_)) { TypeCode::I64 } else { TypeCode::Utf8 };
                cells.push(CellInput::typed(u, v, val, Some(typ)));
            }
            // duplicate (u, v) in one call: the last one is kept
            let mut dedup: BTreeMap<(Nat, Nat), CellInput> = BTreeMap::new();
            for c in cells {
                dedup.insert((c.u.clone(), c.v.clone()), c);
            }
            let cells: Vec<CellInput> = dedup.into_values().collect();
            for c in &cells {
                model.insert((lane as u64, c.u.clone(), c.v.clone()), Some(c.value.clone()));
            }
            for db in dbs.iter_mut() {
                db.put_points(&Nat::S(R_SPARSE), cells.clone(), lane, false, 64).unwrap();
            }
        }
        60..=69 => {
            let keys: Vec<(u64, Nat, Nat)> = model.keys().filter(|k| k.0 == lane as u64).cloned().collect();
            let (u, v) = if keys.is_empty() || rng.below(5) == 0 {
                (Nat::S(rng.below(400) + 1), Nat::S(rng.below(30) + 1))
            } else {
                let k = &keys[rng.below(keys.len() as u64) as usize];
                (k.1.clone(), k.2.clone())
            };
            model.insert((lane as u64, u.clone(), v.clone()), None);
            for db in dbs.iter_mut() {
                db.delete_point(&Nat::S(R_SPARSE), &u, &v, lane, false).unwrap();
            }
        }
        70..=79 => {
            // overlay of a BASE field or extension of a dense record
            let u = Nat::S(rng.below(320) + 1);
            let cell = if rng.below(2) == 0 {
                CellInput::new(u, Nat::S(2), Val::str(format!("ov{}", rng.next())))
            } else {
                CellInput::new(u, Nat::S(50), Val::str(format!("ext{}", rng.next())))
            };
            for db in dbs.iter_mut() {
                db.put_points(&Nat::S(R_DENSE), vec![cell.clone()], lane, false, 64).unwrap();
            }
        }
        80..=85 => {
            let u = Nat::S(rng.below(320) + 1);
            let v = Nat::S(if rng.below(2) == 0 { 2 } else { 50 });
            for db in dbs.iter_mut() {
                db.delete_point(&Nat::S(R_DENSE), &u, &v, lane, false).unwrap();
            }
        }
        86..=91 => {
            let u = Nat::S(rng.below(320) + 1);
            for db in dbs.iter_mut() {
                db.delete(&Nat::S(R_DENSE), &u, lane, false).unwrap();
            }
        }
        _ => {
            for db in dbs.iter_mut() {
                db.commit().unwrap();
            }
        }
    }
}

/// Every observable read of the two realms as text.
fn snapshot(db: &Db) -> Vec<String> {
    let mut out = Vec::new();
    for lane in [NL, NR] {
        for u in 1..=330u64 {
            out.push(format!("D{lane}/{u}: {:?}", db.get_internal(&Nat::S(R_DENSE), &Nat::S(u), lane).unwrap().map(|r| (r.values, r.extensions_by_v))));
        }
        let mut next = Nat::ZERO;
        let mut us = Vec::new();
        while let Some(u) = db.next_dense_u(&Nat::S(R_DENSE), lane, &next).unwrap() {
            // raw coverage includes deleted records (dropped by compaction)
            if db.resolve_segment(&Nat::S(R_DENSE), &u, lane).unwrap().is_some() {
                us.push(u.clone());
            }
            next = u.add_u64(1);
        }
        out.push(format!("next_dense_u {lane}: {us:?}"));
        for u in [Nat::S(1), Nat::S(17), Nat::S(399), big_u(0), big_u(3)] {
            for v in 1..=31u64 {
                out.push(format!("S{lane}/{u}/{v}: {:?}", db.get_cell_value(&Nat::S(R_SPARSE), &u, &Nat::S(v), lane).unwrap()));
            }
        }
    }
    for p in db.iter_materialized_sparse_points(None, None).unwrap() {
        out.push(format!("P {:?}", p));
    }
    out
}

/// Equality of two snapshots, reporting the first difference.
fn same(a: &[String], b: &[String], what: &str) {
    for (x, y) in a.iter().zip(b.iter()) {
        if x != y {
            panic!("{what}: first difference\n  got:      {}\n  expected: {}", &x[..x.len().min(400)], &y[..y.len().min(400)]);
        }
    }
    assert_eq!(a.len(), b.len(), "{what}: length");
}

fn model_points(model: &Model) -> Vec<(u64, Nat, Nat, Val)> {
    model.iter().filter_map(|((l, u, v), x)| x.as_ref().map(|x| (*l, u.clone(), v.clone(), x.clone()))).collect()
}

fn db_points(db: &Db) -> Vec<(u64, Nat, Nat, Val)> {
    db.iter_materialized_sparse_points(Some(&Nat::S(R_SPARSE)), None)
        .unwrap()
        .into_iter()
        .map(|p| (p.lane as u64, p.u, p.v, p.value))
        .collect()
}

fn assert_verify_ok(db: &Db) {
    let v = db.verify(true).unwrap();
    assert!(v.get("ok").unwrap().is_truthy(), "verify: {}", v.py_repr());
}

#[test]
fn directory_twin_equivalence_with_reopen_rollback_and_compaction() {
    for seed in [1u64, 7, 42] {
        let pa = tmp(&format!("twin_a_{seed}"));
        let pb = tmp(&format!("twin_b_{seed}"));
        let mut a = setup(&pa, opts(5));
        let mut b = setup(&pb, opts(u64::MAX));
        let mut model = Model::new();
        let mut rng = Rng(seed);
        for _ in 0..500 {
            random_op(&mut rng, &mut [&mut a, &mut b], &mut model);
        }
        a.commit().unwrap();
        b.commit().unwrap();
        let st = a.directory_stats();
        assert!(st.get("flushes").unwrap().as_i64().unwrap() > 10, "{}", st.py_repr());
        assert!(st.get("merges").unwrap().as_i64().unwrap() > 0, "{}", st.py_repr());
        assert_eq!(b.directory_stats().get("flushes").unwrap().as_i64(), Some(0));
        let sa = snapshot(&a);
        same(&sa, &snapshot(&b), &format!("seed {seed}: directory vs full scan (same handle)"));
        assert_eq!(db_points(&a), model_points(&model), "seed {seed}: merge vs model");
        assert_verify_ok(&a);
        // index built over directory pages
        a.build_indexes(None, true).unwrap();
        b.build_indexes(None, true).unwrap();
        for k in 0..13 {
            let f = vec![("k".to_string(), crate::db::index::Cond::Eq(Val::Int(k)))];
            assert_eq!(a.find_u(&Nat::S(R_DENSE), &f, NL, None, None).unwrap(), b.find_u(&Nat::S(R_DENSE), &f, NL, None, None).unwrap());
        }
        // uncommitted writes + rollback restore the committed directory state
        let before = snapshot(&a);
        let mut scratch = Model::new();
        for _ in 0..40 {
            random_op_c(&mut rng, &mut [&mut a], &mut scratch, false);
        }
        a.rollback().unwrap();
        same(&snapshot(&a), &before, &format!("seed {seed}: rollback"));
        a.close(true).unwrap();
        b.close(true).unwrap();
        // reopen: directory loaded, only the tail is scanned
        let a = Db::open_with(&pa, opts(5)).unwrap();
        assert!(a.directory_stats().get("loaded").unwrap().is_truthy());
        assert!(a.open_warnings.is_empty(), "{:?}", a.open_warnings);
        let b = Db::open_with(&pb, opts(u64::MAX)).unwrap();
        same(&snapshot(&a), &sa, &format!("seed {seed}: after reopen"));
        assert_eq!(snapshot(&b), sa);
        assert_verify_ok(&a);
        drop(a);
        drop(b);
        // compaction (memory-bounded merge) keeps the logical state
        for p in [&pa, &pb] {
            let o = if p == &pa { opts(5) } else { opts(u64::MAX) };
            Db::compact_file(p, &CompactOptions { make_backup: false, open: o, ..Default::default() }).unwrap();
        }
        let a = Db::open_with(&pa, opts(5)).unwrap();
        let b = Db::open_with(&pb, opts(u64::MAX)).unwrap();
        same(&snapshot(&a), &sa, &format!("seed {seed}: after compaction (dir)"));
        same(&snapshot(&b), &sa, &format!("seed {seed}: after compaction (no dir)"));
        assert_eq!(db_points(&a), model_points(&model));
        // the compacted file opens from its directory (one run)
        let st = a.directory_stats();
        assert!(st.get("loaded").unwrap().is_truthy());
        assert_eq!(st.get("runs").unwrap().as_i64(), Some(1));
        assert_eq!(st.get("tail_entries").unwrap().as_i64(), Some(0));
        assert_verify_ok(&a);
    }
}

#[test]
fn readonly_handle_and_catch_up_with_directory() {
    let p = tmp("ro");
    let mut db = setup(&p, opts(3));
    let rows: Vec<Vec<Val>> = (0..4).map(|i| vec![Val::Int(i), Val::str(format!("a{i}"))]).collect();
    for i in 0..40u64 {
        db.put_dense_batch(&Nat::S(R_DENSE), &Nat::S(i * 4 + 1), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
    }
    let ro = Db::open_with(&p, OpenOptions { readonly: true, ..opts(3) }).unwrap();
    assert!(ro.directory_stats().get("loaded").unwrap().is_truthy());
    assert_eq!(snapshot(&ro), snapshot(&db));
    // writer continues (more flushes); the read-only handle keeps its view
    let view = snapshot(&ro);
    for i in 40..80u64 {
        db.put_dense_batch(&Nat::S(R_DENSE), &Nat::S(i * 4 + 1), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
    }
    assert_eq!(snapshot(&ro), view);
    let ro2 = Db::open_with(&p, OpenOptions { readonly: true, ..opts(3) }).unwrap();
    assert_eq!(snapshot(&ro2), snapshot(&db));
}

/// Crash after the directory blocks of a commit were written, before its
/// superblock: the previous directory stays in force, the tail is dropped.
#[test]
fn crash_between_directory_and_superblock() {
    let p = tmp("crash");
    let mut db = setup(&p, opts(4));
    let mut model = Model::new();
    let mut rng = Rng(5);
    for _ in 0..120 {
        random_op(&mut rng, &mut [&mut db], &mut model);
    }
    db.commit().unwrap();
    let committed = snapshot(&db);
    let state_n = std::fs::read(&p).unwrap();
    let root_n = db.dir.root_off;
    // next commit with a directory flush
    for _ in 0..60 {
        random_op(&mut rng, &mut [&mut db], &mut Model::new());
    }
    db.commit().unwrap();
    assert_ne!(db.dir.root_off, root_n);
    db.close(true).unwrap();
    // crash image: new blocks (data + directory) but superblocks of commit N
    let mut img = std::fs::read(&p).unwrap();
    img[..DATA_START as usize].copy_from_slice(&state_n[..DATA_START as usize]);
    std::fs::write(&p, &img).unwrap();
    let db = Db::open_with(&p, opts(4)).unwrap();
    assert!(db.recovered_tail_bytes > 0);
    assert_eq!(db.dir.root_off, root_n);
    assert_eq!(snapshot(&db), committed);
    assert_verify_ok(&db);
}

/// Damaged directory blocks never make damaged or stale data count as valid:
/// a damaged root/run falls back to the full scan; a damaged page raises
/// `CorruptionError` on use; repair drops the directory without cutting data.
#[test]
fn damaged_directory_blocks() {
    let p = tmp("dmg");
    let mut db = setup(&p, opts(4));
    let mut rng = Rng(9);
    let mut model = Model::new();
    for _ in 0..200 {
        random_op(&mut rng, &mut [&mut db], &mut model);
    }
    db.checkpoint().unwrap();
    let good = snapshot(&db);
    let root = db.dir.root_off;
    let run = db.dir.runs[0].off;
    let page = db.dense[&(Nat::S(R_DENSE), NL)].idx.runs[0].pages[0].off;
    db.close(true).unwrap();
    let clean = std::fs::read(&p).unwrap();
    let flip = |off: u64, bytes: &[u8]| {
        let mut img = bytes.to_vec();
        img[off as usize + BLOCK_HEADER_SIZE + 3] ^= 0x40;
        std::fs::write(&p, &img).unwrap();
    };
    // damaged root / run: full scan, identical answers, warning
    for off in [root, run] {
        flip(off, &clean);
        let db = Db::open_with(&p, opts(4)).unwrap();
        assert!(db.open_warnings.iter().any(|w| w.contains("block directory")), "{:?}", db.open_warnings);
        assert!(db.directory_stats().get("fallback_full_scan").unwrap().is_truthy());
        assert_eq!(snapshot(&db), good);
    }
    // damaged page: open works (pages are read on demand), use fails loudly
    flip(page, &clean);
    {
        let db = Db::open_with(&p, opts(4)).unwrap();
        let mut failed = false;
        for u in 1..=330u64 {
            match db.get_internal(&Nat::S(R_DENSE), &Nat::S(u), NL) {
                Err(Error::Corruption(_)) => {
                    failed = true;
                    break;
                }
                Err(e) => panic!("unexpected error {e}"),
                Ok(_) => {}
            }
        }
        assert!(failed, "a damaged directory page must not be ignored");
        let v = db.verify(false).unwrap();
        assert!(!v.get("ok").unwrap().is_truthy());
    }
    // repair: directory dropped, nothing cut, data intact
    let before_len = std::fs::metadata(&p).unwrap().len();
    let r = Db::repair_file_opts(&p, &maint::RepairOptions { apply: true, make_backup: false, rebuild_indexes: false, open: opts(4), ..Default::default() }).unwrap();
    assert!(r.get("applied").unwrap().is_truthy(), "{}", r.py_repr());
    assert_eq!(r.get("lost_blocks").and_then(|x| x.as_list()).map(|l| l.len()), Some(0), "{}", r.py_repr());
    assert!(std::fs::metadata(&p).unwrap().len() >= before_len);
    let mut db = Db::open_with(&p, opts(4)).unwrap();
    assert_eq!(db.sup.dir_root, 0);
    assert_eq!(snapshot(&db), good);
    // the damaged page is garbage now: verify reports it as a warning only
    let v = db.verify(true).unwrap();
    assert!(v.get("ok").unwrap().is_truthy(), "{}", v.py_repr());
    db.checkpoint().unwrap();
    db.close(true).unwrap();
    let db = Db::open_with(&p, opts(4)).unwrap();
    assert!(db.directory_stats().get("loaded").unwrap().is_truthy());
    assert_eq!(snapshot(&db), good);
}

/// A data block covered by the directory is still CRC-checked on first use,
/// and a directory entry that contradicts its block is rejected.
#[test]
fn data_damage_under_directory_is_detected() {
    let p = tmp("datadmg");
    let mut db = setup(&p, opts(2));
    let rows: Vec<Vec<Val>> = (0..8).map(|i| vec![Val::Int(i), Val::str(format!("payload {i}"))]).collect();
    for i in 0..20u64 {
        db.put_dense_batch(&Nat::S(R_DENSE), &Nat::S(i * 8 + 1), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
        db.put_points(&Nat::S(R_SPARSE), vec![CellInput::typed(Nat::S(i + 1), Nat::S(3), Val::str("abc"), Some(TypeCode::Utf8))], NL, true, 64).unwrap();
    }
    db.checkpoint().unwrap();
    let seg = db.dense[&(Nat::S(R_DENSE), NL)].idx.collect_by_start(&db).unwrap()[5].clone();
    let sseg = db.sparse[&(Nat::S(R_SPARSE), NL)].idx.collect_by_start(&db).unwrap()[5].clone();
    db.close(true).unwrap();
    let clean = std::fs::read(&p).unwrap();
    // payload byte of a dense record
    let mut img = clean.clone();
    img[(seg.records_offset + 3) as usize] ^= 1;
    std::fs::write(&p, &img).unwrap();
    let db = Db::open_with(&p, opts(2)).unwrap();
    assert!(matches!(db.get_internal(&Nat::S(R_DENSE), &seg.u_start, NL), Err(Error::Corruption(_))));
    drop(db);
    // payload byte of a sparse cell
    let mut img = clean.clone();
    img[(sseg.entries_offset + 4) as usize] ^= 1;
    std::fs::write(&p, &img).unwrap();
    let db = Db::open_with(&p, opts(2)).unwrap();
    assert!(matches!(db.get_cell_value(&Nat::S(R_SPARSE), &sseg.u_start, &Nat::S(3), NL), Err(Error::Corruption(_))));
    // the merge (iterator, compaction) detects it as well
    assert!(matches!(db.iter_materialized_sparse_points(None, None), Err(Error::Corruption(_))));
    drop(db);
    let r = Db::compact_file(&p, &CompactOptions { make_backup: false, open: opts(2), ..Default::default() });
    assert!(matches!(r, Err(Error::Corruption(_))), "{r:?}");
    // the original is untouched by the failed compaction
    assert_eq!(std::fs::read(&p).unwrap(), img);
    // entry/block binding: an entry describing another block is refused
    std::fs::write(&p, &clean).unwrap();
    let db = Db::open_with(&p, opts(2)).unwrap();
    let mut forged = seg.clone();
    forged.u_start = forged.u_start.add_u64(1);
    forged.u_end = forged.u_end.add_u64(1);
    assert!(matches!(db.ensure_verified_dense(&forged), Err(Error::Corruption(_))));
    assert!(db.ensure_verified_dense(&seg).is_ok());
}

/// verify(deep) compares the directory with the data blocks.
#[test]
fn verify_detects_directory_mismatch() {
    let p = tmp("vmis");
    let mut db = setup(&p, opts(3));
    let mut rng = Rng(3);
    for _ in 0..150 {
        random_op(&mut rng, &mut [&mut db], &mut Model::new());
    }
    db.checkpoint().unwrap();
    assert_verify_ok(&db);
    // drop one persisted run part from memory: the scan no longer matches
    let key = (Nat::S(R_DENSE), NL);
    let parts = db.dense[&key].idx.runs.clone();
    let mut fewer = parts.clone();
    let last = fewer.pop().unwrap();
    let mut pages = last.pages.clone();
    pages.pop();
    if !pages.is_empty() {
        fewer.push(std::sync::Arc::new(segindex::RunPart::new(last.run, pages)));
    }
    db.dense.get_mut(&key).unwrap().idx.set_runs(fewer);
    let v = db.verify(true).unwrap();
    assert!(!v.get("ok").unwrap().is_truthy());
    assert!(v.py_repr().contains("block directory does not match"), "{}", v.py_repr());
}

/// Sparse merge/compaction on unusual distributions: one cell with many
/// versions, values larger than the read window, blocks spanning huge u
/// ranges that all overlap, BigInt u, tombstones of never written cells.
#[test]
fn merge_unusual_distributions() {
    let p = tmp("merge");
    let mut db = Db::create(&p, true, opts(u64::MAX)).unwrap();
    let r = Nat::S(5);
    db.create_realm(&r, "m", "", None, &Nat::ZERO).unwrap();
    let mut model: BTreeMap<(Nat, Nat), Option<Val>> = BTreeMap::new();
    let cell = |u: Nat, v: u64, s: String| CellInput::typed(u, Nat::S(v), Val::str(s), Some(TypeCode::Utf8));
    // 1500 versions of one cell, one block each
    for i in 0..1500u64 {
        db.put_points(&r, vec![cell(Nat::S(7), 1, format!("version {i}"))], NL, false, 1 << 40).unwrap();
    }
    model.insert((Nat::S(7), Nat::S(1)), Some(Val::str("version 1499")));
    // 300 blocks each spanning u 1..10^12 (all overlap everywhere)
    let mut rng = Rng(77);
    for b in 0..300u64 {
        let cells: Vec<CellInput> = (0..40)
            .map(|_| cell(Nat::S(rng.below(1_000_000_000_000) + 1), rng.below(5) + 1, format!("b{b}-{}", rng.next())))
            .collect();
        let mut m: BTreeMap<(Nat, Nat), CellInput> = BTreeMap::new();
        for c in cells {
            m.insert((c.u.clone(), c.v.clone()), c);
        }
        for c in m.values() {
            model.insert((c.u.clone(), c.v.clone()), Some(c.value.clone()));
        }
        db.put_points(&r, m.into_values().collect(), NL, false, u64::MAX).unwrap();
    }
    // values larger than the maximal read window (1 MiB) and BigInt u
    for i in 0..3u64 {
        let big = "y".repeat((1 << 20) + 12_345 * i as usize);
        db.put_points(&r, vec![cell(big_u(i), 2, big.clone())], NL, false, 64).unwrap();
        model.insert((big_u(i), Nat::S(2)), Some(Val::str(big)));
    }
    // one u group with 20 000 cells in one record (longer than one window)
    let wide: Vec<CellInput> = (1..=20_000u64).map(|v| cell(Nat::S(123_456), v, format!("w{v}"))).collect();
    for c in &wide {
        model.insert((c.u.clone(), c.v.clone()), Some(c.value.clone()));
    }
    db.put_points(&r, wide, NL, false, 64).unwrap();
    // tombstones: of written cells and of never written ones
    for (u, v) in [(Nat::S(7), 1u64), (Nat::S(999), 4), (Nat::S(123_456), 500)] {
        db.delete_point(&r, &u, &Nat::S(v), NL, false).unwrap();
        model.insert((u, Nat::S(v)), None);
    }
    db.commit().unwrap();
    let expect: Vec<(Nat, Nat, Val)> = model.iter().filter_map(|((u, v), x)| x.as_ref().map(|x| (u.clone(), v.clone(), x.clone()))).collect();
    let got = |db: &Db| -> Vec<(Nat, Nat, Val)> {
        db.iter_materialized_sparse_points(Some(&r), Some(NL)).unwrap().into_iter().map(|p| (p.u, p.v, p.value)).collect()
    };
    assert_eq!(got(&db), expect);
    db.close(true).unwrap();
    let res = Db::compact_file(&p, &CompactOptions { make_backup: false, open: opts(u64::MAX), ..Default::default() }).unwrap();
    assert!(res.get("after_bytes").unwrap().as_i64().unwrap() < res.get("before_bytes").unwrap().as_i64().unwrap(), "{}", res.py_repr());
    let db = Db::open_with(&p, opts(u64::MAX)).unwrap();
    assert_eq!(got(&db), expect);
    // tombstones without a dense value are not copied
    assert_eq!(db.get_cell_value(&r, &Nat::S(7), &Nat::S(1), NL).unwrap(), None);
    assert_verify_ok(&db);
}

/// RUN7 (no backward compatibility): a file of the old format 0.16
/// (`NHYDB016`, NHyDB 0.28.0) is rejected on open with its format named.
#[test]
fn format_016_superblock_is_rejected() {
    let p = tmp("f016");
    let mut db = setup(&p, opts(u64::MAX));
    let rows: Vec<Vec<Val>> = (0..3).map(|i| vec![Val::Int(i), Val::str("z")]).collect();
    db.put_dense_batch(&Nat::S(R_DENSE), &Nat::S(1), &DenseRows::Tuples(&rows), Some(1), &[], NL, true).unwrap();
    db.close(true).unwrap();
    let mut img = std::fs::read(&p).unwrap();
    for slot in 0..2 {
        let o = slot * SUPERBLOCK_SIZE;
        let mut raw = img[o..o + SUPERBLOCK_SIZE].to_vec();
        raw[0..8].copy_from_slice(b"NHYDB016");
        raw[10..12].copy_from_slice(&16u16.to_le_bytes());
        let crc = crc32(&raw[..SUPER_CRC_OFFSET]);
        raw[SUPER_CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
        img[o..o + SUPERBLOCK_SIZE].copy_from_slice(&raw);
    }
    std::fs::write(&p, &img).unwrap();
    match Db::open_with(&p, opts(1)) {
        Err(e) => assert!(e.to_string().contains("NHYDB016"), "{e}"),
        Ok(_) => panic!("format 0.16 file opened"),
    }
}

/// More than `MAX_OPEN` blocks overlapping at the same u: the merge goes
/// external (temporary runs) and still yields the newest state per cell;
/// compaction and the point iterator agree with the model.
#[test]
fn external_merge_for_heavy_overlap() {
    let p = tmp("ext");
    let mut db = Db::create(&p, true, opts(u64::MAX)).unwrap();
    let r = Nat::S(4);
    db.create_realm(&r, "x", "", None, &Nat::ZERO).unwrap();
    let mut model: BTreeMap<(Nat, Nat), Option<Val>> = BTreeMap::new();
    let mut rng = Rng(123);
    let n_blocks = merge::MAX_OPEN as u64 + 2500;
    for b in 0..n_blocks {
        // every block spans u 1..=3 (all overlap), random v, some tombstones
        let mut cells = Vec::new();
        for u in 1..=3u64 {
            let v = Nat::S(rng.below(40) + 1);
            if rng.below(9) == 0 {
                cells.push(CellInput::tombstone(Nat::S(u), v.clone()));
                model.insert((Nat::S(u), v), None);
            } else {
                let val = Val::str(format!("b{b}u{u}"));
                cells.push(CellInput::typed(Nat::S(u), v.clone(), val.clone(), Some(TypeCode::Utf8)));
                model.insert((Nat::S(u), v), Some(val));
            }
        }
        db.put_points(&r, cells, NL, false, u64::MAX).unwrap();
        if b % 1000 == 0 {
            db.commit().unwrap();
        }
    }
    // plus a block with a big u range overlapping everything
    db.put_points(&r, vec![CellInput::typed(Nat::S(2), Nat::S(41), Val::str("wide"), Some(TypeCode::Utf8)), CellInput::typed(Nat::S(900), Nat::S(1), Val::str("far"), Some(TypeCode::Utf8))], NL, false, u64::MAX).unwrap();
    model.insert((Nat::S(2), Nat::S(41)), Some(Val::str("wide")));
    model.insert((Nat::S(900), Nat::S(1)), Some(Val::str("far")));
    db.commit().unwrap();
    let key = (r.clone(), NL);
    assert!(merge::overlap_exceeds_pub(&db, &key));
    let expect: Vec<(Nat, Nat, Val)> = model.iter().filter_map(|((u, v), x)| x.as_ref().map(|x| (u.clone(), v.clone(), x.clone()))).collect();
    let got = |db: &Db| -> Vec<(Nat, Nat, Val)> {
        db.iter_materialized_sparse_points(Some(&r), Some(NL)).unwrap().into_iter().map(|p| (p.u, p.v, p.value)).collect()
    };
    assert_eq!(got(&db), expect);
    // no temporary run is left behind
    let leftovers = |dir: &Path| std::fs::read_dir(dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains("nhydb_merge_")).count();
    db.close(true).unwrap();
    Db::compact_file(&p, &CompactOptions { make_backup: false, open: opts(u64::MAX), ..Default::default() }).unwrap();
    assert_eq!(leftovers(p.parent().unwrap()), 0);
    let db = Db::open_with(&p, opts(u64::MAX)).unwrap();
    assert_eq!(got(&db), expect);
    assert_verify_ok(&db);
}

// ----------------------------------------------------------------------
// Regression tests of the 0.28.2 review round (finding ids in the names)
// ----------------------------------------------------------------------

/// RD-02: a cached page is not served for a summary that differs from the
/// one it was checked against.
#[test]
fn rd02_cached_page_checked_against_summary() {
    let p = tmp("rd02");
    let mut db = setup(&p, opts(2));
    let r = Nat::S(R_SPARSE);
    for u in 1..=40u64 {
        db.put_points(&r, vec![CellInput::typed(Nat::S(u), Nat::S(1), Val::Int(u as i64), Some(TypeCode::VarInt))], NL, true, 10_000).unwrap();
    }
    let part = db.sparse[&(r.clone(), NL)].idx.runs[0].clone();
    let good = part.pages[0].clone();
    let page = PageSource::<SparseSegment>::load_page(&db, &good).unwrap();
    assert_eq!(page.entries.len(), good.n as usize);
    let mut forged = good.clone();
    forged.max_bid += 1000;
    assert!(matches!(PageSource::<SparseSegment>::load_page(&db, &forged), Err(Error::Corruption(_))));
    forged = good.clone();
    forged.first = forged.first.add_u64(1);
    assert!(matches!(PageSource::<SparseSegment>::load_page(&db, &forged), Err(Error::Corruption(_))));
    assert!(PageSource::<SparseSegment>::load_page(&db, &good).is_ok());
}

/// RD-06: offsets near the end of the u64 range in a page are corruption,
/// never an overflow.
#[test]
fn rd06_page_offsets_do_not_overflow() {
    let zz = |d: i64| ((d << 1) ^ (d >> 63)) as u64;
    let mut out = Vec::new();
    Nat::S(5).put_vu(&mut out); // u_start
    put_vu64(&mut out, zz(7)); // block id
    Nat::ZERO.put_vu(&mut out); // span
    put_vu64(&mut out, zz((u64::MAX / 8) as i64)); // block offset / 8: near 2^64
    put_vu64(&mut out, 20);
    put_vu64(&mut out, 8);
    put_vu64(&mut out, 1);
    put_vu64(&mut out, 1);
    let mut q = 0usize;
    let r = SparseSegment::decode(&out, &mut q, None, &Nat::S(1), NL);
    assert!(r.is_err() || r.unwrap().payload_offset > 0);
}

/// RD-11: roots with absurdly long numbers are rejected before parsing.
#[test]
fn rd11_root_numbers_are_bounded() {
    let mut payload = ROOT_TAG.to_vec();
    let json = format!("{{\"v\":1,\"runs\":[],\"n\":{}}}", "9".repeat(5000));
    put_vu64(&mut payload, json.len() as u64);
    payload.extend_from_slice(json.as_bytes());
    let t = std::time::Instant::now();
    assert!(root_run_offsets(&payload).is_err());
    assert!(t.elapsed().as_secs_f64() < 0.5);
}

/// RA-01: a damaged header is skipped only for a directory block; a data
/// block whose type byte flipped to DIRECTORY stays damage.
#[test]
fn ra01_damaged_directory_header_recognition() {
    let p = tmp("ra01");
    let mut db = setup(&p, opts(3));
    let r = Nat::S(R_SPARSE);
    for u in 1..=12u64 {
        db.put_points(&r, vec![CellInput::typed(Nat::S(u), Nat::S(1), Val::Int(u as i64), Some(TypeCode::VarInt))], NL, true, 10_000).unwrap();
    }
    let end = db.store.len();
    let read_at = |buf: &mut [u8], o: u64| db.store.read_exact_at(o, buf);
    let mut off = DATA_START;
    let (mut dir_seen, mut data_seen) = (false, false);
    while off < end {
        let info = db.read_header_at(off).unwrap();
        let mut raw = [0u8; BLOCK_HEADER_SIZE];
        db.store.read_exact_at(off, &mut raw).unwrap();
        raw[10] ^= 0x10; // header CRC fails
        if info.block_type == BT_DIRECTORY {
            assert_eq!(damaged_directory_extent(&read_at, off, &raw, end), Some(info.next_offset));
            dir_seen = true;
        } else if info.block_type == BT_SPARSE_CELLS {
            raw[5] = BT_DIRECTORY;
            assert_eq!(damaged_directory_extent(&read_at, off, &raw, end), None);
            data_seen = true;
        }
        off = info.next_offset;
    }
    assert!(dir_seen && data_seen);
}

/// G1/G4: the v index is caught up across a directory flush instead of
/// being rebuilt, and answers like a fresh index.
#[test]
fn g4_v_index_survives_directory_flushes() {
    let p = tmp("g4");
    let mut db = setup(&p, opts(8));
    let r = Nat::S(R_SPARSE);
    let n = 2000u64;
    for i in 0..n {
        let v = (i * 7919) % n + 1;
        db.put_points(&r, vec![CellInput::typed(Nat::S(1), Nat::S(v), Val::Int(v as i64), Some(TypeCode::VarInt))], NL, false, 10_000).unwrap();
    }
    db.commit().unwrap();
    assert_eq!(db.get_cell_value(&r, &Nat::S(1), &Nat::S(77), NL).unwrap(), Some(Val::Int(77)));
    let builds = db.cache().uindex_builds;
    for i in 1..=200u64 {
        db.put_points(&r, vec![CellInput::typed(Nat::S(1), Nat::S(i), Val::Int(-(i as i64)), Some(TypeCode::VarInt))], NL, false, 10_000).unwrap();
        assert_eq!(db.get_cell_value(&r, &Nat::S(1), &Nat::S(i), NL).unwrap(), Some(Val::Int(-(i as i64))));
    }
    assert!(db.dir.stats.flushes >= 20);
    assert_eq!(db.cache().uindex_builds, builds, "no rebuild after flushes");
    db.commit().unwrap();
    let fresh = Db::open_with(&p, OpenOptions { readonly: true, ..opts(8) }).unwrap();
    for v in 1..=n {
        assert_eq!(
            db.get_cell_value(&r, &Nat::S(1), &Nat::S(v), NL).unwrap(),
            fresh.get_cell_value(&r, &Nat::S(1), &Nat::S(v), NL).unwrap(),
            "v {v}"
        );
    }
}
