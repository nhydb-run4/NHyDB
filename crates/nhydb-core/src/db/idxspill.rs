//! External sort of the index build (0.28.2).
//!
//! `build_indexes` collects the keys of the selected indexes with their
//! posting lists in memory (`ValueMap`). Once the estimated memory of all
//! maps exceeds the build budget (`[index] build_memory_bytes`), every map is
//! sorted in key order and written as a run to an anonymous temporary file
//! (no name, nothing left behind), and the maps start empty again. Writing
//! the SECONDARY tree then merges the runs and the in-memory rest key by key;
//! lists of one key from several runs are merged by document u. Memory of the
//! merge: one read window per run (together a quarter of the budget) plus
//! the lists of the current key.
//!
//! Run format: records `vu key length | key | kind | vu count | items`,
//! end marker `0`, record count (u64) and CRC32 of all bytes; checked when
//! the run has been read.

use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::nat::{get_vu, put_vu64, Nat};
use crate::platform;

use super::index::{Bucket, UList};

const RUN_MAGIC: &[u8; 8] = b"NHYIDX01";
const WINDOW: usize = 256 << 10;

/// The temporary file of one build (shared by all runs).
pub(crate) struct SpillFile {
    f: std::fs::File,
    end: std::sync::Mutex<u64>,
}

impl SpillFile {
    pub(crate) fn create(dirs: &[&Path]) -> Result<Arc<SpillFile>> {
        let mut first = None;
        for d in dirs {
            match platform::anonymous_temp(d, "nhydb_index") {
                Ok(f) => return Ok(Arc::new(SpillFile { f, end: std::sync::Mutex::new(0) })),
                Err(e) => {
                    first.get_or_insert(e);
                }
            }
        }
        Err(first.unwrap_or_else(|| Error::Value("no directory for temporary index runs".into())))
    }

    fn io(e: std::io::Error) -> Error {
        Error::Io(std::io::Error::new(e.kind(), format!("temporary index run: {e}")))
    }
}

/// A finished run: byte range in the temporary file.
#[derive(Clone)]
pub(crate) struct SpillRun {
    file: Arc<SpillFile>,
    start: u64,
    end: u64,
}

impl SpillRun {
    /// End of the run in the temporary file (= file size after it).
    pub(crate) fn end_offset(&self) -> u64 {
        self.end
    }
}

pub(crate) struct SpillWriter {
    file: Arc<SpillFile>,
    start: u64,
    written: u64,
    out: Vec<u8>,
    crc: crc32fast::Hasher,
    count: u64,
}

impl SpillWriter {
    pub(crate) fn create(file: &Arc<SpillFile>) -> Result<SpillWriter> {
        let start = *file.end.lock().unwrap_or_else(|e| e.into_inner());
        let mut w = SpillWriter { file: file.clone(), start, written: 0, out: Vec::with_capacity(WINDOW), crc: crc32fast::Hasher::new(), count: 0 };
        w.write(RUN_MAGIC)?;
        Ok(w)
    }

