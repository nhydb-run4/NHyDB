//! End-to-end self-test (build + reopen + compact) of a mixed realm with
//! CLASSIC, COLUMN, PACKED and a native ZigZag CUSTOM plugin.

use std::path::PathBuf;
use std::sync::Arc;

use nhydb_core::db::layout::EvolveSpec;
use nhydb_core::plugin::{LayoutPlugin, PluginMethod};
use nhydb_core::schema::LayoutDef;
use nhydb_core::*;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("nhydb_rust_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    // remove the DB file triad plus the lock file
    for suffix in ["", ".layout.enc", ".layout.key", ".lock"] {
        let _ = std::fs::remove_file(dir.join(format!("{name}{suffix}")));
    }
    let _ = std::fs::remove_file(dir.join(format!("{name}.compacting")));
    p
}

fn n(x: u64) -> Nat {
    Nat::S(x)
}

fn i(x: i64) -> Val {
    Val::Int(x)
}

fn s(x: &str) -> Val {
    Val::str(x)
}

fn field(name: &str, typ: &str, offset: i64) -> Val {
    nhydb_core::vmap! {"name" => name, "type" => typ, "offset" => offset}
}

fn fields() -> Vec<Val> {
    vec![field("id", "varuint", 0), field("value", "varint", 1), field("label", "utf8", 2)]
}

fn sample(k: i64) -> Val {
    nhydb_core::vmap! {"id" => k, "value" => k * 7 - 30, "label" => format!("obj-{k}")}
}

struct ZigZag;

impl ZigZag {
    fn p(lay: &LayoutDef, k: &str) -> u64 {
        lay.params.get(k).and_then(|v| v.as_i64()).unwrap() as u64
    }
    fn field<'a>(lay: &'a LayoutDef, f: &Val) -> Result<&'a Val> {
        let fields = lay.params.get("fields").and_then(|v| v.as_list()).unwrap();
        match f {
            Val::None => Ok(&fields[0]),
            Val::Int(o) => fields
                .iter()
                .find(|x| x.get("offset").and_then(|v| v.as_i64()) == Some(*o))
                .ok_or_else(|| Error::Schema("CUSTOM field unknown".into())),
            other => fields
                .iter()
                .find(|x| x.get("name").and_then(|v| v.as_str()) == other.as_str())
                .ok_or_else(|| Error::Schema(format!("CUSTOM field {} unknown", other.py_repr()))),
        }
    }
    fn decode(lay: &LayoutDef, addr: &Val) -> (u64, u64, Option<Val>) {
        let (base_u, lanes, stride) = (Self::p(lay, "base_u"), Self::p(lay, "lanes"), Self::p(lay, "stride"));
        let u = addr.get("u").unwrap().as_i64().unwrap() as u64;
        let v = addr.get("v_local").or_else(|| addr.get("v")).unwrap().as_i64().unwrap() as u64;
        let off = (v - 1) % stride;
        let record = ((v - 1) / stride) * lanes + (u - base_u) + 1;
        let f = lay
            .params
            .get("fields")
            .and_then(|x| x.as_list())
            .unwrap()
            .iter()
            .find(|x| x.get("offset").and_then(|v| v.as_i64()) == Some(off as i64))
            .cloned();
        (record, off, f)
    }
}

impl LayoutPlugin for ZigZag {
    fn has(&self, m: PluginMethod) -> bool {
        m != PluginMethod::OccupiedAddresses && m != PluginMethod::Spacemap
    }
    fn logical_address(&self, lay: &LayoutDef, obj: &Nat, field: &Val, lane: Lane) -> Result<Val> {
        let f = Self::field(lay, field)?;
        let (base_u, lanes, stride) = (Self::p(lay, "base_u"), Self::p(lay, "lanes"), Self::p(lay, "stride"));
        let o = obj.as_u64().unwrap();
        let idx = (o - 1) % lanes;
        let row = (o - 1) / lanes;
        let off = f.get("offset").and_then(|v| v.as_i64()).unwrap() as u64;
        Ok(nhydb_core::vmap! {
            "realm" => &lay.realm_id, "u" => (base_u + idx) as i64,
            "v" => (row * stride + off + 1) as i64, "lane" => lane as i64,
        })
    }
    fn describe(&self, lay: &LayoutDef, addr: &Val) -> Result<Val> {
        let (record, off, f) = Self::decode(lay, addr);
        Ok(nhydb_core::vmap! {
            "kind" => "custom_zigzag", "record" => record as i64, "offset" => off as i64,
            "field" => f.as_ref().and_then(|x| x.get("name").cloned()),
            "type" => f.as_ref().and_then(|x| x.get("type").cloned()),
            "anchor" => off == 0,
        })
    }
    fn type_of(&self, lay: &LayoutDef, addr: &Val) -> Result<Val> {
        let (_, _, f) = Self::decode(lay, addr);
        Ok(f.and_then(|x| x.get("type").cloned()).unwrap_or(Val::None))
    }
    fn spacemap(&self, _lay: &LayoutDef) -> Result<Val> {
        unreachable!()
    }
    fn occupied_addresses(&self, _db: &Db, _lay: &LayoutDef, _o: &Nat, _l: Lane) -> Result<Vec<Val>> {
        unreachable!()
    }
}

