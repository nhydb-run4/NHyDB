//! Result container (0.28.2 round 4, stage C): a sequential file of
//! CRC-32C-protected chunks with a header, a footer and a fixed trailer,
//! written as `<file>.part` and renamed when complete. General purpose: the
//! payload kind names what the items are (`nhydb.rows/1`: one row per item,
//! encoded with `valcodec`); the OER export can use other kinds.
//! Byte layout: `docs/FORMAT.md`, section "Ergebniscontainer".

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::platform::{self, Durability};
use crate::val::Val;
use crate::valcodec;

pub const MAGIC: &[u8; 4] = b"NHYC";
pub const VERSION: u8 = 1;
const CHUNK_TAG: &[u8; 2] = b"CK";
const FOOTER_TAG: &[u8; 2] = b"FT";
pub const TRAILER_LEN: u64 = 12;
/// Raw payload bytes per chunk the writer aims at.
pub const CHUNK_TARGET: usize = 1 << 20;
/// The same for sort runs (anonymous containers).
pub const RUN_CHUNK_TARGET: usize = 64 << 10;
/// Largest raw payload of one chunk a reader accepts.
pub const MAX_CHUNK_RAW: usize = 64 << 20;
pub const CODEC_RAW: u8 = 0;
pub const CODEC_ZLIB: u8 = 1;
/// Payload kind of query results: one row (`Val::Map`) per item.
pub const KIND_ROWS: &str = "nhydb.rows/1";

fn bad(path: &Path, m: impl std::fmt::Display) -> Error {
    Error::Corruption(format!("result container {}: {m}", path.display()))
}

// ----------------------------------------------------------------------
// CRC-32C (Castagnoli, reflected 0x82F63B78), slicing by 8
// ----------------------------------------------------------------------

static CRC_TABLES: std::sync::OnceLock<[[u32; 256]; 8]> = std::sync::OnceLock::new();

fn crc_tables() -> &'static [[u32; 256]; 8] {
    CRC_TABLES.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = (c >> 1) ^ (0x82F6_3B78 & (c & 1).wrapping_neg());
            }
            t[0][i as usize] = c;
        }
        for i in 0..256 {
            for k in 1..8 {
                t[k][i] = (t[k - 1][i] >> 8) ^ t[0][(t[k - 1][i] & 0xff) as usize];
            }
        }
        t
    })
}

/// CRC-32C of `data` (same values as `format::prefix_crc`).
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_update(0, data)
}

/// Continues a CRC-32C (`crc32c(a ++ b) == crc32c_update(crc32c(a), b)`).
pub fn crc32c_update(crc: u32, data: &[u8]) -> u32 {
    let t = crc_tables();
    let mut c = !crc;
    let (chunks, rest) = data.as_chunks::<8>();
    for b in chunks {
        let lo = c ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][b[4] as usize]
            ^ t[2][b[5] as usize]
            ^ t[1][b[6] as usize]
            ^ t[0][b[7] as usize];
    }
    for &b in rest {
        c = (c >> 8) ^ t[0][((c ^ b as u32) & 0xff) as usize];
    }
    !c
}

// ----------------------------------------------------------------------
// Writer
// ----------------------------------------------------------------------

/// Byte budget shared by the writers of one result directory.
pub trait Budget: Send + Sync {
    /// Reserves `n` more bytes; an error stops the writer.
    fn reserve(&self, n: u64) -> Result<()>;
    /// Returns `n` reserved bytes (file deleted or never completed).
    fn release(&self, n: u64);
}

/// Streams items into `<path>.part`; `finish` writes the footer, syncs and
/// renames to `path`. Dropping an unfinished writer removes the part file.
pub struct ContainerWriter {
    path: PathBuf,
    part: PathBuf,
    out: Option<BufWriter<File>>,
    buf: Vec<u8>,
    buf_items: u32,
    codec: u8,
    chunks: u64,
    items: u64,
    raw_bytes: u64,
    written: u64,
    /// `None`: no fsync (temporary results)
    durability: Option<Durability>,
    budget: Option<std::sync::Arc<dyn Budget>>,
    reserved: u64,
    /// anonymous temporary file (no name, nothing to rename or remove)
    anonymous: bool,
    /// raw bytes per chunk the writer aims at
    chunk_target: usize,
}

