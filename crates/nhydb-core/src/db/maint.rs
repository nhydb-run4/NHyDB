//! Wartung: Checkpoint, Verify, Health, Stats, Tail-Inspektion, Repair,
//! Kompaktierung.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::address::{lane_name, realm_span, realm_v_bounds, Lane};
use crate::codec::TypeCode;
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, get_vu_u64, Nat};
use crate::schema::LayoutKind;
use crate::storage::Storage;
use crate::val::Val;
use crate::vmap;

use super::dense::DenseRows;
use super::layout::LayoutSpec;
use super::sparse::CellInput;
use super::system;
use super::{Db, DenseSegment, OpenOptions, SVal, SparseSegment};
use crate::platform::{self, DbLock};

/// `fmt_bytes`: `"1,234.56 KiB"`.
pub fn fmt_bytes(n: u64) -> String {
    let mut x = n as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if x < 1024.0 || unit == "TiB" {
            let s = format!("{x:.2}");
            let (int, frac) = s.split_once('.').unwrap();
            let mut grouped = String::new();
            for (i, c) in int.chars().enumerate() {
                if i > 0 && (int.len() - i) % 3 == 0 {
                    grouped.push(',');
                }
                grouped.push(c);
            }
            return format!("{grouped}.{frac} {unit}");
        }
        x /= 1024.0;
    }
    n.to_string()
}

fn stamp() -> String {
    crate::platform::local_timestamp()
}

fn with_suffix_append(p: &Path, extra: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(extra);
    PathBuf::from(s)
}

/// Optionen für `compact_file`.
#[derive(Clone, Debug)]
pub struct CompactOptions {
    pub output: Option<PathBuf>,
    pub make_backup: bool,
    pub rebuild_indexes: bool,
    pub batch_records: usize,
    /// Öffnungsoptionen der Quelle (Sidecar-Pfade, Konfiguration, Sperre).
    pub open: OpenOptions,
}

impl Default for CompactOptions {
    fn default() -> Self {
        CompactOptions {
            output: None,
            make_backup: true,
            rebuild_indexes: true,
            batch_records: DEFAULT_BATCH_RECORDS,
            open: OpenOptions::default(),
        }
    }
}

impl Db {
    pub fn checkpoint(&mut self) -> Result<u64> {
        let mut realms = Val::map();
        for r in self.realms.keys() {
            let lane_obj = |lane: Lane| {
                let k = (r.clone(), lane);
                vmap! {
                    "segments" => self.dense.get(&k).map(|d| d.segs.len()).unwrap_or(0) as i64,
                    "max_u" => self.max_u.get(&k).cloned().unwrap_or_default(),
                }
            };
            realms.set(r.to_string(), vmap! {"NL" => lane_obj(0), "NR" => lane_obj(1)});
        }
        let mut ssb = Val::map();
        let mut keys: Vec<_> = self.secondary_source_block.iter().collect();
        keys.sort();
        for ((iid, lane), bid) in keys {
            ssb.set(format!("{iid}:{lane}"), Val::Int(*bid as i64));
        }
        let obj = vmap! {
            "version" => 11i64, "data_max_block_id" => self.data_max_block_id as i64,
            "realms" => realms, "secondary_source_block" => ssb,
        };
        let mut payload = CHECKPOINT_TAG.to_vec();
        payload.extend_from_slice(obj.to_json()?.as_bytes());
        let (off, _bid) = self.append_block(BT_CHECKPOINT, &payload, 0)?;
        self.checkpoint_offset = off;
        self.last_checkpoint_obj = obj;
        self.commit()?;
        Ok(off)
    }