fn route(db: &Db, r: &Nat, u: u64) -> Val {
    db.route_address(r, &n(u), None, NL).unwrap().get("layout_id").cloned().unwrap()
}

fn get_obj(db: &Db, lid: u64, k: u64) -> Val {
    db.get_object(&LayoutRef::Id(lid), &n(k), NL, false).unwrap().unwrap_or_else(|| panic!("missing {lid}/{k}"))
}

fn build(path: &PathBuf) {
    let mut db = Db::create(path, true, OpenOptions::default()).unwrap();
    let r = n(700);
    db.create_realm(&r, "Mixed Realm", "", None, &Nat::ZERO).unwrap();
    db.create_shape(
        1,
        &r,
        "base",
        vec![
            FieldDef::new(n(1), "id", TypeCode::VarUInt),
            FieldDef::new(n(2), "value", TypeCode::VarInt),
            FieldDef::new(n(3), "label", TypeCode::Utf8),
        ],
        "",
    )
    .unwrap();
    db.create_classic_layout(1, &r, "Classic fallback", 1, &n(10), &n(1), None, 0, "").unwrap();
    let col = |u: i64, name: &str, t: &str| nhydb_core::vmap! {"u" => u, "name" => name, "type" => t};
    db.create_column_layout(
        2,
        &r,
        "Column zone",
        vec![col(20_000, "id", "varuint"), col(20_001, "value", "varint"), col(20_002, "label", "utf8")],
        Some(&n(20_000)),
        0,
        "",
    )
    .unwrap();
    db.create_packed_layout(3, &r, "Packed zone", &n(30_000), &n(1000), fields(), 0, "").unwrap();
    db.create_custom_layout(
        4,
        &r,
        "Custom zigzag",
        "zigzag-v1",
        &n(40_000),
        Some(&n(40_015)),
        nhydb_core::vmap! {"base_u" => 40_000i64, "lanes" => 16i64, "stride" => 100i64, "fields" => Val::List(fields())},
        Some(nhydb_core::vmap! {"mapping" => "zigzag", "u" => "base_u+(record-1)%lanes", "v" => "floor((record-1)/lanes)*stride+offset+1"}),
        0,
        "",
    )
    .unwrap();
    db.register_layout_plugin("zigzag-v1", Arc::new(ZigZag)).unwrap();

    let diag = db.layout_diagnostics(Some(&r)).unwrap();
    let sm = diag.get("summary").unwrap();
    assert_eq!(sm.get("overlaps"), Some(&i(3)), "{}", diag.py_repr());
    assert_eq!(sm.get("conflicts"), Some(&i(0)));
    assert_eq!(sm.get("gaps"), Some(&i(0)));
    assert_eq!(route(&db, &r, 20_001), i(2));
    assert_eq!(route(&db, &r, 30_000), i(3));
    assert_eq!(route(&db, &r, 40_007), i(4));
    assert_eq!(route(&db, &r, 999), i(1));

    let objects: Vec<(Nat, Val)> = (1..=100).map(|k| (n(k as u64), sample(k))).collect();
    for lid in 1..=4u64 {
        let res = db.put_objects_batch(&LayoutRef::Id(lid), objects.clone(), NL, false, 10_000).unwrap();
        assert_eq!(res.get("objects"), Some(&i(100)));
    }
    for k in [1u64, 7, 16, 17, 59, 100] {
        let exp = sample(k as i64);
        for lid in 1..=4u64 {
            let got = get_obj(&db, lid, k);
            for (key, val) in exp.as_map().unwrap() {
                assert_eq!(got.get(key), Some(val), "layout {lid} obj {k} {}", got.py_repr());
            }
        }
    }
    for lid in 1..=4u64 {
        let member = db.logical_address(&LayoutRef::Id(lid), &n(59), &s("label"), NL).unwrap();
        let cell = db.get_n(&member.n, GetMode::Cell).unwrap();
        assert_eq!(cell.get("result"), Some(&s("cell")));
        assert_eq!(cell.get("value"), Some(&s("obj-59")), "{}", cell.py_repr());
        let rec = db.get_n(&member.n, GetMode::Record).unwrap();
        assert_eq!(rec.get("result"), Some(&s("record")));
        assert_eq!(rec.get("object"), Some(&i(59)));
        assert_eq!(rec.get("data").unwrap().get("label"), Some(&s("obj-59")));
    }
    // CLASSIC record_N with an empty anchor
    db.put_points(&r, vec![CellInput::new(n(150), n(3), s("sparse-only"))], NL, false, 10_000).unwrap();
    let anchor = db.record_anchor(&LayoutRef::Id(1), &n(150), NL).unwrap();
    let c = db.get_n(&anchor.n, GetMode::Cell).unwrap();
    assert_eq!(c.get("anchor"), Some(&Val::Bool(true)));
    assert_eq!(c.get("present"), Some(&Val::Bool(false)));
    assert_eq!(c.get("record_N"), Some(&Val::Str(anchor.n_ref())));
    let rr = db.get_n(&anchor.n, GetMode::Record).unwrap();
    assert_eq!(rr.get("present"), Some(&Val::Bool(true)));
    assert_eq!(rr.get("anchor_present"), Some(&Val::Bool(false)));
    assert_eq!(rr.get("data"), Some(&nhydb_core::vmap! {"label" => "sparse-only"}));
    assert_eq!(db.get_n(&anchor.n, GetMode::Auto).unwrap().get("result"), Some(&s("record")));
    for lid in 1..=4u64 {
        let a = db.logical_address(&LayoutRef::Id(lid), &n(59), &s("label"), NL).unwrap();
        let d = db.describe_n(&a.n).unwrap();
        assert_eq!(d.get("layout").unwrap().get("layout_id"), Some(&i(lid as i64)));
        assert_eq!(d.get("interpretation").unwrap().get("record"), Some(&i(59)));
        assert_eq!(d.get("interpretation").unwrap().get("field"), Some(&s("label")));
        assert_eq!(d.get("occupied"), Some(&Val::Bool(true)));
        let occ = db.occupied_from_n(&a.n, "BOTH").unwrap();
        assert_eq!(occ.get("count"), Some(&i(3)), "{lid} {}", occ.py_repr());
    }
    // Evolution
    db.define_extension(&r, &n(100), "f64", "score", &Val::None, "", false).unwrap();
    let add = |f: Val| EvolveSpec { add_fields: vec![f], ..Default::default() };
    let mut e = add(nhydb_core::vmap! {"u" => 20_003i64, "name" => "score", "type" => "f64"});
    e.description = Some("Column zone evolved".into());
    db.evolve_layout(&LayoutRef::Id(2), &e, false).unwrap();
    db.evolve_layout(&LayoutRef::Id(3), &add(nhydb_core::vmap! {"offset" => 3i64, "name" => "score", "type" => "f64"}), false).unwrap();
    let mut e4 = add(nhydb_core::vmap! {"offset" => 3i64, "name" => "score", "type" => "f64"});
    e4.name = Some("Custom zigzag evolved".into());
    db.evolve_layout(&LayoutRef::Id(4), &e4, false).unwrap();
    assert_eq!(db.get_layout(None, None, Some(&LayoutRef::Id(2))).unwrap().get("u_end"), Some(&i(20_003)));
    db.put_points(&r, vec![], NL, false, 1).unwrap();
    // put_extensions via put_points (put_extensions delegates to put_points)
    db.put_points(&r, vec![CellInput::new(n(59), n(100), Val::Float(9.5))], NL, false, 10_000).unwrap();
    for lid in 2..=4u64 {
        db.put_object(&LayoutRef::Id(lid), &n(59), &nhydb_core::vmap! {"score" => 9.5}, NL, false).unwrap();
    }
    assert_eq!(get_obj(&db, 1, 59).get("score"), Some(&Val::Float(9.5)));
    for lid in 2..=4u64 {
        assert_eq!(get_obj(&db, lid, 59).get("score"), Some(&Val::Float(9.5)));
    }
    // taking over occupied space is rejected
    let mut probe = LayoutSpec::new(5, r.clone(), "Unsafe priority probe", "CUSTOM");
    probe.u_start = n(40_007);
    probe.u_end = Some(n(40_007));
    probe.priority = 10;
    probe.plugin = "zigzag-v1".into();
    probe.params = nhydb_core::vmap! {"base_u" => 40_007i64, "lanes" => 1i64, "stride" => 100i64, "fields" => Val::List(fields())};
    assert!(matches!(db.create_layout(probe.clone()), Err(Error::Schema(_))));
    assert_eq!(route(&db, &r, 40_007), i(4));
    probe.name = "Empty priority probe".into();
    probe.u_start = n(50_000);
    probe.u_end = Some(n(50_000));
    probe.params = nhydb_core::vmap! {"base_u" => 50_000i64, "lanes" => 1i64, "stride" => 100i64, "fields" => Val::List(fields())};
    db.create_layout(probe).unwrap();
    let rt = db.route_address(&r, &n(50_000), None, NL).unwrap();
    assert_eq!(rt.get("layout_id"), Some(&i(5)));
    assert_eq!(rt.get("candidate_count"), Some(&i(2)));
    let bad = EvolveSpec { spacemap_update: Some(nhydb_core::vmap! {"record" => "wrong"}), ..Default::default() };
    assert!(matches!(db.evolve_layout(&LayoutRef::Id(3), &bad, false), Err(Error::Schema(_))));
    let l3 = db.get_layout(None, None, Some(&LayoutRef::Id(3))).unwrap();
    assert_eq!(l3.get("spacemap").unwrap().get("record"), Some(&s("floor((v-1)/span)+1")));
    db.checkpoint().unwrap();
    assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
    let h = db.health(true).unwrap();
    assert_eq!(h.get("ok"), Some(&Val::Bool(true)), "{}", h.py_repr());
    db.close(true).unwrap();
}