/// Creates `<path>.part` with a shared lock already held (RD4-02): the file
/// is made under a hidden name, locked, and only then gets the name
/// `cleanup_results` looks at. The lock lives with the returned handle (and
/// stays across the final rename). Fails if `.part` exists.
fn create_locked_part(part: &Path) -> Result<File> {
    let name = part.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let init = platform::parent_dir(part).join(format!(".{name}.init"));
    let f = platform::create_new(&init, false).map_err(|e| match e {
        Error::Io(io) => Error::IoPath(io, part.into()),
        Error::Exists(_) => Error::Exists(format!(
            "[Errno 17] File exists: '{}' (leftover of an interrupted writer of {}; delete it)",
            init.display(),
            part.display()
        )),
        e => e,
    })?;
    let _ = platform::lock_owner_shared(&f);
    if let Err(e) = platform::rename_no_replace(&init, part) {
        let _ = std::fs::remove_file(&init);
        return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Exists(format!("[Errno 17] File exists: '{}'", part.display()))
        } else {
            Error::IoPath(e, part.into())
        });
    }
    Ok(f)
}

impl ContainerWriter {
    /// `meta`: a map stored in the header (e.g. the columns of a result).
    /// `durability`: `None` writes without fsync (temporary results).
    pub fn create(path: &Path, kind: &str, meta: &Val, compress: bool, durability: Option<Durability>, budget: Option<std::sync::Arc<dyn Budget>>) -> Result<ContainerWriter> {
        if kind.len() > u16::MAX as usize {
            return Err(Error::Value("container kind too long".into()));
        }
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let f = create_locked_part(&part)?;
        Self::start(path.to_path_buf(), part, f, kind, meta, compress, durability, budget, false, CHUNK_TARGET)
    }

    /// A container in an anonymous temporary file of `dir` (sort runs,
    /// RD4-01/RD4-03): no name another process or `cleanup_results` could
    /// see, nothing left after a crash. Read back with `finish_anonymous`.
    pub fn create_anonymous(dir: &Path, kind: &str, compress: bool, budget: Option<std::sync::Arc<dyn Budget>>) -> Result<ContainerWriter> {
        if kind.len() > u16::MAX as usize {
            return Err(Error::Value("container kind too long".into()));
        }
        std::fs::create_dir_all(dir).map_err(|e| Error::IoPath(e, dir.into()))?;
        let f = platform::anonymous_temp(dir, "nhydb-sortrun")?;
        // small chunks: a merge holds one decoded chunk per open run (RY-07)
        Self::start(dir.to_path_buf(), dir.to_path_buf(), f, kind, &Val::map(), compress, None, budget, true, RUN_CHUNK_TARGET)
    }

    #[allow(clippy::too_many_arguments)]
    fn start(path: PathBuf, part: PathBuf, f: File, kind: &str, meta: &Val, compress: bool, durability: Option<Durability>, budget: Option<std::sync::Arc<dyn Budget>>, anonymous: bool, chunk_target: usize) -> Result<ContainerWriter> {
        let mut w = ContainerWriter {
            path,
            part,
            anonymous,
            chunk_target,
            out: Some(BufWriter::with_capacity(chunk_target.min(1 << 20), f)),
            buf: Vec::with_capacity(chunk_target + 4096),
            buf_items: 0,
            codec: if compress { CODEC_ZLIB } else { CODEC_RAW },
            chunks: 0,
            items: 0,
            raw_bytes: 0,
            written: 0,
            durability,
            budget,
            reserved: 0,
        };
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        h.push(VERSION);
        h.push(0);
        h.extend_from_slice(&(kind.len() as u16).to_le_bytes());
        h.extend_from_slice(kind.as_bytes());
        let m = valcodec::encode_vec(meta);
        h.extend_from_slice(&(m.len() as u32).to_le_bytes());
        h.extend_from_slice(&m);
        let c = crc32c(&h);
        h.extend_from_slice(&c.to_le_bytes());
        w.write_raw(&h)?;
        Ok(w)
    }