    /// Strukturprüfung aller Blöcke (`verify`).
    pub fn verify(&self, deep: bool) -> Result<Val> {
        let t0 = Instant::now();
        let size = self.store.len();
        let mut off = DATA_START;
        let mut last = DATA_START;
        let (mut blocks, mut records, mut sec_refs) = (0i64, 0i64, 0i64);
        let mut errors: Vec<String> = Vec::new();
        while off < size {
            let r: Result<u64> = (|| {
                let (info, payload) = self.read_block(off)?;
                blocks += 1;
                match info.block_type {
                    BT_DATA_DENSE => {
                        if !payload.starts_with(DATA_TAG) {
                            return Err(Error::Corruption("DATA Tag falsch".into()));
                        }
                        let mut p = 4;
                        let realm = get_vu(&payload, &mut p)?;
                        let sid = get_vu_u64(&payload, &mut p, "DATA shape")?;
                        let u0 = get_vu(&payload, &mut p)?;
                        let cnt = get_vu_u64(&payload, &mut p, "DATA count")?;
                        let table_end = (cnt as usize)
                            .checked_add(1)
                            .and_then(|c| c.checked_mul(4))
                            .and_then(|t| t.checked_add(p))
                            .unwrap_or(usize::MAX);
                        if table_end > payload.len() {
                            return Err(Error::Corruption("DATA Offsettabelle unvollstaendig".into()));
                        }
                        let tl = p + cnt as usize * 4;
                        let end_last = u32::from_le_bytes(payload[tl..tl + 4].try_into().unwrap()) as usize;
                        if table_end + end_last != payload.len() {
                            return Err(Error::Corruption("DATA Recordbereich/Offsettabelle widersprechen sich".into()));
                        }
                        records += cnt as i64;
                        if deep {
                            let seg = DenseSegment {
                                realm_id: realm,
                                lane: crate::address::lane_from_flags(info.flags),
                                shape_id: sid,
                                u_end: if cnt == 0 { u0.clone() } else { u0.add_u64(cnt - 1) },
                                u_start: u0,
                                count: cnt,
                                block_id: info.block_id,
                                block_offset: info.offset,
                                payload_offset: info.offset + BLOCK_HEADER_SIZE as u64,
                                offsets_offset: info.offset + BLOCK_HEADER_SIZE as u64 + p as u64,
                                records_offset: info.offset + BLOCK_HEADER_SIZE as u64 + table_end as u64,
                                payload_len: info.payload_len,
                            };
                            for i in 0..cnt as usize {
                                let s = u32::from_le_bytes(payload[p + i * 4..p + i * 4 + 4].try_into().unwrap()) as usize;
                                let e = u32::from_le_bytes(payload[p + i * 4 + 4..p + i * 4 + 8].try_into().unwrap()) as usize;
                                if e < s || table_end + e > payload.len() {
                                    return Err(Error::Corruption("DATA Recordoffset ungueltig".into()));
                                }
                                self.decode_dense_record(&seg, &payload[table_end + s..table_end + e])?;
                            }
                        }
                    }
                    BT_SPARSE_CELLS => {
                        if !payload.starts_with(SPARSE_TAG) {
                            return Err(Error::Corruption("SPARSE Tag falsch".into()));
                        }
                        let mut p = 4;
                        let realm = get_vu(&payload, &mut p)?;
                        let u0 = get_vu(&payload, &mut p)?;
                        let u1 = get_vu(&payload, &mut p)?;
                        let groups = get_vu_u64(&payload, &mut p, "SPARSE groups")?;
                        let cells = get_vu_u64(&payload, &mut p, "SPARSE cells")?;
                        if groups == 0 || cells == 0 || u1 < u0 {
                            return Err(Error::Corruption("SPARSE Bereich/Gruppe ungueltig".into()));
                        }
                        let seg = SparseSegment {
                            realm_id: realm.clone(),
                            lane: crate::address::lane_from_flags(info.flags),
                            u_start: u0,
                            u_end: u1,
                            group_count: groups,
                            cell_count: cells,
                            block_id: info.block_id,
                            block_offset: info.offset,
                            payload_offset: info.offset + BLOCK_HEADER_SIZE as u64,
                            entries_offset: info.offset + BLOCK_HEADER_SIZE as u64 + p as u64,
                            payload_len: info.payload_len,
                        };
                        let idx = self.sparse_block_index(&seg)?;
                        if idx.len() as u64 != groups {
                            return Err(Error::Corruption("SPARSE Gruppenzahl widerspricht Stream".into()));
                        }
                        if deep {
                            for (u, abs, len) in idx.iter() {
                                let rel = (abs - seg.payload_offset) as usize;
                                self.decode_sparse_record(&realm, u, &payload[rel..rel + *len as usize])?;
                            }
                        }
                    }
                    BT_SECONDARY if deep => {
                        let mut p = 4;
                        let _iid = get_vu(&payload, &mut p)?;
                        let _src = get_vu(&payload, &mut p)?;
                        let entries = get_vu_u64(&payload, &mut p, "SECONDARY entries")?;
                        sec_refs += entries as i64;
                    }
                    _ => {}
                }
                Ok(info.next_offset)
            })();
            match r {
                Ok(next) => {
                    last = next;
                    off = next;
                }
                Err(e) => {
                    errors.push(format!("bei Byte {off}: {e}"));
                    break;
                }
            }
        }
        if last != size {
            errors.push(format!("letztes gueltiges Ende {last} != Dateigroesse {size}"));
        }
        for (iid, idx) in &self.index_defs {
            for lane in [0u8, 1] {
                if self.secondary_source_block.contains_key(&(*iid, lane)) && !self.index_is_fresh(*iid, lane) {
                    errors.push(format!("Index {}/{} ist stale", idx.name, lane_name(lane)));
                }
            }
        }
        let n_err = errors.len() as i64;
        errors.truncate(100);
        Ok(vmap! {
            "ok" => n_err == 0, "seconds" => t0.elapsed().as_secs_f64(),
            "file_bytes" => size as i64, "file_size" => fmt_bytes(size),
            "scanned_blocks" => blocks, "physical_records" => records,
            "secondary_directory_entries" => sec_refs, "error_count" => n_err,
            "errors" => Val::List(errors.into_iter().map(Val::Str).collect()),
        })
    }

    /// Prüft den physischen Dateischwanz ohne die Datenbank zu öffnen.
    pub fn inspect_tail(path: impl AsRef<Path>) -> Result<Val> {
        let p = path.as_ref();
        if !p.exists() {
            return Err(Error::NotFound(p.display().to_string()));
        }
        let f = Storage::open_readonly(p)?;
        let size = f.metadata()?.len();
        let read_at = |buf: &mut [u8], off: u64| platform::read_exact_at(&f, buf, off);
        let mut off = DATA_START;
        let mut last = DATA_START;
        let (mut blocks, mut maxid) = (0i64, 0u64);
        let mut error: Option<String> = None;
        while off < size {
            if size - off < BLOCK_HEADER_SIZE as u64 {
                error = Some(format!("unvollstaendiger Blockkopf ab Byte {off}"));
                break;
            }
            let mut raw = [0u8; BLOCK_HEADER_SIZE];
            read_at(&mut raw, off)?;
            let h = match unpack_block_header(&raw) {
                Ok(h) => h,
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            };
            let pstart = off + BLOCK_HEADER_SIZE as u64;
            if pstart.checked_add(h.payload_len).map(|e| e > size).unwrap_or(true) {
                error = Some(format!("Block {}: Payload unvollstaendig", h.block_id));
                break;
            }
            let mut payload = vec![0u8; h.payload_len as usize];
            read_at(&mut payload, pstart)?;
            if crc32(&payload) != h.payload_crc {
                error = Some(format!("Block {}: Payload CRC falsch", h.block_id));
                break;
            }
            let nxt = align8(pstart + h.payload_len);
            if nxt > size {
                error = Some(format!("Block {}: Ende unvollstaendig", h.block_id));
                break;
            }
            last = nxt;
            maxid = maxid.max(h.block_id);
            blocks += 1;
            off = nxt;
        }
        Ok(vmap! {
            "file_bytes" => size as i64, "last_good_end" => last as i64,
            "trailing_bad_bytes" => (size - last) as i64, "blocks" => blocks,
            "max_block_id" => maxid as i64, "error" => error.clone(),
            "clean" => error.is_none() && last == size,
        })
    }

