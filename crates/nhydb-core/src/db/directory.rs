//! Persistent block directory (file format 0.28.2, block type `DIRECTORY`).
//!
//! The directory replaces the full open scan and the full in-memory
//! segment registry for large databases:
//!
//! * **Pages** (`DP17`): the entries of one `(kind, realm, lane)` sorted by
//!   `(u_start, block_id)`, delta/varint coded (dense and sparse segments,
//!   DELETE entries). At most [`PAGE_ENTRIES`] entries per page.
//! * **Runs** (`DR17`): one directory run lists, per key, the summaries of
//!   its pages (offset, entry count, first u, largest u end, largest block
//!   id). A run is written for the blocks registered since the previous
//!   flush (level 0); [`MERGE_FANOUT`] runs of the same level are merged into
//!   one run of the next level (amortised O(log n) rewrites per entry).
//! * **Root** (`DT17`): the list of live runs and the state the open scan
//!   would otherwise collect (newest SYSTEM, DB_CONFIG and CHECKPOINT block,
//!   live SECONDARY blocks, per-key aggregates). The superblock (field
//!   `dir_root`) points to the root of the committed directory.
//!
//! A directory covers every block before its root. The open loads the root
//! and the run summaries (CRC-checked blocks) and scans only the blocks
//! behind the root; pages are read on demand (CRC-checked, checked against
//! their summary, cached with a budget). A run is written when the blocks
//! or bytes registered since the last flush reach the configured thresholds
//! – after a write call (an optional attempt: a failure is rolled back and
//! never fails the write call) or in `commit()` – at a `checkpoint()` when
//! it pays off, and once completely merged for a compaction target. Every
//! write lies before the superblock: a crash leaves the previous directory
//! valid. A directory that cannot be read falls back to the full scan; a
//! directory block with a damaged header is skipped as redundant; the
//! directory never replaces an integrity check of the data blocks (their
//! payload CRC and their binding to the entry are checked on first access,
//! `verify(deep=True)` compares the whole directory with the data blocks).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::address::Lane;
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, get_vu_u64, put_vu64, Nat};
use crate::val::Val;
use crate::vmap;

use super::segindex::{Page, PageRef, PageSource, RunPart, SegEntry, SegIndex};
use super::{system, Db, DenseSegment, SparseSegment, RL};

pub const PAGE_TAG: &[u8; 4] = b"DP17";
pub const RUN_TAG: &[u8; 4] = b"DR17";
pub const ROOT_TAG: &[u8; 4] = b"DT17";
/// Entries per directory page.
pub const PAGE_ENTRIES: usize = 32;
/// Runs of one level merged into one run of the next level.
pub const MERGE_FANOUT: usize = 8;

pub(crate) const KIND_DENSE: u8 = 1;
pub(crate) const KIND_SPARSE: u8 = 2;
pub(crate) const KIND_DELETE: u8 = 3;

/// A forced flush (`checkpoint`, compaction) writes a directory only for at
/// least this many new entries or bytes: below that the open scan of the
/// blocks costs less than the directory blocks would take on disk.
pub const FORCED_MIN_ENTRIES: u64 = 1024;
pub const FORCED_MIN_BYTES: u64 = 64 << 20;
/// ... and on average at least this many new entries per page part.
pub const FORCED_MIN_PER_PART: u64 = 8;

/// Defaults of the `[directory]` configuration section.
pub const DEFAULT_FLUSH_BLOCKS: u64 = 65_536;
pub const DEFAULT_FLUSH_BYTES: u64 = 256 << 20;
pub const DEFAULT_CACHE_ENTRIES: usize = 1 << 18;

/// DELETE entry: u deleted by block `block_id`.
#[derive(Clone, Debug, PartialEq)]
pub struct DelEntry {
    pub u: Nat,
    pub block_id: u64,
}

impl SegEntry for DenseSegment {
    fn lo(&self) -> &Nat {
        &self.u_start
    }
    fn hi(&self) -> &Nat {
        &self.u_end
    }
    fn bid(&self) -> u64 {
        self.block_id
    }
}

impl SegEntry for SparseSegment {
    fn lo(&self) -> &Nat {
        &self.u_start
    }
    fn hi(&self) -> &Nat {
        &self.u_end
    }
    fn bid(&self) -> u64 {
        self.block_id
    }
}

impl SegEntry for DelEntry {
    fn lo(&self) -> &Nat {
        &self.u
    }
    fn hi(&self) -> &Nat {
        &self.u
    }
    fn bid(&self) -> u64 {
        self.block_id
    }
}

/// DELETE entries of one key: persisted runs plus the newest delete per u
/// since the last flush.
#[derive(Clone, Debug, Default)]
pub(crate) struct DelIndex {
    pub runs: SegIndex<DelEntry>,
    pub tail: HashMap<Nat, u64>,
}

impl DelIndex {
    pub(crate) fn is_empty(&self) -> bool {
        self.tail.is_empty() && self.runs.is_empty()
    }
}

/// Why a directory flush is attempted.
/// Unlisted blocks (SECONDARY pages) after which a commit writes a root.
pub(crate) const UNLISTED_MIN: u64 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlushMode {
    /// after a write call: only when the thresholds are reached, and never
    /// an error of the write call (RA-05) – a failed attempt is rolled back
    /// and retried at the commit or after the next threshold
    AfterWrite,
    /// inside `commit()` before the superblock: I/O errors are reported
    Commit,
    /// `checkpoint()`: also below the thresholds when it pays off
    Checkpoint,
    /// checkpoint of a compaction target: like `Checkpoint`, and all runs
    /// are merged into one (written once per compaction)
    Compaction,
}

/// Live directory run.
#[derive(Clone, Debug)]
pub(crate) struct RunMeta {
    pub off: u64,
    pub level: u32,
}

/// Directory state of a handle.
#[derive(Clone, Debug, Default)]
pub(crate) struct DirState {
    /// root of the directory this handle's state is based on (0 = none)
    pub root_off: u64,
    /// live runs, oldest first
    pub runs: Vec<RunMeta>,
    /// data/delete entries registered since the last flush
    pub tail_entries: u64,
    /// payload bytes of data blocks registered since the last flush
    pub tail_bytes: u64,
    /// a failed flush attempt: no new attempt after write calls before the
    /// tail reaches these values (RA-04: no retry on every write call)
    pub retry_entries: u64,
    pub retry_bytes: u64,
    /// merging old runs failed (damaged live directory page): new runs are
    /// still written, nothing is merged any more in this session; reported
    /// by `stats()`, `verify()` and `health()` (repair drops the directory)
    pub degraded: Option<String>,
    /// reason of the last rolled-back flush that found damaged data (RD-08:
    /// reported by `directory_stats()`, `verify()` and `health()`)
    pub flush_error: Option<String>,
    /// blocks the open scan visits although no directory entry lists them
    /// (SECONDARY pages written or scanned since the last root): a commit
    /// writes a root once there are `UNLISTED_MIN` of them, so that the next
    /// open skips them (0.28.2)
    pub unlisted_blocks: u64,
    /// block id of the last block before the range an open scan continues
    /// with (the directory root, or the end of an earlier scan): the first
    /// scanned block must have a larger id (RA2-02)
    pub scan_last_id: u64,
    /// newest SYSTEM, DB_CONFIG and CHECKPOINT block (0 = none)
    pub last_system_off: u64,
    pub last_config_off: u64,
    pub last_checkpoint_off: u64,
    /// directory blocks written by this handle or loaded (statistics)
    pub stats: DirStats,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DirStats {
    pub flushes: u64,
    pub merges: u64,
    pub pages_written: u64,
    pub bytes_written: u64,
    pub loaded: bool,
    pub fallback: bool,
    /// flush attempts rolled back (size limit, I/O error, damaged page)
    pub failed: u64,
    /// directory blocks with a damaged header skipped by the open scan
    pub damaged: u64,
}

/// Decoded page of any kind (cache value).
#[derive(Clone)]
pub(crate) enum AnyPage {
    Dense(Arc<Page<DenseSegment>>),
    Sparse(Arc<Page<SparseSegment>>),
    Delete(Arc<Page<DelEntry>>),
}

impl AnyPage {
    pub(crate) fn len(&self) -> usize {
        match self {
            AnyPage::Dense(p) => p.entries.len(),
            AnyPage::Sparse(p) => p.entries.len(),
            AnyPage::Delete(p) => p.entries.len(),
        }
    }

