//! Memory-bounded merge of the sparse cells of one `(realm, lane)`.
//!
//! [`SparseMerge`] yields the newest state of every sparse cell (values and
//! tombstones) in ascending `(u, v)` order without collecting the cells of
//! the realm, the u group or even a single record in memory:
//!
//! * each SPARSE block is read as a stream ([`SegStream`]): sequential
//!   reads through a bounded window, u groups and cells decoded one after
//!   the other; a single value is the largest unit held at once;
//! * the streams are merged by `(u, v, block_id)` – for equal cells the
//!   block written last wins, exactly as in the read path;
//! * segments are opened lazily in `u_start` order (directory iterator):
//!   only blocks whose u range reaches the current position are open, and
//!   the read window per open stream shrinks with their number;
//! * if more than [`MAX_OPEN`] blocks overlap at some u (many versions of
//!   one u group, scattered batches with wide u ranges), the blocks are
//!   taken in groups of `MAX_OPEN` (directory order) that are merged one
//!   after the other into temporary sorted runs; every cell of a run keeps
//!   the id of its block, and the runs are merged by `(u, v, block id)`
//!   (in levels of `MAX_OPEN` runs if needed). Memory stays bounded by
//!   `MAX_OPEN` streams for every distribution, at the price of one extra
//!   write and read of the data for such keys.
//!
//! Integrity: every stream checks the block header, the routing prefix
//! against the directory entry and the complete group/cell structure; the
//! payload CRC is either checked before the first cell is returned
//! (`Verify::Upfront`, streaming check without loading the block) or
//! incrementally while streaming and compared at the end of the block
//! (`Verify::Incremental`, for consumers that discard their output on error,
//! e.g. compaction into a staging file).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::address::Lane;
use crate::codec::TypeCode;
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu, get_vu_u64, put_vu64, Nat};
use crate::platform;

use super::segindex::ByStart;
use super::vindex::{read_head, CellHead, LazyResolver};
use super::{Db, SparseCellMeta, SparseSegment, RL};

/// Read window budget of all open streams of one merge.
pub(crate) const MERGE_BUFFER_BUDGET: usize = 32 << 20;
const MIN_WINDOW: usize = 4 << 10;
const MAX_WINDOW: usize = 1 << 20;
/// Most cell heads decoded ahead per stream (fewer when many are open).
const QUEUE_CELLS: usize = 64;
/// Values up to this size are decoded with their head; larger ones only
/// when their cell wins (a superseded large value is never read – RB-01).
const EAGER_VALUE: usize = 256;
/// Most blocks open at once; above that the merge goes external.
pub(crate) const MAX_OPEN: usize = 4096;
/// Chunk for hashing or copying bytes that are not in a window.
const CHUNK: usize = 1 << 20;

/// How the payload CRC of the streamed blocks is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verify {
    Upfront,
    Incremental,
}

/// Encoded form of a cell for temporary runs: flags (1 tombstone, 2
/// self-typed, 4 null), explicit type, encoded value.
#[derive(Clone, Debug)]
pub(crate) struct RawCell {
    pub flags: u8,
    pub typ: Option<TypeCode>,
    pub value: Vec<u8>,
}

/// Newest state of one cell.
#[derive(Clone, Debug)]
pub(crate) struct MergedCell {
    pub u: Nat,
    pub v: Nat,
    pub meta: SparseCellMeta,
    /// encoded size of the cell (key, type, value) in its block
    pub bytes: usize,
    /// encoded form (only when requested, for temporary runs)
    pub raw: Option<RawCell>,
    /// id of the block the state comes from
    pub bid: u64,
}

/// A taken cell of a source.
struct QCell {
    v: Nat,
    meta: SparseCellMeta,
    bytes: usize,
    raw: Option<RawCell>,
    bid: u64,
}

/// Head of a cell in a block: the value is located, decoded only if small.
struct Head {
    h: CellHead,
    cell_start: u64,
    val_start: u64,
    val_end: u64,
    small: Option<(SparseCellMeta, Option<RawCell>)>,
}

fn corrupt(seg: &SparseSegment, msg: &str) -> Error {
    Error::Corruption(format!("SPARSE block {} (byte {}): {msg}", seg.block_id, seg.block_offset))
}

fn raw_flags(h: &CellHead) -> u8 {
    (h.tomb as u8) | ((h.selftyped as u8) << 1) | ((h.null as u8) << 2)
}

/// One SPARSE block as a cell stream. Holds one read window (sized by the
/// number of open streams) and a few cell heads – never a whole record and
/// never a large value it does not return.
struct SegStream {
    seg: SparseSegment,
    payload_crc: u32,
    verify: Verify,
    crc: crc32fast::Hasher,
    /// absolute offset up to which payload bytes were hashed (= `pos`)
    hashed: u64,
    buf: Vec<u8>,
    buf_off: u64,
    pos: u64,
    end: u64,
    groups_left: u64,
    first_group: bool,
    cur_u: Nat,
    rec_end: u64,
    cells_left: u64,
    in_group: bool,
    prev_v: Nat,
    queue: VecDeque<Head>,
    finished: bool,
    keep_raw: bool,
}