fn check_reopened(path: &PathBuf) {
    let mut db = Db::open(path).unwrap();
    db.register_layout_plugin("zigzag-v1", Arc::new(ZigZag)).unwrap();
    assert_eq!(db.get_layout(None, None, None).unwrap().as_list().unwrap().len(), 5);
    assert_eq!(get_obj(&db, 2, 59).get("score"), Some(&Val::Float(9.5)));
    assert_eq!(get_obj(&db, 3, 59).get("label"), Some(&s("obj-59")));
    assert_eq!(get_obj(&db, 4, 8).get("label"), Some(&s("obj-8")));
    let a = db.record_anchor(&LayoutRef::Id(1), &n(150), NL).unwrap();
    assert_eq!(db.get_n(&a.n, GetMode::Cell).unwrap().get("present"), Some(&Val::Bool(false)));
    assert_eq!(db.get_n(&a.n, GetMode::Record).unwrap().get("data"), Some(&nhydb_core::vmap! {"label" => "sparse-only"}));
    assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
}

#[test]
fn selftest_build_reopen_compact() {
    let p = tmp("selftest.nhydb");
    build(&p);
    check_reopened(&p);
    let out = tmp("selftest_compact.nhydb");
    let res = Db::compact_file(
        &p,
        &CompactOptions { output: Some(out.clone()), make_backup: false, rebuild_indexes: true, batch_records: 50, ..Default::default() },
    )
    .unwrap();
    assert_eq!(res.get("ok"), Some(&Val::Bool(true)));
    check_reopened(&out);
}

