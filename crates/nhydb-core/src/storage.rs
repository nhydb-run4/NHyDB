//! File I/O: append buffer, positional reads, fsync.
//!
//! Behaves like a buffered `r+b` file object: appends first go into a buffer
//! (`pending`) and are written at the latest on commit. Reads also see bytes
//! not yet written (like Python's BufferedRandom) without flushing the
//! buffer.

use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::error::{Error, Result};
use crate::platform::{self, read_exact_at, write_all_at};

const FLUSH_THRESHOLD: usize = 8 << 20;

/// Durability mode (legacy name; see [`platform::Durability`]).
pub type SyncMode = platform::Durability;

pub struct Storage {
    file: File,
    pending: Vec<u8>,
    /// File offset of `pending[0]` == length of the written file.
    pending_start: u64,
    pub sync_mode: SyncMode,
}

impl Storage {
    pub fn open(path: &Path, sync_mode: SyncMode) -> Result<Storage> {
        sync_mode.check_supported()?;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(Storage { file, pending: Vec::new(), pending_start: len, sync_mode })
    }

    pub fn open_readonly(path: &Path) -> Result<File> {
        Ok(File::open(path)?)
    }

    /// Logical file length including unwritten bytes.
    #[inline]
    pub fn len(&self) -> u64 {
        self.pending_start + self.pending.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends bytes to the end of the file.
    pub fn append(&mut self, data: &[u8]) -> Result<()> {
        if self.pending.len() + data.len() > FLUSH_THRESHOLD {
            self.flush()?;
            if data.len() >= FLUSH_THRESHOLD {
                write_all_at(&self.file, data, self.pending_start)?;
                self.pending_start += data.len() as u64;
                return Ok(());
            }
        }
        self.pending.extend_from_slice(data);
        Ok(())
    }

    pub fn append_zeros(&mut self, n: usize) -> Result<()> {
        const Z: [u8; 8] = [0; 8];
        self.append(&Z[..n])
    }

    pub fn flush(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            write_all_at(&self.file, &self.pending, self.pending_start)?;
            self.pending_start += self.pending.len() as u64;
            self.pending.clear();
            if self.pending.capacity() > 2 * FLUSH_THRESHOLD {
                self.pending.shrink_to(FLUSH_THRESHOLD);
            }
        }
        Ok(())
    }

    pub fn fsync(&self) -> Result<()> {
        platform::sync_file(&self.file, self.sync_mode)
    }

    /// Positional write outside the append region (superblocks). The range
    /// must not overlap the append buffer.
    pub fn write_at(&mut self, off: u64, data: &[u8]) -> Result<()> {
        debug_assert!(off + data.len() as u64 <= self.pending_start);
        write_all_at(&self.file, data, off)?;
        Ok(())
    }

    /// Reads exactly `buf.len()` bytes from `off` (including unwritten bytes).
    pub fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        let end = off.saturating_add(buf.len() as u64);
        if end > self.len() {
            return Err(Error::Corruption(format!("read {off}..{end} beyond end of file {}", self.len())));
        }
        if off >= self.pending_start {
            let s = (off - self.pending_start) as usize;
            buf.copy_from_slice(&self.pending[s..s + buf.len()]);
        } else if end <= self.pending_start {
            read_exact_at(&self.file, buf, off)?;
        } else {
            let split = (self.pending_start - off) as usize;
            read_exact_at(&self.file, &mut buf[..split], off)?;
            let rest = buf.len() - split;
            buf[split..].copy_from_slice(&self.pending[..rest]);
        }
        Ok(())
    }

    pub fn read_vec(&self, off: u64, len: usize) -> Result<Vec<u8>> {
        match off.checked_add(len as u64) {
            Some(end) if end <= self.len() => {}
            _ => return Err(Error::Corruption(format!("read at {off} (length {len}) beyond end of file {}", self.len()))),
        }
        let mut v = vec![0u8; len];
        self.read_exact_at(off, &mut v)?;
        Ok(v)
    }

    /// Reads up to `len` bytes (fewer at end of file).
    pub fn read_upto(&self, off: u64, len: usize) -> Result<Vec<u8>> {
        let avail = self.len().saturating_sub(off).min(len as u64) as usize;
        self.read_vec(off, avail)
    }

    /// Discards all bytes from `len` on (rollback of a failed append).
    pub fn rollback_to(&mut self, len: u64) -> Result<()> {
        if len >= self.pending_start {
            self.pending.truncate((len - self.pending_start) as usize);
            Ok(())
        } else {
            self.pending.clear();
            self.file.set_len(len)?;
            self.pending_start = len;
            Ok(())
        }
    }

    pub fn truncate(&mut self, len: u64) -> Result<()> {
        self.flush()?;
        self.file.set_len(len)?;
        self.pending_start = len;
        Ok(())
    }
}