impl SegStream {
    fn open(db: &Db, seg: SparseSegment, verify: Verify, window: usize, keep_raw: bool) -> Result<SegStream> {
        let info = db.read_header_at(seg.block_offset)?;
        if info.block_type != BT_SPARSE_CELLS
            || info.block_id != seg.block_id
            || info.payload_len != seg.payload_len
            || crate::address::lane_from_flags(info.flags) != seg.lane
        {
            return Err(corrupt(&seg, "header contradicts its directory entry"));
        }
        let verify = if verify == Verify::Upfront {
            db.verify_block_streaming(seg.block_offset)?;
            Verify::Upfront
        } else {
            Verify::Incremental
        };
        let end = seg.payload_offset + seg.payload_len;
        let mut st = SegStream {
            payload_crc: info.payload_crc,
            verify,
            crc: crc32fast::Hasher::new(),
            hashed: seg.payload_offset,
            buf: Vec::new(),
            buf_off: seg.payload_offset,
            pos: seg.payload_offset,
            end,
            groups_left: seg.group_count,
            first_group: true,
            cur_u: seg.u_start.clone(),
            rec_end: 0,
            cells_left: 0,
            in_group: false,
            prev_v: Nat::ZERO,
            queue: VecDeque::new(),
            finished: false,
            keep_raw,
            seg,
        };
        // a small block verified up front is in the block cache: stream it
        // from there instead of reading it a second time
        if verify == Verify::Upfront && st.seg.payload_len <= window as u64 {
            let payload = db.block_payload(st.seg.block_offset)?;
            if payload.len() as u64 == st.seg.payload_len {
                st.buf = payload.to_vec();
            }
        }
        // prefix: tag, realm, u range, counts – must match the entry
        let plen = (st.seg.entries_offset - st.seg.payload_offset) as usize;
        st.fill(db, plen, window)?;
        let raw = st.avail(st.seg.entries_offset);
        let mut p = 0usize;
        let ok = raw.starts_with(SPARSE_TAG) && {
            p = 4;
            let r = get_vu(raw, &mut p);
            let a = get_vu(raw, &mut p);
            let b = get_vu(raw, &mut p);
            let g = get_vu_u64(raw, &mut p, "SPARSE groups");
            let c = get_vu_u64(raw, &mut p, "SPARSE cells");
            matches!((r, a, b, g, c), (Ok(r), Ok(a), Ok(b), Ok(g), Ok(c))
                if r == st.seg.realm_id && a == st.seg.u_start && b == st.seg.u_end && g == st.seg.group_count && c == st.seg.cell_count)
                && check_prefix_crc(raw, &mut p).is_ok()
        };
        if !ok || p != plen {
            return Err(corrupt(&st.seg, "prefix contradicts its directory entry"));
        }
        if st.seg.group_count == 0 {
            return Err(corrupt(&st.seg, "no u groups"));
        }
        st.consume(db, st.seg.entries_offset)?;
        Ok(st)
    }

    /// Bytes available in the buffer from `pos` up to `limit`.
    fn avail(&self, limit: u64) -> &[u8] {
        if self.pos < self.buf_off {
            return &[];
        }
        let s = (self.pos - self.buf_off) as usize;
        let e = ((limit.min(self.buf_off + self.buf.len() as u64)).saturating_sub(self.buf_off)) as usize;
        if s >= e {
            return &[];
        }
        &self.buf[s..e]
    }

    /// Makes at least `need` bytes from `pos` available (bounded by the
    /// payload end), reading a window of `window` bytes.
    fn fill(&mut self, db: &Db, need: usize, window: usize) -> Result<()> {
        let have_end = self.buf_off + self.buf.len() as u64;
        let want_end = (self.pos + need as u64).min(self.end);
        if self.pos >= self.buf_off && want_end <= have_end {
            return Ok(());
        }
        let len = (need.max(window) as u64).min(self.end - self.pos) as usize;
        let mut nb = vec![0u8; len];
        db.store.read_exact_at(self.pos, &mut nb)?;
        self.buf = nb;
        self.buf_off = self.pos;
        Ok(())
    }

    /// Keeps at most `window` buffered bytes from `pos` on (more streams
    /// were opened since the buffer was read).
    fn trim(&mut self, window: usize) {
        if self.buf.len() <= window.saturating_mul(2) || self.pos < self.buf_off {
            return;
        }
        let s = ((self.pos - self.buf_off) as usize).min(self.buf.len());
        let e = (s + window).min(self.buf.len());
        self.buf = self.buf[s..e].to_vec();
        self.buf_off = self.pos;
    }

    /// Copies payload bytes `[from, to)` (from the window or the file).
    fn bytes(&self, db: &Db, from: u64, to: u64) -> Result<Vec<u8>> {
        let buf_end = self.buf_off + self.buf.len() as u64;
        if from >= self.buf_off && to <= buf_end {
            return Ok(self.buf[(from - self.buf_off) as usize..(to - self.buf_off) as usize].to_vec());
        }
        let mut v = vec![0u8; (to - from) as usize];
        db.store.read_exact_at(from, &mut v)?;
        Ok(v)
    }