    /// Estimated heap bytes (entries plus the page itself).
    pub(crate) fn cost(&self) -> usize {
        128 + match self {
            AnyPage::Dense(p) => p.entries.capacity() * std::mem::size_of::<DenseSegment>(),
            AnyPage::Sparse(p) => p.entries.capacity() * std::mem::size_of::<SparseSegment>(),
            AnyPage::Delete(p) => p.entries.capacity() * std::mem::size_of::<DelEntry>(),
        }
    }
}

/// Process pot: evicts least recently used directory pages until at most
/// `limit` bytes are held; returns the bytes freed.
pub(crate) fn evict_dir_pages_to(c: &mut super::Caches, limit: usize) -> usize {
    if c.dir_pages_bytes <= limit {
        return 0;
    }
    let before = c.dir_pages_bytes;
    let mut order: Vec<(u64, u64)> = c.dir_pages.iter().map(|(o, (_, t))| (*t, *o)).collect();
    order.sort_unstable();
    for (_, o) in order {
        if c.dir_pages_bytes <= limit {
            break;
        }
        if let Some((p, _)) = c.dir_pages.remove(&o) {
            c.dir_pages_used = c.dir_pages_used.saturating_sub(p.len());
            c.dir_pages_bytes = c.dir_pages_bytes.saturating_sub(p.cost());
        }
    }
    before - c.dir_pages_bytes
}

fn zz(d: i64) -> u64 {
    ((d << 1) ^ (d >> 63)) as u64
}

fn unzz(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corruption(format!("block directory: {}", msg.into()))
}

fn put_delta(out: &mut Vec<u8>, cur: u64, prev: u64) -> Result<()> {
    let d = i64::try_from(cur as i128 - prev as i128).map_err(|_| corrupt("delta out of range"))?;
    put_vu64(out, zz(d));
    Ok(())
}

fn get_delta(buf: &[u8], p: &mut usize, prev: u64) -> Result<u64> {
    let d = unzz(get_vu_u64(buf, p, "directory delta")?);
    u64::try_from(prev as i128 + d as i128).map_err(|_| corrupt("delta out of range"))
}

/// Entry codec of one kind (delta-coded against the previous entry).
pub(crate) trait DirCodec: SegEntry + Sized {
    const KIND: u8;
    fn encode(&self, prev: Option<&Self>, out: &mut Vec<u8>) -> Result<()>;
    fn decode(buf: &[u8], p: &mut usize, prev: Option<&Self>, realm: &Nat, lane: Lane) -> Result<Self>;
    fn wrap(page: Arc<Page<Self>>) -> AnyPage;
    fn unwrap(page: &AnyPage) -> Option<Arc<Page<Self>>>;
    /// End of the referenced data block (offset behind its payload).
    fn block_end(&self) -> Option<u64>;
}

fn lo_delta<T: SegEntry>(e: &T, prev: Option<&T>, out: &mut Vec<u8>) -> Result<()> {
    let base = prev.map(|x| x.lo().clone()).unwrap_or(Nat::ZERO);
    let d = e.lo().checked_sub(&base).ok_or_else(|| corrupt("entries not sorted"))?;
    d.put_vu(out);
    put_delta(out, e.bid(), prev.map(|x| x.bid()).unwrap_or(0))
}

fn read_lo_bid<T: SegEntry>(buf: &[u8], p: &mut usize, prev: Option<&T>) -> Result<(Nat, u64)> {
    let d = get_vu(buf, p)?;
    let lo = match prev {
        Some(x) => x.lo().add(&d),
        None => d,
    };
    let bid = get_delta(buf, p, prev.map(|x| x.bid()).unwrap_or(0))?;
    Ok((lo, bid))
}

fn put_off(out: &mut Vec<u8>, off: u64, prev: u64) -> Result<()> {
    if !off.is_multiple_of(8) {
        return Err(corrupt(format!("block offset {off} not aligned")));
    }
    put_delta(out, off / 8, prev / 8)
}

fn get_off(buf: &[u8], p: &mut usize, prev: u64) -> Result<u64> {
    let q = get_delta(buf, p, prev / 8)?;
    q.checked_mul(8).ok_or_else(|| corrupt("block offset out of range"))
}

impl DirCodec for DenseSegment {
    const KIND: u8 = KIND_DENSE;
    fn encode(&self, prev: Option<&Self>, out: &mut Vec<u8>) -> Result<()> {
        lo_delta(self, prev, out)?;
        put_vu64(out, self.count);
        put_vu64(out, self.shape_id);
        put_off(out, self.block_offset, prev.map(|x| x.block_offset).unwrap_or(0))?;
        put_vu64(out, self.payload_len);
        put_vu64(out, self.offsets_offset - self.payload_offset);
        Ok(())
    }
    fn decode(buf: &[u8], p: &mut usize, prev: Option<&Self>, realm: &Nat, lane: Lane) -> Result<Self> {
        let (u_start, block_id) = read_lo_bid(buf, p, prev)?;
        let count = get_vu_u64(buf, p, "directory count")?;
        let shape_id = get_vu_u64(buf, p, "directory shape")?;
        let block_offset = get_off(buf, p, prev.map(|x| x.block_offset).unwrap_or(0))?;
        let payload_len = get_vu_u64(buf, p, "directory payload_len")?;
        let prefix = get_vu_u64(buf, p, "directory prefix")?;
        let payload_offset = block_offset.checked_add(BLOCK_HEADER_SIZE as u64).ok_or_else(|| corrupt("block offset out of range"))?;
        let offsets_offset = payload_offset.checked_add(prefix).ok_or_else(|| corrupt("prefix out of range"))?;
        let records_offset = count
            .checked_add(1)
            .and_then(|c| c.checked_mul(4))
            .and_then(|t| t.checked_add(offsets_offset))
            .filter(|r| *r <= payload_offset.saturating_add(payload_len))
            .ok_or_else(|| corrupt("dense offset table outside the block"))?;
        if u_start.is_zero() {
            return Err(corrupt("dense u_start = 0"));
        }
        let u_end = if count == 0 { u_start.clone() } else { u_start.add_u64(count - 1) };
        Ok(DenseSegment {
            realm_id: realm.clone(),
            lane,
            shape_id,
            u_start,
            u_end,
            count,
            block_id,
            block_offset,
            payload_offset,
            offsets_offset,
            records_offset,
            payload_len,
        })
    }
    fn wrap(page: Arc<Page<Self>>) -> AnyPage {
        AnyPage::Dense(page)
    }
    fn unwrap(page: &AnyPage) -> Option<Arc<Page<Self>>> {
        match page {
            AnyPage::Dense(p) => Some(p.clone()),
            _ => None,
        }
    }
    fn block_end(&self) -> Option<u64> {
        Some(self.payload_offset.saturating_add(self.payload_len))
    }
}

impl DirCodec for SparseSegment {
    const KIND: u8 = KIND_SPARSE;
    fn encode(&self, prev: Option<&Self>, out: &mut Vec<u8>) -> Result<()> {
        lo_delta(self, prev, out)?;
        self.u_end.checked_sub(&self.u_start).ok_or_else(|| corrupt("u_end < u_start"))?.put_vu(out);
        put_off(out, self.block_offset, prev.map(|x| x.block_offset).unwrap_or(0))?;
        put_vu64(out, self.payload_len);
        put_vu64(out, self.entries_offset - self.payload_offset);
        put_vu64(out, self.group_count);
        put_vu64(out, self.cell_count);
        Ok(())
    }
    fn decode(buf: &[u8], p: &mut usize, prev: Option<&Self>, realm: &Nat, lane: Lane) -> Result<Self> {
        let (u_start, block_id) = read_lo_bid(buf, p, prev)?;
        let span = get_vu(buf, p)?;
        let block_offset = get_off(buf, p, prev.map(|x| x.block_offset).unwrap_or(0))?;
        let payload_len = get_vu_u64(buf, p, "directory payload_len")?;
        let prefix = get_vu_u64(buf, p, "directory prefix")?;
        let group_count = get_vu_u64(buf, p, "directory groups")?;
        let cell_count = get_vu_u64(buf, p, "directory cells")?;
        if prefix > payload_len || u_start.is_zero() {
            return Err(corrupt("sparse prefix outside the block"));
        }
        let payload_offset = block_offset.checked_add(BLOCK_HEADER_SIZE as u64).ok_or_else(|| corrupt("block offset out of range"))?;
        payload_offset.checked_add(payload_len).ok_or_else(|| corrupt("payload length out of range"))?;
        Ok(SparseSegment {
            realm_id: realm.clone(),
            lane,
            u_end: u_start.add(&span),
            u_start,
            group_count,
            cell_count,
            block_id,
            block_offset,
            payload_offset,
            entries_offset: payload_offset + prefix,
            payload_len,
        })
    }
    fn wrap(page: Arc<Page<Self>>) -> AnyPage {
        AnyPage::Sparse(page)
    }
    fn unwrap(page: &AnyPage) -> Option<Arc<Page<Self>>> {
        match page {
            AnyPage::Sparse(p) => Some(p.clone()),
            _ => None,
        }
    }
    fn block_end(&self) -> Option<u64> {
        Some(self.payload_offset.saturating_add(self.payload_len))
    }
}

impl DirCodec for DelEntry {
    const KIND: u8 = KIND_DELETE;
    fn encode(&self, prev: Option<&Self>, out: &mut Vec<u8>) -> Result<()> {
        lo_delta(self, prev, out)
    }
    fn decode(buf: &[u8], p: &mut usize, prev: Option<&Self>, _realm: &Nat, _lane: Lane) -> Result<Self> {
        let (u, block_id) = read_lo_bid(buf, p, prev)?;
        Ok(DelEntry { u, block_id })
    }
    fn wrap(page: Arc<Page<Self>>) -> AnyPage {
        AnyPage::Delete(page)
    }
    fn unwrap(page: &AnyPage) -> Option<Arc<Page<Self>>> {
        match page {
            AnyPage::Delete(p) => Some(p.clone()),
            _ => None,
        }
    }
    fn block_end(&self) -> Option<u64> {
        None
    }
}

/// Encodes one page payload.
fn encode_page<T: DirCodec>(realm: &Nat, lane: Lane, entries: &[T]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(16 + entries.len() * 12);
    out.extend_from_slice(PAGE_TAG);
    out.push(T::KIND);
    realm.put_vu(&mut out);
    out.push(lane);
    put_vu64(&mut out, entries.len() as u64);
    let mut prev: Option<&T> = None;
    for e in entries {
        e.encode(prev, &mut out)?;
        prev = Some(e);
    }
    Ok(out)
}

/// Decodes and validates one page payload against its summary.
fn decode_page<T: DirCodec>(payload: &[u8], r: &PageRef) -> Result<Page<T>> {
    if !payload.starts_with(PAGE_TAG) || payload.len() < 7 {
        return Err(corrupt(format!("page at byte {}: wrong tag", r.off)));
    }
    if payload[4] != T::KIND {
        return Err(corrupt(format!("page at byte {}: kind {} expected {}", r.off, payload[4], T::KIND)));
    }
    let mut p = 5usize;
    let realm = get_vu(payload, &mut p)?;
    let lane = *payload.get(p).ok_or_else(|| corrupt("page truncated"))?;
    p += 1;
    if lane > 1 {
        return Err(corrupt("page lane invalid"));
    }
    let n = get_vu_u64(payload, &mut p, "directory page entries")?;
    if n != r.n as u64 || n == 0 || n > PAGE_ENTRIES as u64 {
        return Err(corrupt(format!("page at byte {}: {n} entries, summary says {}", r.off, r.n)));
    }
    let mut entries: Vec<T> = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let e = T::decode(payload, &mut p, entries.last(), &realm, lane)?;
        if let Some(last) = entries.last() {
            if (e.lo(), e.bid()) <= (last.lo(), last.bid()) {
                return Err(corrupt("page entries not sorted"));
            }
        }
        if e.hi() < e.lo() {
            return Err(corrupt("entry interval invalid"));
        }
        entries.push(e);
    }
    if p != payload.len() {
        return Err(corrupt("page trailing bytes"));
    }
    let page = Page::new(entries);
    let max_bid = page.entries.iter().map(|e| e.bid()).max().unwrap_or(0);
    if page.entries[0].lo() != &r.first || page.pmax.last() != Some(&r.max_hi) || max_bid != r.max_bid {
        return Err(corrupt(format!("page at byte {} contradicts its summary", r.off)));
    }
    Ok(page)
}