    pub fn part_path(&self) -> &Path {
        &self.part
    }

    fn write_raw(&mut self, b: &[u8]) -> Result<()> {
        if let Some(bu) = &self.budget {
            bu.reserve(b.len() as u64)?;
            self.reserved += b.len() as u64;
        }
        self.out.as_mut().unwrap().write_all(b).map_err(|e| Error::IoPath(e, self.part.as_path().into()))?;
        self.written += b.len() as u64;
        Ok(())
    }

    /// Appends one encoded item (`valcodec` bytes for `nhydb.rows/1`).
    pub fn push_raw(&mut self, item: &[u8]) -> Result<()> {
        if item.len() > MAX_CHUNK_RAW {
            return Err(Error::Value("container item larger than a chunk".into()));
        }
        if self.buf.len() + item.len() > self.chunk_target && self.buf_items > 0 {
            self.flush_chunk()?;
        }
        self.buf.extend_from_slice(item);
        self.buf_items += 1;
        if self.buf.len() >= self.chunk_target {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Appends one value (encoded with `valcodec`).
    pub fn push(&mut self, v: &Val) -> Result<()> {
        let mut e = Vec::new();
        valcodec::encode(v, &mut e);
        self.push_raw(&e)
    }

    fn flush_chunk(&mut self) -> Result<()> {
        if self.buf_items == 0 {
            return Ok(());
        }
        let raw = std::mem::take(&mut self.buf);
        let stored = if self.codec == CODEC_ZLIB { crate::zlib::compress(&raw, 1) } else { raw.clone() };
        let mut h = Vec::with_capacity(20);
        h.extend_from_slice(CHUNK_TAG);
        h.push(self.codec);
        h.push(0);
        h.extend_from_slice(&self.buf_items.to_le_bytes());
        h.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        h.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        let hc = crc32c(&h);
        h.extend_from_slice(&hc.to_le_bytes());
        self.write_raw(&h)?;
        self.write_raw(&stored)?;
        self.write_raw(&crc32c(&stored).to_le_bytes())?;
        self.chunks += 1;
        self.items += self.buf_items as u64;
        self.raw_bytes += raw.len() as u64;
        self.buf_items = 0;
        self.buf = raw;
        self.buf.clear();
        Ok(())
    }

    pub fn items(&self) -> u64 {
        self.items + self.buf_items as u64
    }

    /// Writes footer and trailer, syncs, renames `<path>.part` to `path`.
    /// Returns the file size.
    pub fn finish(self, meta: &Val) -> Result<u64> {
        self.finish_locked(meta).map(|(_, n)| n)
    }

    fn write_footer(&mut self, meta: &Val) -> Result<()> {
        self.flush_chunk()?;
        let footer_off = self.written;
        let mut f = Vec::new();
        f.extend_from_slice(FOOTER_TAG);
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&self.chunks.to_le_bytes());
        f.extend_from_slice(&self.items.to_le_bytes());
        f.extend_from_slice(&self.raw_bytes.to_le_bytes());
        let m = valcodec::encode_vec(meta);
        f.extend_from_slice(&(m.len() as u32).to_le_bytes());
        f.extend_from_slice(&m);
        let c = crc32c(&f);
        f.extend_from_slice(&c.to_le_bytes());
        f.extend_from_slice(&footer_off.to_le_bytes());
        f.extend_from_slice(MAGIC);
        self.write_raw(&f)
    }

    /// `finish`, returning the file handle that still holds the writer's
    /// lock (RD4-02: the caller opens the result before it lets go; dropping
    /// the `OwnerLock` releases it). Any
    /// failure removes the part file (RD4-08); an existing `path` is never
    /// replaced, also not by a concurrent writer.
    pub fn finish_locked(mut self, meta: &Val) -> Result<(platform::OwnerLock, u64)> {
        self.write_footer(meta)?;
        let out = self.out.take().unwrap();
        let mut linked = false;
        let done = (|| -> Result<platform::OwnerLock> {
            // released explicitly on every path from here (`OwnerLock::drop`)
            let file = platform::OwnerLock::new(out.into_inner().map_err(|e| Error::IoPath(e.into_error(), self.part.as_path().into()))?);
            if let Some(d) = self.durability {
                platform::sync_file(file.file(), d)?;
            }
            platform::rename_no_replace(&self.part, &self.path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Error::Exists(format!("result file {} already exists", self.path.display()))
                } else {
                    Error::IoPath(e, self.path.as_path().into())
                }
            })?;
            linked = true;
            if let Some(d) = self.durability {
                platform::sync_dir(&platform::parent_dir(&self.path), d)?;
                platform::sync_renamed(&self.path, d)?;
            }
            Ok(file)
        })();
        match done {
            Ok(file) => {
                self.reserved = 0; // the budget keeps the bytes of the finished file
                Ok((file, self.written))
            }
            Err(e) => {
                let _ = std::fs::remove_file(&self.part);
                // RY-06: a target this writer already created is removed
                // again (its budget is released by `Drop`)
                if linked {
                    let _ = std::fs::remove_file(&self.path);
                }
                Err(e)
            }
        }
    }

    /// Completes an anonymous container and returns it positioned for
    /// reading with its size (the reserved bytes stay with the caller,
    /// who releases them when the file is dropped).
    pub fn finish_anonymous(mut self, meta: &Val) -> Result<(File, u64)> {
        self.write_footer(meta)?;
        let out = self.out.take().unwrap();
        let mut file = out.into_inner().map_err(|e| Error::IoPath(e.into_error(), self.part.as_path().into()))?;
        file.seek(SeekFrom::Start(0)).map_err(|e| Error::IoPath(e, self.part.as_path().into()))?;
        self.reserved = 0;
        Ok((file, self.written))
    }
}