    /// Advances `pos` to `to`, hashing the passed bytes (incremental CRC:
    /// bytes outside the window are read in chunks, never kept).
    fn consume(&mut self, db: &Db, to: u64) -> Result<()> {
        if self.verify == Verify::Incremental && to > self.hashed {
            let buf_end = self.buf_off + self.buf.len() as u64;
            if self.hashed >= self.buf_off && self.hashed < buf_end {
                let e = to.min(buf_end);
                self.crc.update(&self.buf[(self.hashed - self.buf_off) as usize..(e - self.buf_off) as usize]);
                self.hashed = e;
            }
            if self.hashed < to {
                let mut scratch = vec![0u8; ((to - self.hashed) as usize).min(CHUNK)];
                while self.hashed < to {
                    let n = ((to - self.hashed) as usize).min(scratch.len());
                    db.store.read_exact_at(self.hashed, &mut scratch[..n])?;
                    self.crc.update(&scratch[..n]);
                    self.hashed += n as u64;
                }
            }
        }
        self.hashed = self.hashed.max(to);
        self.pos = to;
        Ok(())
    }

    /// Ends the stream: structure and (incremental) CRC complete.
    fn finish(&mut self) -> Result<()> {
        if self.groups_left != 0 || self.pos != self.end {
            return Err(corrupt(&self.seg, "trailing bytes after the last u group"));
        }
        if self.cur_u != self.seg.u_end {
            return Err(corrupt(&self.seg, "u range contradicts stream"));
        }
        if self.verify == Verify::Incremental {
            let crc = std::mem::replace(&mut self.crc, crc32fast::Hasher::new()).finalize();
            if self.hashed != self.end || crc != self.payload_crc {
                return Err(corrupt(&self.seg, "payload CRC mismatch"));
            }
        }
        self.finished = true;
        Ok(())
    }

    /// Starts the next u group (header and cell count).
    fn begin_group(&mut self, db: &Db, window: usize) -> Result<()> {
        let mut need = 32usize;
        loop {
            self.fill(db, need, window)?;
            let raw = self.avail(self.end);
            let mut p = 0usize;
            let parsed = (|| -> Result<(Nat, u64)> {
                let d = get_vu(raw, &mut p)?;
                let l = get_vu_u64(raw, &mut p, "SPARSE rec_len")?;
                Ok((d, l))
            })();
            match parsed {
                Ok((delta, rec_len)) => {
                    if self.first_group != delta.is_zero() {
                        return Err(corrupt(&self.seg, "u delta invalid"));
                    }
                    let u = if self.first_group { self.seg.u_start.clone() } else { self.cur_u.add(&delta) };
                    if u > self.seg.u_end {
                        return Err(corrupt(&self.seg, "u group outside the block range"));
                    }
                    let start = self.pos + p as u64;
                    let rec_end = start.checked_add(rec_len).filter(|e| *e <= self.end && rec_len > 0 && rec_len <= u32::MAX as u64);
                    let Some(rec_end) = rec_end else { return Err(corrupt(&self.seg, "record length invalid")) };
                    self.consume(db, start)?;
                    self.first_group = false;
                    self.groups_left -= 1;
                    self.cur_u = u;
                    self.rec_end = rec_end;
                    // cell count
                    let mut need2 = 16usize;
                    loop {
                        self.fill(db, need2, window)?;
                        let raw = self.avail(self.rec_end);
                        let mut q = 0usize;
                        match get_vu_u64(raw, &mut q, "SPARSE count") {
                            Ok(c) => {
                                let at = self.pos + q as u64;
                                self.consume(db, at)?;
                                self.cells_left = c;
                                break;
                            }
                            Err(e) => {
                                if self.pos + raw.len() as u64 >= self.rec_end {
                                    return Err(e);
                                }
                                need2 *= 2;
                            }
                        }
                    }
                    self.prev_v = Nat::ZERO;
                    self.in_group = true;
                    return Ok(());
                }
                Err(e) => {
                    if self.pos + raw.len() as u64 >= self.end {
                        return Err(e);
                    }
                    need = need.saturating_mul(2);
                }
            }
        }
    }

    /// Reads the heads of up to `max` cells of the current group (one type
    /// resolution for all of them); small values are decoded right away,
    /// large ones are only located and passed over.
    fn decode_heads(&mut self, db: &Db, window: usize, max: usize) -> Result<()> {
        let realm = self.seg.realm_id.clone();
        let u = self.cur_u.clone();
        let mut res = LazyResolver::new(db, &realm, &u);
        let mut n = 0usize;
        while self.cells_left > 0 && n < max {
            let mut need = 64usize;
            let (h, hlen, vlen) = loop {
                self.fill(db, need, window)?;
                let raw = self.avail(self.rec_end);
                let complete = self.pos + raw.len() as u64 >= self.rec_end;
                let mut p = 0usize;
                match read_head(raw, &mut p, &self.prev_v, &realm) {
                    Ok(h) => match res.extent(&h, raw, p)? {
                        Some(len) => break (h, p, len),
                        None if !complete => need = need.saturating_mul(2),
                        None => return Err(corrupt(&self.seg, "cell value incomplete")),
                    },
                    Err(e) if complete => return Err(e),
                    Err(_) => need = need.saturating_mul(2),
                }
            };
            let cell_start = self.pos;
            let val_start = cell_start + hlen as u64;
            let val_end = val_start.checked_add(vlen as u64).filter(|e| *e <= self.rec_end);
            let Some(val_end) = val_end else { return Err(corrupt(&self.seg, "cell value outside the record")) };
            let small = if vlen <= EAGER_VALUE {
                self.fill(db, hlen + vlen, window)?;
                let raw = self.avail(self.rec_end);
                let mut p = hlen;
                let value = res.decode(&h, raw, &mut p)?;
                if p != hlen + vlen {
                    return Err(corrupt(&self.seg, "cell value length"));
                }
                let rawc = self.keep_raw.then(|| RawCell { flags: raw_flags(&h), typ: h.typ, value: raw[hlen..p].to_vec() });
                Some((SparseCellMeta { value, typ: h.typ, selftyped: h.selftyped }, rawc))
            } else {
                None
            };
            self.consume(db, val_end)?;
            self.prev_v = h.v.clone();
            self.cells_left -= 1;
            self.queue.push_back(Head { h, cell_start, val_start, val_end, small });
            n += 1;
        }
        if self.cells_left == 0 {
            if self.pos != self.rec_end {
                return Err(corrupt(&self.seg, "record trailing bytes"));
            }
            self.in_group = false;
        }
        Ok(())
    }

