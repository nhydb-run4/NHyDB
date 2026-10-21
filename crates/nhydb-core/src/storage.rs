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
    /// `None` after [`Storage::close`]: the OS handle is released so that the
    /// file can be replaced, renamed or deleted (required on Windows, where
    /// an open handle blocks `MoveFileExW` with "Access is denied").
    file: Option<File>,
    pending: Vec<u8>,
    /// File offset of `pending[0]` == length of the written file.
    pending_start: u64,
    pub sync_mode: SyncMode,
    /// Opened read-only (`O_RDONLY`): every write fails, fsync is a no-op.
    readonly: bool,
    /// Test-only fault injection: writes beyond this file offset fail after
    /// writing the part below it (emulates a file size limit / full disk).
    #[cfg(test)]
    pub(crate) fail_writes_beyond: Option<u64>,
    /// Test-only fault injection: every fsync/barrier fails with EIO.
    #[cfg(test)]
    pub(crate) fail_fsync: bool,
}

#[cfg(test)]
thread_local! {
    /// Test-only: `fail_writes_beyond` for every `Storage` opened afterwards on
    /// this thread (fault injection into internally opened files, e.g. the
    /// compaction target).
    pub(crate) static FAIL_NEW_WRITES_BEYOND: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Test-only (T017): positional file reads and bytes read on this thread
    /// (complexity tests count I/O instead of measuring wall-clock time).
    pub(crate) static FILE_READS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
fn count_read(len: usize) {
    FILE_READS.with(|c| {
        let (n, b) = c.get();
        c.set((n + 1, b + len as u64));
    });
}

fn read_only_error() -> Error {
    Error::NHyDB("database is opened read-only (readonly=True); writing is not possible".into())
}

impl Storage {
    pub fn open(path: &Path, sync_mode: SyncMode) -> Result<Storage> {
        Self::open_mode(path, sync_mode, false)
    }

    /// Opens read-write or (`readonly`) with `O_RDONLY`; a read-only
    /// storage never changes the file.
    pub fn open_mode(path: &Path, sync_mode: SyncMode, readonly: bool) -> Result<Storage> {
        sync_mode.check_supported()?;
        let file = OpenOptions::new().read(true).write(!readonly).open(path)?;
        let len = file.metadata()?.len();
        Ok(Storage {
            file: Some(file),
            pending: Vec::new(),
            pending_start: len,
            sync_mode,
            readonly,
            #[cfg(test)]
            fail_writes_beyond: FAIL_NEW_WRITES_BEYOND.with(|c| c.get()),
            #[cfg(test)]
            fail_fsync: false,
        })
    }

    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// Read-only view: limits the logical file length to `len` (bytes behind
    /// the commit boundary stay invisible without truncating the file).
    pub fn limit_view(&mut self, len: u64) {
        debug_assert!(self.readonly && self.pending.is_empty());
        if len < self.pending_start {
            self.pending_start = len;
        }
    }

    /// Physical length of the underlying file (ignores the read-only view).
    pub fn file_len(&self) -> Result<u64> {
        Ok(self.file()?.metadata()?.len())
    }

    /// Read-only view: moves the logical end forward to `len` (bytes a
    /// concurrent writer has committed since the open, N1). `len` must lie
    /// inside the file.
    pub fn extend_view(&mut self, len: u64) -> Result<()> {
        if !self.readonly || !self.pending.is_empty() {
            return Err(Error::NHyDB("extend_view requires a read-only storage".into()));
        }
        let physical = self.file_len()?;
        if len > physical {
            return Err(Error::Corruption(format!("commit boundary {len} lies outside the file ({physical} bytes)")));
        }
        self.pending_start = self.pending_start.max(len);
        Ok(())
    }

    fn file(&self) -> Result<&File> {
        self.file.as_ref().ok_or_else(|| Error::NHyDB("database is closed".into()))
    }

    /// Positional write of the underlying file (single point for all writes).
    fn write_file_at(&self, data: &[u8], off: u64) -> Result<()> {
        if self.readonly {
            return Err(read_only_error());
        }
        #[cfg(test)]
        if let Some(limit) = self.fail_writes_beyond {
            if off + data.len() as u64 > limit {
                let n = limit.saturating_sub(off).min(data.len() as u64) as usize;
                write_all_at(self.file()?, &data[..n], off)?;
                return Err(Error::Io(std::io::Error::other("injected write error (file size limit)")));
            }
        }
        write_all_at(self.file()?, data, off)?;
        Ok(())
    }

    /// Writes the buffer and releases the OS file handle, also if that
    /// write fails (like closing a Python file object); the error is still
    /// returned. Further accesses fail with "database is closed". Idempotent.
    pub fn close(&mut self) -> Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        let res = self.flush();
        self.file = None;
        self.pending = Vec::new();
        res
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
        self.file()?;
        if self.readonly {
            return Err(read_only_error());
        }
        if self.pending.len() + data.len() > FLUSH_THRESHOLD {
            self.flush()?;
            if data.len() >= FLUSH_THRESHOLD {
                self.write_file_at(data, self.pending_start)?;
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
            self.write_file_at(&self.pending, self.pending_start)?;
            self.pending_start += self.pending.len() as u64;
            self.pending.clear();
            if self.pending.capacity() > 2 * FLUSH_THRESHOLD {
                self.pending.shrink_to(FLUSH_THRESHOLD);
            }
        }
        Ok(())
    }

    pub fn fsync(&self) -> Result<()> {
        let f = self.file()?;
        if self.readonly {
            return Ok(());
        }
        #[cfg(test)]
        if self.fail_fsync {
            return Err(Error::Io(std::io::Error::other("injected fsync error (EIO)")));
        }
        platform::sync_file(f, self.sync_mode)
    }

    /// Synchronization before the superblock write of a commit (ordering
    /// barrier, see [`platform::barrier_sync`]).
    pub fn fsync_barrier(&self) -> Result<bool> {
        let f = self.file()?;
        if self.readonly {
            return Ok(false);
        }
        #[cfg(test)]
        if self.fail_fsync {
            return Err(Error::Io(std::io::Error::other("injected fsync error (EIO)")));
        }
        platform::barrier_sync(f, self.sync_mode)
    }

    /// Positional write outside the append region (superblocks). The range
    /// must not overlap the append buffer.
    pub fn write_at(&mut self, off: u64, data: &[u8]) -> Result<()> {
        debug_assert!(off + data.len() as u64 <= self.pending_start);
        self.write_file_at(data, off)
    }

    /// Reads exactly `buf.len()` bytes from `off` (including unwritten bytes).
    pub fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.file()?;
        let end = off.saturating_add(buf.len() as u64);
        if end > self.len() {
            return Err(Error::Corruption(format!("read {off}..{end} beyond end of file {}", self.len())));
        }
        if off >= self.pending_start {
            let s = (off - self.pending_start) as usize;
            buf.copy_from_slice(&self.pending[s..s + buf.len()]);
        } else if end <= self.pending_start {
            read_exact_at(self.file()?, buf, off)?;
            #[cfg(test)]
            count_read(buf.len());
        } else {
            let split = (self.pending_start - off) as usize;
            read_exact_at(self.file()?, &mut buf[..split], off)?;
            #[cfg(test)]
            count_read(split);
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

    /// Drops the unwritten append buffer without touching the file (G2-1:
    /// blocks of a `lock=False` handle that may never reach a file another
    /// writer has extended meanwhile).
    pub fn discard_pending(&mut self) {
        self.pending.clear();
    }

    /// Discards all bytes from `len` on (rollback of a failed append).
    pub fn rollback_to(&mut self, len: u64) -> Result<()> {
        self.file()?;
        if self.readonly {
            return Err(read_only_error());
        }
        if len >= self.pending_start {
            self.pending.truncate((len - self.pending_start) as usize);
            Ok(())
        } else {
            self.pending.clear();
            self.file()?.set_len(len)?;
            self.pending_start = len;
            Ok(())
        }
    }

    pub fn truncate(&mut self, len: u64) -> Result<()> {
        self.file()?;
        if self.readonly {
            return Err(read_only_error());
        }
        self.flush()?;
        self.file()?.set_len(len)?;
        self.pending_start = len;
        Ok(())
    }
}