impl Drop for ContainerWriter {
    fn drop(&mut self) {
        if let Some(out) = self.out.take() {
            if !self.anonymous {
                platform::unlock_owner(out.get_ref());
                drop(out);
                let _ = std::fs::remove_file(&self.part);
            }
        }
        if self.reserved > 0 {
            if let Some(b) = &self.budget {
                b.release(self.reserved);
            }
        }
    }
}

// ----------------------------------------------------------------------
// Reader
// ----------------------------------------------------------------------

/// Header and footer of a container (all CRCs checked on open).
#[derive(Clone, Debug)]
pub struct ContainerInfo {
    pub kind: String,
    pub header_meta: Val,
    pub footer_meta: Val,
    pub chunks: u64,
    pub items: u64,
    pub raw_bytes: u64,
    pub file_bytes: u64,
}

/// Sequential reader; every chunk is CRC-checked before its items are
/// returned, the totals are checked against the footer at the end.
pub struct ContainerReader {
    path: PathBuf,
    f: std::io::BufReader<File>,
    pos: u64,
    footer_off: u64,
    pub info: ContainerInfo,
    chunk: Vec<u8>,
    chunk_pos: usize,
    chunk_left: u32,
    seen_chunks: u64,
    seen_items: u64,
}

fn read_exact(f: &mut impl Read, n: usize, path: &Path) -> Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    f.read_exact(&mut b).map_err(|_| bad(path, "truncated"))?;
    Ok(b)
}

impl ContainerReader {
    pub fn open(path: &Path) -> Result<ContainerReader> {
        let file = File::open(path).map_err(|e| Error::IoPath(e, path.into()))?;
        Self::from_file(file, path, 1 << 20)
    }