#[test]
fn commit_and_crash_tail() {
    let p = tmp("crash.nhydb");
    let r = n(1);
    {
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "R", "", None, &Nat::ZERO).unwrap();
        db.create_shape(1, &r, "s", vec![FieldDef::new(n(1), "x", TypeCode::VarUInt)], "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &n(1), None, 0, "").unwrap();
        let rows: Vec<Vec<Val>> = (0..10).map(|k| vec![i(k)]).collect();
        db.put_dense_batch(&r, &n(1), &DenseRows::Tuples(&rows), None, &[], NL, true).unwrap();
        // uncommitted append
        let rows2: Vec<Vec<Val>> = (0..5).map(|k| vec![i(100 + k)]).collect();
        db.put_dense_batch(&r, &n(11), &DenseRows::Tuples(&rows2), None, &[], NL, false).unwrap();
        db.close(false).unwrap(); // buffer written, but no commit
    }
    let db = Db::open(&p).unwrap();
    assert!(db.recovered_tail_bytes > 0);
    assert!(db.get(&r, &n(10), NL, None, None, true).unwrap().is_some());
    assert!(db.get(&r, &n(11), NL, None, None, true).unwrap().is_none());
    assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
    drop(db);
    // truncated block after a commit + garbage at the end
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"NHBLgarbage-garbage").unwrap();
    }
    let db = Db::open(&p).unwrap();
    assert_eq!(db.recovered_tail_bytes, 19);
    assert_eq!(db.verify(false).unwrap().get("ok"), Some(&Val::Bool(true)));
}