    /// Schneidet einen defekten Schwanz ab und setzt beide Superblöcke neu
    /// (`repair_file`).
    ///
    /// v0.27-Korrekturen: (S4) Es werden nur Blöcke übernommen, deren
    /// Schema durch eine im Layoutplan vorhandene Generation gedeckt ist –
    /// Datenblöcke, die auf nie veröffentlichte Realms/Shapes/Indizes
    /// verweisen, und SYSTEM-Blöcke ohne Plan-Generation werden abgeschnitten;
    /// (S5) Sidecar-Pfade und Konfiguration werden für Prüfung und
    /// Indexneuaufbau durchgereicht.
    pub fn repair_file(path: impl AsRef<Path>, apply: bool, make_backup: bool, rebuild_indexes: bool) -> Result<Val> {
        Self::repair_file_with(path, apply, make_backup, rebuild_indexes, &OpenOptions::default())
    }

    pub fn repair_file_with(
        path: impl AsRef<Path>,
        apply: bool,
        make_backup: bool,
        rebuild_indexes: bool,
        open: &OpenOptions,
    ) -> Result<Val> {
        let p = path.as_ref();
        let rep = Self::inspect_tail(p)?;
        if !apply || rep.get("clean").map(|v| v.is_truthy()).unwrap_or(false) {
            return Ok(rep);
        }
        let cfg = open.resolve_config(p)?;
        let lock = DbLock::acquire(p, cfg.lock)?;
        let last_good = rep.get("last_good_end").and_then(|v| v.as_i64()).unwrap_or(DATA_START as i64) as u64;
        let (safe_end, max_id, cut_reason) = Self::repair_safe_end(p, last_good, open, &cfg)?;
        let mut backup = Val::None;
        if make_backup {
            let b = with_suffix_append(p, &format!(".bak_{}", stamp()));
            std::fs::copy(p, &b)?;
            backup = Val::Str(b.display().to_string());
        }
        {
            let mut st = Storage::open(p, cfg.durability)?;
            st.truncate(safe_end)?;
            st.fsync()?;
            let t = super::now_ns();
            let s = SuperState {
                generation: 1,
                checkpoint_offset: 0,
                last_valid_end: safe_end,
                next_block_id: max_id + 1,
                created_ns: t,
                updated_ns: t,
                flags: 0,
            };
            let sb = pack_super(&s);
            st.write_at(0, &sb)?;
            st.write_at(SUPERBLOCK_SIZE as u64, &sb)?;
            st.fsync()?;
        }
        let mut rebuilt = Val::None;
        if rebuild_indexes {
            let mut db = Db::open_locked(p, open, cfg.clone(), DbLock::none(p))?;
            if !db.index_defs.is_empty() {
                rebuilt = db.build_indexes(None, false)?;
            }
            db.checkpoint()?;
            db.close(true)?;
        }
        drop(lock);
        let mut out = rep;
        out.set("applied", Val::Bool(true));
        out.set("backup", backup);
        out.set("rebuild", rebuilt);
        out.set("safe_end", Val::Int(safe_end as i64));
        out.set("cut_reason", cut_reason.map(Val::Str).unwrap_or(Val::None));
        out.set("after", Self::inspect_tail(p)?);
        Ok(out)
    }

    /// Bestimmt das sichere neue Dateiende für `repair_file` (S4).
    fn repair_safe_end(p: &Path, last_good: u64, open: &OpenOptions, cfg: &crate::config::Config) -> Result<(u64, u64, Option<String>)> {
        use std::collections::HashSet;
        let st = Storage::open(p, cfg.durability)?;
        let committed = super::best_super(&st).ok().map(|(_, s)| s.last_valid_end).filter(|e| *e <= last_good);
        // Blöcke sammeln
        struct B {
            off: u64,
            next: u64,
            bt: u8,
            bid: u64,
            payload: Vec<u8>,
        }
        let mut blocks = Vec::new();
        let mut off = DATA_START;
        while off < last_good {
            let mut raw = [0u8; BLOCK_HEADER_SIZE];
            st.read_exact_at(off, &mut raw)?;
            let h = unpack_block_header(&raw)?;
            let next = align8(off + BLOCK_HEADER_SIZE as u64 + h.payload_len);
            let want = matches!(h.block_type, BT_SYSTEM | BT_DATA_DENSE | BT_SPARSE_CELLS | BT_DELETE | BT_SECONDARY);
            let payload = if want {
                let n = if h.block_type == BT_SYSTEM { h.payload_len as usize } else { (h.payload_len as usize).min(96) };
                st.read_vec(off + BLOCK_HEADER_SIZE as u64, n)?
            } else {
                Vec::new()
            };
            blocks.push(B { off, next, bt: h.block_type, bid: h.block_id, payload });
            off = next;
        }
        // R0-Kette und Plan
        let mut first: Option<Val> = None;
        let mut prev: Option<Val> = None;
        let mut systems: Vec<(usize, Val)> = Vec::new();
        let mut end_idx = blocks.len();
        let mut reason: Option<String> = None;
        for (i, b) in blocks.iter().enumerate() {
            if b.bt != BT_SYSTEM {
                continue;
            }
            let item = std::str::from_utf8(&b.payload).ok().and_then(|t| Val::from_json(t).ok());
            match item {
                Some(it) if system::check_record(&it, prev.as_ref()).is_ok() => {
                    if first.is_none() {
                        first = Some(it.clone());
                    }
                    prev = Some(it.clone());
                    systems.push((i, it));
                }
                _ => {
                    end_idx = i;
                    reason = Some(format!("ungueltiger R0-Block bei Byte {}", b.off));
                    break;
                }
            }
        }
        let first = first.ok_or_else(|| Error::Corruption("repair: R0,u1 Bootstrap fehlt".into()))?;
        let cell = |r: &Val, k: &str| r.get("cells").and_then(|c| c.get(k)).cloned().unwrap_or(Val::None);
        let db_id = cell(&first, "1").py_str();
        let db_name = cell(&first, "2").py_str();
        let (lp, kp) = open.sidecars(p, cfg);
        let key = system::key_load(&kp, &db_id, &db_name)?;
        let entries = system::read_envelope(&lp, &db_id, &key)?;
        let in_plan = |r: &Val| {
            let g = cell(r, "5").as_i64();
            let h = cell(r, "6").py_str();
            entries.iter().find(|e| system::entry_generation(e).ok().map(|x| x as i64) == g && system::entry_digest(e).ok() == Some(h.as_str()))
        };
        let mut active: Option<&Val> = None;
        for (i, rec) in &systems {
            if *i >= end_idx {
                break;
            }
            match in_plan(rec) {
                Some(e) => active = Some(e),
                None => {
                    end_idx = *i;
                    reason = Some(format!("R0-Generation {} nicht im Layoutplan (Byte {})", cell(rec, "5").py_repr(), blocks[*i].off));
                    break;
                }
            }
        }
        let active = active.ok_or_else(|| Error::Corruption("repair: keine Layoutplan-Generation passt zu R0,u1".into()))?;
        let snap = system::open_entry(&db_id, active, &key)?;
        let ids = |k: &str, f: &str| -> HashSet<String> {
            snap.get(k).and_then(|v| v.as_list()).into_iter().flatten().filter_map(|r| r.get(f).map(|x| x.py_repr())).collect()
        };
        let realms = ids("realms", "realm_id");
        let shapes = ids("shapes", "shape_id");
        let indexes = ids("indexes", "index_id");
        for (i, b) in blocks.iter().enumerate().take(end_idx) {
            let mut q = 4usize;
            let ok = match b.bt {
                BT_DATA_DENSE => {
                    let r = get_vu(&b.payload, &mut q).ok();
                    let sh = get_vu(&b.payload, &mut q).ok();
                    matches!((r, sh), (Some(r), Some(sh)) if realms.contains(&r.to_string()) && shapes.contains(&sh.to_string()))
                }
                BT_SPARSE_CELLS => get_vu(&b.payload, &mut q).ok().map(|r| realms.contains(&r.to_string())).unwrap_or(false),
                BT_DELETE => {
                    let mut q0 = 0usize;
                    get_vu(&b.payload, &mut q0).ok().map(|r| realms.contains(&r.to_string())).unwrap_or(false)
                }
                BT_SECONDARY => get_vu(&b.payload, &mut q).ok().map(|x| indexes.contains(&x.to_string())).unwrap_or(false),
                _ => true,
            };
            if !ok {
                end_idx = i;
                reason = Some(format!(
                    "Block {} ({}) verweist auf nicht veroeffentlichtes Schema (Byte {})",
                    b.bid,
                    block_name(b.bt).unwrap_or("?"),
                    b.off
                ));
                break;
            }
        }
        let safe_end = if end_idx == 0 { DATA_START } else { blocks[end_idx - 1].next };
        if let Some(c) = committed {
            if safe_end < c {
                return Err(Error::Corruption(format!(
                    "repair: committeter Bereich bis Byte {c} ist inkonsistent ({}); keine automatische Reparatur",
                    reason.unwrap_or_default()
                )));
            }
        }
        let max_id = blocks.iter().take(end_idx).map(|b| b.bid).max().unwrap_or(0);
        Ok((safe_end, max_id, reason))
    }