    /// Reader over an open file (anonymous sort runs); `path` names it in
    /// messages, `buffer` is the read buffer size.
    pub fn from_file(file: File, path: &Path, buffer: usize) -> Result<ContainerReader> {
        let len = file.metadata().map_err(|e| Error::IoPath(e, path.into()))?.len();
        if len < 12 + TRAILER_LEN {
            return Err(bad(path, "too short"));
        }
        let mut f = std::io::BufReader::with_capacity(buffer, file);
        f.seek(SeekFrom::Start(0)).map_err(|e| Error::IoPath(e, path.into()))?;
        // header
        let fixed = read_exact(&mut f, 8, path)?;
        if &fixed[..4] != MAGIC {
            return Err(bad(path, "not a result container (magic)"));
        }
        if fixed[4] != VERSION {
            return Err(bad(path, format!("unsupported version {}", fixed[4])));
        }
        // RD4-09: the flags byte is reserved (0)
        if fixed[5] != 0 {
            return Err(bad(path, format!("unknown header flags {}", fixed[5])));
        }
        let kind_len = u16::from_le_bytes([fixed[6], fixed[7]]) as usize;
        let kind_b = read_exact(&mut f, kind_len, path)?;
        let ml = read_exact(&mut f, 4, path)?;
        let meta_len = u32::from_le_bytes(ml.clone().try_into().unwrap()) as u64;
        if meta_len > len {
            return Err(bad(path, "header length"));
        }
        let meta_b = read_exact(&mut f, meta_len as usize, path)?;
        let crc_b = read_exact(&mut f, 4, path)?;
        let mut h = fixed.clone();
        h.extend_from_slice(&kind_b);
        h.extend_from_slice(&ml);
        h.extend_from_slice(&meta_b);
        if crc32c(&h) != u32::from_le_bytes(crc_b.try_into().unwrap()) {
            return Err(bad(path, "header checksum mismatch"));
        }
        let kind = String::from_utf8(kind_b).map_err(|_| bad(path, "kind is not UTF-8"))?;
        let header_meta = valcodec::decode_all(&meta_b)?;
        let header_end = h.len() as u64 + 4;
        // trailer + footer (RD4-09: offsets checked without overflow; the
        // footer is read only in the size its own length field gives)
        let seek = |f: &mut std::io::BufReader<File>, o: u64| f.seek(SeekFrom::Start(o)).map(|_| ()).map_err(|e| Error::IoPath(e, path.into()));
        seek(&mut f, len - TRAILER_LEN)?;
        let tr = read_exact(&mut f, TRAILER_LEN as usize, path)?;
        if &tr[8..] != MAGIC {
            return Err(bad(path, "trailer missing (incomplete file)"));
        }
        let footer_off = u64::from_le_bytes(tr[..8].try_into().unwrap());
        if footer_off < header_end || footer_off.checked_add(36 + TRAILER_LEN).is_none_or(|e| e > len) {
            return Err(bad(path, "footer offset"));
        }
        seek(&mut f, footer_off)?;
        let head = read_exact(&mut f, 32, path)?;
        if &head[..2] != FOOTER_TAG {
            return Err(bad(path, "footer tag"));
        }
        if head[2..4] != [0, 0] {
            return Err(bad(path, "footer reserved bytes"));
        }
        let fm_len = u32::from_le_bytes(head[28..32].try_into().unwrap()) as u64;
        if footer_off + 32 + fm_len + 4 + TRAILER_LEN != len {
            return Err(bad(path, "footer length"));
        }
        let rest = read_exact(&mut f, fm_len as usize + 4, path)?;
        let mut body = head;
        body.extend_from_slice(&rest[..fm_len as usize]);
        if crc32c(&body) != u32::from_le_bytes(rest[fm_len as usize..].try_into().unwrap()) {
            return Err(bad(path, "footer checksum mismatch"));
        }
        let rd = |o: usize| u64::from_le_bytes(body[o..o + 8].try_into().unwrap());
        let (chunks, items, raw_bytes) = (rd(4), rd(12), rd(20));
        let footer_meta = valcodec::decode_all(&body[32..])?;
        seek(&mut f, header_end)?;
        Ok(ContainerReader {
            path: path.to_path_buf(),
            f,
            pos: header_end,
            footer_off,
            info: ContainerInfo { kind, header_meta, footer_meta, chunks, items, raw_bytes, file_bytes: len },
            chunk: Vec::new(),
            chunk_pos: 0,
            chunk_left: 0,
            seen_chunks: 0,
            seen_items: 0,
        })
    }