#[test]
fn corrupt_payload_detected() {
    let p = tmp("corrupt.nhydb");
    let r = n(1);
    let off;
    {
        let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
        db.create_realm(&r, "R", "", None, &Nat::ZERO).unwrap();
        db.create_shape(1, &r, "s", vec![FieldDef::new(n(1), "x", TypeCode::Utf8)], "").unwrap();
        db.create_classic_layout(1, &r, "c", 1, &Nat::ZERO, &n(1), None, 0, "").unwrap();
        let rows: Vec<Vec<Val>> = (0..10).map(|k| vec![s(&format!("value-{k}"))]).collect();
        let res = db.put_dense_batch(&r, &n(1), &DenseRows::Tuples(&rows), None, &[], NL, true).unwrap();
        off = res.get("block_offset").unwrap().as_i64().unwrap() as u64;
        db.close(true).unwrap();
    }
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).read(true).open(&p).unwrap();
        let mut b = [0u8; 1];
        f.seek(SeekFrom::Start(off + 70)).unwrap();
        f.read_exact(&mut b).unwrap();
        b[0] ^= 0xFF;
        f.seek(SeekFrom::Start(off + 70)).unwrap();
        f.write_all(&b).unwrap();
    }
    let db = Db::open(&p).unwrap();
    let v = db.verify(false).unwrap();
    assert_eq!(v.get("ok"), Some(&Val::Bool(false)));
    assert!(db.get(&r, &n(3), NL, None, None, true).is_err(), "CRC error must be detected on read");
}

#[test]
fn lanes_sparse_null_tombstone_big() {
    let p = tmp("lanes.nhydb");
    let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
    let r = n(23);
    db.create_realm(&r, "R23", "", None, &Nat::ZERO).unwrap();
    // v744 without v1..v743, separate lanes
    let cells = vec![CellInput::typed(n(14), n(744), s("links"), Some(TypeCode::Utf8))];
    db.put_points(&r, cells, NL, true, 10_000).unwrap();
    let cells = vec![CellInput::typed(n(14), n(744), s("rechts"), Some(TypeCode::Utf8))];
    db.put_points(&r, cells, NR, true, 10_000).unwrap();
    let nl = address::dynadron_n(&r, &n(14), &n(744), NL).unwrap();
    assert_eq!(nl.to_string().len(), 203);
    assert_eq!(db.get_cell_n(&nl, false).unwrap().unwrap().get("value"), Some(&s("links")));
    let nr = address::dynadron_n(&r, &n(14), &n(744), NR).unwrap();
    assert_eq!(db.get_cell_n(&nr, false).unwrap().unwrap().get("value"), Some(&s("rechts")));
    // null ≠ tombstone ≠ missing
    let big_v = address::realm_span().clone();
    let big_u = Nat::S(u64::MAX).mul(&Nat::S(1000));
    db.put_points(
        &r,
        vec![
            CellInput::typed(n(5), n(1), Val::None, Some(TypeCode::Bytes)),
            CellInput::typed(n(5), n(2), i(7), Some(TypeCode::VarInt)),
            CellInput::typed(big_u.clone(), big_v.clone(), Val::Big(num_bigint::BigInt::from(10u32).pow(60)), Some(TypeCode::VarUInt)),
        ],
        NL,
        false,
        10_000,
    )
    .unwrap();
    db.delete_point(&r, &n(5), &n(2), NL, false).unwrap();
    let c1 = db.get_cell(&r, &n(5), &i(1), NL, false).unwrap().unwrap();
    assert_eq!(c1.get("value"), Some(&Val::None));
    assert!(db.get_cell(&r, &n(5), &i(2), NL, false).unwrap().is_none());
    assert!(db.get_cell(&r, &n(5), &i(3), NL, false).unwrap().is_none());
    let cb = db.get_cell(&r, &big_u, &Val::nat(&big_v), NL, false).unwrap().unwrap();
    assert_eq!(cb.get("value"), Some(&Val::Big(num_bigint::BigInt::from(10u32).pow(60))));
    db.commit().unwrap();
    drop(db);
    let db = Db::open(&p).unwrap();
    let cb = db.get_cell(&r, &big_u, &Val::nat(&big_v), NL, false).unwrap().unwrap();
    assert_eq!(cb.get("value"), Some(&Val::Big(num_bigint::BigInt::from(10u32).pow(60))));
    assert!(db.get_cell(&r, &n(5), &i(2), NL, false).unwrap().is_none());
    let pts = db.iter_materialized_sparse_points(None, None).unwrap();
    assert_eq!(pts.len(), 4);
    assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
}