    /// Current head cell `(u, v)` or `None` at the end of the block.
    fn head(&mut self, db: &Db, window: usize) -> Result<Option<(Nat, Nat)>> {
        loop {
            if let Some(c) = self.queue.front() {
                return Ok(Some((self.cur_u.clone(), c.h.v.clone())));
            }
            if self.finished {
                return Ok(None);
            }
            if self.in_group {
                if self.cells_left == 0 {
                    self.in_group = false;
                    continue;
                }
                let max = (window / (2 * EAGER_VALUE)).clamp(1, QUEUE_CELLS);
                self.decode_heads(db, window, max)?;
                continue;
            }
            if self.groups_left == 0 {
                self.finish()?;
                return Ok(None);
            }
            self.begin_group(db, window)?;
        }
    }

    /// Takes the head cell: decoded (a large value is read now).
    fn take(&mut self, db: &Db) -> Result<QCell> {
        let hd = self.queue.pop_front().expect("head present");
        let bytes = (hd.val_end - hd.cell_start) as usize;
        let (meta, raw) = match hd.small {
            Some(x) => x,
            None => {
                let value = self.bytes(db, hd.val_start, hd.val_end)?;
                let realm = self.seg.realm_id.clone();
                let mut res = LazyResolver::new(db, &realm, &self.cur_u);
                let mut p = 0usize;
                let v = res.decode(&hd.h, &value, &mut p)?;
                if p != value.len() {
                    return Err(corrupt(&self.seg, "cell value length"));
                }
                let rawc = self.keep_raw.then(|| RawCell { flags: raw_flags(&hd.h), typ: hd.h.typ, value });
                (SparseCellMeta { value: v, typ: hd.h.typ, selftyped: hd.h.selftyped }, rawc)
            }
        };
        Ok(QCell { v: hd.h.v, meta, bytes, raw, bid: self.seg.block_id })
    }

    /// Drops the head cell (superseded by a newer one) without reading it.
    fn skip(&mut self) {
        self.queue.pop_front();
    }
}

// ----------------------------------------------------------------------
// Temporary runs (external merge)
// ----------------------------------------------------------------------

const RUN_MAGIC: &[u8; 8] = b"NHYMRG02";

/// The temporary file of one external merge: anonymous (no name, nothing
/// left behind), one file descriptor for all runs (RB-05); runs are
/// appended one after the other.
struct TempFile {
    f: std::fs::File,
    end: Mutex<u64>,
}

impl TempFile {
    fn create(dirs: &[&Path]) -> Result<Arc<TempFile>> {
        let mut first_err = None;
        for d in dirs {
            match platform::anonymous_temp(d, "nhydb_merge") {
                Ok(f) => return Ok(Arc::new(TempFile { f, end: Mutex::new(0) })),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| Error::Value("no directory for temporary merge runs".into())))
    }

    fn io(e: std::io::Error) -> Error {
        Error::Io(std::io::Error::new(e.kind(), format!("temporary merge run: {e}")))
    }

    fn write_at(&self, data: &[u8], off: u64) -> Result<()> {
        platform::write_all_at(&self.f, data, off).map_err(Self::io)
    }

    fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> {
        platform::read_exact_at(&self.f, buf, off).map_err(Self::io)
    }
}

/// A finished run: byte range in the temporary file.
struct TempRun {
    file: Arc<TempFile>,
    start: u64,
    end: u64,
}

/// Writes a sorted cell run: records `1 | vu u | vu v | vu block id | flags |
/// type | vu len | value`, end marker `0`, record count and CRC32 of all
/// bytes.
struct TempWriter {
    file: Arc<TempFile>,
    start: u64,
    written: u64,
    out: Vec<u8>,
    crc: crc32fast::Hasher,
    count: u64,
}

impl TempWriter {
    fn create(file: &Arc<TempFile>) -> Result<TempWriter> {
        let start = *file.end.lock().unwrap_or_else(|e| e.into_inner());
        let mut w = TempWriter { file: file.clone(), start, written: 0, out: Vec::with_capacity(CHUNK), crc: crc32fast::Hasher::new(), count: 0 };
        w.write(RUN_MAGIC)?;
        Ok(w)
    }

