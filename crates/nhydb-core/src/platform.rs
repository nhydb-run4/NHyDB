//! Portable operating-system interface.
//!
//! The only place with OS-specific code: positional read/write, durability
//! (`fsync`/`full`), directory sync, atomic file replacement and file locks.
//! Target platforms: macOS, Linux, Windows.
//!
//! System calls used by the durability modes:
//!
//! | Mode | macOS | Linux | Windows |
//! |---|---|---|---|
//! | `fsync` | `fsync(2)` | `fsync(2)` | `FlushFileBuffers` |
//! | `full`  | `fcntl(F_FULLFSYNC)` | `fsync(2)` (relies on the kernel flushing the device cache) | `FlushFileBuffers` (flushes the device cache) |
//!
//! If `full` is requested on a platform without a known cache-flushing call,
//! or `F_FULLFSYNC` fails on macOS, an error is returned instead of silently
//! falling back to plain `fsync(2)`. On Linux and Windows `full` issues the
//! same call as `fsync`. On Windows, directory sync is a no-op in both modes
//! (see [`sync_dir`]).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Durability mode for commit/sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Durability {
    /// Plain OS synchronization (`fsync(2)`; `FlushFileBuffers` on Windows).
    #[default]
    Fsync,
    /// Additionally flush the drive cache (strongest guarantee).
    Full,
}

impl Durability {
    pub fn parse(s: &str) -> Result<Durability> {
        match s.to_ascii_lowercase().as_str() {
            "fsync" | "default" | "normal" => Ok(Durability::Fsync),
            "full" | "fullfsync" => Ok(Durability::Full),
            _ => Err(Error::Value("durability must be 'fsync' or 'full'".into())),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Durability::Fsync => "fsync",
            Durability::Full => "full",
        }
    }
    /// Checks whether the platform can guarantee the mode.
    pub fn check_supported(self) -> Result<()> {
        if self == Durability::Full && !FULL_SUPPORTED {
            return Err(Error::Value(format!(
                "durability='full' is not guaranteed on this platform ({})",
                std::env::consts::OS
            )));
        }
        Ok(())
    }
}

const FULL_SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "ios", target_os = "linux", target_os = "android", windows));

// ----------------------------------------------------------------------
// Positional I/O
// ----------------------------------------------------------------------

#[cfg(unix)]
pub fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(unix)]
pub fn write_all_at(f: &File, buf: &[u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
pub fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, off) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "end of file reached")),
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn write_all_at(f: &File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_write(buf, off) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "write failed")),
            Ok(n) => {
                buf = &buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Durability
// ----------------------------------------------------------------------

/// Synchronizes a file's data and metadata according to `mode`.
pub fn sync_file(f: &File, mode: Durability) -> Result<()> {
    match mode {
        Durability::Fsync => plain_sync(f),
        Durability::Full => full_sync(f),
    }
}

/// Calls `fsync(2)` directly. Note: Rust's `File::sync_all` uses
/// `F_FULLFSYNC` on Apple platforms and would therefore already be mode `full`.
#[cfg(unix)]
fn plain_sync(f: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::fsync(f.as_raw_fd()) } != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(windows)]
fn plain_sync(f: &File) -> Result<()> {
    f.sync_all()?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn full_sync(f: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
    if rc == -1 {
        let e = std::io::Error::last_os_error();
        return Err(Error::Io(std::io::Error::new(
            e.kind(),
            format!("F_FULLFSYNC failed (durability='full' not guaranteed on this file system): {e}"),
        )));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android", windows))]
fn full_sync(f: &File) -> Result<()> {
    // Linux: per the kernel contract, fsync(2) also flushes the volatile
    // device cache (write barrier/FUA). Windows: FlushFileBuffers likewise.
    f.sync_all()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux", target_os = "android", windows)))]
fn full_sync(_f: &File) -> Result<()> {
    Err(Error::Value("durability='full' is not guaranteed on this platform".into()))
}

/// Makes directory entries (create/rename) durable.
#[cfg(unix)]
pub fn sync_dir(dir: &Path, mode: Durability) -> Result<()> {
    let d = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let f = File::open(d)?;
    sync_file(&f, mode)
}

/// Windows: no-op for both modes. Directory metadata is protected by the
/// NTFS journal; renames use `MOVEFILE_WRITE_THROUGH` (see
/// [`atomic_replace`]).
#[cfg(windows)]
pub fn sync_dir(_dir: &Path, _mode: Durability) -> Result<()> {
    Ok(())
}

pub fn parent_dir(p: &Path) -> PathBuf {
    match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Atomically replaces `dst` with `src` (same directory/file system) and
/// makes the replacement durable.
#[cfg(unix)]
pub fn atomic_replace(src: &Path, dst: &Path, mode: Durability) -> Result<()> {
    std::fs::rename(src, dst)?;
    sync_dir(&parent_dir(dst), mode)
}

#[cfg(windows)]
pub fn atomic_replace(src: &Path, dst: &Path, _mode: Durability) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let w = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(std::iter::once(0)).collect() };
    let (s, d) = (w(src), w(dst));
    let ok = unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Restricts the permissions of a secret file to "owner only".
#[cfg(unix)]
pub fn restrict_to_owner(f: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Windows: files in the user profile inherit the owner ACL; no additional
/// restriction is applied with standard means.
#[cfg(windows)]
pub fn restrict_to_owner(_f: &File) -> Result<()> {
    Ok(())
}

/// Creates a new file exclusively (error if it exists); with owner-only
/// permissions if `secret`.
pub fn create_new(path: &Path, secret: bool) -> Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let f = o.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Exists(format!("[Errno 17] File exists: '{}'", path.display()))
        } else {
            Error::Io(e)
        }
    })?;
    if secret {
        restrict_to_owner(&f)?;
    }
    Ok(f)
}

// ----------------------------------------------------------------------
// Local time (backup names, format like Python's `time.strftime("%Y%m%d_%H%M%S")`)
// ----------------------------------------------------------------------

#[cfg(unix)]
pub fn local_timestamp() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(windows)]
pub fn local_timestamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!("{:04}{:02}{:02}_{:02}{:02}{:02}", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond)
}