    fn write(&mut self, b: &[u8]) -> Result<()> {
        self.crc.update(b);
        self.out.extend_from_slice(b);
        if self.out.len() >= WINDOW {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.out.is_empty() {
            platform::write_all_at(&self.file.f, &self.out, self.start + self.written).map_err(SpillFile::io)?;
            self.written += self.out.len() as u64;
            self.out.clear();
        }
        Ok(())
    }

    /// One key (encoded) with its bucket (lists sorted by u).
    pub(crate) fn push(&mut self, key: &[u8], b: &Bucket) -> Result<()> {
        let mut r = Vec::with_capacity(key.len() + 16 + b.len() * 3);
        put_vu64(&mut r, key.len() as u64);
        r.extend_from_slice(key);
        match b {
            Bucket::Us(us) => {
                r.push(0);
                put_vu64(&mut r, us.len() as u64);
                let mut prev = Nat::ZERO;
                for u in us.to_nats() {
                    u.checked_sub(&prev).unwrap_or_default().put_vu(&mut r);
                    prev = u;
                }
            }
            Bucket::Ft(items) => {
                r.push(1);
                put_vu64(&mut r, items.len() as u64);
                for (u, pos) in items {
                    u.put_vu(&mut r);
                    put_vu64(&mut r, pos.len() as u64);
                    for p in pos {
                        put_vu64(&mut r, *p);
                    }
                }
            }
            Bucket::Doc(items) => {
                r.push(2);
                put_vu64(&mut r, items.len() as u64);
                for (u, n) in items {
                    u.put_vu(&mut r);
                    put_vu64(&mut r, *n);
                }
            }
        }
        self.write(&r)?;
        self.count += 1;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<SpillRun> {
        let mut tail = vec![0u8];
        tail.extend_from_slice(&self.count.to_le_bytes());
        self.write(&tail)?;
        let crc = self.crc.clone().finalize();
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.flush()?;
        let end = self.start + self.written;
        *self.file.end.lock().unwrap_or_else(|e| e.into_inner()) = end;
        Ok(SpillRun { file: self.file.clone(), start: self.start, end })
    }
}

/// Sequential reader of one run.
pub(crate) struct SpillReader {
    run: SpillRun,
    /// read window (the merge divides its budget among the runs)
    window: usize,
    pos: u64,
    buf: Vec<u8>,
    at: usize,
    crc: crc32fast::Hasher,
    count: u64,
    /// header of the next record; its entries are read on demand (RUN9 V4)
    pub(crate) head: Option<RunHead>,
}

/// Header of a run record: key, posting kind (0 u, 1 FULLTEXT positions,
/// 2 document lengths) and the entries not read yet (RUN9 V4: the merge
/// reads them one by one, so a key spread over many runs is never decoded
/// as a whole).
pub(crate) struct RunHead {
    pub(crate) key: Vec<u8>,
    pub(crate) kind: u8,
    pub(crate) left: u64,
    /// last u of a u list (stored as deltas)
    prev: Nat,
}

/// One entry of a run record.
pub(crate) enum RunEntry {
    U(Nat),
    Ft(Nat, Vec<u64>),
    Doc(Nat, u64),
}

fn damaged(what: &str) -> Error {
    Error::Corruption(format!("temporary index run damaged ({what})"))
}

impl SpillReader {
    pub(crate) fn open(run: SpillRun, window: usize) -> Result<SpillReader> {
        let mut r = SpillReader { pos: run.start, run, window: window.clamp(4096, WINDOW), buf: Vec::new(), at: 0, crc: crc32fast::Hasher::new(), count: 0, head: None };
        if r.take(8)? != RUN_MAGIC {
            return Err(damaged("magic"));
        }
        r.advance()?;
        Ok(r)
    }

    /// Makes `n` bytes available at `at`.
    fn fill(&mut self, n: usize) -> Result<()> {
        if self.buf.len() - self.at >= n {
            return Ok(());
        }
        let keep = self.buf.split_off(self.at);
        let want = n.max(self.window) - keep.len();
        let len = (want as u64).min(self.run.end - self.pos) as usize;
        let mut nb = keep;
        let old = nb.len();
        nb.resize(old + len, 0);
        platform::read_exact_at(&self.run.file.f, &mut nb[old..], self.pos).map_err(SpillFile::io)?;
        self.pos += len as u64;
        self.buf = nb;
        self.at = 0;
        if self.buf.len() < n {
            return Err(damaged("truncated"));
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<Vec<u8>> {
        self.fill(n)?;
        let v = self.buf[self.at..self.at + n].to_vec();
        self.crc.update(&v);
        self.at += n;
        Ok(v)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn vu(&mut self) -> Result<Nat> {
        // varints are short; read up to 32 bytes ahead without consuming
        let avail = (self.buf.len() - self.at) as u64 + (self.run.end - self.pos);
        self.fill((avail as usize).min(32))?;
        let mut p = 0;
        let slice = &self.buf[self.at..];
        let v = get_vu(slice, &mut p).map_err(|_| damaged("varint"))?;
        self.crc.update(&slice[..p]);
        self.at += p;
        Ok(v)
    }

    fn vu64(&mut self) -> Result<u64> {
        self.vu()?.as_u64().ok_or_else(|| damaged("count"))
    }

    /// Reads the next record header into `head` (`None` at the end; the trailer
    /// and the CRC are checked then).
    pub(crate) fn advance(&mut self) -> Result<()> {
        let klen = self.vu64()? as usize;
        if klen == 0 {
            let n = u64::from_le_bytes(self.take(8)?.try_into().unwrap());
            let want = self.crc.clone().finalize();
            self.fill(4)?;
            let got = u32::from_le_bytes(self.buf[self.at..self.at + 4].try_into().unwrap());
            if n != self.count || got != want {
                return Err(damaged("count/CRC"));
            }
            self.head = None;
            return Ok(());
        }
        let key = self.take(klen)?;
        let kind = self.byte()?;
        if kind > 2 {
            return Err(damaged("kind"));
        }
        let left = self.vu64()?;
        self.count += 1;
        self.head = Some(RunHead { key, kind, left, prev: Nat::ZERO });
        Ok(())
    }

    /// Next entry of the current record (`None` once all are read).
    pub(crate) fn next_entry(&mut self) -> Result<Option<RunEntry>> {
        let Some(h) = &self.head else { return Ok(None) };
        if h.left == 0 {
            return Ok(None);
        }
        let kind = h.kind;
        let e = match kind {
            0 => {
                let d = self.vu()?;
                let h = self.head.as_mut().unwrap();
                h.prev = h.prev.add(&d);
                RunEntry::U(h.prev.clone())
            }
            1 => {
                let u = self.vu()?;
                let k = self.vu64()?;
                let mut pos = Vec::with_capacity((k as usize).min(1 << 16));
                for _ in 0..k {
                    pos.push(self.vu64()?);
                }
                RunEntry::Ft(u, pos)
            }
            _ => {
                let u = self.vu()?;
                RunEntry::Doc(u, self.vu64()?)
            }
        };
        self.head.as_mut().unwrap().left -= 1;
        Ok(Some(e))
    }

    /// The remaining entries of the current record as a bucket (the tight
    /// loops of the whole-record decoding; `next_entry` is for the merge).
    pub(crate) fn take_bucket(&mut self) -> Result<Bucket> {
        let (kind, n, mut prev) = match &self.head {
            Some(h) => (h.kind, h.left, h.prev.clone()),
            None => return Err(Error::Value("index build: no run record".into())),
        };
        let b = match kind {
            0 => {
                let mut us = UList::default();
                for _ in 0..n {
                    prev = prev.add(&self.vu()?);
                    us.push(&prev)?;
                }
                Bucket::Us(us)
            }
            1 => {
                let mut v = Vec::with_capacity((n as usize).min(1 << 20));
                for _ in 0..n {
                    let u = self.vu()?;
                    let k = self.vu64()?;
                    let mut pos = Vec::with_capacity((k as usize).min(1 << 16));
                    for _ in 0..k {
                        pos.push(self.vu64()?);
                    }
                    v.push((u, pos));
                }
                Bucket::Ft(v)
            }
            _ => {
                let mut v = Vec::with_capacity((n as usize).min(1 << 20));
                for _ in 0..n {
                    let u = self.vu()?;
                    v.push((u, self.vu64()?));
                }
                Bucket::Doc(v)
            }
        };
        let h = self.head.as_mut().unwrap();
        h.left = 0;
        h.prev = prev;
        Ok(b)
    }

    /// Moves to the next record; the entries of the current one must have
    /// been read.
    pub(crate) fn next_record(&mut self) -> Result<()> {
        if self.head.as_ref().is_some_and(|h| h.left > 0) {
            return Err(Error::Value("index build: run record not read completely".into()));
        }
        self.advance()
    }
}

/// Merges the buckets of one key from several sources (each sorted by
/// u, disjoint u) in their compact form: `u32`/`u64` lists are joined and
/// sorted as plain integers (4/8 bytes per reference), not as `Nat`.
pub(crate) fn merge_buckets(parts: Vec<Bucket>) -> Result<Bucket> {
    let mut it = parts.into_iter();
    let Some(first) = it.next() else {
        return Err(Error::Value("index build: no bucket to merge".into()));
    };
    let rest: Vec<Bucket> = it.collect();
    if rest.is_empty() {
        return Ok(first);
    }
    let kind = |b: &Bucket| match b {
        Bucket::Us(_) => 0,
        Bucket::Ft(_) => 1,
        Bucket::Doc(_) => 2,
    };
    if rest.iter().any(|b| kind(b) != kind(&first)) {
        return Err(Error::Value("index build: one key with different posting kinds".into()));
    }
    Ok(match first {
        Bucket::Us(a) => {
            let mut all = vec![a];
            all.extend(rest.into_iter().map(|b| match b {
                Bucket::Us(x) => x,
                _ => unreachable!(),
            }));
            let total: usize = all.iter().map(|x| x.len()).sum();
            if all.iter().all(|x| matches!(x, UList::Tiny(_))) {
                let mut v: Vec<u32> = Vec::new();
                v.try_reserve_exact(total).map_err(|e| Error::Memory(format!("index build: {e}")))?;
                for x in all {
                    if let UList::Tiny(t) = x {
                        v.extend_from_slice(&t);
                    }
                }
                v.sort_unstable();
                Bucket::Us(UList::Tiny(v))
            } else if all.iter().all(|x| !matches!(x, UList::Big(_))) {
                let mut v: Vec<u64> = Vec::new();
                v.try_reserve_exact(total).map_err(|e| Error::Memory(format!("index build: {e}")))?;
                for x in all {
                    match x {
                        UList::Tiny(t) => v.extend(t.iter().map(|&y| y as u64)),
                        UList::Small(t) => v.extend_from_slice(&t),
                        UList::Big(_) => unreachable!(),
                    }
                }
                v.sort_unstable();
                Bucket::Us(UList::Small(v))
            } else {
                let mut v: Vec<Nat> = Vec::with_capacity(total);
                for x in all {
                    v.extend(x.to_nats());
                }
                v.sort();
                Bucket::Us(UList::Big(v))
            }
        }
        Bucket::Ft(mut x) => {
            for b in rest {
                if let Bucket::Ft(y) = b {
                    x.extend(y);
                }
            }
            x.sort_by(|p, q| p.0.cmp(&q.0));
            Bucket::Ft(x)
        }
        Bucket::Doc(mut x) => {
            for b in rest {
                if let Bucket::Doc(y) = b {
                    x.extend(y);
                }
            }
            x.sort_by(|p, q| p.0.cmp(&q.0));
            Bucket::Doc(x)
        }
    })
}