    fn write(&mut self, b: &[u8]) -> Result<()> {
        self.crc.update(b);
        self.out.extend_from_slice(b);
        if self.out.len() >= CHUNK {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.out.is_empty() {
            self.file.write_at(&self.out, self.start + self.written)?;
            self.written += self.out.len() as u64;
            self.out.clear();
        }
        Ok(())
    }

    fn push(&mut self, u: &Nat, v: &Nat, bid: u64, c: &RawCell) -> Result<()> {
        let mut b = Vec::with_capacity(24);
        b.push(1);
        u.put_vu(&mut b);
        v.put_vu(&mut b);
        put_vu64(&mut b, bid);
        b.push(c.flags);
        b.push(c.typ.map(|t| t.code()).unwrap_or(0xFF));
        put_vu64(&mut b, c.value.len() as u64);
        self.write(&b)?;
        self.write(&c.value)?;
        self.count += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<(TempRun, u64)> {
        let mut tail = vec![0u8];
        tail.extend_from_slice(&self.count.to_le_bytes());
        self.write(&tail)?;
        let crc = self.crc.clone().finalize();
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.flush()?;
        let end = self.start + self.written;
        *self.file.end.lock().unwrap_or_else(|e| e.into_inner()) = end;
        Ok((TempRun { file: self.file.clone(), start: self.start, end }, self.count))
    }
}

/// Head of a run record: the value is located, not read.
struct TempHead {
    u: Nat,
    v: Nat,
    bid: u64,
    flags: u8,
    typ: Option<TypeCode>,
    val_start: u64,
    val_len: usize,
}

/// Reads a temporary run in order through a small window; values are read
/// when taken (hashed in chunks when skipped); the CRC is checked at the
/// end.
struct TempStream {
    run: TempRun,
    pos: u64,
    buf: Vec<u8>,
    buf_off: u64,
    crc: crc32fast::Hasher,
    count: u64,
    head: Option<TempHead>,
    done: bool,
}

impl TempStream {
    fn open(run: TempRun, window: usize) -> Result<TempStream> {
        let mut st = TempStream { pos: run.start, buf: Vec::new(), buf_off: run.start, crc: crc32fast::Hasher::new(), count: 0, head: None, done: false, run };
        let m = st.take_bytes(8, window)?;
        if m != RUN_MAGIC {
            return Err(Error::Corruption("temporary merge run damaged (magic)".into()));
        }
        st.advance(window)?;
        Ok(st)
    }

    fn damaged(what: &str) -> Error {
        Error::Corruption(format!("temporary merge run damaged ({what})"))
    }

    /// Buffered bytes from `pos` (at least `need` unless the run ends).
    fn peek(&mut self, need: usize, window: usize) -> Result<&[u8]> {
        let have_end = self.buf_off + self.buf.len() as u64;
        if self.pos < self.buf_off || self.pos + need as u64 > have_end {
            let len = (need.max(window) as u64).min(self.run.end - self.pos) as usize;
            let mut nb = vec![0u8; len];
            self.run.file.read_at(&mut nb, self.pos)?;
            self.buf = nb;
            self.buf_off = self.pos;
        }
        let s = (self.pos - self.buf_off) as usize;
        Ok(&self.buf[s..])
    }

    /// Consumes `n` bytes (hashed) and returns them.
    fn take_bytes(&mut self, n: usize, window: usize) -> Result<Vec<u8>> {
        if n > window {
            let mut v = vec![0u8; n];
            if self.pos + n as u64 > self.run.end {
                return Err(Self::damaged("truncated"));
            }
            self.run.file.read_at(&mut v, self.pos)?;
            self.crc.update(&v);
            self.pos += n as u64;
            return Ok(v);
        }
        let b = self.peek(n, window)?;
        if b.len() < n {
            return Err(Self::damaged("truncated"));
        }
        let v = b[..n].to_vec();
        self.crc.update(&v);
        self.pos += n as u64;
        Ok(v)
    }

    /// Consumes `n` bytes (hashed) without keeping them.
    fn pass(&mut self, n: usize, window: usize) -> Result<()> {
        let mut left = n;
        while left > 0 {
            let k = left.min(CHUNK);
            self.take_bytes(k, window.max(k))?;
            left -= k;
        }
        Ok(())
    }

    fn read_vu(&mut self, window: usize) -> Result<Nat> {
        let b = self.peek(16, window)?;
        let Some(e) = b.iter().position(|x| *x < 0x80).map(|i| i + 1) else {
            // a long varint (BigInt): byte by byte
            let mut raw = Vec::new();
            loop {
                let x = self.take_bytes(1, window)?[0];
                raw.push(x);
                if x < 0x80 {
                    break;
                }
            }
            let mut p = 0;
            return get_vu(&raw, &mut p);
        };
        let raw = self.take_bytes(e, window)?;
        let mut p = 0;
        get_vu(&raw, &mut p)
    }

    fn advance(&mut self, window: usize) -> Result<()> {
        if let Some(h) = self.head.take() {
            if self.pos == h.val_start {
                self.pass(h.val_len, window)?;
            }
        }
        if self.done {
            return Ok(());
        }
        let m = self.take_bytes(1, window)?[0];
        if m == 0 {
            let c = self.take_bytes(8, window)?;
            let want = std::mem::replace(&mut self.crc, crc32fast::Hasher::new()).finalize();
            let got = self.take_bytes(4, window)?;
            if u64::from_le_bytes(c.try_into().unwrap()) != self.count || u32::from_le_bytes(got.try_into().unwrap()) != want || self.pos != self.run.end {
                return Err(Self::damaged("CRC"));
            }
            self.done = true;
            return Ok(());
        }
        if m != 1 {
            return Err(Self::damaged("record marker"));
        }
        let u = self.read_vu(window)?;
        let v = self.read_vu(window)?;
        let bid = self.read_vu(window)?.as_u64().ok_or_else(|| Self::damaged("block id"))?;
        let ft = self.take_bytes(2, window)?;
        let typ = if ft[1] == 0xFF { None } else { Some(TypeCode::from_code(ft[1]).ok_or_else(|| Self::damaged("type code"))?) };
        let len = self.read_vu(window)?.as_u64().ok_or_else(|| Self::damaged("length"))? as usize;
        if self.pos + len as u64 > self.run.end {
            return Err(Self::damaged("length"));
        }
        self.count += 1;
        self.head = Some(TempHead { u, v, bid, flags: ft[0], typ, val_start: self.pos, val_len: len });
        Ok(())
    }

    /// Decodes the head cell (type of non-self-typed cells from the schema,
    /// as when it was read from its block).
    fn take(&mut self, db: &Db, realm: &Nat, keep_raw: bool, window: usize) -> Result<QCell> {
        let (u, v, bid, flags, typ, len) = {
            let h = self.head.as_ref().expect("head present");
            (h.u.clone(), h.v.clone(), h.bid, h.flags, h.typ, h.val_len)
        };
        let value = self.take_bytes(len, window)?;
        let h = CellHead { v: v.clone(), tomb: flags & 1 != 0, null: flags & 4 != 0, selftyped: flags & 2 != 0, typ };
        let mut res = LazyResolver::new(db, realm, &u);
        let mut p = 0usize;
        let meta_v = res.decode(&h, &value, &mut p)?;
        if p != value.len() {
            return Err(Self::damaged("value length"));
        }
        let bytes = value.len() + 4;
        let cell = QCell { v, meta: SparseCellMeta { value: meta_v, typ: h.typ, selftyped: h.selftyped }, bytes, raw: keep_raw.then_some(RawCell { flags, typ, value }), bid };
        self.advance(window)?;
        Ok(cell)
    }
}

// ----------------------------------------------------------------------
// Merge core
// ----------------------------------------------------------------------

/// A sorted cell source: a block stream or a temporary run.
enum Src {
    Seg(SegStream),
    Tmp(TempStream),
}

impl Src {
    /// Head cell `(u, v, block id)`.
    fn head(&mut self, db: &Db, window: usize) -> Result<Option<(Nat, Nat, u64)>> {
        match self {
            Src::Seg(s) => {
                let bid = s.seg.block_id;
                Ok(s.head(db, window)?.map(|(u, v)| (u, v, bid)))
            }
            Src::Tmp(t) => Ok(t.head.as_ref().map(|h| (h.u.clone(), h.v.clone(), h.bid))),
        }
    }

    /// Takes the head cell (decoded).
    fn take(&mut self, db: &Db, realm: &Nat, keep_raw: bool, window: usize) -> Result<QCell> {
        match self {
            Src::Seg(s) => s.take(db),
            Src::Tmp(t) => t.take(db, realm, keep_raw, window),
        }
    }

    /// Drops the head cell (superseded by a newer one).
    fn skip(&mut self, window: usize) -> Result<()> {
        match self {
            Src::Seg(s) => {
                s.skip();
                Ok(())
            }
            Src::Tmp(t) => t.advance(window),
        }
    }

    fn trim(&mut self, window: usize) {
        if let Src::Seg(s) = self {
            s.trim(window);
        }
    }
}

/// Blocks still to be opened (direct merge): the directory iterator or a
/// group of blocks of an external merge, in `(u_start, block id)` order.
enum Blocks {
    Iter(ByStart<SparseSegment>),
    List(VecDeque<SparseSegment>),
}

struct Pending {
    it: Blocks,
    peeked: Option<SparseSegment>,
    /// blocks with a larger id are ignored (snapshot)
    hi: u64,
    /// blocks ending before this u are skipped (`SparseMerge::new_from`)
    lo: Option<Nat>,
    exhausted: bool,
}

struct Core {
    realm: Nat,
    pending: Option<Pending>,
    verify: Verify,
    keep_raw: bool,
    streams: Vec<Option<Src>>,
    free: Vec<usize>,
    active: usize,
    /// number of open sources at the last buffer trim
    trimmed_at: usize,
    heap: BinaryHeap<Reverse<(Nat, Nat, u64, usize)>>,
    /// keeps the anonymous temporary file of an external merge alive
    _temp: Option<Arc<TempFile>>,
}

impl Core {
    fn window(&self) -> usize {
        (MERGE_BUFFER_BUDGET / self.active.max(1)).clamp(MIN_WINDOW, MAX_WINDOW)
    }

    fn add(&mut self, db: &Db, mut src: Src) -> Result<()> {
        self.active += 1;
        // the window shrinks with the number of open sources: older
        // buffers are cut down whenever that number doubled (bounded
        // total instead of one large window per early stream)
        if self.active >= 2 * self.trimmed_at.max(16) {
            let w = self.window();
            for s in self.streams.iter_mut().flatten() {
                s.trim(w);
            }
            self.trimmed_at = self.active;
        }
        let w = self.window();
        match src.head(db, w)? {
            Some((u, v, bid)) => {
                let i = match self.free.pop() {
                    Some(i) => {
                        self.streams[i] = Some(src);
                        i
                    }
                    None => {
                        self.streams.push(Some(src));
                        self.streams.len() - 1
                    }
                };
                self.heap.push(Reverse((u, v, bid, i)));
            }
            None => self.active -= 1,
        }
        Ok(())
    }

    fn peek_pending<'a>(pending: &'a mut Pending, db: &Db) -> Result<Option<&'a SparseSegment>> {
        while pending.peeked.is_none() && !pending.exhausted {
            let next = match &mut pending.it {
                Blocks::Iter(it) => it.next(db)?,
                Blocks::List(l) => l.pop_front(),
            };
            match next {
                Some(s) if s.block_id <= pending.hi && pending.lo.as_ref().is_none_or(|l| &s.u_end >= l) => pending.peeked = Some(s),
                Some(_) => {}
                None => pending.exhausted = true,
            }
        }
        Ok(pending.peeked.as_ref())
    }

