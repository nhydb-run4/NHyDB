//! Maintenance: checkpoint, verify, health, stats, tail inspection, repair,
//! compaction.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::address::{lane_name, realm_span, realm_v_bounds, Lane};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, get_vu_u64, Nat};
use crate::schema::{LayoutKind, ShapeDef};
use crate::storage::Storage;
use crate::val::Val;
use crate::vmap;

use super::dense::DenseRows;
use super::layout::LayoutSpec;
use super::merge::{MergedCell, SparseMerge, Verify};
use super::sparse::CellInput;
use super::system;
use super::{judge_superblocks, Db, DenseSegment, OpenOptions, SVal, SlotState, SparseSegment, SuperJudgement, RL};
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

/// Options for `compact_file`.
#[derive(Clone, Debug)]
pub struct CompactOptions {
    pub output: Option<PathBuf>,
    pub make_backup: bool,
    pub rebuild_indexes: bool,
    pub batch_records: usize,
    /// Open options of the source (sidecar paths, configuration, lock).
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

/// Options for `repair_file`.
#[derive(Clone, Debug)]
pub struct RepairOptions {
    pub apply: bool,
    pub make_backup: bool,
    pub rebuild_indexes: bool,
    /// Discard intact committed blocks behind damage (listed in the report).
    pub allow_data_loss: bool,
    /// Keep complete blocks after the commit boundary (make them committed).
    pub keep_uncommitted: bool,
    pub open: OpenOptions,
}

impl Default for RepairOptions {
    fn default() -> Self {
        RepairOptions {
            apply: false,
            make_backup: true,
            rebuild_indexes: true,
            allow_data_loss: false,
            keep_uncommitted: false,
            open: OpenOptions::default(),
        }
    }
}

/// A block found by the physical scan.
#[derive(Clone, Debug)]
pub(crate) struct ScannedBlock {
    pub off: u64,
    pub next: u64,
    pub bt: u8,
    pub bid: u64,
    pub payload_len: u64,
    /// Header and payload CRC valid.
    pub ok: bool,
    pub error: Option<String>,
}

/// Result of a physical scan of a DB file (`inspect_tail`, `repair_file`).
pub(crate) struct Inspection {
    pub size: u64,
    pub slots: [SlotState; 2],
    pub judgement: SuperJudgement,
    /// Blocks: the contiguous valid chain from `DATA_START`, then (after a
    /// damaged spot) further blocks found by resynchronization.
    pub blocks: Vec<ScannedBlock>,
    /// End of the contiguous valid chain.
    pub last_good: u64,
    pub first_error: Option<String>,
    pub first_error_at: u64,
    /// DIRECTORY blocks with intact header but damaged payload: redundant
    /// data, they never cut the chain.
    pub dir_damaged: Vec<u64>,
    /// The directory of the chosen superblock uses a damaged block (it must
    /// be dropped: the open then scans every block).
    pub dir_ref_damaged: bool,
}

/// Next offset ≥ `from` (8-byte aligned) with a valid block header, or `size`.
pub(crate) fn resync_block(read_at: &dyn Fn(&mut [u8], u64) -> Result<()>, from: u64, size: u64) -> Result<u64> {
    const WIN: u64 = 64 << 10;
    let mut off = align8(from);
    while off + BLOCK_HEADER_SIZE as u64 <= size {
        let n = (size - off).min(WIN + BLOCK_HEADER_SIZE as u64) as usize;
        let mut buf = vec![0u8; n];
        read_at(&mut buf, off)?;
        let mut i = 0usize;
        while i + BLOCK_HEADER_SIZE <= n && (i as u64) < WIN {
            if &buf[i..i + 4] == BLOCK_MAGIC && unpack_block_header(&buf[i..i + BLOCK_HEADER_SIZE]).is_ok() {
                return Ok(off + i as u64);
            }
            i += 8;
        }
        off += WIN;
    }
    Ok(size)
}

impl Inspection {
    pub fn run(f: &std::fs::File) -> Result<Inspection> {
        let size = f.metadata()?.len();
        let read_at = |buf: &mut [u8], off: u64| -> Result<()> { Ok(platform::read_exact_at(f, buf, off)?) };
        let mut head = vec![0u8; size.min(DATA_START) as usize];
        read_at(&mut head, 0)?;
        let slots = super::slots_from_bytes(&head);
        let judgement = judge_superblocks(&slots, size);
        let mut ins = Inspection {
            size,
            slots,
            judgement,
            blocks: Vec::new(),
            last_good: DATA_START.min(size),
            first_error: None,
            first_error_at: 0,
            dir_damaged: Vec::new(),
            dir_ref_damaged: false,
        };
        if size < DATA_START {
            ins.last_good = 0;
            ins.first_error = Some(format!("file is shorter than the superblock area ({size} < {DATA_START} bytes)"));
            return Ok(ins);
        }
        let mut chain_ok = true;
        let mut off = DATA_START;
        let mut prev_bid = 0u64;
        while off < size {
            let fail = |ins: &mut Inspection, chain_ok: &mut bool, at: u64, msg: String| {
                if *chain_ok {
                    *chain_ok = false;
                    ins.first_error = Some(msg);
                    ins.first_error_at = at;
                }
            };
            if size - off < BLOCK_HEADER_SIZE as u64 {
                fail(&mut ins, &mut chain_ok, off, format!("incomplete block header at byte {off}"));
                break;
            }
            let mut raw = [0u8; BLOCK_HEADER_SIZE];
            read_at(&mut raw, off)?;
            let h = match unpack_block_header(&raw) {
                Ok(h) => h,
                Err(e) => {
                    if let Some(next) = super::directory::damaged_directory_extent(&read_at, off, &raw, size) {
                        // RA-01: redundant directory block, never cuts the chain
                        ins.dir_damaged.push(off);
                        if chain_ok {
                            ins.last_good = next;
                        }
                        ins.blocks.push(ScannedBlock { off, next, bt: BT_DIRECTORY, bid: 0, payload_len: next - off - BLOCK_HEADER_SIZE as u64, ok: true, error: None });
                        off = next;
                        continue;
                    }
                    fail(&mut ins, &mut chain_ok, off, e.to_string());
                    let next = resync_block(&read_at, off + 8, size)?;
                    ins.blocks.push(ScannedBlock { off, next, bt: 0, bid: 0, payload_len: 0, ok: false, error: Some(e.to_string()) });
                    off = next;
                    continue;
                }
            };
            let pstart = off + BLOCK_HEADER_SIZE as u64;
            let complete = pstart.checked_add(h.payload_len).filter(|e| *e <= size);
            let Some(pend) = complete else {
                let msg = format!("Block {}: payload incomplete", h.block_id);
                fail(&mut ins, &mut chain_ok, off, msg.clone());
                let next = resync_block(&read_at, off + 8, size)?;
                ins.blocks.push(ScannedBlock { off, next, bt: h.block_type, bid: h.block_id, payload_len: h.payload_len, ok: false, error: Some(msg) });
                off = next;
                continue;
            };
            let mut payload = vec![0u8; h.payload_len as usize];
            read_at(&mut payload, pstart)?;
            let nxt = align8(pend);
            let mut err = None;
            if h.block_id <= prev_bid && off > DATA_START {
                // RD-05: a copied or swapped block (ids grow with the position)
                err = Some(format!("Block {}: block ids not ascending (previous {prev_bid})", h.block_id));
            } else if crc32(&payload) != h.payload_crc {
                if h.block_type == BT_DIRECTORY && nxt <= size {
                    // block directory: redundant, rebuilt from the data blocks
                    ins.dir_damaged.push(off);
                } else {
                    err = Some(format!("Block {}: payload CRC mismatch", h.block_id));
                }
            } else if nxt > size {
                err = Some(format!("Block {}: end incomplete", h.block_id));
            }
            let next = nxt.min(size);
            if err.is_none() {
                prev_bid = h.block_id;
            }
            match err {
                None => {
                    if chain_ok {
                        ins.last_good = next;
                    }
                    ins.blocks.push(ScannedBlock { off, next, bt: h.block_type, bid: h.block_id, payload_len: h.payload_len, ok: true, error: None });
                }
                Some(msg) => {
                    fail(&mut ins, &mut chain_ok, off, msg.clone());
                    ins.blocks.push(ScannedBlock { off, next, bt: h.block_type, bid: h.block_id, payload_len: h.payload_len, ok: false, error: Some(msg) });
                }
            }
            off = next;
        }
        if !ins.dir_damaged.is_empty() {
            ins.dir_ref_damaged = Self::directory_uses_damaged(&ins, &read_at);
        }
        Ok(ins)
    }

    /// Whether the directory of the chosen superblock references a damaged
    /// DIRECTORY block (or cannot be read at all).
    fn directory_uses_damaged(ins: &Inspection, read_at: &dyn Fn(&mut [u8], u64) -> Result<()>) -> bool {
        let Some((_, sup)) = ins.judgement.chosen else { return false };
        if sup.dir_root == 0 {
            return false;
        }
        let damaged: std::collections::HashSet<u64> = ins.dir_damaged.iter().copied().collect();
        let payload_of = |off: u64| -> Option<Vec<u8>> {
            let b = ins.blocks.iter().find(|b| b.off == off && b.ok && b.bt == BT_DIRECTORY)?;
            let mut v = vec![0u8; b.payload_len as usize];
            read_at(&mut v, off + BLOCK_HEADER_SIZE as u64).ok()?;
            Some(v)
        };
        if damaged.contains(&sup.dir_root) {
            return true;
        }
        let Some(root) = payload_of(sup.dir_root) else { return true };
        let Ok(runs) = super::directory::root_run_offsets(&root) else { return true };
        for r in runs {
            if damaged.contains(&r) {
                return true;
            }
            let Some(rp) = payload_of(r) else { return true };
            let Ok(pages) = super::directory::run_page_offsets(&rp, r) else { return true };
            if pages.iter().any(|p| damaged.contains(p)) {
                return true;
            }
        }
        false
    }

    pub fn committed(&self) -> Option<u64> {
        self.judgement.chosen.map(|c| c.1.last_valid_end)
    }