/// Whether a decoded page has the summary `r` (entry count, first u,
/// largest u end, largest block id).
fn page_matches<T: SegEntry>(p: &Page<T>, r: &PageRef) -> bool {
    p.entries.len() == r.n as usize
        && p.entries.first().map(|e| e.lo()) == Some(&r.first)
        && p.pmax.last() == Some(&r.max_hi)
        && p.entries.iter().map(|e| e.bid()).max() == Some(r.max_bid)
}

/// One run part as written into a run block.
struct PartOut {
    kind: u8,
    realm: Nat,
    lane: Lane,
    pages: Vec<PageRef>,
}

/// Collects entries into pages and appends them as DIRECTORY blocks.
struct PageWriter<T> {
    realm: Nat,
    lane: Lane,
    buf: Vec<T>,
    pages: Vec<PageRef>,
}

impl<T: DirCodec> PageWriter<T> {
    fn new(realm: Nat, lane: Lane) -> Self {
        PageWriter { realm, lane, buf: Vec::with_capacity(PAGE_ENTRIES), pages: Vec::new() }
    }

    fn push(&mut self, db: &mut Db, e: T) -> Result<()> {
        self.buf.push(e);
        if self.buf.len() >= PAGE_ENTRIES {
            self.emit(db)?;
        }
        Ok(())
    }

    fn emit(&mut self, db: &mut Db) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let payload = encode_page(&self.realm, self.lane, &self.buf)?;
        let first = self.buf[0].lo().clone();
        let mut max_hi = self.buf[0].hi().clone();
        for e in &self.buf {
            if e.hi() > &max_hi {
                max_hi = e.hi().clone();
            }
        }
        let n = self.buf.len() as u32;
        let max_bid = self.buf.iter().map(|e| e.bid()).max().unwrap_or(0);
        let (off, _) = db.append_block(BT_DIRECTORY, &payload, 0)?;
        db.dir.stats.pages_written += 1;
        db.dir.stats.bytes_written += (BLOCK_HEADER_SIZE + payload.len()) as u64;
        self.pages.push(PageRef { off, n, first, max_hi, max_bid });
        self.buf.clear();
        Ok(())
    }

    fn finish(mut self, db: &mut Db) -> Result<PartOut> {
        self.emit(db)?;
        Ok(PartOut { kind: T::KIND, realm: self.realm, lane: self.lane, pages: self.pages })
    }
}

