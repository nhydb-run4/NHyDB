//! Differential test of targeted sparse access (`vindex.rs`):
//! point and range accesses against an independent model and against the
//! full merge (`sparse_updates_for_u`).
//! Covered: overwrites across many blocks, tombstones, null,
//! self-typed cells, both lanes, BigInt u/v, interleaved
//! writing/reading (incremental index maintenance), commit + reopen.

use std::collections::BTreeMap;
use std::path::PathBuf;

use nhydb_core::*;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("nhydb_rust_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    // remove the DB file triad plus the lock file
    for suffix in ["", ".layout.enc", ".layout.key", ".lock"] {
        let _ = std::fs::remove_file(dir.join(format!("{name}{suffix}")));
    }
    p
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

/// A coordinate zone with a type rule for value generation.
#[derive(Clone)]
struct Zone {
    realm: Nat,
    us: Vec<Nat>,
    v_base: Nat,
    v_width: u64,
    /// Type per (u index, v offset); None → write self-typed
    typ: fn(usize, u64) -> Option<TypeCode>,
}

type Key = (Nat, Lane, Nat, Nat);

fn value_for(rng: &mut Rng, t: TypeCode) -> Val {
    if rng.below(10) == 0 {
        return Val::None;
    }
    match t {
        TypeCode::VarUInt => Val::Int(rng.below(1 << 40) as i64),
        TypeCode::VarInt => Val::Int(rng.below(1 << 20) as i64 - (1 << 19)),
        TypeCode::Utf8 => Val::str(format!("s{}", rng.below(1_000_000))),
        TypeCode::U8 => Val::Int(rng.below(256) as i64),
        _ => Val::Float(rng.below(1000) as f64 / 8.0),
    }
}

fn check_all(db: &Db, zones: &[Zone], model: &BTreeMap<Key, Option<Val>>, rng: &mut Rng) {
    // point accesses to written and never-written cells
    for z in zones {
        for (ui, u) in z.us.iter().enumerate() {
            for lane in [NL, NR] {
                let merged = db.sparse_updates_for_u(&z.realm, u, lane).unwrap();
                for _ in 0..60 {
                    let v = z.v_base.add_u64(rng.below(z.v_width + 2));
                    let key = (z.realm.clone(), lane, u.clone(), v.clone());
                    let got = db.get_cell_value(&z.realm, u, &v, lane).unwrap();
                    let want = match model.get(&key) {
                        Some(Some(x)) => Some(x.clone()),
                        _ => None,
                    };
                    assert_eq!(got, want, "point R{} u{} v{} lane{} (u index {ui})", z.realm, u, v, lane);
                    let from_merge = match merged.get(&v) {
                        Some((SVal::V(x), _)) => Some(x.clone()),
                        _ => None,
                    };
                    assert_eq!(got, from_merge, "merge oracle R{} u{} v{}", z.realm, u, v);
                }
                // ranges (including tombstones) against the full merge
                for _ in 0..8 {
                    let a = z.v_base.add_u64(rng.below(z.v_width + 2));
                    let b = a.add_u64(rng.below(z.v_width / 3 + 1));
                    let got = db.sparse_updates_for_u_range(&z.realm, u, &a, &b, lane).unwrap();
                    let want: Vec<(Nat, SVal)> = merged.range(a.clone()..=b.clone()).map(|(k, (s, _))| (k.clone(), s.clone())).collect();
                    assert_eq!(got, want, "range R{} u{} [{a},{b}]", z.realm, u);
                }
            }
        }
    }
}

fn run(seed: u64) {
    let p = tmp(&format!("sparse_access_{seed}.nhydb"));
    let mut rng = Rng(seed);
    let big_u = Nat::from(num_bigint::BigUint::from(1u8) << 70usize);
    let big_v = Nat::from(num_bigint::BigUint::from(10u8).pow(40));
    let zones = vec![
        // PACKED: u=1, span 5, offsets 0..2 declared, 3..4 self-typed
        Zone {
            realm: Nat::S(7),
            us: vec![Nat::S(1)],
            v_base: Nat::S(1),
            v_width: 400,
            typ: |_, off| match (off) % 5 {
                0 => Some(TypeCode::VarUInt),
                1 => Some(TypeCode::VarInt),
                2 => Some(TypeCode::Utf8),
                _ => None,
            },
        },
        // COLUMN: three field u, v = object
        Zone {
            realm: Nat::S(8),
            us: vec![Nat::S(10), Nat::S(11), Nat::S(12)],
            v_base: Nat::S(1),
            v_width: 300,
            typ: |ui, _| [Some(TypeCode::VarUInt), Some(TypeCode::Utf8), Some(TypeCode::U8)][ui],
        },
        // BigInt: COLUMN field at u = 2^70, v from 10^40 (PACKED span 10^39)
        Zone {
            realm: Nat::S(9),
            us: vec![big_u.clone()],
            v_base: big_v.clone(),
            v_width: 200,
            typ: |_, _| Some(TypeCode::VarInt),
        },
        // no layout: self-typed cells only
        Zone { realm: Nat::S(7), us: vec![Nat::S(5000), Nat::S(5001)], v_base: Nat::S(1), v_width: 150, typ: |_, _| None },
    ];
    let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
    for r in [7u64, 8, 9] {
        db.create_realm(&Nat::S(r), "R", "", None, &Nat::ZERO).unwrap();
    }
    let pf = |name: &str, t: &str, off: i64| vmap! {"name" => name, "type" => t, "offset" => off};
    db.create_packed_layout(1, &Nat::S(7), "P", &Nat::S(1), &Nat::S(5), vec![pf("a", "varuint", 0), pf("b", "varint", 1), pf("c", "utf8", 2)], 0, "")
        .unwrap();
    let cf = |u: Val, name: &str, t: &str| vmap! {"u" => u, "name" => name, "type" => t};
    db.create_column_layout(
        2,
        &Nat::S(8),
        "C",
        vec![cf(Val::Int(10), "x", "varuint"), cf(Val::Int(11), "y", "utf8"), cf(Val::Int(12), "z", "u8")],
        None,
        0,
        "",
    )
    .unwrap();
    db.create_column_layout(3, &Nat::S(9), "B", vec![cf(Val::nat(&big_u), "w", "varint")], None, 0, "").unwrap();

    let mut model: BTreeMap<Key, Option<Val>> = BTreeMap::new();
    for step in 0..120 {
        let z = &zones[rng.below(zones.len() as u64) as usize];
        let lane = if rng.below(3) == 0 { NR } else { NL };
        if rng.below(5) == 0 && !model.is_empty() {
            // tombstone on a random cell of this zone (also never-written ones)
            let u = z.us[rng.below(z.us.len() as u64) as usize].clone();
            let v = z.v_base.add_u64(rng.below(z.v_width));
            db.delete_point(&z.realm, &u, &v, lane, false).unwrap();
            model.insert((z.realm.clone(), lane, u, v), None);
        } else {
            // batch: contiguous v range (like object batches) or scattered
            let n = 1 + rng.below(60);
            let start = rng.below(z.v_width);
            let scatter = rng.below(3) == 0;
            let mut cells = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for k in 0..n {
                let ui = rng.below(z.us.len() as u64) as usize;
                let off = if scatter { rng.below(z.v_width) } else { (start + k) % z.v_width };
                let u = z.us[ui].clone();
                let v = z.v_base.add_u64(off);
                if !seen.insert((ui, off)) {
                    continue;
                }
                let declared = (z.typ)(ui, off);
                let (val, typ) = match declared {
                    Some(t) => (value_for(&mut rng, t), None),
                    None => {
                        let t = [TypeCode::Utf8, TypeCode::VarInt, TypeCode::F64][rng.below(3) as usize];
                        (value_for(&mut rng, t), Some(t))
                    }
                };
                model.insert((z.realm.clone(), lane, u.clone(), v.clone()), Some(val.clone()));
                cells.push(CellInput::typed(u, v, val, typ));
            }
            db.put_points(&z.realm, cells, lane, false, 1 + rng.below(4)).unwrap();
        }
        if step % 15 == 7 {
            check_all(&db, &zones, &model, &mut rng);
        }
        if step % 40 == 39 {
            db.commit().unwrap();
            db.close(true).unwrap();
            db = Db::open(&p).unwrap();
        }
    }
    check_all(&db, &zones, &model, &mut rng);
    db.commit().unwrap();
    db.close(true).unwrap();
    let db = Db::open(&p).unwrap();
    check_all(&db, &zones, &model, &mut rng);
}

#[test]
fn sparse_point_and_range_match_full_merge() {
    for seed in 1..=12 {
        run(seed);
    }
}

/// get_object / occupied_n / get_record_view for PACKED and COLUMN across many
/// blocks, with overwrites and tombstones, against the model.
#[test]
fn objects_over_many_blocks() {
    let p = tmp("sparse_access_objects.nhydb");
    let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
    let r = Nat::S(3);
    db.create_realm(&r, "R", "", None, &Nat::ZERO).unwrap();
    let pf = |name: &str, t: &str, off: i64| vmap! {"name" => name, "type" => t, "offset" => off};
    db.create_packed_layout(1, &r, "P", &Nat::S(1), &Nat::S(100), vec![pf("a", "varuint", 0), pf("b", "utf8", 1)], 0, "").unwrap();
    db.create_realm(&Nat::S(4), "R4", "", None, &Nat::ZERO).unwrap();
    let cf = |u: i64, name: &str, t: &str| vmap! {"u" => u, "name" => name, "type" => t};
    db.create_column_layout(2, &Nat::S(4), "C", vec![cf(1, "a", "varuint"), cf(2, "b", "utf8")], None, 0, "").unwrap();
    let obj = |k: u64, gn: u64| vmap! {"a" => (k * 10 + gn) as i64, "b" => format!("o{k}g{gn}")};
    // 50 blocks of 20 objects each, then overwrite individual objects
    for b in 0..50u64 {
        let batch: Vec<(Nat, Val)> = (0..20).map(|j| (Nat::S(b * 20 + j + 1), obj(b * 20 + j + 1, 0))).collect();
        for lid in [1u64, 2] {
            db.put_objects_batch(&LayoutRef::Id(lid), batch.clone(), NL, false, 10_000).unwrap();
        }
    }
    // read first (builds the v indexes), then write (incremental maintenance)
    for lid in [1u64, 2] {
        let o = db.get_object(&LayoutRef::Id(lid), &Nat::S(500), NL, false).unwrap().unwrap();
        assert_eq!(o.get("b"), Some(&Val::str("o500g0")));
    }
    for k in [3u64, 500, 999] {
        for lid in [1u64, 2] {
            db.put_object(&LayoutRef::Id(lid), &Nat::S(k), &obj(k, 1), NL, false).unwrap();
        }
    }
    for lid in [1u64, 2] {
        db.delete_object(&LayoutRef::Id(lid), &Nat::S(42), NL, false).unwrap();
    }
    for reopen in [false, true] {
        if reopen {
            db.commit().unwrap();
            db.close(true).unwrap();
            db = Db::open(&p).unwrap();
        }
        for lid in [1u64, 2] {
            let lr = LayoutRef::Id(lid);
            for k in [1u64, 3, 42, 500, 777, 999, 1000, 1001] {
                let got = db.get_object(&lr, &Nat::S(k), NL, false).unwrap();
                let occ = db.occupied_n(&lr, &Nat::S(k), NL).unwrap();
                match k {
                    42 | 1001 => {
                        assert!(got.is_none(), "lid {lid} obj {k}");
                        assert!(occ.is_empty());
                    }
                    _ => {
                        let gn = if [3, 500, 999].contains(&k) { 1 } else { 0 };
                        let o = got.unwrap();
                        assert_eq!(o.get("a"), Some(&Val::Int((k * 10 + gn) as i64)), "lid {lid} obj {k}");
                        assert_eq!(o.get("b"), Some(&Val::str(format!("o{k}g{gn}"))));
                        assert_eq!(occ.len(), 2);
                        let view = db.get_record_view(&lr, &Nat::S(k), NL, None).unwrap();
                        assert!(view.is_truthy());
                    }
                }
            }
        }
    }
}
