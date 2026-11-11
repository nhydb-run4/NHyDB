//! Memory bounds of the 0.28.2 storage changes, measured with a counting
//! global allocator (exact live heap of this test binary):
//!
//! * compaction of a sparse realm needs memory independent of the number of
//!   live cells (memory-bounded merge instead of a map of all cells);
//! * opening a database with very many physical blocks keeps only the
//!   directory summaries and the tail since the last flush in memory;
//! * RB-01: merging many versions of large values (iterator, compaction)
//!   holds read windows and the winning value, not one value per version;
//! * G2/G3: the v index of a cell overwritten very often costs no memory per
//!   overwritten version.
//!
//! Both checks run in one test function: the allocator counts the whole
//! process, parallel tests would disturb each other.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use nhydb_core::db::maint::CompactOptions;
use nhydb_core::{CellInput, Config, Db, Nat, OpenOptions, TypeCode, Val, NL};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let n = LIVE.fetch_add(l.size(), Relaxed) + l.size();
        PEAK.fetch_max(n, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if n > l.size() {
            let x = LIVE.fetch_add(n - l.size(), Relaxed) + n - l.size();
            PEAK.fetch_max(x, Relaxed);
        } else {
            LIVE.fetch_sub(l.size() - n, Relaxed);
        }
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nhydb_mem_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join(format!("{name}.nhydb"))
}

/// Peak heap above the level before `f`.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    let r = f();
    (r, PEAK.load(Relaxed).saturating_sub(base))
}