fn encode_run(level: u32, parts: &[PartOut]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(RUN_TAG);
    put_vu64(&mut out, level as u64);
    put_vu64(&mut out, parts.len() as u64);
    for part in parts {
        out.push(part.kind);
        part.realm.put_vu(&mut out);
        out.push(part.lane);
        put_vu64(&mut out, part.pages.len() as u64);
        let mut prev_first = Nat::ZERO;
        for pg in &part.pages {
            put_vu64(&mut out, pg.off / 8);
            put_vu64(&mut out, pg.n as u64);
            // pages of a part are sorted by their first entry
            pg.first.checked_sub(&prev_first).unwrap_or(Nat::ZERO).put_vu(&mut out);
            pg.max_hi.checked_sub(&pg.first).unwrap_or(Nat::ZERO).put_vu(&mut out);
            put_vu64(&mut out, pg.max_bid);
            prev_first = pg.first.clone();
        }
    }
    out
}

/// Parsed run block: level and parts `(kind, realm, lane, pages)`.
#[allow(clippy::type_complexity)]
fn decode_run(payload: &[u8], run_off: u64) -> Result<(u32, Vec<(u8, Nat, Lane, Vec<PageRef>)>)> {
    if !payload.starts_with(RUN_TAG) {
        return Err(corrupt(format!("run at byte {run_off}: wrong tag")));
    }
    let mut p = 4usize;
    let level = u32::try_from(get_vu_u64(payload, &mut p, "run level")?).map_err(|_| corrupt("run level"))?;
    let nparts = get_vu_u64(payload, &mut p, "run parts")?;
    let mut parts = Vec::new();
    for _ in 0..nparts {
        let kind = *payload.get(p).ok_or_else(|| corrupt("run truncated"))?;
        p += 1;
        if !(KIND_DENSE..=KIND_DELETE).contains(&kind) {
            return Err(corrupt("run part kind invalid"));
        }
        let realm = get_vu(payload, &mut p)?;
        let lane = *payload.get(p).ok_or_else(|| corrupt("run truncated"))?;
        p += 1;
        if lane > 1 {
            return Err(corrupt("run part lane invalid"));
        }
        let npages = get_vu_u64(payload, &mut p, "run pages")?;
        if npages > (payload.len() as u64) {
            return Err(corrupt("run page count exceeds the block"));
        }
        let mut pages = Vec::with_capacity(npages as usize);
        let mut prev_first = Nat::ZERO;
        let mut prev_off = 0u64;
        for _ in 0..npages {
            let off = get_vu_u64(payload, &mut p, "page offset")?.checked_mul(8).ok_or_else(|| corrupt("page offset"))?;
            let n = get_vu_u64(payload, &mut p, "page entries")?;
            let first = prev_first.add(&get_vu(payload, &mut p)?);
            let max_hi = first.add(&get_vu(payload, &mut p)?);
            let max_bid = get_vu_u64(payload, &mut p, "page max block id")?;
            if off < DATA_START || off >= run_off || off <= prev_off || n == 0 || n > PAGE_ENTRIES as u64 {
                return Err(corrupt(format!("run at byte {run_off}: page reference invalid")));
            }
            prev_off = off;
            prev_first = first.clone();
            pages.push(PageRef { off, n: n as u32, first, max_hi, max_bid });
        }
        parts.push((kind, realm, lane, pages));
    }
    if p != payload.len() {
        return Err(corrupt("run trailing bytes"));
    }
    Ok((level, parts))
}

/// RA-01: a block at `off` whose header does not check (`raw`, 32 bytes)
/// but that is recognisably a DIRECTORY block – magic and type byte intact,
/// a directory tag at the start of its payload, and either its payload CRC
/// matching or a valid block header (or `limit`) right behind it. Returns
/// the offset behind it. Directory blocks are redundant: such a block is
/// skipped instead of cutting the chain like a damaged data block. Any
/// other block (also a data block whose type byte flipped to DIRECTORY: its
/// payload carries a data tag) stays a damaged block.
pub(crate) fn damaged_directory_extent(read_at: &dyn Fn(&mut [u8], u64) -> Result<()>, off: u64, raw: &[u8], limit: u64) -> Option<u64> {
    if raw.len() != BLOCK_HEADER_SIZE || &raw[0..4] != BLOCK_MAGIC || raw[5] != BT_DIRECTORY {
        return None;
    }
    let plen = u64::from_le_bytes(raw[16..24].try_into().ok()?);
    let pcrc = u32::from_le_bytes(raw[24..28].try_into().ok()?);
    let pstart = off.checked_add(BLOCK_HEADER_SIZE as u64)?;
    let next = align8(pstart.checked_add(plen)?);
    if next > limit || !(4..=MAX_DIRECTORY_BLOCK).contains(&plen) {
        return None;
    }
    let mut payload = vec![0u8; plen as usize];
    read_at(&mut payload, pstart).ok()?;
    if ![PAGE_TAG, RUN_TAG, ROOT_TAG].iter().any(|t| payload.starts_with(*t)) {
        return None;
    }
    let behind_ok = next == limit || {
        let mut h = [0u8; BLOCK_HEADER_SIZE];
        next.checked_add(BLOCK_HEADER_SIZE as u64).is_some_and(|e| e <= limit) && read_at(&mut h, next).is_ok() && unpack_block_header(&h).is_ok()
    };
    (crc32(&payload) == pcrc || behind_ok).then_some(next)
}

/// Largest directory block considered by [`damaged_directory_extent`].
const MAX_DIRECTORY_BLOCK: u64 = 256 << 20;

/// RD-11: root JSON with absurdly long numbers (parsing decimal BigInts is
/// quadratic) is rejected before it is parsed; real roots hold counts,
/// offsets and u values (at most 101 digits).
fn root_json(payload: &[u8], p: usize) -> Result<Val> {
    let txt = std::str::from_utf8(&payload[p..]).map_err(|_| corrupt("root UTF-8"))?;
    let mut run = 0usize;
    for b in txt.bytes() {
        run = if b.is_ascii_digit() { run + 1 } else { 0 };
        if run > 1000 {
            return Err(corrupt("root number too long"));
        }
    }
    Val::from_json(txt)
}

/// Run offsets listed in a root payload.
pub(crate) fn root_run_offsets(payload: &[u8]) -> Result<Vec<u64>> {
    if !payload.starts_with(ROOT_TAG) {
        return Err(corrupt("wrong root tag"));
    }
    let mut p = 4usize;
    let jl = get_vu_u64(payload, &mut p, "root length")? as usize;
    if p.checked_add(jl) != Some(payload.len()) {
        return Err(corrupt("root length"));
    }
    let obj = root_json(payload, p)?;
    obj.get("runs")
        .and_then(|v| v.as_list())
        .ok_or_else(|| corrupt("root runs"))?
        .iter()
        .map(|r| r.as_i64().filter(|o| *o >= 0).map(|o| o as u64).ok_or_else(|| corrupt("run offset")))
        .collect()
}

/// Page offsets listed in a run payload.
pub(crate) fn run_page_offsets(payload: &[u8], run_off: u64) -> Result<Vec<u64>> {
    Ok(decode_run(payload, run_off)?.1.into_iter().flat_map(|(_, _, _, pages)| pages.into_iter().map(|p| p.off)).collect())
}