    /// End of the contiguous valid chain.
    pub fn chain_end_ok(&self) -> u64 {
        self.last_good
    }

    /// Largest end of a block of the contiguous valid chain ≤ `limit`.
    pub fn chain_end_at_most(&self, limit: u64) -> u64 {
        let mut end = DATA_START;
        for b in self.blocks.iter().take_while(|b| b.ok && b.next <= self.last_good) {
            if b.next <= limit {
                end = b.next;
            }
        }
        end
    }

    /// The whole file is valid and the commit boundary is unambiguous.
    pub fn clean(&self) -> bool {
        let boundary_on_block = self
            .committed()
            .is_some_and(|c| self.blocks.iter().any(|b| b.ok && b.next == c && b.next <= self.last_good));
        self.first_error.is_none() && self.last_good == self.size && self.judgement.clean && boundary_on_block && !self.dir_ref_damaged
    }

    pub fn to_val(&self) -> Val {
        let chain: Vec<&ScannedBlock> = self.blocks.iter().take_while(|b| b.ok && b.next <= self.last_good).collect();
        let later: Vec<&ScannedBlock> = self.blocks.iter().skip(chain.len()).collect();
        let sb: Vec<Val> = self
            .slots
            .iter()
            .enumerate()
            .map(|(i, s)| {
                vmap! {
                    "slot" => i as i64, "valid" => s.sup.is_some(),
                    "generation" => s.sup.map(|x| x.generation as i64), "last_valid_end" => s.sup.map(|x| x.last_valid_end as i64),
                    "error" => s.error.clone(),
                }
            })
            .collect();
        let committed = self.committed();
        vmap! {
            "file_bytes" => self.size as i64, "last_good_end" => self.last_good as i64,
            "trailing_bad_bytes" => self.size.saturating_sub(self.last_good) as i64, "blocks" => chain.len() as i64,
            "max_block_id" => chain.iter().map(|b| b.bid).max().unwrap_or(0) as i64, "error" => self.first_error.clone(),
            "clean" => self.clean(),
            "superblocks" => Val::List(sb),
            "committed_end" => committed.map(|c| c as i64),
            "superblock_problem" => self.judgement.problem.clone(),
            "uncommitted_bytes" => committed.map(|c| self.size.saturating_sub(c) as i64),
            "damaged_blocks" => Val::List(later.iter().filter(|b| !b.ok).take(100).map(|b| vmap! {
                "offset" => b.off as i64, "end" => b.next as i64, "error" => b.error.clone(),
            }).collect()),
            "later_valid_blocks" => later.iter().filter(|b| b.ok).count() as i64,
            "damaged_directory_blocks" => Val::List(self.dir_damaged.iter().take(100).map(|o| Val::Int(*o as i64)).collect()),
            "directory_damaged_in_use" => self.dir_ref_damaged,
        }
    }
}

/// Planned repair.
struct RepairPlan {
    target: u64,
    max_id: u64,
    refusal: Option<String>,
    cut_reason: Option<String>,
    lost: Vec<Val>,
    damaged: Vec<Val>,
    discarded: Vec<Val>,
    kept_uncommitted: Vec<Val>,
    missing: u64,
}

impl RepairPlan {
    fn report(&self, out: &mut Val) {
        out.set("safe_end", Val::Int(self.target as i64));
        out.set("cut_reason", self.cut_reason.clone().map(Val::Str).unwrap_or(Val::None));
        out.set("lost_blocks", Val::List(self.lost.clone()));
        out.set("damaged_committed_blocks", Val::List(self.damaged.clone()));
        out.set("discarded_uncommitted_blocks", Val::List(self.discarded.clone()));
        out.set("kept_uncommitted_blocks", Val::List(self.kept_uncommitted.clone()));
        out.set("missing_committed_bytes", Val::Int(self.missing as i64));
    }
}

/// Upper bound for the collision counter of backup names (N3).
const BACKUP_NAME_ATTEMPTS: u32 = 1000;

/// Durable backup of DB and layout plan: `<db><suffix>` and
/// `<db><suffix>.layout.enc` (the key file stays unchanged and is only
/// referenced). Returns the paths.
///
/// The suffix carries a timestamp with one-second resolution. If a backup of
/// that name (or its plan copy) already exists – a second backup within the
/// same second (N3) – a counter is appended (`<suffix>_2`, `<suffix>_3`, ...);
/// existing backups are never overwritten. The files are created exclusively,
/// so a concurrent process taking the same name also moves on to the next
/// counter value.
fn backup_triad(p: &Path, open: &OpenOptions, cfg: &crate::config::Config, suffix: &str) -> Result<(PathBuf, Option<PathBuf>)> {
    let (lp, _kp) = open.sidecars(p, cfg);
    let with_plan = lp.exists();
    for n in 1..=BACKUP_NAME_ATTEMPTS {
        let name = if n == 1 { suffix.to_string() } else { format!("{suffix}_{n}") };
        let b = with_suffix_append(p, &name);
        let (bp, _) = system::default_sidecars(&b);
        if std::fs::symlink_metadata(&b).is_ok() || (with_plan && std::fs::symlink_metadata(&bp).is_ok()) {
            continue;
        }
        match platform::durable_copy(p, &b, cfg.durability) {
            Ok(()) => {}
            Err(Error::Exists(_)) => continue,
            Err(e) => return Err(e),
        }
        if !with_plan {
            return Ok((b, None));
        }
        match platform::durable_copy(&lp, &bp, cfg.durability) {
            Ok(()) => return Ok((b, Some(bp))),
            Err(e) => {
                let _ = std::fs::remove_file(&b);
                if matches!(e, Error::Exists(_)) {
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(Error::Exists(format!(
        "no free backup name '{}{suffix}[_<n>]' after {BACKUP_NAME_ATTEMPTS} attempts",
        p.display()
    )))
}

impl Db {
    pub fn checkpoint(&mut self) -> Result<u64> {
        let mut realms = Val::map();
        for r in self.realms.keys() {
            let lane_obj = |lane: Lane| {
                let k = (r.clone(), lane);
                vmap! {
                    "segments" => self.dense.get(&k).map(|d| d.idx.len()).unwrap_or(0) as i64,
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
        self.dir.last_checkpoint_off = off;
        self.last_checkpoint_obj = obj;
        // the checkpoint writes the directory tail when it pays off
        self.dir_force.get_or_insert(super::directory::FlushMode::Checkpoint);
        self.commit()?;
        Ok(off)
    }

    /// Structural check of all blocks (`verify`). All damaged blocks are
    /// reported (the scan continues behind a damaged block); stale indexes
    /// are reported as `warnings` (they are rebuilt, not corrupt).
    pub fn verify(&self, deep: bool) -> Result<Val> {
        let t0 = Instant::now();
        let size = self.store.len();
        let mut off = DATA_START;
        let mut last = DATA_START;
        let (mut blocks, mut records, mut sec_refs) = (0i64, 0i64, 0i64);
        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        // block directory (0.17): live blocks must be intact; with `deep`
        // the directory must describe exactly the data blocks of the file
        let live_dir = self.live_directory_blocks();
        let mut fp = DirFingerprint::default();
        let mut delete_errors = 0u64;
        // deep: aggregates recomputed from the blocks (RA-07/RD-03)
        let mut agg: HashMap<RL, DeepAgg> = HashMap::new();
        let mut deletes_seen: HashSet<(RL, Nat, u64)> = HashSet::new();
        let mut max_data_bid = 0u64;
        let mut prev_bid = 0u64;
        while off < size {
            let r: Result<u64> = (|| {
                let (info, payload) = match self.read_block(off) {
                    Ok(x) => x,
                    Err(e) => {
                        let next = match self.read_header_at(off) {
                            Ok(h) if h.block_type == BT_DIRECTORY => Some(h.next_offset),
                            Ok(_) => None,
                            Err(_) => {
                                let mut raw = [0u8; BLOCK_HEADER_SIZE];
                                self.store.read_exact_at(off, &mut raw)?;
                                let read_at = |buf: &mut [u8], o: u64| self.store.read_exact_at(o, buf);
                                super::directory::damaged_directory_extent(&read_at, off, &raw, size)
                            }
                        };
                        if let Some(next) = next {
                            // RD-09: one rule for every damaged directory
                            // block – unused: warning; in use: error (the
                            // data is intact, repair_file drops the directory)
                            if live_dir.contains(&off) {
                                errors.push(format!("at byte {off}: block directory block in use damaged ({e}); repair_file drops the directory without data loss"));
                            } else {
                                warnings.push(format!("at byte {off}: unused block directory block damaged ({e})"));
                            }
                            return Ok(next);
                        }
                        return Err(e);
                    }
                };
                blocks += 1;
                // RD-05: block ids grow with the file position
                if info.block_id <= prev_bid && off > DATA_START {
                    let prev = prev_bid;
                    prev_bid = info.block_id;
                    return Err(Error::Corruption(format!("block {}: block ids not ascending (previous {prev})", info.block_id)));
                }
                prev_bid = info.block_id;
                match info.block_type {
                    BT_DATA_DENSE => {
                        if !payload.starts_with(DATA_TAG) {
                            return Err(Error::Corruption("DATA wrong tag".into()));
                        }
                        let mut p = 4;
                        let realm = get_vu(&payload, &mut p)?;
                        let sid = get_vu_u64(&payload, &mut p, "DATA shape")?;
                        let u0 = get_vu(&payload, &mut p)?;
                        let cnt = get_vu_u64(&payload, &mut p, "DATA count")?;
                        check_prefix_crc(&payload, &mut p)?;
                        let table_end = (cnt as usize)
                            .checked_add(1)
                            .and_then(|c| c.checked_mul(4))
                            .and_then(|t| t.checked_add(p))
                            .unwrap_or(usize::MAX);
                        if table_end > payload.len() {
                            return Err(Error::Corruption("DATA offset table incomplete".into()));
                        }
                        let tl = p + cnt as usize * 4;
                        let end_last = u32::from_le_bytes(payload[tl..tl + 4].try_into().unwrap()) as usize;
                        if table_end + end_last != payload.len() {
                            return Err(Error::Corruption("DATA record area/offset table contradict each other".into()));
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
                                    return Err(Error::Corruption("DATA record offset invalid".into()));
                                }
                                self.decode_dense_record(&seg, &payload[table_end + s..table_end + e])?;
                            }
                            let a = agg.entry((seg.realm_id.clone(), seg.lane)).or_default();
                            a.records += cnt;
                            a.shapes.insert(sid);
                            if a.max_u.as_ref().is_none_or(|m| &seg.u_end > m) {
                                a.max_u = Some(seg.u_end.clone());
                            }
                            a.max_bid = a.max_bid.max(info.block_id);
                            a.dense.push((seg.u_start.clone(), seg.u_end.clone()));
                            max_data_bid = max_data_bid.max(info.block_id);
                            fp.add_dense(&seg, 1);
                        }
                    }
                    BT_SPARSE_CELLS => {
                        if !payload.starts_with(SPARSE_TAG) {
                            return Err(Error::Corruption("SPARSE wrong tag".into()));
                        }
                        let mut p = 4;
                        let realm = get_vu(&payload, &mut p)?;
                        let u0 = get_vu(&payload, &mut p)?;
                        let u1 = get_vu(&payload, &mut p)?;
                        let groups = get_vu_u64(&payload, &mut p, "SPARSE groups")?;
                        let cells = get_vu_u64(&payload, &mut p, "SPARSE cells")?;
                        check_prefix_crc(&payload, &mut p)?;
                        if groups == 0 || cells == 0 || u1 < u0 {
                            return Err(Error::Corruption("SPARSE range/group invalid".into()));
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
                        // group directory from the payload in hand (no cache
                        // insert: verify streams over the whole file)
                        let idx = self.sparse_group_index(&seg, &payload)?;
                        if idx.len() as u64 != groups {
                            return Err(Error::Corruption("SPARSE group count contradicts stream".into()));
                        }
                        if deep {
                            for (u, abs, len) in idx.iter() {
                                let rel = (abs - seg.payload_offset) as usize;
                                self.decode_sparse_record(&realm, u, &payload[rel..rel + *len as usize])?;
                            }
                            let a = agg.entry((seg.realm_id.clone(), seg.lane)).or_default();
                            a.groups += groups;
                            a.cells += cells;
                            a.max_bid = a.max_bid.max(info.block_id);
                            a.overlay = true;
                            max_data_bid = max_data_bid.max(info.block_id);
                            fp.add_sparse(&seg, 1);
                        }
                    }
                    BT_DELETE if deep => {
                        let mut p = 0usize;
                        let r = get_vu(&payload, &mut p)?;
                        let u = get_vu(&payload, &mut p)?;
                        let key = (r, crate::address::lane_from_flags(info.flags));
                        if !self.delete_block_id(&key, &u)?.is_some_and(|b| b >= info.block_id) {
                            delete_errors += 1;
                        }
                        let a = agg.entry(key.clone()).or_default();
                        a.max_bid = a.max_bid.max(info.block_id);
                        a.overlay = true;
                        max_data_bid = max_data_bid.max(info.block_id);
                        deletes_seen.insert((key, u, info.block_id));
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
                    errors.push(format!("at byte {off}: {e}"));
                    // continue behind the damaged block (intact header) or at
                    // the next valid block header
                    off = match self.read_header_at(off) {
                        Ok(info) if info.next_offset > off => info.next_offset,
                        _ => {
                            let read_at = |buf: &mut [u8], o: u64| self.store.read_exact_at(o, buf);
                            resync_block(&read_at, off + 8, size)?
                        }
                    };
                }
            }
        }
        if last != size && errors.is_empty() {
            errors.push(format!("last valid end {last} != file size {size}"));
        }
        if deep && errors.is_empty() {
            match self.directory_fingerprint_mismatch(&mut fp) {
                Ok(None) => {}
                Ok(Some(m)) => errors.push(format!("block directory does not match the data blocks: {m}")),
                Err(e) => errors.push(format!("block directory not readable: {e}")),
            }
            if delete_errors > 0 {
                errors.push(format!("block directory misses {delete_errors} DELETE block(s)"));
            }
            // SECONDARY key trees: page binding, key order, counts, postings
            let mut sec_keys: Vec<(u64, Lane)> = self.secondary_blocks.keys().copied().collect();
            sec_keys.sort();
            for (iid, lane) in sec_keys {
                if let Err(e) = self.sec_verify_tree(iid, lane) {
                    errors.push(format!("SECONDARY index {iid}/{}: {e}", lane_name(lane)));
                }
            }
            match self.deep_state_mismatch(&mut agg, &deletes_seen, max_data_bid) {
                Ok(m) => errors.extend(m),
                Err(e) => errors.push(format!("block directory not readable: {e}")),
            }
        }
        for (iid, idx) in &self.index_defs {
            for lane in [0u8, 1] {
                if self.secondary_source_block.contains_key(&(*iid, lane)) && !self.index_is_fresh(*iid, lane) {
                    warnings.push(if !self.config.index_maint.auto {
                        format!("Index {}/{} is stale (rebuild with build_indexes())", idx.name, lane_name(lane))
                    } else {
                        format!("Index {}/{} is stale: queries use the overlay of the changes until the automatic maintenance renews it", idx.name, lane_name(lane))
                    });
                }
            }
        }
        warnings.extend(self.open_warnings.iter().cloned());
        warnings.extend(self.dir.flush_error.iter().cloned());
        if let Some(d) = &self.dir.degraded {
            warnings.push(format!("block directory is not merged any more in this session ({d}); repair_file drops the directory without data loss"));
        }
        let n_err = errors.len() as i64;
        errors.truncate(100);
        Ok(vmap! {
            "ok" => n_err == 0, "seconds" => t0.elapsed().as_secs_f64(),
            "file_bytes" => size as i64, "file_size" => fmt_bytes(size),
            "scanned_blocks" => blocks, "physical_records" => records,
            "secondary_directory_entries" => sec_refs, "error_count" => n_err,
            "errors" => Val::List(errors.into_iter().map(Val::Str).collect()),
            "warnings" => Val::List(warnings.into_iter().map(Val::Str).collect()),
        })
    }

    /// Checks the physical file without opening the database (read-only):
    /// block chain (header and payload CRC), both superblocks and the commit
    /// boundary. After a damaged block the scan resynchronizes on the next
    /// valid block header, so later intact blocks are reported as well.
    ///
    /// `clean` is true if the whole file consists of valid blocks, both
    /// superblocks are valid and consistent and the commit boundary lies on a
    /// block end inside the file.
    pub fn inspect_tail(path: impl AsRef<Path>) -> Result<Val> {
        let p = platform::absolute(path.as_ref())?;
        if !p.exists() {
            return Err(Error::NotFound(p.display().to_string()));
        }
        let f = Storage::open_readonly(&p)?;
        Ok(Inspection::run(&f)?.to_val())
    }

    /// Repairs a database file (`repair_file`).
    ///
    /// Default (`apply=False`): dry run, the file is not touched; the report
    /// describes what an apply would do. With `apply=True` the write lock is
    /// taken first, then the file is inspected and – if needed – the
    /// superblocks are rewritten and the file is cut at the new end.
    ///
    /// Committed, CRC-valid blocks are never discarded silently: if damage
    /// lies inside the committed range and intact committed blocks follow,
    /// the repair aborts with `CorruptionError` unless
    /// `allow_data_loss=True` (the discarded blocks are listed in
    /// `lost_blocks`). Uncommitted blocks after an unambiguous commit
    /// boundary are discarded (`keep_uncommitted=True` keeps them). If the
    /// boundary is ambiguous (a superblock damaged or inconsistent and
    /// complete blocks after the usable boundary), one of the two switches is
    /// required. Only blocks whose schema is covered by a generation present
    /// in the layout plan are kept.
    ///
    /// Backup (`make_backup=True`): `<db>.bak_<timestamp>` and the layout
    /// plan as `<db>.bak_<timestamp>.layout.enc`, both made durable before
    /// the file is changed; the key file is unchanged (it stays valid for
    /// the backup). Restore: `NHyDB.restore_backup(backup, db)` or copy both
    /// files back (`<db>`, plan path of `<db>`). A second backup within the
    /// same second gets a counter suffix (`.bak_<timestamp>_2`, ...); the
    /// path is reported as `backup`.
    pub fn repair_file(path: impl AsRef<Path>, apply: bool, make_backup: bool, rebuild_indexes: bool) -> Result<Val> {
        let o = RepairOptions { apply, make_backup, rebuild_indexes, ..Default::default() };
        Self::repair_file_opts(path, &o)
    }

    pub fn repair_file_with(
        path: impl AsRef<Path>,
        apply: bool,
        make_backup: bool,
        rebuild_indexes: bool,
        open: &OpenOptions,
    ) -> Result<Val> {
        let o = RepairOptions { apply, make_backup, rebuild_indexes, open: open.clone(), ..Default::default() };
        Self::repair_file_opts(path, &o)
    }

    pub fn repair_file_opts(path: impl AsRef<Path>, o: &RepairOptions) -> Result<Val> {
        let p = platform::absolute(path.as_ref())?;
        let p = p.as_path();
        if !p.exists() {
            return Err(Error::NotFound(p.display().to_string()));
        }
        let open = o.open.absolutized()?;
        let cfg = open.resolve_config(p)?;
        // R014: lock first, then inspect (no change between check and cut).
        let lock = if o.apply { DbLock::acquire(p, cfg.lock)? } else { DbLock::none(p) };
        let insp = Inspection::run(&Storage::open_readonly(p)?)?;
        let mut out = insp.to_val();
        let wants_keep = o.keep_uncommitted && insp.committed().is_some_and(|c| insp.chain_end_ok() > c);
        if insp.clean() && !wants_keep {
            // G2-2: the block structure is intact, but the database must also
            // open (key, layout plan, schema binding – e.g. a plan of NHyDB
            // 0.27.1 never does); repair cannot fix that.
            let ro = OpenOptions { readonly: true, ..open.clone() };
            if let Err(e) = Db::open_locked(p, &ro, cfg.clone(), DbLock::none(p)) {
                if o.apply {
                    return Err(e);
                }
                out.set("action", Val::str("abort"));
                out.set("plan_error", Val::Str(e.to_string()));
                return Ok(out);
            }
            if !o.apply {
                out.set("action", Val::str("none"));
            }
            return Ok(out);
        }
        let plan = match Self::repair_plan(p, &insp, o, &open, &cfg) {
            Ok(plan) => plan,
            Err(e) if !o.apply => {
                out.set("action", Val::str("abort"));
                out.set("plan_error", Val::Str(e.to_string()));
                return Ok(out);
            }
            Err(e) => return Err(e),
        };
        plan.report(&mut out);
        if !o.apply {
            out.set("action", Val::str(if plan.refusal.is_some() { "abort" } else { "repair" }));
            if let Some(r) = &plan.refusal {
                out.set("plan_error", Val::Str(r.clone()));
            }
            return Ok(out);
        }
        if let Some(r) = plan.refusal {
            return Err(Error::Corruption(r));
        }
        let mode = cfg.durability;
        let mut backup = Val::None;
        let mut backup_plan = Val::None;
        if o.make_backup {
            let (b, bp) = backup_triad(p, &open, &cfg, &format!(".bak_{}", stamp()))?;
            backup = Val::Str(b.display().to_string());
            backup_plan = bp.map(|x| Val::Str(x.display().to_string())).unwrap_or(Val::None);
        }
        {
            // Superblocks first (higher generations, inactive slot first, each
            // synchronized), then cut the file: a crash in between leaves a
            // consistent pair of superblocks with an uncommitted tail.
            let mut st = Storage::open(p, mode)?;
            let t = super::now_ns();
            let max_gen = insp.slots.iter().filter_map(|s| s.sup.map(|x| x.generation)).max().unwrap_or(0);
            let base = insp.judgement.chosen.map(|c| c.1);
            let first_slot = match insp.judgement.chosen {
                Some((slot, _)) => 1 - slot,
                None => 1,
            };
            for (i, slot) in [first_slot, 1 - first_slot].into_iter().enumerate() {
                let s = SuperState {
                    generation: max_gen + 1 + i as u64,
                    checkpoint_offset: base.map(|b| b.checkpoint_offset).filter(|c| *c < plan.target).unwrap_or(0),
                    last_valid_end: plan.target,
                    next_block_id: plan.max_id + 1,
                    created_ns: base.map(|b| b.created_ns).unwrap_or(t),
                    updated_ns: t,
                    flags: base.map(|b| b.flags).unwrap_or(0),
                    // a directory whose root lies in the kept range covers
                    // only blocks before it; otherwise the open scans fully
                    dir_root: base
                        .map(|b| b.dir_root)
                        .filter(|r| *r >= DATA_START && *r < plan.target && !insp.dir_ref_damaged)
                        .unwrap_or(0),
                };
                st.write_at((slot * SUPERBLOCK_SIZE) as u64, &pack_super(&s))?;
                st.fsync()?;
            }
            if st.len() > plan.target {
                st.truncate(plan.target)?;
                st.fsync()?;
            }
            st.close()?;
        }
        let mut rebuilt = Val::None;
        if o.rebuild_indexes {
            let mut db = Db::open_locked(p, &open, cfg.clone(), DbLock::held_elsewhere(p))?;
            if !db.index_defs.is_empty() {
                rebuilt = db.build_indexes(None, false)?;
            }
            db.checkpoint()?;
            db.close(true)?;
        }
        drop(lock);
        out.set("applied", Val::Bool(true));
        out.set("backup", backup);
        out.set("backup_layout_plan", backup_plan);
        out.set("rebuild", rebuilt);
        out.set("after", Self::inspect_tail(p)?);
        Ok(out)
    }

    /// Plans a repair: new end of file and the blocks affected.
    fn repair_plan(p: &Path, insp: &Inspection, o: &RepairOptions, open: &OpenOptions, cfg: &crate::config::Config) -> Result<RepairPlan> {
        let size = insp.size;
        let (safe_end, schema_reason) = Self::repair_safe_end(p, insp, open, cfg)?;
        let committed = insp.committed();
        let ambiguous = !insp.judgement.clean;
        let mut reasons: Vec<String> = Vec::new();
        if let Some(e) = &insp.first_error {
            reasons.push(format!("damaged at byte {}: {e}", insp.first_error_at));
        }
        if let Some(r) = schema_reason {
            reasons.push(r);
        }
        if let Some(pr) = &insp.judgement.problem {
            reasons.push(pr.clone());
        }
        // End of the kept range: all CRC-valid blocks covered by the plan,
        // limited to the commit boundary unless uncommitted blocks are kept.
        let limit = match committed {
            Some(c) if !o.keep_uncommitted => safe_end.min(c),
            _ => safe_end,
        };
        let target = insp.chain_end_at_most(limit);
        // committed blocks that would be discarded although intact
        let reference_end = committed.unwrap_or(size).max(target);
        let lost: Vec<&ScannedBlock> = insp.blocks.iter().filter(|b| b.ok && b.off >= target && b.off < reference_end).collect();
        let damaged: Vec<&ScannedBlock> = insp.blocks.iter().filter(|b| !b.ok && b.off < reference_end).collect();
        let beyond: Vec<&ScannedBlock> = insp.blocks.iter().filter(|b| b.ok && b.off >= reference_end && b.off >= target).collect();
        let kept_uncommitted: Vec<&ScannedBlock> = match committed {
            Some(c) => insp.blocks.iter().filter(|b| b.ok && b.off >= c && b.next <= target).collect(),
            None => Vec::new(),
        };
        // committed bytes beyond the end of the file (truncated file, F020)
        let newest_end = insp.slots.iter().filter_map(|s| s.sup.map(|x| x.last_valid_end)).max().unwrap_or(0);
        let missing = newest_end.saturating_sub(size);
        if missing > 0 {
            reasons.push(format!("{missing} committed bytes are missing at the end of the file"));
        }
        if !beyond.is_empty() && !o.keep_uncommitted {
            reasons.push(format!("{} uncommitted block(s) after the commit boundary discarded", beyond.len()));
        }
        if insp.dir_ref_damaged {
            reasons.push(format!(
                "block directory damaged ({} block(s)); it is dropped (no data is cut, the next open scans all blocks, \
                 the next checkpoint writes a new directory)",
                insp.dir_damaged.len()
            ));
        }
        let mut refusal = None;
        if !lost.is_empty() && !o.allow_data_loss {
            refusal = Some(format!(
                "repair: the committed range up to byte {reference_end} is inconsistent ({}); {} intact committed block(s) \
                 (bytes {target}..{reference_end}) would be lost. Nothing was changed. Use allow_data_loss=True to cut \
                 them (they are listed in 'lost_blocks'), restore a backup, or inspect with readonly=True",
                reasons.join("; "),
                lost.len()
            ));
        } else if ambiguous && !beyond.is_empty() && !o.keep_uncommitted && !o.allow_data_loss {
            refusal = Some(format!(
                "repair: the commit boundary is ambiguous ({}); {} complete block(s) after byte {} may be committed. Nothing \
                 was changed. Use keep_uncommitted=True to keep them or allow_data_loss=True to discard them",
                insp.judgement.problem.clone().unwrap_or_default(),
                beyond.len(),
                reference_end
            ));
        }
        let max_id = insp.blocks.iter().filter(|b| b.ok && b.next <= target).map(|b| b.bid).max().unwrap_or(0);
        let blk = |b: &&ScannedBlock| {
            vmap! {"offset" => b.off as i64, "block_id" => b.bid as i64, "type" => block_name(b.bt).unwrap_or("?"),
                   "error" => b.error.clone()}
        };
        Ok(RepairPlan {
            target,
            max_id,
            refusal,
            cut_reason: if target < size || !reasons.is_empty() { Some(reasons.join("; ")) } else { None },
            lost: lost.iter().map(blk).collect(),
            damaged: damaged.iter().map(blk).collect(),
            discarded: beyond.iter().filter(|_| !o.keep_uncommitted).map(|b| Val::Int(b.bid as i64)).collect(),
            kept_uncommitted: kept_uncommitted.iter().map(|b| Val::Int(b.bid as i64)).collect(),
            missing,
        })
    }

    /// Determines the end of the range whose blocks are covered by the
    /// layout plan (R0 chain and published schema), within the valid chain.
    fn repair_safe_end(p: &Path, insp: &Inspection, open: &OpenOptions, cfg: &crate::config::Config) -> Result<(u64, Option<String>)> {
        use std::collections::HashSet;
        let f = Storage::open_readonly(p)?;
        // collect blocks of the contiguous valid chain
        struct B {
            off: u64,
            next: u64,
            bt: u8,
            bid: u64,
            payload: Vec<u8>,
        }
        let mut blocks = Vec::new();
        for b in insp.blocks.iter().take_while(|b| b.ok && b.next <= insp.last_good) {
            let want = matches!(b.bt, BT_SYSTEM | BT_DATA_DENSE | BT_SPARSE_CELLS | BT_DELETE | BT_SECONDARY);
            let payload = if want {
                let n = if b.bt == BT_SYSTEM { b.payload_len as usize } else { (b.payload_len as usize).min(96) };
                let mut v = vec![0u8; n];
                platform::read_exact_at(&f, &mut v, b.off + BLOCK_HEADER_SIZE as u64)?;
                v
            } else {
                Vec::new()
            };
            blocks.push(B { off: b.off, next: b.next, bt: b.bt, bid: b.bid, payload });
        }
        // R0 chain and plan
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
                    reason = Some(format!("invalid R0 block at byte {}", b.off));
                    break;
                }
            }
        }
        let first = first.ok_or_else(|| Error::Corruption("repair: R0,u1 bootstrap missing".into()))?;
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
        // The plan holds the last two generations: older R0 records are
        // valid history; records after the newest one found in the plan
        // were never published and are cut.
        let in_range: Vec<&(usize, Val)> = systems.iter().filter(|(i, _)| *i < end_idx).collect();
        let last_in_plan = in_range.iter().rposition(|(_, rec)| in_plan(rec).is_some());
        let active: Option<&Val> = last_in_plan.and_then(|k| in_plan(&in_range[k].1));
        if let Some(k) = last_in_plan {
            if let Some((i, rec)) = in_range.get(k + 1) {
                end_idx = *i;
                reason = Some(format!("R0 generation {} not in the layout plan (byte {})", cell(rec, "5").py_repr(), blocks[*i].off));
            }
        }
        let active = active.ok_or_else(|| Error::Corruption("repair: no layout plan generation matches R0,u1".into()))?;
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
                    "Block {} ({}) refers to an unpublished schema (byte {})",
                    b.bid,
                    block_name(b.bt).unwrap_or("?"),
                    b.off
                ));
                break;
            }
        }
        let safe_end = if end_idx == 0 { DATA_START } else { blocks[end_idx - 1].next };
        Ok((safe_end, reason))
    }

    /// Restores a backup made by `repair_file`/`compact_file`
    /// (`<backup>` plus `<backup>.layout.enc`) as database `path`: both files
    /// are checked (the backup must open with the plan backup and the key of
    /// `path`), copied durably next to the targets and swapped in (plan first,
    /// then the DB). Requires the write lock of `path`.
    pub fn restore_backup(backup: impl AsRef<Path>, path: impl AsRef<Path>, open: &OpenOptions) -> Result<Val> {
        let b = platform::absolute(backup.as_ref())?;
        let p = platform::absolute(path.as_ref())?;
        let open = open.absolutized()?;
        let cfg = open.resolve_config(&p)?;
        let mode = cfg.durability;
        let lock = DbLock::acquire(&p, cfg.lock)?;
        let (bplan, _) = system::default_sidecars(&b);
        if !b.is_file() || !bplan.is_file() {
            return Err(Error::NotFound(format!(
                "backup '{}' or its layout plan '{}' not found",
                b.display(),
                bplan.display()
            )));
        }
        let (lp, kp) = open.sidecars(&p, &cfg);
        // verify: the backup opens read-only with the plan backup and the key
        let check = OpenOptions {
            readonly: true,
            internal_compaction: true,
            layout_path: Some(bplan.clone()),
            key_path: Some(kp.clone()),
            ..open.clone()
        };
        let db = Db::open_locked(&b, &check, cfg.clone(), DbLock::none(&b))?;
        let expected = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if db.sys.db_name() != expected {
            return Err(Error::Value(format!(
                "backup belongs to database '{}', not to '{}'",
                db.sys.db_name(),
                expected
            )));
        }
        drop(db);
        let tdb = super::sibling_temp(&p, "restoring")?;
        let tlp = super::sibling_temp(&lp, "restoring")?;
        let res = (|| -> Result<()> {
            platform::durable_copy(&bplan, &tlp, mode)?;
            platform::durable_copy(&b, &tdb, mode)?;
            platform::atomic_replace(&tlp, &lp, mode).map_err(|e| e.at_path(&lp))?; // F048
            platform::atomic_replace(&tdb, &p, mode)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tdb);
            let _ = std::fs::remove_file(&tlp);
        }
        res?;
        let db = Db::open_locked(&p, &OpenOptions { readonly: true, ..open.clone() }, cfg, DbLock::none(&p))?;
        let v = db.verify(true)?;
        drop(db);
        drop(lock);
        Ok(vmap! {"ok" => v.get("ok").cloned().unwrap_or(Val::Bool(false)), "file" => p.display().to_string(),
                  "layout_path" => lp.display().to_string(), "key_path" => kp.display().to_string(), "verify" => v})
    }

    /// E6: path and size of the staging file of an in-place compaction
    /// (`<db>.compacting`), if one exists.
    pub fn compaction_leftover(&self) -> Option<(PathBuf, u64)> {
        let p = with_suffix_append(&self.path, ".compacting");
        let m = std::fs::symlink_metadata(&p).ok()?;
        Some((p, m.len()))
    }

    /// E6: removes a leftover `<db>.compacting` (`cleanup_compaction()`).
    /// Only a writer holding the write lock of the database may: a running
    /// compaction holds that lock, so none can be running. Returns the bytes
    /// removed (0: nothing there). Never touches anything but a regular file
    /// of exactly that name.
    pub fn remove_compaction_leftover(&mut self) -> Result<u64> {
        self.check_open()?;
        if self.readonly {
            return Err(Error::NHyDB("database is opened read-only (readonly=True); cleanup_compaction() is not possible".into()));
        }
        if !self.lock.is_held() {
            return Err(Error::NHyDB(
                "cleanup_compaction() needs the write lock of the database (a running compaction holds it); \
                 with [lock] enabled = false a running compaction cannot be ruled out"
                    .into(),
            ));
        }
        self.remove_compaction_leftover_unchecked()
    }

    pub(crate) fn remove_compaction_leftover_unchecked(&self) -> Result<u64> {
        let Some((p, n)) = self.compaction_leftover() else { return Ok(0) };
        let m = std::fs::symlink_metadata(&p)?;
        if !m.file_type().is_file() {
            return Err(Error::NHyDB(format!("'{}' is not a regular file; not removed", p.display())));
        }
        std::fs::remove_file(&p)?;
        Ok(n)
    }

    pub fn health(&self, deep: bool) -> Result<Val> {
        let t0 = Instant::now();
        let mut checks = Vec::new();
        // E6 (R9B-04): leftover staging file of a killed compaction (a check
        // of its own only when there is one)
        if let Some((p, n)) = self.compaction_leftover() {
            checks.push(vmap! {
                "name" => "compaction_leftover", "status" => "warning",
                "details" => vmap! {
                    "path" => p.display().to_string(), "bytes" => n as i64,
                    "removable" => !self.readonly && self.lock.is_held(),
                },
            });
        }
        let verify = self.verify(deep)?;
        let vok = verify.get("ok").map(|v| v.is_truthy()).unwrap_or(false);
        checks.push(vmap! {
            "name" => "container", "status" => if vok { "ok" } else { "error" },
            "details" => vmap! {"blocks" => verify.get("scanned_blocks").cloned(), "errors" => verify.get("errors").cloned()},
        });
        let mut layout_errors: Vec<Val> = Vec::new();
        for (lid, lay) in &self.layouts {
            if !self.realms.contains_key(&lay.realm_id) {
                layout_errors.push(Val::Str(format!("Layout {lid}: realm {} missing", lay.realm_id)));
            }
            if lay.u_start.is_zero() || lay.u_end.as_ref().map(|e| e < &lay.u_start).unwrap_or(false) {
                layout_errors.push(Val::Str(format!("Layout {lid}: u range invalid")));
            }
            match lay.kind {
                LayoutKind::Classic => {
                    if lay.shape_id.map(|s| !self.shapes.contains_key(&s)).unwrap_or(true) {
                        layout_errors.push(Val::Str(format!("Layout {lid}: CLASSIC shape missing")));
                    }
                }
                LayoutKind::Column => {
                    if lay.fields().is_empty() {
                        layout_errors.push(Val::Str(format!("Layout {lid}: COLUMN without fields")));
                    }
                }
                LayoutKind::Packed => {
                    if lay.packed_span().is_err() || lay.packed_u().is_err() {
                        layout_errors.push(Val::Str(format!("Layout {lid}: PACKED parameters invalid")));
                    }
                }
                LayoutKind::Custom => {
                    if lay.plugin.is_empty() {
                        layout_errors.push(Val::Str(format!("Layout {lid}: CUSTOM without plugin ID")));
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
                        sm_err.push(Val::Str(format!("Layout {lid}: persisted SpaceMap differs from the mapping logic")));
                    } else if lay.kind == LayoutKind::Custom {
                        if let Some(p) = self.plugin_for(lay) {
                            if p.has(crate::plugin::PluginMethod::Spacemap) && lay.spacemap != eff {
                                sm_warn.push(Val::Str(format!(
                                    "Layout {lid}: CUSTOM plugin SpaceMap differs from the persisted snapshot"
                                )));
                            }
                        }
                    }
                }
                Err(e) => sm_err.push(Val::Str(format!("Layout {lid}: SpaceMap cannot be derived: {e}"))),
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
                ext_errors.push(Val::Str(format!("Extension R{rid}/v{v}: realm missing")));
            }
            if v.is_zero() || v > realm_span() {
                ext_errors.push(Val::Str(format!("Extension R{rid}/v{v}: outside realm")));
            }
        }
        checks.push(vmap! {
            "name" => "definitions", "status" => if ext_errors.is_empty() { "ok" } else { "error" },
            "details" => vmap! {"extensions" => self.extension_defs.len() as i64, "errors" => Val::List(ext_errors)},
        });
        let mut stale = Vec::new();
        let mut overlay = Vec::new();
        let mut index_errors = Vec::new();
        for (iid, idef) in &self.index_defs {
            if let Err(e) = crate::textindex::normalize_options(idef.kind, &idef.options)
                .and_then(|_| self.resolve_v(&idef.realm_id, &Val::nat(idef.field_v())))
            {
                index_errors.push(Val::Str(format!("{}: {e}", idef.name)));
            }
            for lane in [0u8, 1] {
                if self.secondary_source_block.contains_key(&(*iid, lane)) && !self.index_is_fresh(*iid, lane) {
                    // AP9: a stale index with a base answers through the overlay of the changes
                    // (manual maintenance: a warning, nobody renews it)
                    let name = Val::Str(format!("{}/{}", idef.name, lane_name(lane)));
                    if self.config.index_maint.auto {
                        overlay.push(name);
                    } else {
                        stale.push(name);
                    }
                }
            }
        }
        checks.push(vmap! {
            "name" => "indexes",
            "status" => if !index_errors.is_empty() { "error" } else if !stale.is_empty() { "warning" } else { "ok" },
            "details" => vmap! {
                "count" => self.index_defs.len() as i64, "stale" => Val::List(stale), "overlay" => Val::List(overlay),
                "errors" => Val::List(index_errors), "maintenance" => self.ixm_status(),
            },
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
            let info = match self.read_header_at(off) {
                Ok(i) => i,
                Err(e) => {
                    // RD-09: a damaged directory block does not break stats()
                    let mut raw = [0u8; BLOCK_HEADER_SIZE];
                    self.store.read_exact_at(off, &mut raw)?;
                    let read_at = |buf: &mut [u8], o: u64| self.store.read_exact_at(o, buf);
                    let next = super::directory::damaged_directory_extent(&read_at, off, &raw, size).ok_or(e)?;
                    let d = by.entry("DIRECTORY").or_default();
                    d.0 += 1;
                    d.1 += (next - off) as i64;
                    d.2 += (next - off - BLOCK_HEADER_SIZE as u64) as i64;
                    off = next;
                    continue;
                }
            };
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
                let d = self.dense.get(&key);
                let sp = self.sparse.get(&key);
                let live = if overlay { Val::None } else { Val::Int(d.map(|x| x.records).unwrap_or(0) as i64) };
                lanes.set(
                    lane_name(lane),
                    vmap! {
                        "lane" => lane as i64, "dense_segments" => d.map(|x| x.idx.len()).unwrap_or(0) as i64,
                        "max_u" => self.max_u.get(&key).cloned().unwrap_or_default(),
                        "live_records" => live, "has_overlays" => overlay,
                        "sparse_segments" => sp.map(|x| x.idx.len()).unwrap_or(0) as i64,
                        "sparse_u_groups" => sp.map(|x| x.groups).unwrap_or(0) as i64,
                        "materialized_sparse_cells" => sp.map(|x| x.cells).unwrap_or(0) as i64,
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
        // own statements: a cache guard inside the map below would live
        // until its end (and `cache_stats` locks again)
        let posting_entries = self.cache().postings.len() as i64;
        let cache = self.cache_stats();
        let phys = self.physical_block_stats()?;
        let sparse_bytes = phys.get("SPARSE_CELLS").map(|e| e.1).unwrap_or(0);
        let sparse_cells: i64 = self.sparse.values().map(|s| s.cells as i64).sum();
        let sparse_groups: i64 = self.sparse.values().map(|s| s.groups as i64).sum();
        let dense_records: i64 = self.dense.values().map(|s| s.records as i64).sum();
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
            "block_cache" => self.block_cache_stats(),
            "directory" => self.directory_stats(),
            "realms" => Val::List(realm_stats),
            "shapes" => Val::List(self.shapes.values().map(|s| vmap! {
                "shape_id" => s.shape_id as i64, "realm_id" => &s.realm_id, "name" => s.name.as_str(),
                "field_count" => s.fields.len() as i64,
            }).collect()),
            "layouts" => self.get_layout(None, None, None)?,
            "math_layouts" => Val::List(self.math_layouts.keys().map(|lid| self.describe_math(*lid)).collect::<Result<Vec<_>>>()?),
            "extensions" => Val::List(self.extension_defs.values().map(|e| e.to_val()).collect()),
            "indexes" => Val::List(idx_stats),
            "posting_cache_entries" => posting_entries,
            "cache" => cache,
            "has_overlays" => self.has_overlays,
            "overlay_view" => {
                let c = self.cache();
                vmap! {"enabled" => self.config.query_overlay_view, "max_bytes" => self.config.query_overlay_view_max_bytes as i64,
                       "built" => c.overlay_view_built as i64, "declined" => c.overlay_view_declined as i64}
            },
        })
    }

    // ------------------------------------------------------------------
    // Compaction
    // ------------------------------------------------------------------

    /// Writes the current logical state into a new file
    /// (`compact_file`). With `output=None` the original is replaced.
    ///
    /// Backup (`make_backup=True`, in-place only): `<db>.before_compact_<ts>`
    /// and the layout plan as `<db>.before_compact_<ts>.layout.enc`, made
    /// durable before the swap; restore with `NHyDB.restore_backup()`.
    /// Within the same second a counter suffix keeps the name unique
    /// (`.before_compact_<ts>_2`, ...); the path is reported as `backup`.
    /// A failed compaction removes every file it created (staging file and,
    /// for an export, the new plan and key).
    pub fn compact_file(path: impl AsRef<Path>, opts: &CompactOptions) -> Result<Val> {
        let src_path = platform::absolute(path.as_ref())?;
        if !src_path.exists() {
            return Err(Error::NotFound(src_path.display().to_string()));
        }
        let mut opts = opts.clone();
        opts.open = opts.open.absolutized()?;
        if opts.open.readonly {
            return Err(Error::Value("compact_file() cannot be combined with readonly=True".into()));
        }
        opts.output = opts.output.as_deref().map(platform::absolute).transpose()?;
        let mut created: Vec<PathBuf> = Vec::new();
        let mut res = Self::compact_inner(&src_path, &opts, &mut created);
        if res.is_err() {
            for f in created.iter().rev() {
                let _ = std::fs::remove_file(f);
            }
        }
        // RA-01: a damaged block directory of the source (the data blocks
        // are intact) – compact from a full scan instead
        let dir_err = |m: &str| m.starts_with("block directory") || m.contains("contradicts its directory entry");
        if matches!(&res, Err(Error::Corruption(m)) if dir_err(m)) && !opts.open.ignore_directory {
            opts.open.ignore_directory = true;
            created.clear();
            res = Self::compact_inner(&src_path, &opts, &mut created);
            if res.is_err() {
                for f in created.iter().rev() {
                    let _ = std::fs::remove_file(f);
                }
            }
        }
        res
    }

    fn compact_inner(src_path: &Path, opts: &CompactOptions, created: &mut Vec<PathBuf>) -> Result<Val> {
        let src_path = src_path.to_path_buf();
        let batch_records = opts.batch_records.max(1);
        let replace_original = opts.output.is_none();
        let final_path = opts.output.clone().unwrap_or_else(|| src_path.clone());
        let tmp_path = with_suffix_append(&final_path, ".compacting");
        let cfg = opts.open.resolve_config(&src_path)?;
        let mode = cfg.durability;
        // Locks: the source for the whole duration incl. the atomic swap,
        // and additionally the target when exporting.
        let _src_lock = DbLock::acquire(&src_path, cfg.lock)?;
        // G2-5/F048: OS errors at the export target name the output path.
        let _dst_lock = if replace_original { None } else { Some(DbLock::acquire(&final_path, cfg.lock).map_err(|e| e.at_path(&final_path))?) };
        if !replace_original && final_path.exists() {
            return Err(Error::Exists(format!("[Errno 17] File exists: '{}'", final_path.display())));
        }
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
        }
        let before = std::fs::metadata(&src_path)?.len();
        let t0 = Instant::now();
        let mut written_records = 0i64;
        let mut written_batches = 0i64;
        let mut lane_counts = [0i64; 2];
        let (dst_layout, dst_key);
        {
            let mut src = Db::open_locked(&src_path, &opts.open, cfg.clone(), DbLock::held_elsewhere(&src_path))?;
            // one sequential pass: a small block cache suffices (records of
            // one batch block), a large one would only hold read blocks
            src.config.block_cache_bytes = src.config.block_cache_bytes.min(16 << 20);
            // Staging file: in-place with identical R0 identity and the same
            // sidecars; export with a new DB ID and its own sidecars.
            let mut stage_opts = OpenOptions { internal_compaction: true, ignore_directory: false, ..opts.open.clone() };
            if replace_original {
                created.push(tmp_path.clone());
                system::write_initial_db(&tmp_path, &system::initial_record_from(&src.sys.record, src.sys.math_constants_stored), mode)?;
                stage_opts.layout_path = Some(src.sys.layout_path.clone());
                stage_opts.key_path = Some(src.sys.key_path.clone());
            } else {
                let base = OpenOptions { layout_path: None, key_path: None, ..opts.open.clone() };
                let (lp, kp) = base.sidecars(&final_path, &cfg);
                if lp.exists() || kp.exists() {
                    return Err(Error::Exists(format!(
                        "Target database already has a layout or key file ('{}' / '{}')",
                        lp.display(),
                        kp.display()
                    )));
                }
                super::validate_paths(&final_path, &lp, &kp)?;
                let fname = |q: &Path| q.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let db_id = crate::crypto::uuid4()?;
                let key = system::key_create(&kp, &db_id, &fname(&final_path), mode)?;
                created.push(kp.clone());
                let entry = system::seal_entry(&db_id, 0, system::empty_schema().to_json()?.as_bytes(), &key)?;
                system::atomic_write(&lp, &system::envelope_bytes(&db_id, std::slice::from_ref(&entry), &key)?, mode)?;
                created.push(lp.clone());
                let rec = system::bootstrap_record(
                    &db_id,
                    &fname(&final_path),
                    &fname(&lp),
                    &fname(&kp),
                    0,
                    system::entry_digest(&entry)?,
                    super::now_ns(),
                );
                created.push(tmp_path.clone());
                system::write_initial_db(&tmp_path, &rec, mode).map_err(|e| e.at_path(&final_path))?;
                stage_opts.layout_path = Some(lp);
                stage_opts.key_path = Some(kp);
            }
            let mut dst = Db::open_locked(&tmp_path, &stage_opts, cfg.clone(), DbLock::held_elsewhere(&tmp_path))?;
            dst_layout = dst.sys.layout_path.clone();
            dst_key = dst.sys.key_path.clone();
            // Copy physical cells into exclusive MATH realms as well.
            dst.internal_math_writes = true;
            dst.write_cache = false;
            let res = (|| -> Result<()> {
                if dst.max_db_size_bytes != src.max_db_size_bytes {
                    dst.set_max_db_size(src.max_db_size_bytes, false)?;
                }
                if !replace_original {
                    // Realm complete incl. default shape and reserve.
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
                    // Exact MATH state (definition, fingerprint, RealmMap),
                    // no re-planning.
                    for (lid, st) in &src.math_layouts {
                        dst.math_layouts.insert(*lid, st.clone());
                    }
                    if !src.math_layouts.is_empty() {
                        dst.sys.layout_dirty = true;
                    }
                }
                // P002: memory bounded by one batch. Dense records are copied
                // as raw BASE bytes (same shape in source and target, every
                // record is decoded again by the deep verify below); dense EXT
                // values follow as sparse cells in bounded chunks. Sparse
                // cells come from a memory-bounded merge (`merge.rs`) that
                // runs in u order next to the dense records of the same
                // realm/lane, so no map of all cells is ever built.
                let mut keys: Vec<RL> = src.dense.keys().chain(src.sparse.keys()).cloned().collect();
                keys.sort();
                keys.dedup();
                let mut cnt = CompactCounters::default();
                // temporary runs of an external sparse merge next to the target
                let run_dir = platform::parent_dir(&tmp_path);
                for key in keys {
                    compact_key(&src, &mut dst, &key, batch_records, &run_dir, &mut cnt)?;
                }
                written_records += cnt.records;
                written_batches += cnt.batches;
                lane_counts[0] += cnt.lanes[0];
                lane_counts[1] += cnt.lanes[1];
                if opts.rebuild_indexes && !dst.index_defs.is_empty() {
                    dst.build_indexes(None, false)?;
                }
                // the compacted file gets one directory run
                dst.dir_force = Some(super::directory::FlushMode::Compaction);
                dst.checkpoint()?;
                let v = dst.verify(true)?;
                if !v.get("ok").map(|x| x.is_truthy()).unwrap_or(false) {
                    return Err(Error::Corruption(format!(
                        "Compaction target file verify failed: {}",
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
            let check = Db::open_locked(&tmp_path, &check_opts, cfg.clone(), DbLock::held_elsewhere(&tmp_path))?;
            let v = check.verify(true)?;
            if !v.get("ok").map(|x| x.is_truthy()).unwrap_or(false) {
                return Err(Error::Corruption(format!(
                    "Compaction reopen verify failed: {}",
                    v.get("errors").map(|e| e.py_repr()).unwrap_or_default()
                )));
            }
            v
        };
        let after = std::fs::metadata(&tmp_path)?.len();
        let mut backup = Val::None;
        let mut backup_plan = Val::None;
        let result_path = if replace_original {
            if opts.make_backup {
                // RB-04: the backup is a full copy of the original – fail
                // before writing it instead of filling the disk
                let dir = platform::parent_dir(&src_path);
                if let Some(free) = platform::free_space(&dir) {
                    if free < before.saturating_add(1 << 20) {
                        return Err(Error::Value(format!(
                            "not enough free disk space for the backup copy of the compaction ({} needed, {} free in {}); \
                             free space or compact with make_backup=False",
                            fmt_bytes(before),
                            fmt_bytes(free),
                            dir.display()
                        )));
                    }
                }
                let (b, bp) = backup_triad(&src_path, &opts.open, &cfg, &format!(".before_compact_{}", stamp()))?;
                backup = Val::Str(b.display().to_string());
                backup_plan = bp.map(|x| Val::Str(x.display().to_string())).unwrap_or(Val::None);
            }
            platform::atomic_replace(&tmp_path, &src_path, mode)?;
            src_path.clone()
        } else {
            platform::atomic_replace(&tmp_path, &final_path, mode)?;
            final_path.clone()
        };
        created.clear();
        Ok(vmap! {
            "ok" => true, "file" => result_path.display().to_string(), "replaced_original" => replace_original,
            "backup" => backup, "backup_layout_plan" => backup_plan, "before_bytes" => before as i64, "after_bytes" => after as i64,
            "saved_bytes" => if replace_original { Val::Int(before as i64 - after as i64) } else { Val::None },
            "records_written" => written_records,
            "lane_records" => vmap! {"NL" => lane_counts[0], "NR" => lane_counts[1]},
            "batches_written" => written_batches, "indexes_rebuilt" => opts.rebuild_indexes,
            "seconds" => t0.elapsed().as_secs_f64(), "verify" => final_verify,
        })
    }
}

/// Order-independent fingerprint of segment entries per key (verify: the
/// block scan adds, the directory subtracts; all zero = identical sets).
#[derive(Default)]
struct DirFingerprint {
    acc: BTreeMap<(u8, RL), (u64, i64)>,
}

impl DirFingerprint {
    fn mix(parts: &[u64]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        parts.hash(&mut h);
        h.finish()
    }

    fn nat(n: &Nat) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        n.to_string().hash(&mut h);
        h.finish()
    }

    fn put(&mut self, kind: u8, key: RL, f: u64, sign: i64) {
        let e = self.acc.entry((kind, key)).or_insert((0, 0));
        e.0 = if sign > 0 { e.0.wrapping_add(f) } else { e.0.wrapping_sub(f) };
        e.1 += sign;
    }

    fn add_dense(&mut self, s: &DenseSegment, sign: i64) {
        let f = Self::mix(&[
            Self::nat(&s.u_start), Self::nat(&s.u_end), s.count, s.shape_id, s.block_id, s.block_offset,
            s.payload_offset, s.offsets_offset, s.records_offset, s.payload_len,
        ]);
        self.put(1, (s.realm_id.clone(), s.lane), f, sign);
    }

    fn add_sparse(&mut self, s: &SparseSegment, sign: i64) {
        let f = Self::mix(&[
            Self::nat(&s.u_start), Self::nat(&s.u_end), s.group_count, s.cell_count, s.block_id, s.block_offset,
            s.payload_offset, s.entries_offset, s.payload_len,
        ]);
        self.put(2, (s.realm_id.clone(), s.lane), f, sign);
    }
}

/// Per-key state recomputed by `verify(deep=True)` from the data blocks.
#[derive(Default)]
struct DeepAgg {
    records: u64,
    shapes: std::collections::BTreeSet<u64>,
    groups: u64,
    cells: u64,
    max_u: Option<Nat>,
    max_bid: u64,
    overlay: bool,
    dense: Vec<(Nat, Nat)>,
}

impl Db {
    /// RA-07/RD-03: the in-memory state that came from the directory root
    /// (aggregates, routing flags) and the DELETE entries of the directory
    /// must match what the data blocks say. Returns the differences.
    fn deep_state_mismatch(&self, agg: &mut HashMap<RL, DeepAgg>, deletes: &HashSet<(RL, Nat, u64)>, max_data_bid: u64) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut keys: Vec<RL> = agg.keys().cloned().collect();
        keys.extend(self.dense.keys().cloned());
        keys.extend(self.sparse.keys().cloned());
        keys.extend(self.max_u.keys().cloned());
        keys.sort();
        keys.dedup();
        let empty = DeepAgg::default();
        for key in &keys {
            let a = agg.get_mut(key).map(|a| &mut *a);
            let a: &DeepAgg = match a {
                Some(a) => {
                    a.dense.sort();
                    // sorted by start: a range overlaps if it starts at or
                    // before the largest end seen so far
                    let mut reach: Option<&Nat> = None;
                    let mut overlap = false;
                    for (lo, hi) in &a.dense {
                        if reach.is_some_and(|r| lo <= r) {
                            overlap = true;
                            break;
                        }
                        if reach.is_none_or(|r| hi > r) {
                            reach = Some(hi);
                        }
                    }
                    if overlap && !self.dense_overlap_keys.contains(key) {
                        out.push(format!("realm {} lane {}: overlapping dense segments not flagged", key.0, key.1));
                    }
                    if (overlap || a.overlay) && (!self.overlay_keys.contains(key) || !self.has_overlays) {
                        out.push(format!("realm {} lane {}: overlays not flagged", key.0, key.1));
                    }
                    a
                }
                None => &empty,
            };
            let d = self.dense.get(key);
            let s = self.sparse.get(key);
            let mem = (
                d.map(|x| x.records).unwrap_or(0),
                d.map(|x| x.shapes.clone()).unwrap_or_default(),
                s.map(|x| x.groups).unwrap_or(0),
                s.map(|x| x.cells).unwrap_or(0),
            );
            if mem != (a.records, a.shapes.clone(), a.groups, a.cells) {
                out.push(format!("realm {} lane {}: aggregates differ from the data blocks", key.0, key.1));
            }
            if self.max_u.get(key).filter(|m| !m.is_zero()) != a.max_u.as_ref() {
                out.push(format!("realm {} lane {}: largest dense u differs from the data blocks", key.0, key.1));
            }
            if self.data_max_block_id_by_lane.get(key).copied().unwrap_or(0) < a.max_bid {
                out.push(format!("realm {} lane {}: newest data block id differs", key.0, key.1));
            }
        }
        if self.data_max_block_id < max_data_bid {
            out.push("newest data block id differs from the data blocks".into());
        }
        // every DELETE entry of the directory belongs to a DELETE block
        let mut forged = 0u64;
        for (key, d) in &self.deletes {
            for (u, bid) in &d.tail {
                if !deletes.contains(&(key.clone(), u.clone(), *bid)) {
                    forged += 1;
                }
            }
            let mut it = d.runs.iter_by_start();
            while let Some(e) = it.next(self)? {
                if !deletes.contains(&(key.clone(), e.u.clone(), e.block_id)) {
                    forged += 1;
                }
            }
        }
        if forged > 0 {
            out.push(format!("block directory lists {forged} DELETE entr(y/ies) without a DELETE block"));
        }
        Ok(out)
    }
}

impl Db {
    /// Subtracts every dense/sparse entry of the in-memory segment indexes
    /// (directory runs + tail) from `fp`; returns a description of the first
    /// key whose entries differ from the scanned blocks.
    fn directory_fingerprint_mismatch(&self, fp: &mut DirFingerprint) -> Result<Option<String>> {
        for ix in self.dense.values() {
            let mut it = ix.idx.iter_by_start();
            while let Some(s) = it.next(self)? {
                fp.add_dense(&s, -1);
            }
        }
        for ix in self.sparse.values() {
            let mut it = ix.idx.iter_by_start();
            while let Some(s) = it.next(self)? {
                fp.add_sparse(&s, -1);
            }
        }
        Ok(fp.acc.iter().find(|(_, (f, n))| *f != 0 || *n != 0).map(|((kind, (r, l)), (_, n))| {
            format!("{} segments of realm {r} lane {l} differ (count difference {n})", if *kind == 1 { "dense" } else { "sparse" })
        }))
    }
}

/// Records/batches written by a compaction.
#[derive(Default)]
struct CompactCounters {
    records: i64,
    batches: i64,
    lanes: [i64; 2],
}

/// Sparse cells collected for one `put_points` of the compaction target.
struct CellChunk {
    cells: Vec<CellInput>,
    bytes: usize,
}

impl CellChunk {
    const CELLS: usize = 50_000;
    const BYTES: usize = 32 << 20;

    fn push(&mut self, dst: &mut Db, rid: &Nat, lane: Lane, cell: CellInput, bytes: usize) -> Result<()> {
        if self.add(cell, bytes) {
            self.flush(dst, rid, lane)?;
        }
        Ok(())
    }

    /// Adds a cell without writing; `true`: the chunk is full and must be
    /// flushed by the caller.
    fn add(&mut self, cell: CellInput, bytes: usize) -> bool {
        self.cells.push(cell);
        self.bytes += bytes;
        self.cells.len() >= Self::CELLS || self.bytes >= Self::BYTES
    }

    fn flush(&mut self, dst: &mut Db, rid: &Nat, lane: Lane) -> Result<()> {
        if !self.cells.is_empty() {
            // the chunk is sorted by (u, v): one block per chunk covers a u
            // range disjoint from the other chunks, a u window would only
            // split scattered u into many tiny blocks
            dst.put_points(rid, std::mem::take(&mut self.cells), lane, false, u64::MAX)?;
        }
        self.bytes = 0;
        Ok(())
    }
}

/// Sparse cell for the target: values always, tombstones only where they
/// hide a dense value of the source (BASE field of the stored shape or a
/// dense EXT value of that record) – `dense` describes the live dense
/// record at the cell's u, if any.
///
/// F1: a state older than the live dense record of one of its shape fields
/// is hidden by the record and dropped (`dense_bid` = block id of the
/// record in the source). The surviving states of a u must reach the target
/// after its dense record (higher block id), see `compact_key`.
fn sparse_cell_out(c: &MergedCell, dense: Option<(&ShapeDef, &[Nat], u64)>) -> Option<CellInput> {
    if let Some((shape, _, dense_bid)) = dense {
        if super::dense_hides(shape, dense_bid, &c.v, c.bid) {
            return None;
        }
    }
    let dense = dense.map(|(shape, ext_vs, _)| (shape, ext_vs));
    match &c.meta.value {
        SVal::V(val) => Some(CellInput::typed(c.u.clone(), c.v.clone(), val.clone(), if c.meta.selftyped { c.meta.typ } else { None })),
        SVal::Tomb => {
            let shadows = dense.is_some_and(|(shape, ext_vs)| shape.by_v(&c.v).is_some() || ext_vs.binary_search(&c.v).is_ok());
            shadows.then(|| CellInput::tombstone(c.u.clone(), c.v.clone()))
        }
    }
}

/// Copies the live state of one `(realm, lane)` into the compaction target:
/// dense records in u order (raw BASE bytes, batches of `batch_records`),
/// their EXT values and the newest sparse states from the merge, consumed
/// in the same u order (bounded memory, `merge.rs`).
fn compact_key(src: &Db, dst: &mut Db, key: &RL, batch_records: usize, run_dir: &Path, cnt: &mut CompactCounters) -> Result<()> {
    let mut meter = crate::limiter::Meter::new(crate::limiter::JobKind::Rigid, crate::limiter::JobPath::Compact);
    let (rid, lane) = key.clone();
    let mut merge = match src.sparse.contains_key(key) {
        true => Some(SparseMerge::with_temp_dir(src, &rid, lane, Verify::Incremental, run_dir)?),
        false => None,
    };
    let mut look: Option<MergedCell> = None;
    let mut sparse_out = CellChunk { cells: Vec::new(), bytes: 0 };
    let mut ext_out = CellChunk { cells: Vec::new(), bytes: 0 };
    // next sparse cell (lookahead)
    macro_rules! peek_cell {
        () => {{
            if look.is_none() {
                if let Some(m) = merge.as_mut() {
                    look = m.next(src)?;
                }
            }
            look.as_ref()
        }};
    }
    if let Some(idx) = src.dense.get(key) {
        let mut area: Vec<u8> = Vec::new();
        let mut offsets: Vec<u32> = vec![0];
        let mut start: Option<Nat> = None;
        let mut last_u: Option<Nat> = None;
        let mut cur_shape: Option<u64> = None;
        let mut flush = |dst: &mut Db, area: &mut Vec<u8>, offsets: &mut Vec<u32>, start: &Option<Nat>, shape: Option<u64>| -> Result<usize> {
            let n = offsets.len() - 1;
            if n == 0 {
                return Ok(0);
            }
            dst.put_dense_batch(&rid, start.as_ref().unwrap(), &DenseRows::Raw { area, offsets }, shape, &[], lane, false)?;
            // RUN¹⁰: checkpoint of the RAM limiter per MiB copied
            meter.grow(area.len() as u64)?;
            area.clear();
            offsets.truncate(1);
            Ok(n)
        };
        let mut cursor: Option<Nat> = None;
        let mut segs = idx.idx.iter_by_start();
        // without overlapping segments and deletes every u of a segment is
        // served by that segment: no lookup per u
        let direct = !src.dense_overlap_keys.contains(key) && !src.has_deletes(key);
        while let Some(seg) = segs.next(src)? {
            let mut u = match &cursor {
                Some(c) if c > &seg.u_start => c.clone(),
                _ => seg.u_start.clone(),
            };
            while u <= seg.u_end {
                let win = if direct { Some(seg.clone()) } else { src.resolve_segment(&rid, &u, lane)? };
                if let Some(win) = win {
                    // sparse cells of u without a live dense record
                    while let Some(c) = peek_cell!() {
                        if c.u >= u {
                            break;
                        }
                        let c = look.take().unwrap();
                        if let Some(cell) = sparse_cell_out(&c, None) {
                            // the chunk may hold cells of a pending dense
                            // batch: that batch goes first (F1)
                            if sparse_out.add(cell, c.bytes) {
                                let n = flush(dst, &mut area, &mut offsets, &start, cur_shape)?;
                                if n > 0 {
                                    cnt.records += n as i64;
                                    cnt.lanes[lane as usize] += n as i64;
                                    cnt.batches += 1;
                                }
                                sparse_out.flush(dst, &rid, lane)?;
                            }
                        }
                    }
                    let shape = src.shape_of(win.shape_id)?;
                    let rows = offsets.len() - 1;
                    let compatible = rows > 0
                        && cur_shape == Some(win.shape_id)
                        && last_u.as_ref().map(|l| l.add_u64(1) == u).unwrap_or(false)
                        && rows < batch_records;
                    if rows > 0 && !compatible {
                        let n = flush(dst, &mut area, &mut offsets, &start, cur_shape)?;
                        cnt.records += n as i64;
                        cnt.lanes[lane as usize] += n as i64;
                        cnt.batches += 1;
                    }
                    if offsets.len() == 1 {
                        start = Some(u.clone());
                        cur_shape = Some(win.shape_id);
                    }
                    let mut exts = src.with_record_bytes(&win, &u, |raw| {
                        let mut p = 0usize;
                        for f in &shape.fields {
                            crate::codec::skip_value(f, raw, &mut p)?;
                        }
                        area.extend_from_slice(&raw[..p]);
                        let (ext, end) = src.decode_extensions(&rid, raw, p)?;
                        if end != raw.len() {
                            return Err(Error::Corruption("Dense record trailing bytes".into()));
                        }
                        Ok(ext)
                    })?;
                    let end = u32::try_from(area.len())
                        .map_err(|_| Error::Value("a DATA batch may contain at most 4 GiB of record data".into()))?;
                    offsets.push(end);
                    // sparse cells of u: they win over the dense EXT values
                    exts.sort_by(|a, b| a.0.cmp(&b.0));
                    let ext_vs: Vec<Nat> = exts.iter().map(|(v, _)| v.clone()).collect();
                    let mut hidden = vec![false; exts.len()];
                    while let Some(c) = peek_cell!() {
                        if c.u != u {
                            break;
                        }
                        let c = look.take().unwrap();
                        if let Ok(i) = ext_vs.binary_search(&c.v) {
                            hidden[i] = true;
                        }
                        if let Some(cell) = sparse_cell_out(&c, Some((shape, &ext_vs, win.block_id))) {
                            // F1: the cells of u must follow the dense record
                            // of u in the target, so the pending dense batch
                            // is written before a full chunk
                            if sparse_out.add(cell, c.bytes) {
                                let n = flush(dst, &mut area, &mut offsets, &start, cur_shape)?;
                                if n > 0 {
                                    cnt.records += n as i64;
                                    cnt.lanes[lane as usize] += n as i64;
                                    cnt.batches += 1;
                                }
                                sparse_out.flush(dst, &rid, lane)?;
                            }
                        }
                    }
                    for ((v, val), h) in exts.into_iter().zip(hidden) {
                        if !h {
                            ext_out.push(dst, &rid, lane, CellInput::new(u.clone(), v, val), 0)?;
                        }
                    }
                    last_u = Some(u.clone());
                    if offsets.len() > batch_records {
                        let n = flush(dst, &mut area, &mut offsets, &start, cur_shape)?;
                        cnt.records += n as i64;
                        cnt.lanes[lane as usize] += n as i64;
                        cnt.batches += 1;
                    }
                }
                u = u.add_u64(1);
            }
            cursor = Some(u);
        }
        let n = flush(dst, &mut area, &mut offsets, &start, cur_shape)?;
        if n > 0 {
            cnt.records += n as i64;
            cnt.lanes[lane as usize] += n as i64;
            cnt.batches += 1;
        }
    }
    // remaining sparse cells: no live dense record at their u
    while peek_cell!().is_some() {
        let c = look.take().unwrap();
        if let Some(cell) = sparse_cell_out(&c, None) {
            sparse_out.push(dst, &rid, lane, cell, c.bytes)?;
        }
    }
    ext_out.flush(dst, &rid, lane)?;
    sparse_out.flush(dst, &rid, lane)?;
    Ok(())
}