    fn next(&mut self, db: &Db) -> Result<Option<MergedCell>> {
        // open every block that may hold a cell at or before the head
        while let Some(mut pending) = self.pending.take() {
            let head_u = self.heap.peek().map(|Reverse((u, _, _, _))| u.clone());
            let open = match Self::peek_pending(&mut pending, db)? {
                Some(p) => head_u.as_ref().is_none_or(|hu| &p.u_start <= hu),
                None => false,
            };
            if !open {
                if !pending.exhausted || pending.peeked.is_some() {
                    self.pending = Some(pending);
                }
                break;
            }
            let seg = pending.peeked.take().expect("peeked");
            self.pending = Some(pending);
            let w = self.window();
            let st = SegStream::open(db, seg, self.verify, w, self.keep_raw)?;
            self.add(db, Src::Seg(st))?;
        }
        let Some(Reverse((u, v, _, _))) = self.heap.peek().cloned() else { return Ok(None) };
        // all sources at (u, v); ascending block id: the last one wins
        let mut at: Vec<usize> = Vec::new();
        while let Some(Reverse((hu, hv, _, i))) = self.heap.peek().cloned() {
            if hu != u || hv != v {
                break;
            }
            self.heap.pop();
            at.push(i);
        }
        let win = *at.last().expect("at least one source");
        let mut cell: Option<QCell> = None;
        for &i in &at {
            let w = self.window();
            let realm = self.realm.clone();
            let keep_raw = self.keep_raw;
            let src = self.streams[i].as_mut().expect("source");
            if i == win {
                cell = Some(src.take(db, &realm, keep_raw, w)?);
            } else {
                src.skip(w)?;
            }
            match src.head(db, w)? {
                Some((nu, nv, nb)) => self.heap.push(Reverse((nu, nv, nb, i))),
                None => {
                    self.streams[i] = None;
                    self.free.push(i);
                    self.active -= 1;
                }
            }
        }
        let c = cell.expect("winner");
        Ok(Some(MergedCell { u, v: c.v, meta: c.meta, bytes: c.bytes, raw: c.raw, bid: c.bid }))
    }
}