    pub fn health(&self, deep: bool) -> Result<Val> {
        let t0 = Instant::now();
        let mut checks = Vec::new();
        let verify = self.verify(deep)?;
        let vok = verify.get("ok").map(|v| v.is_truthy()).unwrap_or(false);
        checks.push(vmap! {
            "name" => "container", "status" => if vok { "ok" } else { "error" },
            "details" => vmap! {"blocks" => verify.get("scanned_blocks").cloned(), "errors" => verify.get("errors").cloned()},
        });
        let mut layout_errors: Vec<Val> = Vec::new();
        for (lid, lay) in &self.layouts {
            if !self.realms.contains_key(&lay.realm_id) {
                layout_errors.push(Val::Str(format!("Layout {lid}: Realm {} fehlt", lay.realm_id)));
            }
            if lay.u_start.is_zero() || lay.u_end.as_ref().map(|e| e < &lay.u_start).unwrap_or(false) {
                layout_errors.push(Val::Str(format!("Layout {lid}: u-Bereich ungueltig")));
            }
            match lay.kind {
                LayoutKind::Classic => {
                    if lay.shape_id.map(|s| !self.shapes.contains_key(&s)).unwrap_or(true) {
                        layout_errors.push(Val::Str(format!("Layout {lid}: CLASSIC Shape fehlt")));
                    }
                }
                LayoutKind::Column => {
                    if lay.fields().is_empty() {
                        layout_errors.push(Val::Str(format!("Layout {lid}: COLUMN ohne Felder")));
                    }
                }
                LayoutKind::Packed => {
                    if lay.packed_span().is_err() || lay.packed_u().is_err() {
                        layout_errors.push(Val::Str(format!("Layout {lid}: PACKED Parameter ungueltig")));
                    }
                }
                LayoutKind::Custom => {
                    if lay.plugin.is_empty() {
                        layout_errors.push(Val::Str(format!("Layout {lid}: CUSTOM ohne Plugin-ID")));
                    }
                }
            }
        }
        checks.push(vmap! {
            "name" => "layouts", "status" => if layout_errors.is_empty() { "ok" } else { "error" },
            "details" => vmap! {"count" => self.layouts.len() as i64, "errors" => Val::List(layout_errors)},
        });
        let mut sm_err: Vec<Val> = Vec::new();
        let mut sm_warn: Vec<Val> = Vec::new();
        for (lid, lay) in &self.layouts {
            match self.effective_spacemap(lay) {
                Ok(eff) => {
                    if lay.kind.is_builtin() && lay.spacemap != eff {
                        sm_err.push(Val::Str(format!("Layout {lid}: persistierte SpaceMap weicht von Mappinglogik ab")));
                    } else if lay.kind == LayoutKind::Custom {
                        if let Some(p) = self.plugin_for(lay) {
                            if p.has(crate::plugin::PluginMethod::Spacemap) && lay.spacemap != eff {
                                sm_warn.push(Val::Str(format!(
                                    "Layout {lid}: CUSTOM Plugin-SpaceMap weicht vom persistierten Snapshot ab"
                                )));
                            }
                        }
                    }
                }
                Err(e) => sm_err.push(Val::Str(format!("Layout {lid}: SpaceMap nicht ableitbar: {e}"))),
            }
        }
        let sm_status = if !sm_err.is_empty() { "error" } else if !sm_warn.is_empty() { "warning" } else { "ok" };
        checks.push(vmap! {
            "name" => "spacemap", "status" => sm_status,
            "details" => vmap! {"errors" => Val::List(sm_err), "warnings" => Val::List(sm_warn)},
        });
        let routing = self.layout_diagnostics(None)?;
        let summary = routing.get("summary").cloned().unwrap_or_default();
        let conflicts = summary.get("conflicts").and_then(|v| v.as_i64()).unwrap_or(0);
        let mut missing = Vec::new();
        if let Some(Val::List(rs)) = routing.get("realms") {
            for r in rs {
                if let Some(Val::List(m)) = r.get("missing_runtime_plugins") {
                    missing.extend(m.iter().cloned());
                }
            }
        }
        let rstatus = if conflicts > 0 || !missing.is_empty() { "warning" } else { "ok" };
        checks.push(vmap! {
            "name" => "layout_routing", "status" => rstatus,
            "details" => vmap! {
                "overlaps" => summary.get("overlaps").cloned(), "conflicts" => conflicts,
                "gaps" => summary.get("gaps").cloned(), "missing_runtime_plugins" => Val::List(missing),
            },
        });
        let math_errors: Vec<Val> = match self.validate_math_layout_state() {
            Ok(()) => vec![],
            Err(e) => vec![Val::Str(e.to_string())],
        };
        checks.push(vmap! {
            "name" => "math_layouts", "status" => if math_errors.is_empty() { "ok" } else { "error" },
            "details" => vmap! {"count" => self.math_layouts.len() as i64, "errors" => Val::List(math_errors)},
        });
        let mut ext_errors = Vec::new();
        for (rid, v) in self.extension_defs.keys() {
            if !self.realms.contains_key(rid) {
                ext_errors.push(Val::Str(format!("Extension R{rid}/v{v}: Realm fehlt")));
            }
            if v.is_zero() || v > realm_span() {
                ext_errors.push(Val::Str(format!("Extension R{rid}/v{v}: ausserhalb Realm")));
            }
        }
        checks.push(vmap! {
            "name" => "definitions", "status" => if ext_errors.is_empty() { "ok" } else { "error" },
            "details" => vmap! {"extensions" => self.extension_defs.len() as i64, "errors" => Val::List(ext_errors)},
        });
        let mut stale = Vec::new();
        let mut index_errors = Vec::new();
        for (iid, idef) in &self.index_defs {
            if let Err(e) = crate::textindex::normalize_options(idef.kind, &idef.options)
                .and_then(|_| self.resolve_v(&idef.realm_id, &Val::nat(idef.field_v())))
            {
                index_errors.push(Val::Str(format!("{}: {e}", idef.name)));
            }
            for lane in [0u8, 1] {
                if self.secondary_source_block.contains_key(&(*iid, lane)) && !self.index_is_fresh(*iid, lane) {
                    stale.push(Val::Str(format!("{}/{}", idef.name, lane_name(lane))));
                }
            }
        }
        checks.push(vmap! {
            "name" => "indexes", "status" => if stale.is_empty() && index_errors.is_empty() { "ok" } else { "error" },
            "details" => vmap! {"count" => self.index_defs.len() as i64, "stale" => Val::List(stale), "errors" => Val::List(index_errors)},
        });
        checks.push(vmap! {
            "name" => "checkpoint", "status" => if self.checkpoint_offset != 0 { "ok" } else { "warning" },
            "details" => vmap! {"offset" => self.checkpoint_offset as i64, "generation" => self.sup.generation as i64},
        });
        let has_err = checks.iter().any(|c| c.get("status").and_then(|s| s.as_str()) == Some("error"));
        let has_warn = checks.iter().any(|c| c.get("status").and_then(|s| s.as_str()) == Some("warning"));
        Ok(vmap! {
            "status" => if has_err { "unhealthy" } else if has_warn { "warning" } else { "healthy" },
            "ok" => !has_err, "deep" => deep, "engine" => ENGINE_VERSION, "format" => FILE_FORMAT_VERSION,
            "file" => self.path.display().to_string(), "seconds" => t0.elapsed().as_secs_f64(),
            "checks" => Val::List(checks),
        })
    }