impl Db {
    /// Offsets of all blocks of the live directory (root, runs, pages).
    pub(crate) fn live_directory_blocks(&self) -> BTreeSet<u64> {
        let mut out = BTreeSet::new();
        if self.dir.root_off == 0 {
            return out;
        }
        out.insert(self.dir.root_off);
        for r in &self.dir.runs {
            out.insert(r.off);
        }
        let parts = self
            .dense
            .values()
            .flat_map(|x| x.idx.runs.iter())
            .chain(self.sparse.values().flat_map(|x| x.idx.runs.iter()))
            .chain(self.deletes.values().flat_map(|x| x.runs.runs.iter()));
        for part in parts {
            out.extend(part.pages.iter().map(|p| p.off));
        }
        out
    }

    /// Directory configuration: (flush blocks, flush bytes, cache entries).
    fn dir_cfg(&self) -> (u64, u64, usize) {
        (self.config.directory_flush_blocks, self.config.directory_flush_bytes, self.config.directory_cache_entries)
    }

    fn load_page_any<T: DirCodec>(&self, r: &PageRef) -> Result<Arc<Page<T>>> {
        {
            let mut c = self.cache();
            let c = &mut *c;
            c.dir_tick += 1;
            if let Some((p, tick)) = c.dir_pages.get_mut(&r.off) {
                if let Some(p) = T::unwrap(p) {
                    // RD-02: a cached page is served only for the summary
                    // it was checked against (another run part referring to
                    // the same block with a different summary is corrupt)
                    if !page_matches(&p, r) {
                        return Err(corrupt(format!("page at byte {} contradicts its summary", r.off)));
                    }
                    *tick = c.dir_tick;
                    return Ok(p);
                }
            }
        }
        let (info, payload) = self.read_block(r.off).map_err(|e| match e {
            Error::Io(_) => e,
            e => corrupt(format!("page at byte {}: {e}", r.off)),
        })?;
        if info.block_type != BT_DIRECTORY {
            return Err(corrupt(format!("block at byte {} is no directory page", r.off)));
        }
        let page = decode_page::<T>(&payload, r)?;
        // entries refer to complete blocks before the page
        if page.entries.iter().any(|e| e.block_end().is_some_and(|end| end > r.off)) {
            return Err(corrupt(format!("page at byte {} refers to blocks behind it", r.off)));
        }
        let arc = Arc::new(page);
        // RC-04: at least a few full pages fit (a budget below one page
        // would cache nothing)
        let budget = self.dir_cfg().2.max(4 * PAGE_ENTRIES);
        let mut c = self.cache();
        let c = &mut *c;
        let any = T::wrap(arc.clone());
        let n = any.len();
        if n > budget {
            return Ok(arc);
        }
        if c.dir_pages_used + n > budget {
            // LRU in batches: evict the least recently used pages until a
            // quarter of the budget is free
            let limit = budget - budget / 4;
            let mut order: Vec<(u64, u64)> = c.dir_pages.iter().map(|(o, (_, t))| (*t, *o)).collect();
            order.sort_unstable();
            for (_, o) in order {
                if c.dir_pages_used + n <= limit {
                    break;
                }
                if let Some((p, _)) = c.dir_pages.remove(&o) {
                    c.dir_pages_used = c.dir_pages_used.saturating_sub(p.len());
                    c.dir_pages_bytes = c.dir_pages_bytes.saturating_sub(p.cost());
                }
            }
        }
        c.dir_tick += 1;
        let cost = any.cost();
        if let Some((old, _)) = c.dir_pages.insert(r.off, (any, c.dir_tick)) {
            c.dir_pages_used = c.dir_pages_used.saturating_sub(old.len());
            c.dir_pages_bytes = c.dir_pages_bytes.saturating_sub(old.cost());
        }
        c.dir_pages_used += n;
        c.dir_pages_bytes += cost;
        Ok(arc)
    }

    // ------------------------------------------------------------------
    // Flush
    // ------------------------------------------------------------------

    /// Number of `(kind, key)` parts with tail entries.
    fn tail_parts(&self) -> u64 {
        self.dense.values().filter(|x| !x.idx.tail.is_empty()).count() as u64
            + self.sparse.values().filter(|x| !x.idx.tail.is_empty()).count() as u64
            + self.deletes.values().filter(|x| !x.tail.is_empty()).count() as u64
    }

    /// A checkpoint writes the tail as a run when that pays off: enough
    /// entries or bytes, and on average several entries per page part
    /// (many keys with one block each are scanned faster than they are
    /// listed – RB-06).
    pub(crate) fn directory_checkpoint_pending(&self, mode: FlushMode) -> bool {
        let (blocks, bytes, _) = self.dir_cfg();
        let t = &self.dir;
        if mode == FlushMode::Compaction && (t.runs.len() > 1 || (!t.runs.is_empty() && t.tail_entries > 0)) {
            return true;
        }
        t.tail_entries > 0
            && (t.tail_entries >= FORCED_MIN_ENTRIES.min(blocks) || t.tail_bytes >= FORCED_MIN_BYTES.min(bytes))
            && t.tail_entries >= FORCED_MIN_PER_PART * self.tail_parts()
    }

    /// Writes a directory run when the blocks or bytes registered since the
    /// last flush exceed the configured thresholds (after write calls and in
    /// `commit()`, so that a long transaction does not keep its whole tail
    /// in memory: a run written before the commit only becomes the
    /// directory of the file when a superblock points to its root), or at
    /// a checkpoint when it pays off. A failed write is rolled back (the
    /// directory is optional; the next open scans the tail instead) and not
    /// retried before the tail has grown by another threshold.
    pub(crate) fn maybe_flush_directory(&mut self, mode: FlushMode) -> Result<()> {
        let (blocks, bytes, _) = self.dir_cfg();
        let t = &self.dir;
        let due = t.tail_entries > 0
            && (t.tail_entries >= blocks.max(t.retry_entries) || t.tail_bytes >= bytes.max(t.retry_bytes));
        let wanted = due
            || (mode != FlushMode::Commit && mode != FlushMode::AfterWrite && self.directory_checkpoint_pending(mode))
            || (mode != FlushMode::AfterWrite && t.unlisted_blocks >= UNLISTED_MIN);
        if !wanted {
            return Ok(());
        }
        let start = self.store.len();
        let (next_id, last_end) = (self.next_block_id, self.last_valid_end);
        match self.write_directory(mode == FlushMode::Compaction) {
            Ok(()) => {
                self.dir.retry_entries = 0;
                self.dir.retry_bytes = 0;
                self.dir.unlisted_blocks = 0;
                Ok(())
            }
            Err(e) => {
                // drop the partial directory (also after I/O errors: the
                // append buffer keeps the blocks before `start`)
                let _ = self.store.rollback_to(start);
                self.invalidate_blocks_from(start);
                self.next_block_id = next_id;
                self.last_valid_end = last_end;
                self.dir.stats.failed += 1;
                self.dir.retry_entries = self.dir.tail_entries.saturating_add(blocks);
                self.dir.retry_bytes = self.dir.tail_bytes.saturating_add(bytes);
                if let Error::Corruption(m) = &e {
                    self.dir.flush_error = Some(format!(
                        "block directory flush aborted, damaged block in the tail: {m}; the directory was not written (run verify())"
                    ));
                }
                match e {
                    Error::Io(_) | Error::IoPath(..) if mode == FlushMode::Commit => Err(e),
                    _ => Ok(()),
                }
            }
        }
    }