/// Whether more than [`MAX_OPEN`] blocks with ids `<= hi` overlap at some u
/// (sweep over the directory in u_start order, memory `MAX_OPEN`).
fn overlap_exceeds(db: &Db, key: &RL, hi: u64) -> Result<bool> {
    let Some(ix) = db.sparse.get(key) else { return Ok(false) };
    if ix.idx.len() <= MAX_OPEN as u64 {
        return Ok(false);
    }
    let mut it = ix.idx.iter_by_start();
    let mut ends: BinaryHeap<Reverse<Nat>> = BinaryHeap::new();
    while let Some(e) = it.next(db)? {
        if e.block_id > hi {
            continue;
        }
        while ends.peek().is_some_and(|Reverse(end)| end < &e.u_start) {
            ends.pop();
        }
        ends.push(Reverse(e.u_end.clone()));
        if ends.len() > MAX_OPEN {
            return Ok(true);
        }
    }
    Ok(false)
}

fn empty_core(realm: &Nat, verify: Verify, keep_raw: bool) -> Core {
    Core {
        realm: realm.clone(),
        pending: None,
        verify,
        keep_raw,
        streams: Vec::new(),
        free: Vec::new(),
        active: 0,
        trimmed_at: 0,
        heap: BinaryHeap::new(),
        _temp: None,
    }
}

/// Writes all cells of `core` (raw form required) into a temporary run.
fn spill(db: &Db, mut core: Core, file: &Arc<TempFile>) -> Result<Option<TempRun>> {
    let mut w = TempWriter::create(file)?;
    while let Some(c) = core.next(db)? {
        w.push(&c.u, &c.v, c.bid, c.raw.as_ref().expect("raw cell"))?;
    }
    drop(core);
    let (run, count) = w.finish()?;
    Ok((count > 0).then_some(run))
}