// ----------------------------------------------------------------------
// File locks
// ----------------------------------------------------------------------

/// Locking behaviour (from `nhydb_config.toml`, section `[lock]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockConfig {
    pub enabled: bool,
    /// 0 = fail immediately, otherwise maximum wait time.
    pub timeout_ms: u64,
}

impl Default for LockConfig {
    fn default() -> Self {
        LockConfig { enabled: true, timeout_ms: 0 }
    }
}

/// Held process lock on `<db>.lock`. The lock file lives next to the DB and
/// is never removed, so the lock stays valid across an atomic replacement of
/// the DB file. Released on drop.
#[derive(Debug)]
pub struct DbLock {
    file: Option<File>,
    pub path: PathBuf,
}

impl DbLock {
    pub fn lock_path(db_path: &Path) -> PathBuf {
        let mut s = db_path.as_os_str().to_owned();
        s.push(".lock");
        PathBuf::from(s)
    }

    /// No lock (disabled or already held internally).
    pub fn none(db_path: &Path) -> DbLock {
        DbLock { file: None, path: Self::lock_path(db_path) }
    }

    pub fn is_held(&self) -> bool {
        self.file.is_some()
    }

    /// Acquires an exclusive write lock according to `cfg`.
    pub fn acquire(db_path: &Path, cfg: LockConfig) -> Result<DbLock> {
        if !cfg.enabled {
            return Ok(Self::none(db_path));
        }
        let path = Self::lock_path(db_path);
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
        let deadline = Instant::now() + Duration::from_millis(cfg.timeout_ms);
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(DbLock { file: Some(f), path }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(Error::Locked(format!(
                            "database is already being written by another process (lock {} not acquired after {} ms)",
                            path.display(),
                            cfg.timeout_ms
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(Error::Io(e)),
            }
        }
    }

    pub fn release(&mut self) {
        if let Some(f) = self.file.take() {
            let _ = f.unlock();
        }
    }
}

impl Drop for DbLock {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nhydb_platform_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lock_is_exclusive_and_released() {
        let d = tmpdir("lock");
        let db = d.join("x.nhydb");
        let a = DbLock::acquire(&db, LockConfig::default()).unwrap();
        assert!(a.is_held());
        let e = DbLock::acquire(&db, LockConfig { enabled: true, timeout_ms: 30 }).unwrap_err();
        assert!(matches!(e, Error::Locked(_)), "{e}");
        drop(a);
        let b = DbLock::acquire(&db, LockConfig::default()).unwrap();
        assert!(b.is_held());
        let off = DbLock::acquire(&db, LockConfig { enabled: false, timeout_ms: 0 }).unwrap();
        assert!(!off.is_held());
    }

    #[test]
    fn lock_survives_atomic_replace() {
        let d = tmpdir("replace");
        let db = d.join("x.nhydb");
        std::fs::write(&db, b"alt").unwrap();
        let l = DbLock::acquire(&db, LockConfig::default()).unwrap();
        let tmp = d.join("x.nhydb.compacting");
        std::fs::write(&tmp, b"neu").unwrap();
        atomic_replace(&tmp, &db, Durability::Fsync).unwrap();
        assert_eq!(std::fs::read(&db).unwrap(), b"neu");
        assert!(DbLock::acquire(&db, LockConfig::default()).is_err());
        drop(l);
    }

    #[test]
    fn positional_io_and_sync() {
        let d = tmpdir("io");
        let p = d.join("f");
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        write_all_at(&f, b"hello world", 0).unwrap();
        write_all_at(&f, b"W", 6).unwrap();
        let mut b = [0u8; 11];
        read_exact_at(&f, &mut b, 0).unwrap();
        assert_eq!(&b, b"hello World");
        sync_file(&f, Durability::Fsync).unwrap();
        Durability::Full.check_supported().unwrap();
        sync_file(&f, Durability::Full).unwrap();
        sync_dir(&d, Durability::Full).unwrap();
        let s = create_new(&d.join("secret"), true).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(s.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        drop(s);
        assert!(matches!(create_new(&d.join("secret"), true), Err(Error::Exists(_))));
    }
}