    /// Writes the tail as a level-0 run, merges runs by levels and writes a
    /// new root. The in-memory indexes are changed only after every block
    /// was appended. A merge that fails on a damaged old page is rolled
    /// back to the level-0 run; the directory is then marked degraded and
    /// not merged any more (RA-04).
    fn write_directory(&mut self, merge_all: bool) -> Result<()> {
        // 1) level-0 run of the tails
        let mut new_parts: Vec<PartOut> = Vec::new();
        let mut dkeys: Vec<RL> = self.dense.keys().cloned().collect();
        dkeys.sort();
        for key in &dkeys {
            let mut tail = self.dense[key].idx.tail.clone();
            if tail.is_empty() {
                continue;
            }
            tail.sort_by(|a, b| (a.lo(), a.bid()).cmp(&(b.lo(), b.bid())));
            let mut w = PageWriter::<DenseSegment>::new(key.0.clone(), key.1);
            for e in tail {
                w.push(self, e)?;
            }
            new_parts.push(w.finish(self)?);
        }
        let mut skeys: Vec<RL> = self.sparse.keys().cloned().collect();
        skeys.sort();
        for key in &skeys {
            let mut tail = self.sparse[key].idx.tail.clone();
            if tail.is_empty() {
                continue;
            }
            tail.sort_by(|a, b| (a.lo(), a.bid()).cmp(&(b.lo(), b.bid())));
            let mut w = PageWriter::<SparseSegment>::new(key.0.clone(), key.1);
            for e in tail {
                w.push(self, e)?;
            }
            new_parts.push(w.finish(self)?);
        }
        let mut xkeys: Vec<RL> = self.deletes.keys().cloned().collect();
        xkeys.sort();
        for key in &xkeys {
            let mut tail: Vec<DelEntry> = self.deletes[key].tail.iter().map(|(u, b)| DelEntry { u: u.clone(), block_id: *b }).collect();
            if tail.is_empty() {
                continue;
            }
            tail.sort_by(|a, b| (a.lo(), a.bid()).cmp(&(b.lo(), b.bid())));
            let mut w = PageWriter::<DelEntry>::new(key.0.clone(), key.1);
            for e in tail {
                w.push(self, e)?;
            }
            new_parts.push(w.finish(self)?);
        }
        // planned state: per key the run parts (run offset, part)
        type Parts = BTreeMap<(u8, RL), Vec<(u64, Arc<RunPart>)>>;
        let mut parts: Parts = BTreeMap::new();
        for (key, ix) in &self.dense {
            parts.entry((KIND_DENSE, key.clone())).or_default().extend(ix.idx.runs.iter().map(|r| (r.run, r.clone())));
        }
        for (key, ix) in &self.sparse {
            parts.entry((KIND_SPARSE, key.clone())).or_default().extend(ix.idx.runs.iter().map(|r| (r.run, r.clone())));
        }
        for (key, ix) in &self.deletes {
            parts.entry((KIND_DELETE, key.clone())).or_default().extend(ix.runs.runs.iter().map(|r| (r.run, r.clone())));
        }
        let mut runs = self.dir.runs.clone();
        if !new_parts.is_empty() {
            let off = self.append_run(0, &new_parts)?;
            for part in new_parts {
                let rp = Arc::new(RunPart::new(off, part.pages));
                parts.entry((part.kind, (part.realm, part.lane))).or_default().push((off, rp));
            }
            runs.push(RunMeta { off, level: 0 });
        }
        // 2) merges: the youngest MERGE_FANOUT runs of one level
        if self.dir.degraded.is_none() {
            let (at, next_id, last_end) = (self.store.len(), self.next_block_id, self.last_valid_end);
            let (parts0, runs0) = (parts.clone(), runs.clone());
            if let Err(e) = self.merge_levels(&mut parts, &mut runs, merge_all) {
                if matches!(e, Error::Io(_) | Error::IoPath(..)) {
                    return Err(e);
                }
                self.store.rollback_to(at)?;
                self.invalidate_blocks_from(at);
                self.next_block_id = next_id;
                self.last_valid_end = last_end;
                parts = parts0;
                runs = runs0;
                self.dir.degraded = Some(e.to_string());
            }
        }
        // 3) root
        let root_payload = self.encode_root(&runs)?;
        let (root_off, _) = self.append_block(BT_DIRECTORY, &root_payload, 0)?;
        self.dir.stats.bytes_written += (BLOCK_HEADER_SIZE + root_payload.len()) as u64;
        // 4) apply
        for ((kind, key), list) in parts {
            let rps: Vec<Arc<RunPart>> = list.into_iter().map(|(_, p)| p).collect();
            match kind {
                KIND_DENSE => {
                    if let Some(ix) = self.dense.get_mut(&key) {
                        ix.idx.set_runs(rps);
                    }
                }
                KIND_SPARSE => {
                    if let Some(ix) = self.sparse.get_mut(&key) {
                        ix.idx.set_runs(rps);
                    }
                }
                _ => {
                    if let Some(ix) = self.deletes.get_mut(&key) {
                        ix.runs.set_runs(rps);
                    }
                }
            }
        }
        // every tail was written into the level-0 run
        for ix in self.dense.values_mut() {
            ix.idx.clear_tail();
        }
        for ix in self.sparse.values_mut() {
            ix.idx.clear_tail();
        }
        for ix in self.deletes.values_mut() {
            ix.tail.clear();
        }
        self.dir.runs = runs;
        self.dir.root_off = root_off;
        self.dir.tail_entries = 0;
        self.dir.tail_bytes = 0;
        self.dir.stats.flushes += 1;
        Ok(())
    }

    /// Merges the youngest [`MERGE_FANOUT`] runs of one level while there
    /// are that many (amortised O(log n) rewrites per entry).
    #[allow(clippy::type_complexity)]
    fn merge_levels(&mut self, parts: &mut BTreeMap<(u8, RL), Vec<(u64, Arc<RunPart>)>>, runs: &mut Vec<RunMeta>, merge_all: bool) -> Result<()> {
        loop {
            let n = runs.len();
            let group: Vec<RunMeta> = if merge_all {
                if n <= 1 {
                    return Ok(());
                }
                runs.clone()
            } else {
                if n < MERGE_FANOUT {
                    return Ok(());
                }
                let lvl = runs[n - 1].level;
                if !runs[n - MERGE_FANOUT..].iter().all(|r| r.level == lvl) {
                    return Ok(());
                }
                runs[n - MERGE_FANOUT..].to_vec()
            };
            let level = group.iter().map(|r| r.level).max().unwrap_or(0) + 1;
            let offs: BTreeSet<u64> = group.iter().map(|r| r.off).collect();
            let mut merged: Vec<PartOut> = Vec::new();
            for ((kind, key), list) in parts.iter() {
                let inputs: Vec<Arc<RunPart>> = list.iter().filter(|(o, _)| offs.contains(o)).map(|(_, p)| p.clone()).collect();
                if inputs.is_empty() {
                    continue;
                }
                merged.push(match *kind {
                    KIND_DENSE => self.merge_parts::<DenseSegment>(key, inputs)?,
                    KIND_SPARSE => self.merge_parts::<SparseSegment>(key, inputs)?,
                    _ => self.merge_parts::<DelEntry>(key, inputs)?,
                });
            }
            let off = self.append_run(level, &merged)?;
            for list in parts.values_mut() {
                list.retain(|(o, _)| !offs.contains(o));
            }
            for part in merged {
                let rp = Arc::new(RunPart::new(off, part.pages));
                parts.entry((part.kind, (part.realm, part.lane))).or_default().push((off, rp));
            }
            runs.retain(|r| !offs.contains(&r.off));
            runs.push(RunMeta { off, level });
            self.dir.stats.merges += 1;
        }
    }

    fn append_run(&mut self, level: u32, parts: &[PartOut]) -> Result<u64> {
        let payload = encode_run(level, parts);
        let (off, _) = self.append_block(BT_DIRECTORY, &payload, 0)?;
        self.dir.stats.bytes_written += (BLOCK_HEADER_SIZE + payload.len()) as u64;
        Ok(off)
    }