    fn physical_block_stats(&self) -> Result<BTreeMap<&'static str, (i64, i64, i64)>> {
        let mut by: BTreeMap<&'static str, (i64, i64, i64)> = BTreeMap::new();
        let size = self.store.len();
        let mut off = DATA_START;
        while off < size {
            let info = self.read_header_at(off)?;
            let name = block_name(info.block_type).unwrap();
            let e = by.entry(name).or_default();
            e.0 += 1;
            e.1 += (info.next_offset - info.offset) as i64;
            e.2 += info.payload_len as i64;
            off = info.next_offset;
        }
        Ok(by)
    }

    pub fn stats(&self) -> Result<Val> {
        let mut realm_stats = Vec::new();
        for (rid, r) in &self.realms {
            let (vlo, vhi) = realm_v_bounds(rid)?;
            let mut lanes = Val::map();
            for lane in [0u8, 1] {
                let key = (rid.clone(), lane);
                let overlay = self.overlay_keys.contains(&key);
                let dsegs = self.dense.get(&key).map(|d| d.segs.as_slice()).unwrap_or(&[]);
                let ssegs = self.sparse.get(&key).map(|d| d.segs.as_slice()).unwrap_or(&[]);
                let live = if overlay { Val::None } else { Val::Int(dsegs.iter().map(|s| s.count as i64).sum()) };
                lanes.set(
                    lane_name(lane),
                    vmap! {
                        "lane" => lane as i64, "dense_segments" => dsegs.len() as i64,
                        "max_u" => self.max_u.get(&key).cloned().unwrap_or_default(),
                        "live_records" => live, "has_overlays" => overlay,
                        "sparse_segments" => ssegs.len() as i64,
                        "sparse_u_groups" => ssegs.iter().map(|s| s.group_count as i64).sum::<i64>(),
                        "materialized_sparse_cells" => ssegs.iter().map(|s| s.cell_count as i64).sum::<i64>(),
                    },
                );
            }
            realm_stats.push(vmap! {
                "realm_id" => rid, "name" => r.name.as_str(), "default_shape_id" => r.default_shape_id.map(|x| x as i64),
                "lanes" => lanes, "base_reserve_to" => &r.base_reserve_to,
                "extension_definitions" => self.extension_defs.keys().filter(|(rr, _)| rr == rid).count() as i64,
                "layouts" => Val::List(self.layouts_by_realm.get(rid).map(|v| v.iter().map(|x| Val::Int(*x as i64)).collect()).unwrap_or_default()),
                "local_v_min" => 1i64, "local_v_max" => realm_span(), "global_v_min" => vlo, "global_v_max" => vhi,
            });
        }
        let mut idx_stats = Vec::new();
        for (iid, idx) in &self.index_defs {
            let mut ls = Val::map();
            for lane in [0u8, 1] {
                let mut entries = self.secondary_entry_count(*iid, lane) as i64;
                if self.secondary_source_block.contains_key(&(*iid, lane)) && idx.kind == crate::textindex::IndexKind::Fulltext {
                    entries = (entries - 1).max(0);
                }
                ls.set(
                    lane_name(lane),
                    vmap! {
                        "distinct_values" => entries,
                        "directory_loaded" => self.secondary_directory_loaded(*iid, lane),
                        "fresh" => self.index_is_fresh(*iid, lane),
                        "source_block_id" => self.secondary_source_block.get(&(*iid, lane)).map(|x| *x as i64),
                    },
                );
            }
            idx_stats.push(vmap! {
                "index_id" => *iid as i64, "realm_id" => &idx.realm_id, "name" => idx.name.as_str(),
                "kind" => idx.kind.name(), "v" => idx.field_v(), "options" => idx.options.clone(), "lanes" => ls,
            });
        }
        let phys = self.physical_block_stats()?;
        let sparse_bytes = phys.get("SPARSE_CELLS").map(|e| e.1).unwrap_or(0);
        let sparse_cells: i64 = self.sparse.values().flat_map(|s| s.segs.iter()).map(|s| s.cell_count as i64).sum();
        let sparse_groups: i64 = self.sparse.values().flat_map(|s| s.segs.iter()).map(|s| s.group_count as i64).sum();
        let dense_records: i64 = self.dense.values().flat_map(|s| s.segs.iter()).map(|s| s.count as i64).sum();
        let mut by_type = Val::map();
        for (k, (b, by, pb)) in &phys {
            by_type.set(*k, vmap! {"blocks" => *b, "bytes" => *by, "payload_bytes" => *pb});
        }
        let storage = vmap! {
            "superblocks_bytes" => DATA_START as i64, "by_block_type" => by_type,
            "physical_dense_records" => dense_records, "materialized_sparse_cells" => sparse_cells,
            "sparse_u_groups" => sparse_groups, "sparse_bytes" => sparse_bytes,
            "sparse_bytes_per_cell" => if sparse_cells > 0 { Val::Float(sparse_bytes as f64 / sparse_cells as f64) } else { Val::None },
        };
        let size = self.store.len();
        Ok(vmap! {
            "file" => self.path.display().to_string(), "engine" => ENGINE_VERSION, "format" => FILE_FORMAT_VERSION,
            "realm_span" => realm_span(), "max_db_size_bytes" => Val::int(self.max_db_size_bytes as i128),
            "file_bytes" => size as i64, "file_size" => fmt_bytes(size),
            "generation" => self.sup.generation as i64, "checkpoint_offset" => self.checkpoint_offset as i64,
            "committed_end" => self.sup.last_valid_end as i64, "current_end" => self.last_valid_end as i64,
            "recovered_tail_bytes" => self.recovered_tail_bytes as i64,
            "data_max_block_id" => self.data_max_block_id as i64, "storage" => storage,
            "realms" => Val::List(realm_stats),
            "shapes" => Val::List(self.shapes.values().map(|s| vmap! {
                "shape_id" => s.shape_id as i64, "realm_id" => &s.realm_id, "name" => s.name.as_str(),
                "field_count" => s.fields.len() as i64,
            }).collect()),
            "layouts" => self.get_layout(None, None, None)?,
            "math_layouts" => Val::List(self.math_layouts.keys().map(|lid| self.describe_math(*lid)).collect::<Result<Vec<_>>>()?),
            "extensions" => Val::List(self.extension_defs.values().map(|e| e.to_val()).collect()),
            "indexes" => Val::List(idx_stats),
            "posting_cache_entries" => self.cache().postings.len() as i64,
            "has_overlays" => self.has_overlays,
        })
    }

    // ------------------------------------------------------------------
    // Kompaktierung
    // ------------------------------------------------------------------

    /// Schreibt den aktuellen logischen Zustand in eine neue Datei
    /// (`compact_file`). Bei `output=None` wird das Original ersetzt.
    pub fn compact_file(path: impl AsRef<Path>, opts: &CompactOptions) -> Result<Val> {
        let src_path = path.as_ref().to_path_buf();
        if !src_path.exists() {
            return Err(Error::NotFound(src_path.display().to_string()));
        }
        let batch_records = opts.batch_records.max(1);
        let replace_original = opts.output.is_none();
        let final_path = opts.output.clone().unwrap_or_else(|| src_path.clone());
        let tmp_path = with_suffix_append(&final_path, ".compacting");
        let cfg = opts.open.resolve_config(&src_path)?;
        let mode = cfg.durability;
        // Sperren: Quelle für die gesamte Dauer inkl. atomarem Austausch,
        // beim Export zusätzlich das Ziel.
        let _src_lock = DbLock::acquire(&src_path, cfg.lock)?;
        let _dst_lock = if replace_original { None } else { Some(DbLock::acquire(&final_path, cfg.lock)?) };
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
        }
        if !replace_original && final_path.exists() {
            return Err(Error::Exists(format!("[Errno 17] File exists: '{}'", final_path.display())));
        }
        let before = std::fs::metadata(&src_path)?.len();
        let t0 = Instant::now();
        let mut written_records = 0i64;
        let mut written_batches = 0i64;
        let mut lane_counts = [0i64; 2];
        let (dst_layout, dst_key);
        {
            let src = Db::open_locked(&src_path, &opts.open, cfg.clone(), DbLock::none(&src_path))?;
            // Staging-Datei: in-place mit identischer R0-Identität und denselben
            // Sidecars; Export mit neuer DB-ID und eigenen Sidecars.
            let mut stage_opts = OpenOptions { internal_compaction: true, ..opts.open.clone() };
            if replace_original {
                system::write_initial_db(&tmp_path, &system::initial_record_from(&src.sys.record, src.sys.math_constants_stored), mode)?;
                stage_opts.layout_path = Some(src.sys.layout_path.clone());
                stage_opts.key_path = Some(src.sys.key_path.clone());
            } else {
                let base = OpenOptions { layout_path: None, key_path: None, ..opts.open.clone() };
                let (lp, kp) = base.sidecars(&final_path, &cfg);
                if lp.exists() || kp.exists() {
                    return Err(Error::Exists("Zieldatenbank hat bereits Layout- oder Key-Datei".into()));
                }
                let fname = |q: &Path| q.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let db_id = crate::crypto::uuid4()?;
                let key = system::key_create(&kp, &db_id, &fname(&final_path), mode)?;
                let entry = system::seal_entry(&db_id, 0, system::empty_schema().to_json()?.as_bytes(), &key)?;
                system::atomic_write(&lp, &system::envelope_bytes(&db_id, std::slice::from_ref(&entry), &key)?, mode)?;
                let rec = system::bootstrap_record(
                    &db_id,
                    &fname(&final_path),
                    &fname(&lp),
                    &fname(&kp),
                    0,
                    system::entry_digest(&entry)?,
                    super::now_ns(),
                );
                system::write_initial_db(&tmp_path, &rec, mode)?;
                stage_opts.layout_path = Some(lp);
                stage_opts.key_path = Some(kp);
            }
            let mut dst = Db::open_locked(&tmp_path, &stage_opts, cfg.clone(), DbLock::none(&tmp_path))?;
            dst_layout = dst.sys.layout_path.clone();
            dst_key = dst.sys.key_path.clone();
            // Kopie physischer Zellen auch in exklusive MATH-Realms (Fix S2).
            dst.internal_math_writes = true;
            let res = (|| -> Result<()> {
                if dst.max_db_size_bytes != src.max_db_size_bytes {
                    dst.set_max_db_size(src.max_db_size_bytes, false)?;
                }
                if !replace_original {
                    // Realm vollständig inkl. Default-Shape und Reserve (Fix S3).
                    for (rid, realm) in &src.realms {
                        dst.create_realm(rid, &realm.name, &realm.description, realm.default_shape_id, &realm.base_reserve_to)?;
                    }
                    for sh in src.shapes.values() {
                        dst.create_shape(sh.shape_id, &sh.realm_id, &sh.name, sh.fields.clone(), &sh.description)?;
                    }
                    for lay in src.layouts.values() {
                        dst.create_layout(LayoutSpec {
                            layout_id: lay.layout_id,
                            realm_id: lay.realm_id.clone(),
                            name: lay.name.clone(),
                            kind: lay.kind.name().into(),
                            u_start: lay.u_start.clone(),
                            u_end: lay.u_end.clone(),
                            priority: lay.priority,
                            shape_id: lay.shape_id,
                            base_reserve_to: lay.base_reserve_to.clone(),
                            params: lay.params.clone(),
                            spacemap: Some(lay.spacemap.clone()),
                            plugin: lay.plugin.clone(),
                            description: lay.description.clone(),
                            replace: false,
                        })?;
                    }
                    for ext in src.extension_defs.values() {
                        let en = ext.enum_map.as_ref().map(crate::codec::enum_to_val).unwrap_or(Val::None);
                        dst.define_extension(&ext.realm_id, &ext.v, ext.typ.name(), &ext.name, &en, &ext.description, false)?;
                    }
                    for idef in src.index_defs.values() {
                        let idx = dst.create_index_ext(
                            idef.index_id,
                            &idef.realm_id,
                            &idef.name,
                            &Val::nat(idef.field_v()),
                            &Val::str(idef.kind.name()),
                            &idef.options,
                            None,
                            None,
                            None,
                            &idef.description,
                        )?;
                        let _ = idx;
                    }
                    // Exakter MATH-Zustand (Definition, Fingerprint, RealmMap),
                    // keine Neuplanung.
                    for (lid, st) in &src.math_layouts {
                        dst.math_layouts.insert(*lid, st.clone());
                    }
                    if !src.math_layouts.is_empty() {
                        dst.sys.layout_dirty = true;
                    }
                }
                // Dense: lebende Records mit ihren gespeicherten BASE-Rohwerten.
                // Dense-EXT-Werte und Overlays folgen als Sparse-Zellen.
                let mut dense_present: std::collections::HashSet<(Nat, Lane, Nat)> = Default::default();
                let mut dense_ext: Vec<(Nat, Lane, Nat, Nat, Val)> = Vec::new();
                let mut shape_fields: std::collections::HashSet<(Nat, Lane, Nat, Nat)> = Default::default();
                let mut keys: Vec<_> = src.dense.keys().cloned().collect();
                keys.sort();
                for key in keys {
                    let (rid, lane) = key.clone();
                    let idx = &src.dense[&key];
                    let mut rows: Vec<Vec<Val>> = Vec::new();
                    let mut start: Option<Nat> = None;
                    let mut last_u: Option<Nat> = None;
                    let mut cur_shape: Option<u64> = None;
                    let flush = |dst: &mut Db, rows: &mut Vec<Vec<Val>>, start: &Option<Nat>, shape: Option<u64>| -> Result<usize> {
                        if rows.is_empty() {
                            return Ok(0);
                        }
                        let n = rows.len();
                        dst.put_dense_batch(&rid, start.as_ref().unwrap(), &DenseRows::Tuples(rows), shape, &[], lane, false)?;
                        rows.clear();
                        Ok(n)
                    };
                    let mut cursor: Option<Nat> = None;
                    for seg in idx.sorted() {
                        let mut u = match &cursor {
                            Some(c) if c > &seg.u_start => c.clone(),
                            _ => seg.u_start.clone(),
                        };
                        while u <= seg.u_end {
                            if let Some(win) = src.resolve_segment(&rid, &u, lane) {
                                let rec = src.with_record_bytes(win, &u, |raw| src.decode_dense_record(win, raw))?;
                                let shape = src.shape_of(win.shape_id)?;
                                let compatible = !rows.is_empty()
                                    && cur_shape == Some(win.shape_id)
                                    && last_u.as_ref().map(|l| l.add_u64(1) == u).unwrap_or(false)
                                    && rows.len() < batch_records;
                                if !rows.is_empty() && !compatible {
                                    let n = flush(&mut dst, &mut rows, &start, cur_shape)?;
                                    written_records += n as i64;
                                    lane_counts[lane as usize] += n as i64;
                                    written_batches += 1;
                                }
                                if rows.is_empty() {
                                    start = Some(u.clone());
                                    cur_shape = Some(win.shape_id);
                                }
                                rows.push(shape.fields.iter().map(|f| rec.base_by_v[&f.v].clone()).collect());
                                for f in &shape.fields {
                                    shape_fields.insert((rid.clone(), lane, u.clone(), f.v.clone()));
                                }
                                for (v, val) in &rec.extensions_by_v {
                                    dense_ext.push((rid.clone(), lane, u.clone(), v.clone(), val.clone()));
                                }
                                dense_present.insert((rid.clone(), lane, u.clone()));
                                last_u = Some(u.clone());
                                if rows.len() >= batch_records {
                                    let n = flush(&mut dst, &mut rows, &start, cur_shape)?;
                                    written_records += n as i64;
                                    lane_counts[lane as usize] += n as i64;
                                    written_batches += 1;
                                }
                            }
                            u = u.add_u64(1);
                        }
                        cursor = Some(u);
                    }
                    let n = flush(&mut dst, &mut rows, &start, cur_shape)?;
                    if n > 0 {
                        written_records += n as i64;
                        lane_counts[lane as usize] += n as i64;
                        written_batches += 1;
                    }
                }
                // Sparse: neuester Zustand; Tombstones nur, wo sie Dense-Werte verdecken.
                let states = src.latest_sparse_states()?;
                let mut by_key: BTreeMap<(Nat, Lane), Vec<CellInput>> = BTreeMap::new();
                for ((rid, lane, u, v), meta) in &states {
                    let cell = match &meta.value {
                        SVal::V(val) => CellInput::typed(u.clone(), v.clone(), val.clone(), if meta.selftyped { meta.typ } else { None }),
                        SVal::Tomb => {
                            let shadows = dense_present.contains(&(rid.clone(), *lane, u.clone()))
                                && (shape_fields.contains(&(rid.clone(), *lane, u.clone(), v.clone()))
                                    || dense_ext.iter().any(|(r, l, uu, vv, _)| r == rid && l == lane && uu == u && vv == v));
                            if !shadows {
                                continue;
                            }
                            CellInput::tombstone(u.clone(), v.clone())
                        }
                    };
                    by_key.entry((rid.clone(), *lane)).or_default().push(cell);
                }
                for (rid, lane, u, v, val) in dense_ext {
                    if !states.contains_key(&(rid.clone(), lane, u.clone(), v.clone())) {
                        by_key.entry((rid, lane)).or_default().push(CellInput::new(u, v, val));
                    }
                }
                for ((rid, lane), cells) in by_key {
                    let mut tombs = Vec::new();
                    let mut live = Vec::new();
                    for c in cells {
                        if c.tomb { tombs.push(c) } else { live.push(c) }
                    }
                    for chunk in live.chunks(50_000) {
                        dst.put_points(&rid, chunk.to_vec(), lane, false, 10_000)?;
                    }
                    for c in tombs {
                        dst.delete_point(&rid, &c.u, &c.v, lane, false)?;
                    }
                }
                if opts.rebuild_indexes && !dst.index_defs.is_empty() {
                    dst.build_indexes(None, false)?;
                }
                dst.checkpoint()?;
                let v = dst.verify(true)?;
                if !v.get("ok").map(|x| x.is_truthy()).unwrap_or(false) {
                    return Err(Error::Corruption(format!(
                        "Compaction-Zieldatei Verify fehlgeschlagen: {}",
                        v.get("errors").map(|e| e.py_repr()).unwrap_or_default()
                    )));
                }
                Ok(())
            })();
            let _ = dst.close(false);
            res?;
        }
        let final_verify = {
            let check_opts = OpenOptions {
                internal_compaction: true,
                layout_path: Some(dst_layout.clone()),
                key_path: Some(dst_key.clone()),
                ..opts.open.clone()
            };
            let check = Db::open_locked(&tmp_path, &check_opts, cfg.clone(), DbLock::none(&tmp_path))?;
            let v = check.verify(true)?;
            if !v.get("ok").map(|x| x.is_truthy()).unwrap_or(false) {
                return Err(Error::Corruption(format!(
                    "Compaction-Reopen Verify fehlgeschlagen: {}",
                    v.get("errors").map(|e| e.py_repr()).unwrap_or_default()
                )));
            }
            v
        };
        let after = std::fs::metadata(&tmp_path)?.len();
        let mut backup = Val::None;
        let result_path = if replace_original {
            if opts.make_backup {
                let b = with_suffix_append(&src_path, &format!(".before_compact_{}", stamp()));
                std::fs::copy(&src_path, &b)?;
                backup = Val::Str(b.display().to_string());
            }
            platform::atomic_replace(&tmp_path, &src_path, mode)?;
            src_path.clone()
        } else {
            platform::atomic_replace(&tmp_path, &final_path, mode)?;
            final_path.clone()
        };
        Ok(vmap! {
            "ok" => true, "file" => result_path.display().to_string(), "replaced_original" => replace_original,
            "backup" => backup, "before_bytes" => before as i64, "after_bytes" => after as i64,
            "saved_bytes" => if replace_original { Val::Int(before as i64 - after as i64) } else { Val::None },
            "records_written" => written_records,
            "lane_records" => vmap! {"NL" => lane_counts[0], "NR" => lane_counts[1]},
            "batches_written" => written_batches, "indexes_rebuilt" => opts.rebuild_indexes,
            "seconds" => t0.elapsed().as_secs_f64(), "verify" => final_verify,
        })
    }
}

#[allow(dead_code)]
fn _unused(_: TypeCode) {}