#[test]
fn indexes_and_find() {
    let p = tmp("index.nhydb");
    let mut db = Db::create(&p, true, OpenOptions::default()).unwrap();
    let r = n(500);
    db.create_realm(&r, "R", "", None, &Nat::ZERO).unwrap();
    let mut lage = FieldDef::new(n(2), "lage", TypeCode::U8);
    lage.enum_map = Some(vec![("A".into(), 0), ("B".into(), 1)]);
    db.create_shape(1, &r, "s", vec![FieldDef::new(n(1), "x", TypeCode::VarUInt), lage], "").unwrap();
    db.create_classic_layout(1, &r, "c", 1, &n(100), &n(1), None, 0, "").unwrap();
    db.define_extension(&r, &n(200), "varuint", "ext", &Val::None, "", false).unwrap();
    db.create_index(1, &r, "x", &s("x"), "").unwrap();
    db.create_index(2, &r, "lage", &s("lage"), "").unwrap();
    db.create_index(3, &r, "ext", &s("ext"), "").unwrap();
    let rows: Vec<Vec<Val>> = (1..=1000).map(|k| vec![i(k % 10), s(if k % 2 == 0 { "A" } else { "B" })]).collect();
    let ext: Vec<(Nat, Vec<(Val, Val)>)> = (1..=1000u64).filter(|k| k % 100 == 0).map(|k| (n(k), vec![(s("ext"), i(k as i64))])).collect();
    db.put_dense_batch(&r, &n(1), &DenseRows::Tuples(&rows), None, &ext, NL, false).unwrap();
    assert!(matches!(db.find_u(&r, &[("x".into(), Cond::Eq(i(3)))], NL, None, None), Err(Error::StaleIndex(_))));
    db.build_indexes(None, true).unwrap();
    let us = db.find_u(&r, &[("x".into(), Cond::Eq(i(3))), ("lage".into(), Cond::Eq(s("B")))], NL, None, None).unwrap();
    assert_eq!(us.len(), 100);
    assert!(us.iter().all(|u| u.as_u64().unwrap() % 10 == 3));
    let us = db.find_u(&r, &[("x".into(), Cond::Range(i(2), i(3)))], NL, Some(&n(1)), Some(&n(100))).unwrap();
    assert_eq!(us.len(), 20);
    let us = db.find_u(&r, &[("ext".into(), Cond::Eq(i(500)))], NL, None, None).unwrap();
    assert_eq!(us, vec![n(500)]);
    drop(db);
    let db = Db::open(&p).unwrap();
    let us = db.find_u(&r, &[("x".into(), Cond::Eq(i(3)))], NL, None, None).unwrap();
    assert_eq!(us.len(), 100);
    assert_eq!(db.verify(true).unwrap().get("ok"), Some(&Val::Bool(true)));
}