    /// k-way merge of run parts of one key into new pages.
    fn merge_parts<T: DirCodec>(&mut self, key: &RL, inputs: Vec<Arc<RunPart>>) -> Result<PartOut>
    where
        Db: PageSource<T>,
    {
        let mut tmp: SegIndex<T> = SegIndex::default();
        tmp.set_runs(inputs);
        let mut it = tmp.iter_by_start();
        let mut w = PageWriter::<T>::new(key.0.clone(), key.1);
        loop {
            let next = it.next(&*self)?;
            let Some(e) = next else { break };
            w.push(self, e)?;
        }
        w.finish(self)
    }

    /// Root payload: tag + JSON state.
    fn encode_root(&self, runs: &[RunMeta]) -> Result<Vec<u8>> {
        let mut keys: Vec<RL> = self.dense.keys().chain(self.sparse.keys()).chain(self.deletes.keys()).cloned().collect();
        keys.extend(self.data_max_block_id_by_lane.keys().cloned());
        keys.extend(self.max_u.keys().cloned());
        keys.sort();
        keys.dedup();
        let mut kv = Vec::new();
        for key in &keys {
            let d = self.dense.get(key);
            let s = self.sparse.get(key);
            kv.push(vmap! {
                "realm" => Val::nat(&key.0), "lane" => key.1 as i64,
                "dense_records" => d.map(|x| x.records).unwrap_or(0) as i64,
                "shapes" => Val::List(d.map(|x| x.shapes.iter().map(|s| Val::Int(*s as i64)).collect()).unwrap_or_default()),
                "sparse_groups" => s.map(|x| x.groups).unwrap_or(0) as i64,
                "sparse_cells" => s.map(|x| x.cells).unwrap_or(0) as i64,
                "max_u" => self.max_u.get(key).map(Val::nat).unwrap_or(Val::None),
                "data_max_bid" => self.data_max_block_id_by_lane.get(key).copied().unwrap_or(0) as i64,
                "overlay" => self.overlay_keys.contains(key),
                "dense_overlap" => self.dense_overlap_keys.contains(key),
            });
        }
        let mut secondary: Vec<Val> = self
            .secondary_blocks
            .iter()
            .map(|((iid, lane), m)| Val::List(vec![Val::Int(*iid as i64), Val::Int(*lane as i64), Val::Int(m.block_offset as i64)]))
            .collect();
        secondary.sort_by_key(|v| v.py_repr());
        let obj = vmap! {
            "v" => 1i64,
            "runs" => Val::List(runs.iter().map(|r| Val::Int(r.off as i64)).collect()),
            "system_off" => self.dir.last_system_off as i64,
            "config_off" => self.dir.last_config_off as i64,
            "checkpoint_off" => self.dir.last_checkpoint_off as i64,
            "math_constants_stored" => self.sys.math_constants_stored,
            "secondary" => Val::List(secondary),
            "keys" => Val::List(kv),
            "data_max_bid" => self.data_max_block_id as i64,
            "has_overlays" => self.has_overlays,
        };
        let json = obj.to_json()?;
        let mut out = Vec::with_capacity(json.len() + 8);
        out.extend_from_slice(ROOT_TAG);
        put_vu64(&mut out, json.len() as u64);
        out.extend_from_slice(json.as_bytes());
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Load (open)
    // ------------------------------------------------------------------

    /// Loads the directory with root `root_off` (inside the committed range
    /// `end`) into the empty in-memory state. Returns the newest R0 record
    /// and the offset behind the root, where the open scan continues.
    pub(crate) fn load_directory(&mut self, root_off: u64, end: u64) -> Result<(Val, u64)> {
        let (info, payload) = self.read_block(root_off)?;
        if info.block_type != BT_DIRECTORY || !payload.starts_with(ROOT_TAG) {
            return Err(corrupt(format!("block at byte {root_off} is no directory root")));
        }
        if info.next_offset > end {
            return Err(corrupt("root behind the commit boundary"));
        }
        let mut p = 4usize;
        let jl = get_vu_u64(&payload, &mut p, "root length")? as usize;
        if p.checked_add(jl) != Some(payload.len()) {
            return Err(corrupt("root length"));
        }
        let obj = root_json(&payload, p)?;
        if obj.get("v").and_then(|v| v.as_i64()) != Some(1) {
            return Err(corrupt("unknown root version"));
        }
        let geti = |k: &str| -> Result<u64> {
            obj.get(k).and_then(|v| v.as_i64()).filter(|v| *v >= 0).map(|v| v as u64).ok_or_else(|| corrupt(format!("root field {k}")))
        };
        // runs
        let mut runs: Vec<RunMeta> = Vec::new();
        let run_list = obj.get("runs").and_then(|v| v.as_list()).ok_or_else(|| corrupt("root runs"))?;
        // RD-04: every run and every page is listed once (a root repeating
        // a run would multiply the in-memory state and the aggregates)
        let mut seen_pages: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for r in run_list {
            let off = r.as_i64().filter(|o| *o as u64 >= DATA_START && (*o as u64) < root_off).ok_or_else(|| corrupt("run offset"))? as u64;
            if runs.last().is_some_and(|l| l.off >= off) {
                return Err(corrupt("root runs not strictly ascending"));
            }
            let (rinfo, rpayload) = self.read_block(off)?;
            if rinfo.block_type != BT_DIRECTORY {
                return Err(corrupt(format!("block at byte {off} is no directory run")));
            }
            let (level, parts) = decode_run(&rpayload, off)?;
            for (_, _, _, pages) in &parts {
                for pg in pages {
                    if !seen_pages.insert(pg.off) {
                        return Err(corrupt(format!("page at byte {} listed twice", pg.off)));
                    }
                }
            }
            for (kind, realm, lane, pages) in parts {
                let rp = Arc::new(RunPart::new(off, pages));
                let key = (realm, lane);
                match kind {
                    KIND_DENSE => self.dense.entry(key).or_default().idx.add_run(rp),
                    KIND_SPARSE => self.sparse.entry(key).or_default().idx.add_run(rp),
                    _ => self.deletes.entry(key).or_default().runs.add_run(rp),
                }
            }
            runs.push(RunMeta { off, level });
        }
        // per-key aggregates
        for k in obj.get("keys").and_then(|v| v.as_list()).ok_or_else(|| corrupt("root keys"))? {
            let realm = k.get("realm").and_then(|v| v.as_nat()).ok_or_else(|| corrupt("key realm"))?;
            let lane = k.get("lane").and_then(|v| v.as_i64()).filter(|l| *l == 0 || *l == 1).ok_or_else(|| corrupt("key lane"))? as Lane;
            let key = (realm, lane);
            let gi = |n: &str| k.get(n).and_then(|v| v.as_i64()).filter(|v| *v >= 0).map(|v| v as u64).ok_or_else(|| corrupt(format!("key field {n}")));
            let (records, groups, cells, dmax) = (gi("dense_records")?, gi("sparse_groups")?, gi("sparse_cells")?, gi("data_max_bid")?);
            let shapes: BTreeSet<u64> = k
                .get("shapes")
                .and_then(|v| v.as_list())
                .ok_or_else(|| corrupt("key shapes"))?
                .iter()
                .map(|s| s.as_i64().filter(|x| *x >= 0).map(|x| x as u64).ok_or_else(|| corrupt("shape id")))
                .collect::<Result<_>>()?;
            if let Some(ix) = self.dense.get_mut(&key) {
                ix.records = records;
                ix.shapes = shapes;
            } else if records > 0 || !shapes.is_empty() {
                return Err(corrupt("dense aggregate without dense runs"));
            }
            if let Some(ix) = self.sparse.get_mut(&key) {
                ix.groups = groups;
                ix.cells = cells;
            }
            if let Some(m) = k.get("max_u").and_then(|v| v.as_nat()) {
                self.max_u.insert(key.clone(), m);
            }
            if dmax > 0 {
                self.data_max_block_id_by_lane.insert(key.clone(), dmax);
            }
            if k.get("overlay").map(|v| v.is_truthy()).unwrap_or(false) {
                self.overlay_keys.insert(key.clone());
            }
            if k.get("dense_overlap").map(|v| v.is_truthy()).unwrap_or(false) {
                self.dense_overlap_keys.insert(key);
            }
        }
        self.data_max_block_id = geti("data_max_bid")?;
        self.has_overlays = obj.get("has_overlays").map(|v| v.is_truthy()).unwrap_or(false);
        // SYSTEM: newest record (CRC-checked) bound to the first one
        let sys_off = geti("system_off")?;
        if sys_off < DATA_START || sys_off >= root_off {
            return Err(corrupt("root system offset"));
        }
        let read_sys = |db: &Db, off: u64| -> Result<Val> {
            let (i, pl) = db.read_block(off)?;
            if i.block_type != BT_SYSTEM {
                return Err(corrupt(format!("block at byte {off} is no SYSTEM block")));
            }
            Self::json_payload(&pl)
        };
        let record = read_sys(self, sys_off)?;
        let first = read_sys(self, DATA_START)?;
        system::check_record(&record, None)?;
        system::check_record_bound(&record, &first)?;
        self.sys.math_constants_stored = obj.get("math_constants_stored").map(|v| v.is_truthy()).unwrap_or(false)
            || record.get("cells").and_then(|c| c.get(system::MATH_CONSTANTS_CELL)).is_some();
        // DB_CONFIG / CHECKPOINT: re-read their newest block
        for (field, bt) in [("config_off", BT_DB_CONFIG), ("checkpoint_off", BT_CHECKPOINT)] {
            let off = geti(field)?;
            if off == 0 {
                continue;
            }
            if off < DATA_START || off >= root_off {
                return Err(corrupt(format!("root field {field}")));
            }
            let (i, pl) = self.read_block(off)?;
            if i.block_type != bt {
                return Err(corrupt(format!("root field {field}: wrong block type")));
            }
            self.apply_meta_block(&i, &pl)?;
        }
        // SECONDARY: newest block per index and lane
        for s in obj.get("secondary").and_then(|v| v.as_list()).ok_or_else(|| corrupt("root secondary"))? {
            let off = s.as_list().and_then(|l| l.get(2)).and_then(|v| v.as_i64()).filter(|o| *o as u64 >= DATA_START && (*o as u64) < root_off);
            let off = off.ok_or_else(|| corrupt("secondary offset"))? as u64;
            let i = self.read_header_at(off)?;
            if i.block_type != BT_SECONDARY {
                return Err(corrupt(format!("block at byte {off} is no SECONDARY block")));
            }
            self.parse_secondary_directory(&i)?;
        }
        self.dir.runs = runs;
        self.dir.root_off = root_off;
        self.dir.scan_last_id = info.block_id;
        self.dir.last_system_off = sys_off;
        self.dir.last_config_off = geti("config_off")?;
        self.dir.last_checkpoint_off = geti("checkpoint_off")?;
        self.dir.tail_entries = 0;
        self.dir.tail_bytes = 0;
        self.dir.stats.loaded = true;
        Ok((record, info.next_offset))
    }

    /// Builds the in-memory state for the committed range `[DATA_START,
    /// end)`: from the directory of the superblock plus a scan of the blocks
    /// behind it, or – without a usable directory – by the full scan.
    pub(crate) fn load_storage_state(&mut self, end: u64) -> Result<Val> {
        let root = if self.ignore_directory { 0 } else { self.sup.dir_root };
        if root != 0 && !(root >= DATA_START && root < end) {
            self.open_warnings.push(format!("block directory root at byte {root} outside the committed range; full scan"));
            self.dir.stats.fallback = true;
        }
        if root >= DATA_START && root < end {
            match self.load_directory(root, end) {
                Ok((record, resume)) => return self.open_scan_range(resume, end, Some(record)),
                Err(e) => {
                    if matches!(e, Error::Io(_)) {
                        return Err(e);
                    }
                    self.open_warnings.push(format!("block directory at byte {root} not usable ({e}); full scan"));
                    self.reset_in_memory_state();
                    self.dir = DirState::default();
                    self.dir.stats.fallback = true;
                }
            }
        }
        self.open_scan(end)
    }

    /// `stats()["directory"]`.
    pub(crate) fn directory_stats(&self) -> Val {
        let tail: u64 = self.dense.values().map(|x| x.idx.tail.len() as u64).sum::<u64>()
            + self.sparse.values().map(|x| x.idx.tail.len() as u64).sum::<u64>()
            + self.deletes.values().map(|x| x.tail.len() as u64).sum::<u64>();
        let persisted: u64 = self.dense.values().map(|x| x.idx.flushed).sum::<u64>()
            + self.sparse.values().map(|x| x.idx.flushed).sum::<u64>()
            + self.deletes.values().map(|x| x.runs.flushed).sum::<u64>();
        let pages: u64 = self.dense.values().flat_map(|x| x.idx.runs.iter()).map(|r| r.pages.len() as u64).sum::<u64>()
            + self.sparse.values().flat_map(|x| x.idx.runs.iter()).map(|r| r.pages.len() as u64).sum::<u64>()
            + self.deletes.values().flat_map(|x| x.runs.runs.iter()).map(|r| r.pages.len() as u64).sum::<u64>();
        let c = self.cache();
        vmap! {
            "root_offset" => self.dir.root_off as i64, "runs" => self.dir.runs.len() as i64,
            "levels" => Val::List(self.dir.runs.iter().map(|r| Val::Int(r.level as i64)).collect()),
            "persisted_entries" => persisted as i64, "live_pages" => pages as i64,
            "tail_entries" => tail as i64, "tail_bytes" => self.dir.tail_bytes as i64,
            "flushes" => self.dir.stats.flushes as i64, "merges" => self.dir.stats.merges as i64,
            "pages_written" => self.dir.stats.pages_written as i64, "bytes_written" => self.dir.stats.bytes_written as i64,
            "loaded" => self.dir.stats.loaded, "fallback_full_scan" => self.dir.stats.fallback,
            "failed_flushes" => self.dir.stats.failed as i64, "damaged_blocks_skipped" => self.dir.stats.damaged as i64,
            "degraded" => self.dir.degraded.clone().map(Val::Str).unwrap_or(Val::None),
            "last_flush_error" => self.dir.flush_error.clone().map(Val::Str).unwrap_or(Val::None),
            "cached_pages" => c.dir_pages.len() as i64, "cached_entries" => c.dir_pages_used as i64,
        }
    }
}

impl PageSource<DenseSegment> for Db {
    fn load_page(&self, p: &PageRef) -> Result<Arc<Page<DenseSegment>>> {
        self.load_page_any::<DenseSegment>(p)
    }
}

impl PageSource<SparseSegment> for Db {
    fn load_page(&self, p: &PageRef) -> Result<Arc<Page<SparseSegment>>> {
        self.load_page_any::<SparseSegment>(p)
    }
}

impl PageSource<DelEntry> for Db {
    fn load_page(&self, p: &PageRef) -> Result<Arc<Page<DelEntry>>> {
        self.load_page_any::<DelEntry>(p)
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "directory_inline_tests.rs"]
mod tests;