#[test]
fn memory_bounds() {
    let mb = |b: usize| b as f64 / 1e6;
    // 1) compaction: N live cells + one overwritten version each, for two N:
    //    the peak must not grow with the number of cells
    let r = Nat::S(1);
    let compact_peak = |n: u64| -> usize {
        let p = tmp(&format!("compact{n}"));
        {
            let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
            db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
            for round in 0..2u64 {
                let mut u = 1u64;
                while u <= n {
                    let cells: Vec<CellInput> = (u..u + 10_000)
                        .map(|k| CellInput::typed(Nat::S(k), Nat::S(1 + k % 3), Val::str(format!("value {k} round {round}")), Some(TypeCode::Utf8)))
                        .collect();
                    db.put_points(&r, cells, NL, false, u64::MAX).unwrap();
                    u += 10_000;
                }
            }
            db.close(true).unwrap();
        }
        let (res, peak) = peak_of(|| Db::compact_file(&p, &CompactOptions { make_backup: false, ..Default::default() }).unwrap());
        println!(
            "compaction of {n} live sparse cells: peak heap {:.1} MB ({} -> {} bytes, {:.2} s)",
            mb(peak),
            res.get("before_bytes").unwrap().py_repr(),
            res.get("after_bytes").unwrap().py_repr(),
            match res.get("seconds") { Some(Val::Float(f)) => *f, _ => 0.0 }
        );
        let db = Db::open_with(&p, OpenOptions::default()).unwrap();
        let k = n / 2 + 7;
        assert_eq!(db.get_cell_value(&r, &Nat::S(k), &Nat::S(1 + k % 3), NL).unwrap(), Some(Val::str(format!("value {k} round 1"))));
        peak
    };
    let small = compact_peak(300_000);
    let large = compact_peak(1_200_000);
    // The former global map needed ~300 B per live cell (~90 / ~360 MB here).
    // What remains is constant: append buffer of the target (8-16 MiB), one
    // chunk of 50 000 cells for put_points, read windows, verify buffers.
    assert!(large < 64_000_000, "compaction peak heap {:.1} MB", mb(large));
    assert!(large < small + 8_000_000, "peak grows with the cells: {:.1} -> {:.1} MB", mb(small), mb(large));

    // 2) open with 300 000 physical blocks (one cell each)
    let p = tmp("blocks");
    let cfg = Config { directory_flush_blocks: 4096, ..Config::default() };
    let o = OpenOptions { config: Some(cfg), ..OpenOptions::default() };
    {
        let mut db = Db::create(&p, true, o.clone()).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        for i in 1..=300_000u64 {
            db.put_points(&r, vec![CellInput::typed(Nat::S(i), Nat::S(1), Val::str(format!("v{i}")), Some(TypeCode::Utf8))], NL, false, 10_000).unwrap();
            if i % 50_000 == 0 {
                db.commit().unwrap();
            }
        }
        db.close(true).unwrap();
    }
    let base = LIVE.load(Relaxed);
    let t = std::time::Instant::now();
    let (db, peak) = peak_of(|| Db::open_with(&p, o.clone()).unwrap());
    let open_s = t.elapsed().as_secs_f64();
    let live = LIVE.load(Relaxed).saturating_sub(base);
    println!("open with 300 000 blocks: {open_s:.3} s, live heap {:.2} MB, peak {:.2} MB", mb(live), mb(peak));
    // without directory: ~229 B per block (~69 MB)
    assert!(live < 8_000_000, "live heap after open {:.1} MB", mb(live));
    for u in [1u64, 123_457, 300_000] {
        assert_eq!(db.get_cell_value(&r, &Nat::S(u), &Nat::S(1), NL).unwrap(), Some(Val::str(format!("v{u}"))));
    }
    assert_eq!(db.get_cell_value(&r, &Nat::S(300_001), &Nat::S(1), NL).unwrap(), None);
    let after_reads = LIVE.load(Relaxed).saturating_sub(base);
    println!("live heap after reads: {:.2} MB", mb(after_reads));
    assert!(after_reads < 64_000_000);

    drop(db);

    // 3) RB-01: 300 versions of one cell with 1 MiB values (315 MB data):
    //    iterator and compaction stay near the fixed budgets (block cache,
    //    merge windows) instead of growing with versions x value size
    let p = tmp("bigvals");
    let big = Val::Bytes(vec![7u8; 1 << 20]);
    {
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        for i in 0..300u64 {
            let mut v = vec![7u8; 1 << 20];
            v[0] = (i % 251) as u8;
            db.put_points(&r, vec![CellInput::typed(Nat::S(1), Nat::S(1), Val::Bytes(v), Some(TypeCode::Bytes))], NL, false, 10_000).unwrap();
            if i % 50 == 49 {
                db.commit().unwrap();
            }
        }
        db.close(true).unwrap();
    }
    let _ = big;
    {
        let db = Db::open_with(&p, OpenOptions::default()).unwrap();
        let (n, peak) = peak_of(|| {
            let mut c = nhydb_core::db::cursor::SparsePointCursor::new(&db, Some(&r), Some(NL));
            let mut n = 0usize;
            loop {
                let b = c.next_batch(&db, 1000).unwrap();
                if b.is_empty() {
                    break;
                }
                n += b.len();
            }
            n
        });
        println!("iterate 300 x 1 MiB versions: {n} point(s), peak {:.1} MB", mb(peak));
        assert_eq!(n, 1);
        assert!(peak < 200_000_000, "iterator peak {:.1} MB (was 684 MB)", mb(peak));
    }
    let (_, peak) = peak_of(|| Db::compact_file(&p, &CompactOptions { make_backup: false, ..Default::default() }).unwrap());
    println!("compact 300 x 1 MiB versions: peak {:.1} MB", mb(peak));
    assert!(peak < 150_000_000, "compaction peak {:.1} MB (was 630 MB)", mb(peak));

    // 4) G2/G3: 200 000 versions of one sparse cell, one live cell
    let p = tmp("versions");
    {
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        for i in 0..200_000u64 {
            db.put_points(&r, vec![CellInput::typed(Nat::S(1), Nat::S(1), Val::Int(i as i64), Some(TypeCode::VarInt))], NL, false, 10_000).unwrap();
            if i % 50_000 == 49_999 {
                db.commit().unwrap();
            }
        }
        db.close(true).unwrap();
    }
    let db = Db::open_with(&p, OpenOptions::default()).unwrap();
    let base = LIVE.load(Relaxed);
    assert_eq!(db.get_cell_value(&r, &Nat::S(1), &Nat::S(1), NL).unwrap(), Some(Val::Int(199_999)));
    let cache = db.stats().unwrap();
    let vb = cache.get("block_cache").and_then(|b| b.get("vindex_bytes")).and_then(|v| v.as_i64()).unwrap();
    let live = LIVE.load(Relaxed).saturating_sub(base);
    println!("200 000 versions: v index {vb} bytes, live heap after first read {:.1} MB", mb(live));
    assert!(vb < 4096, "v index {vb} bytes for one live cell");
    // block cache (64 MiB budget) + directory pages; was ~80 MB of v index alone
    assert!(live < 120_000_000, "live heap {:.1} MB", mb(live));

    drop(db);

    // 5) 0.28.2: SECONDARY indexes with N distinct keys, for two N: the
    //    build stays near its budget (spilled sorted runs), the first and
    //    later lookups near the page cache budget – neither grows with N
    let budget = 8usize << 20;
    let page_cache = 2usize << 20;
    let sec = |n: u64| -> (usize, usize, usize) {
        let p = tmp(&format!("secondary{n}"));
        let cfg = Config { index_build_memory_bytes: budget, index_page_cache_bytes: page_cache, ..Config::default() };
        let o = || OpenOptions { config: Some(cfg.clone()), ..OpenOptions::default() };
        {
            let mut db = Db::create(&p, true, o()).unwrap();
            db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
            db.create_shape(1, &r, "s", vec![nhydb_core::FieldDef::new(Nat::S(1), "k", TypeCode::VarUInt), nhydb_core::FieldDef::new(Nat::S(2), "s", TypeCode::Utf8)], "").unwrap();
            db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
            db.create_index(10, &r, "k", &Val::Int(1), "").unwrap();
            db.create_index(11, &r, "s", &Val::Int(2), "").unwrap();
            let mut u = 1u64;
            while u <= n {
                let rows: Vec<Vec<Val>> = (u..u + 50_000).map(|k| vec![Val::Int(k as i64 * 3), Val::str(format!("key{k:09}"))]).collect();
                db.put_dense_batch(&r, &Nat::S(u), &nhydb_core::DenseRows::Tuples(&rows), Some(1), &[], NL, false).unwrap();
                u += 50_000;
            }
            db.commit().unwrap();
            db.close(true).unwrap();
        }
        let mut db = Db::open_with(&p, o()).unwrap();
        let (_, build) = peak_of(|| db.build_indexes(None, true).unwrap());
        db.close(true).unwrap();
        let db = Db::open_with(&p, o()).unwrap();
        let (_, first) = peak_of(|| db.find_u(&r, &[("k".into(), nhydb_core::Cond::Eq(Val::Int(3 * (n as i64 / 2))))], NL, None, None).unwrap());
        let base = LIVE.load(Relaxed);
        let mut x = 12345u64;
        for _ in 0..20_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let k = 1 + (x >> 33) % n;
            let hit = db.find_u(&r, &[("s".into(), nhydb_core::Cond::Eq(Val::str(format!("key{k:09}"))))], NL, None, None).unwrap();
            assert_eq!(hit, vec![Nat::S(k)]);
            let lo = 3 * k as i64;
            let hits = db.find_u(&r, &[("k".into(), nhydb_core::Cond::Range(Val::Int(lo), Val::Int(lo + 300)))], NL, None, None).unwrap();
            assert!(!hits.is_empty() && hits.len() <= 101);
        }
        let live = LIVE.load(Relaxed).saturating_sub(base);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        (build, first, live)
    };
    let (b1, f1, l1) = sec(200_000);
    let (b2, f2, l2) = sec(800_000);
    println!(
        "SECONDARY 200k/800k keys: build peak {:.1}/{:.1} MB, first lookup {:.2}/{:.2} MB, live after 40 000 lookups {:.1}/{:.1} MB",
        mb(b1), mb(b2), mb(f1), mb(f2), mb(l1), mb(l2)
    );
    // the build: budget + per-run windows + one DATA block; no growth with the keys
    assert!(b2 < 4 * budget + 48_000_000, "build peak {:.1} MB", mb(b2));
    assert!(b2 < b1 + b1 / 2, "build peak grows with the keys: {:.1} -> {:.1} MB", mb(b1), mb(b2));
    // first lookup: a few pages, not the directory of all keys
    assert!(f2 < 1_000_000, "first lookup {:.2} MB", mb(f2));
    // after many lookups: page cache + posting cache + block cache budgets
    assert!(l2 < l1 + 16_000_000 && l2 < 120_000_000, "lookups: {:.1} -> {:.1} MB", mb(l1), mb(l2));

    // 6) round 3: FULLTEXT document lengths are paged; a ranked search for
    //    a rare word keeps a few pages and segments, not the corpus (0.28.1:
    //    about 49 bytes per document, per index, lane and handle)
    let words = |u: u64| -> String {
        let n = 1 + (u * 7919) % 9;
        let mut t: Vec<String> = (0..n).map(|k| format!("w{}", (u * 31 + k * 17) % 4001)).collect();
        if u == 77 || u % 250_000 == 123 {
            t.push("seltenwort".into());
        }
        t.join(" ")
    };
    let ft_db = |name: &str, n: u64, n_ft: u64| -> std::path::PathBuf {
        let p = tmp(name);
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        db.create_shape(1, &r, "s", vec![nhydb_core::FieldDef::new(Nat::S(1), "k", TypeCode::VarUInt), nhydb_core::FieldDef::new(Nat::S(2), "t", TypeCode::Utf8)], "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &Nat::S(1), None, 0, "").unwrap();
        db.create_index(10, &r, "k", &Val::Int(1), "").unwrap();
        for i in 0..n_ft {
            db.create_index_ext(20 + i, &r, &format!("ft{i}"), &Val::Int(2), &Val::str("FULLTEXT"), &Val::None, None, None, None, "").unwrap();
        }
        let mut u = 1u64;
        while u <= n {
            let rows: Vec<Vec<Val>> = (u..(u + 50_000).min(n + 1)).map(|k| vec![Val::Int((k % 1000) as i64), Val::str(words(k))]).collect();
            db.put_dense_batch(&r, &Nat::S(u), &nhydb_core::DenseRows::Tuples(&rows), Some(1), &[], NL, false).unwrap();
            u += 50_000;
        }
        db.build_indexes(None, true).unwrap();
        db.close(true).unwrap();
        p
    };
    // the core of a ranked search: term postings, candidates, their lengths
    let rare_search = |db: &Db, index: u64| -> (usize, u64) {
        let posts = db.fulltext_payloads(index, &["seltenwort".to_string()], NL).unwrap();
        let mut cand: Vec<Nat> = posts.iter().flat_map(|(_, p)| p.iter().map(|x| x.u.clone())).collect();
        cand.sort();
        let c = db.fulltext_doc_lengths(index, NL, &cand).unwrap();
        (c.lengths.len(), c.total_docs)
    };
    let mut ft_live = Vec::new();
    for n in [300_000u64, 900_000] {
        let p = ft_db(&format!("fulltext{n}"), n, 1);
        let db = Db::open_with(&p, OpenOptions::default()).unwrap();
        let base = LIVE.load(Relaxed);
        let (hits, docs) = rare_search(&db, 20);
        let live = LIVE.load(Relaxed).saturating_sub(base);
        assert_eq!(docs, n);
        assert!(hits >= 1);
        println!("FULLTEXT {n} documents: live heap after a rare-word search {:.2} MB ({hits} hits)", mb(live));
        ft_live.push(live);
        drop(db);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
    assert!(ft_live[1] < 4_000_000, "rare search keeps {:.2} MB (0.28.1: ~44 MB corpus)", mb(ft_live[1]));
    assert!(ft_live[1] < ft_live[0] + 1_000_000, "grows with the corpus: {:.2} -> {:.2} MB", mb(ft_live[0]), mb(ft_live[1]));

    // 7) round 3: the process pot bounds all handles together. Three
    //    databases with three FULLTEXT and one VALUE index each, two handles
    //    per database, many searches: the live heap held by the handles
    //    stays near `total_bytes` (default pot for comparison: it grows),
    //    the results are the same.
    let paths: Vec<_> = (0..3).map(|i| ft_db(&format!("pot{i}"), 60_000, 3)).collect();
    let pot_run = |total: Option<u64>| -> (usize, Vec<(usize, u64)>, nhydb_core::Val) {
        let cfg = Config { cache_total_bytes: total.unwrap_or(nhydb_core::db::mempot::DEFAULT_TOTAL_BYTES), ..Config::default() };
        let o = || OpenOptions { readonly: true, config: Some(cfg.clone()), ..OpenOptions::default() };
        let base = LIVE.load(Relaxed);
        let hs: Vec<Db> = paths.iter().flat_map(|p| [Db::open_with(p, o()).unwrap(), Db::open_with(p, o()).unwrap()]).collect();
        let opened = LIVE.load(Relaxed).saturating_sub(base);
        let mut max_live = 0usize;
        let mut res = Vec::new();
        for round in 0..3u64 {
            for (i, h) in hs.iter().enumerate() {
                for idx in 20..23u64 {
                    for w in 0..12u64 {
                        let term = format!("w{}", (round * 977 + i as u64 * 131 + idx * 17 + w * 41) % 4001);
                        let posts = h.fulltext_payloads(idx, &[term], NL).unwrap();
                        let mut cand: Vec<Nat> = posts.iter().flat_map(|(_, p)| p.iter().map(|x| x.u.clone())).collect();
                        cand.sort();
                        cand.dedup();
                        let c = h.fulltext_doc_lengths(idx, NL, &cand).unwrap();
                        res.push((c.lengths.len(), c.lengths.values().sum::<u64>()));
                        drop((posts, cand, c));
                        max_live = max_live.max(LIVE.load(Relaxed).saturating_sub(base + opened));
                    }
                }
                let hit = h.find_u(&r, &[("k".into(), nhydb_core::Cond::Eq(Val::Int(7)))], NL, None, None).unwrap();
                assert_eq!(hit.len(), 60);
            }
        }
        let st = hs[0].stats().unwrap().get("cache").cloned().unwrap();
        drop(hs);
        (max_live, res, st)
    };
    let (free_live, free_res, _) = pot_run(None);
    let limit = 6u64 << 20;
    let (pot_live, pot_res, st) = pot_run(Some(limit));
    let proc = st.get("process").unwrap();
    let reclaims = proc.get("reclaims").and_then(|v| v.as_i64()).unwrap();
    println!(
        "6 handles x 3 FULLTEXT indexes: live heap of the caches {:.1} MB with the default pot, {:.1} MB with total_bytes = {:.0} MiB ({reclaims} reclaims)",
        mb(free_live), mb(pot_live), limit as f64 / 1048576.0
    );
    assert_eq!(pot_res, free_res, "results do not depend on the pot");
    assert!(reclaims > 0);
    // pot + at most the entries inserted while a reclaim ran + transient
    // query memory (one posting, one block of at most 1 MiB)
    assert!(pot_live < limit as usize + 3_000_000, "live {:.1} MB over the pot", mb(pot_live));
    assert!(free_live > pot_live, "the default pot would hold more ({:.1} MB)", mb(free_live));
    for p in &paths {
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    // 8) round 3: the booked bytes of a pool are honest. 400 000 single-cell
    //    sparse blocks fill the group directory cache from the write path;
    //    the real heap difference between two budgets matches the booked
    //    difference, and the pool stays within its budget (the first version
    //    of round 3 booked ~64 bytes per entry for a ~150-byte table slot)
    let sparse_run = |budget: usize| -> (usize, i64) {
        let p = tmp(&format!("sparsepool{budget}"));
        let cfg = Config { sparse_cache_bytes: budget, ..Config::default() };
        let mut db = Db::create(&p, true, OpenOptions { config: Some(cfg), ..OpenOptions::default() }).unwrap();
        db.create_realm(&r, "r", "", None, &Nat::ZERO).unwrap();
        let base = LIVE.load(Relaxed);
        for u in 1..=400_000u64 {
            db.put_points(&r, vec![CellInput::typed(Nat::S(u), Nat::S(1), Val::Int(u as i64), Some(TypeCode::VarInt))], NL, false, 10_000).unwrap();
        }
        let live = LIVE.load(Relaxed).saturating_sub(base);
        let st = db.stats().unwrap();
        let pool = st.get("cache").and_then(|c| c.get("pools")).and_then(|p| p.get("sparse")).cloned().unwrap();
        let booked = pool.get("bytes").and_then(|v| v.as_i64()).unwrap();
        assert!(booked as usize <= budget, "sparse pool {booked} over its budget {budget}");
        db.close(false).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        (live, booked)
    };
    let (live_s, booked_s) = sparse_run(8 << 20);
    let (live_l, booked_l) = sparse_run(64 << 20);
    let real = live_l as f64 - live_s as f64;
    let booked = (booked_l - booked_s) as f64;
    println!("sparse pool 8 vs 64 MiB: booked +{:.1} MB, real heap +{:.1} MB", booked / 1e6, real / 1e6);
    assert!(booked > 30e6, "the larger budget is used ({:.1} MB)", booked / 1e6);
    // never much below the real heap (the budget is an upper bound), not
    // wasting much of it either
    assert!(real < 1.1 * booked && real > 0.6 * booked, "booked {:.1} MB vs real {:.1} MB", booked / 1e6, real / 1e6);
}