    fn next_chunk(&mut self) -> Result<bool> {
        if self.pos == self.footer_off {
            if self.seen_chunks != self.info.chunks || self.seen_items != self.info.items {
                return Err(bad(&self.path, "chunk or item count differs from the footer"));
            }
            return Ok(false);
        }
        if self.pos + 20 > self.footer_off {
            return Err(bad(&self.path, "chunk header beyond the footer"));
        }
        let h = read_exact(&mut self.f, 20, &self.path)?;
        if &h[..2] != CHUNK_TAG || h[3] != 0 {
            return Err(bad(&self.path, format!("chunk tag at byte {}", self.pos)));
        }
        if crc32c(&h[..16]) != u32::from_le_bytes(h[16..20].try_into().unwrap()) {
            return Err(bad(&self.path, format!("chunk header checksum mismatch at byte {}", self.pos)));
        }
        let codec = h[2];
        let items = u32::from_le_bytes(h[4..8].try_into().unwrap());
        let raw_len = u32::from_le_bytes(h[8..12].try_into().unwrap()) as usize;
        let stored_len = u32::from_le_bytes(h[12..16].try_into().unwrap()) as u64;
        if raw_len > MAX_CHUNK_RAW || self.pos + 20 + stored_len + 4 > self.footer_off || items == 0 {
            return Err(bad(&self.path, format!("chunk lengths at byte {}", self.pos)));
        }
        let stored = read_exact(&mut self.f, stored_len as usize, &self.path)?;
        let c = read_exact(&mut self.f, 4, &self.path)?;
        if crc32c(&stored) != u32::from_le_bytes(c.try_into().unwrap()) {
            return Err(bad(&self.path, format!("chunk checksum mismatch at byte {}", self.pos)));
        }
        let raw = match codec {
            CODEC_RAW => stored,
            CODEC_ZLIB => crate::zlib::decompress_with_limit(&stored, raw_len).map_err(|e| bad(&self.path, format!("chunk zlib: {e}")))?,
            _ => return Err(bad(&self.path, format!("unknown chunk codec {codec}"))),
        };
        if raw.len() != raw_len {
            return Err(bad(&self.path, "chunk raw length"));
        }
        self.pos += 20 + stored_len + 4;
        self.seen_chunks += 1;
        self.seen_items += items as u64;
        self.chunk = raw;
        self.chunk_pos = 0;
        self.chunk_left = items;
        Ok(true)
    }

    /// The next item as a value (`None`: end of the container).
    pub fn next_value(&mut self) -> Result<Option<Val>> {
        while self.chunk_left == 0 {
            if self.chunk_pos != self.chunk.len() {
                return Err(bad(&self.path, "chunk has bytes after its last item"));
            }
            if !self.next_chunk()? {
                return Ok(None);
            }
        }
        let v = valcodec::decode(&self.chunk, &mut self.chunk_pos)?;
        // RD4-13: an item of a query result is a row (map)
        if self.info.kind == KIND_ROWS && !matches!(v, Val::Map(_)) {
            return Err(bad(&self.path, "result row is not a map"));
        }
        self.chunk_left -= 1;
        if self.chunk_left == 0 && self.chunk_pos != self.chunk.len() {
            return Err(bad(&self.path, "chunk has bytes after its last item"));
        }
        Ok(Some(v))
    }

    /// Up to `n` items.
    pub fn next_values(&mut self, n: usize) -> Result<Vec<Val>> {
        let mut out = Vec::with_capacity(n.min(4096));
        while out.len() < n {
            match self.next_value()? {
                Some(v) => out.push(v),
                None => break,
            }
        }
        Ok(out)
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "container_tests.rs"]
mod tests;