/// Opens runs as sources of `core` (window per run from their number).
fn add_runs(db: &Db, core: &mut Core, runs: Vec<TempRun>) -> Result<()> {
    let w = (MERGE_BUFFER_BUDGET / runs.len().max(1)).clamp(MIN_WINDOW, MAX_WINDOW);
    for run in runs {
        core.add(db, Src::Tmp(TempStream::open(run, w)?))?;
    }
    Ok(())
}

/// Builds the merge of the blocks with ids `<= hi`: direct, or – if too
/// many of them overlap – external: groups of `MAX_OPEN` blocks merged into
/// temporary runs, then the runs (in levels of `MAX_OPEN` runs). The runs
/// live in one anonymous temporary file in the first usable of `dirs`.
fn build(db: &Db, key: &RL, hi: u64, lo: Option<&Nat>, verify: Verify, keep_raw: bool, dirs: &[&Path]) -> Result<Core> {
    let Some(ix) = db.sparse.get(key) else { return Ok(empty_core(&key.0, verify, keep_raw)) };
    if !overlap_exceeds(db, key, hi)? {
        let mut core = empty_core(&key.0, verify, keep_raw);
        core.pending = Some(Pending { it: Blocks::Iter(ix.idx.iter_by_start()), peeked: None, hi, lo: lo.cloned(), exhausted: false });
        return Ok(core);
    }
    let file = TempFile::create(dirs)?;
    // every block is consumed completely (CRC included) while the runs are
    // written, before the first cell is returned: incremental checking
    // suffices for the groups
    let mut runs: Vec<TempRun> = Vec::new();
    let mut it = ix.idx.iter_by_start();
    let mut group: VecDeque<SparseSegment> = VecDeque::new();
    loop {
        let next = it.next(db)?;
        if let Some(s) = &next {
            if s.block_id <= hi && lo.is_none_or(|l| &s.u_end >= l) {
                group.push_back(s.clone());
            }
        }
        if group.len() >= MAX_OPEN || (next.is_none() && !group.is_empty()) {
            let mut core = empty_core(&key.0, Verify::Incremental, true);
            core.pending = Some(Pending { it: Blocks::List(std::mem::take(&mut group)), peeked: None, hi, lo: None, exhausted: false });
            if let Some(run) = spill(db, core, &file)? {
                runs.push(run);
            }
        }
        if next.is_none() {
            break;
        }
    }
    // levels of MAX_OPEN runs
    while runs.len() > MAX_OPEN {
        let mut next_level = Vec::new();
        let mut rest = runs.into_iter().peekable();
        while rest.peek().is_some() {
            let mut core = empty_core(&key.0, Verify::Incremental, true);
            add_runs(db, &mut core, rest.by_ref().take(MAX_OPEN).collect())?;
            if let Some(run) = spill(db, core, &file)? {
                next_level.push(run);
            }
        }
        runs = next_level;
    }
    let mut core = empty_core(&key.0, verify, keep_raw);
    add_runs(db, &mut core, runs)?;
    core._temp = Some(file);
    Ok(core)
}

/// Newest sparse cell states of one key in `(u, v)` order.
pub(crate) struct SparseMerge {
    core: Core,
}

impl SparseMerge {
    /// Merge over the current segments of `(realm, lane)` (snapshot: blocks
    /// registered later are not part of it); temporary runs of an external
    /// merge go to the system temp directory, or next to the database if
    /// that is not usable (anonymous file, nothing is left behind).
    pub(crate) fn new(db: &Db, realm: &Nat, lane: Lane, verify: Verify) -> Result<SparseMerge> {
        let tmp = std::env::temp_dir();
        let db_dir = platform::parent_dir(&db.path);
        let key = (realm.clone(), lane);
        let hi = db.next_block_id.saturating_sub(1);
        Ok(SparseMerge { core: build(db, &key, hi, None, verify, false, &[&tmp, &db_dir])? })
    }

    /// Like [`SparseMerge::new`], skipping the blocks that end before
    /// `from` (cells of smaller u from blocks reaching `from` are still
    /// returned; the caller skips them).
    pub(crate) fn new_from(db: &Db, realm: &Nat, lane: Lane, verify: Verify, from: &Nat) -> Result<SparseMerge> {
        let tmp = std::env::temp_dir();
        let db_dir = platform::parent_dir(&db.path);
        let key = (realm.clone(), lane);
        let hi = db.next_block_id.saturating_sub(1);
        Ok(SparseMerge { core: build(db, &key, hi, Some(from), verify, false, &[&tmp, &db_dir])? })
    }

    /// Like [`SparseMerge::new`] with temporary runs in `dir`.
    pub(crate) fn with_temp_dir(db: &Db, realm: &Nat, lane: Lane, verify: Verify, dir: &Path) -> Result<SparseMerge> {
        let key = (realm.clone(), lane);
        let hi = db.next_block_id.saturating_sub(1);
        Ok(SparseMerge { core: build(db, &key, hi, None, verify, false, &[dir])? })
    }

    /// Next cell (newest state), `None` at the end.
    pub(crate) fn next(&mut self, db: &Db) -> Result<Option<MergedCell>> {
        self.core.next(db)
    }
}

#[cfg(all(test, nhydb_internal_tests))]
pub(crate) fn overlap_exceeds_pub(db: &Db, key: &RL) -> bool {
    overlap_exceeds(db, key, u64::MAX).unwrap()
}
