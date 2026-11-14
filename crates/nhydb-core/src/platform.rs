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

/// Ordering point before a superblock write (commit protocol).
///
/// Apple platforms, both modes: `fcntl(F_BARRIERFSYNC)` – like `fsync(2)`,
/// additionally an I/O barrier: all writes issued before are persisted before
/// any write issued afterwards (the superblock). Much cheaper than
/// `F_FULLFSYNC`. If the file system does not support the barrier
/// (`EINVAL`/`ENOTSUP`/`ENOTTY`), [`sync_file`] with the same mode is used
/// (`fsync(2)` or `F_FULLFSYNC`). Other platforms: like [`sync_file`].
/// Returns whether a barrier was issued.
///
/// P023, mode `full`: the commit is data → barrier → superblock →
/// `F_FULLFSYNC`. The barrier orders the data before the superblock (a
/// power loss can never persist the new superblock without the data it
/// points to), the final `F_FULLFSYNC` flushes the drive cache – data and
/// superblock – before `commit()` returns. Same guarantees as two
/// `F_FULLFSYNC`, one cache flush less.
pub fn barrier_sync(f: &File, mode: Durability) -> Result<bool> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_BARRIERFSYNC) };
        if rc != -1 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINVAL) | Some(libc::ENOTSUP) | Some(libc::ENOTTY) => {}
            _ => return Err(Error::Io(e)),
        }
    }
    sync_file(f, mode)?;
    Ok(false)
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

/// Windows: no-op for both modes (a directory cannot be flushed with
/// standard rights). Directory entries are NTFS metadata of the file they
/// name and are journaled: a file created and then flushed with
/// [`sync_file`] is durable together with its entry (the flush commits the
/// log up to the file's last change). Renames are made durable by
/// [`atomic_replace`], which flushes the target after the rename.
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

/// Windows: `std::fs::rename` replaces an existing target with POSIX
/// semantics where available (a target still open in another handle, e.g. a
/// read-only reader during compaction, can be replaced; the standard library
/// adds the `\\?\\` prefix for long paths itself). `MoveFileExW` with
/// `MOVEFILE_WRITE_THROUGH` is not used: it has the classic semantics that
/// refuse such a replacement, and its write-through only concerns moves done
/// as copy and delete. Durability (R005): after the rename the target is
/// flushed in the requested mode – the rename is a journaled change of the
/// target's NTFS metadata, and `FlushFileBuffers` commits the log at least up
/// to it. Virus scanners or indexing services may hold the file briefly
/// (sharing violation / access denied): both steps retry a few times with a
/// short backoff.
#[cfg(windows)]
pub fn atomic_replace(src: &Path, dst: &Path, mode: Durability) -> Result<()> {
    retry_busy(|| std::fs::rename(src, dst))?;
    sync_renamed(dst, mode)
}

/// Renames `src` to `dst` only if `dst` does not exist, atomically (RD4-08:
/// no check-then-rename race): a hard link `dst` is created (fails if it
/// exists), then the name `src` is removed – the file, its open handles and
/// their locks stay the same. Where hard links are not supported the
/// existence check followed by a rename is the fallback.
pub fn rename_no_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(src, dst) {
        Ok(()) => std::fs::remove_file(src),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if dst.exists() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            std::fs::rename(src, dst)
        }
    }
}

/// Whether a process with this id runs (Unix: `kill(pid, 0)`; elsewhere
/// always assumed, so nothing of a foreign process is taken away).
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else { return true };
    // SAFETY: signal 0 only checks for existence and permission
    let r = unsafe { libc::kill(p, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
pub fn process_alive(_pid: u32) -> bool {
    true
}

/// Makes a completed rename to `dst` durable. Unix: nothing to do here (the
/// caller syncs the directory with [`sync_dir`]). Windows: flushes `dst`
/// (see [`atomic_replace`]).
#[cfg(unix)]
pub fn sync_renamed(_dst: &Path, _mode: Durability) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn sync_renamed(dst: &Path, mode: Durability) -> Result<()> {
    let f = retry_busy(|| std::fs::OpenOptions::new().write(true).open(dst))?;
    sync_file(&f, mode)
}

/// Runs `op` up to 8 times while it fails with `ERROR_ACCESS_DENIED` (5) or
/// `ERROR_SHARING_VIOLATION` (32), backoff 10 → 200 ms.
#[cfg(windows)]
fn retry_busy<T>(mut op: impl FnMut() -> std::io::Result<T>) -> Result<T> {
    let mut delay = 10u64;
    let mut attempt = 0;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if attempt < 7 && matches!(e.raw_os_error(), Some(5) | Some(32)) => {
                std::thread::sleep(Duration::from_millis(delay));
                delay = (delay * 2).min(200);
                attempt += 1;
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
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

/// Free bytes available to this user on the file system of `dir`
/// (`None` where it cannot be determined).
#[cfg(unix)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path, `st` a writable struct
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(windows)]
pub fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// Creates an anonymous temporary file in `dir` (read/write, owner only):
/// the name is removed right after creation (Unix: `unlink`; Windows: POSIX
/// delete semantics, NTFS on Windows 10 1709+). Where Windows cannot do that,
/// `FILE_FLAG_DELETE_ON_CLOSE` deletes the file when it is closed. Nothing is
/// left behind after a crash or `kill -9`, and no other process can open it
/// by name (RB-02/RB-03, RD4-01).
pub fn anonymous_temp(dir: &Path, stem: &str) -> Result<File> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut last = None;
    for _ in 0..16 {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let path = dir.join(format!(".{stem}_{}_{n}_{t:08x}.tmp", std::process::id()));
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_DELETE_ON_CLOSE; share read/write/delete
            o.custom_flags(0x0400_0000).share_mode(0x7);
        }
        match o.open(&path) {
            Ok(f) => {
                #[cfg(unix)]
                std::fs::remove_file(&path).map_err(|e| Error::IoPath(e, path.as_path().into()))?;
                #[cfg(windows)]
                unlink_open(&path);
                return Ok(f);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(Error::IoPath(e, path.into_boxed_path())),
            Err(e) => return Err(Error::IoPath(e, path.into_boxed_path())),
        }
    }
    Err(last.unwrap_or_else(|| Error::Value("no temporary file name available".into())))
}

/// Windows: removes the name of an open file at once (POSIX delete
/// semantics). Windows unlinks a POSIX-deleted file when the handle that
/// set the disposition is closed, so a second handle (DELETE access only)
/// sets it and is closed right away; the caller's handle stays usable and
/// the name cannot be opened any more (measured on NTFS, Windows 11).
/// Returns false where the file system or Windows version cannot (the file
/// is then deleted when the caller's handle is closed).
#[cfg(windows)]
fn unlink_open(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfoEx, SetFileInformationByHandle, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX,
    };
    // DELETE access only; share read/write/delete with the caller's handle
    let Ok(h) = std::fs::OpenOptions::new().access_mode(0x0001_0000).share_mode(0x7).open(path) else {
        return false;
    };
    let info = FILE_DISPOSITION_INFO_EX { Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS };
    // SAFETY: valid handle `h`; `info` lives across the call, size matches
    let ok = unsafe {
        SetFileInformationByHandle(
            h.as_raw_handle() as _,
            FileDispositionInfoEx,
            (&info as *const FILE_DISPOSITION_INFO_EX).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        ) != 0
    };
    drop(h); // the name goes now
    ok
}

/// Copies `src` to the new file `dst` and makes the copy durable (file and
/// directory synchronized) – used for backups before in-place changes.
/// OS errors name the affected file (`Error::IoPath`, F048): `src` for
/// opening it, `dst` for everything else.
pub fn durable_copy(src: &Path, dst: &Path, mode: Durability) -> Result<()> {
    let mut input = File::open(src).map_err(|e| Error::IoPath(e, src.into()))?;
    let mut out = create_new(dst, false).map_err(|e| e.at_path(dst))?;
    let res = (|| -> Result<()> {
        std::io::copy(&mut input, &mut out)?;
        sync_file(&out, mode)?;
        Ok(())
    })()
    .map_err(|e| e.at_path(dst));
    drop(out);
    if res.is_err() {
        let _ = std::fs::remove_file(dst);
        return res;
    }
    sync_dir(&parent_dir(dst), mode).map_err(|e| e.at_path(dst))
}

/// Anchors a path absolutely (relative paths against the current working
/// directory, without resolving symlinks). Empty paths are rejected.
pub fn absolute(p: &Path) -> Result<PathBuf> {
    if p.as_os_str().is_empty() {
        return Err(Error::Value("path must not be empty".into()));
    }
    Ok(std::path::absolute(p)?)
}

/// Whether the directory entry through which `path` is reached is (also)
/// called `name` on disk. Case-insensitive or normalization-insensitive file
/// systems (macOS APFS/HFS+, Windows NTFS) find a file under a spelling that
/// differs from the stored name (`case.nhydb` for `Case.nhydb`, NFD for
/// NFC); this checks the name actually stored in the directory.
pub fn stored_name_is(path: &Path, name: &str) -> bool {
    if path.file_name().map(|n| n.to_string_lossy() == name).unwrap_or(false) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(md) = std::fs::metadata(path) else { return false };
        let Ok(rd) = std::fs::read_dir(parent_dir(path)) else { return false };
        for e in rd.flatten() {
            if e.file_name().to_string_lossy() == name {
                return std::fs::metadata(e.path()).map(|m| m.dev() == md.dev() && m.ino() == md.ino()).unwrap_or(false);
            }
        }
        false
    }
    #[cfg(not(unix))]
    {
        std::fs::canonicalize(path)
            .ok()
            .and_then(|c| c.file_name().map(|n| n.to_string_lossy() == name))
            .unwrap_or(false)
    }
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

// Owner locks of managed result files (RD4-02). Owners (writers, open
// results) hold a shared lock, `cleanup_results` removes a file only while
// it holds the exclusive lock. On Unix these are `flock` locks over the
// whole file. Windows byte-range locks are mandatory: a shared lock over
// the data forbids writes through every handle, the owner's own included
// (ERROR_LOCK_VIOLATION, P-01). There the owner locks one byte far beyond
// any real file size; writes never touch it, while the exclusive lock of
// `cleanup_results` (whole range) still collides with it.

/// Windows: offset of the owner's lock byte (beyond every real file size).
#[cfg(windows)]
pub const OWNER_LOCK_OFFSET: u64 = (1 << 63) - 1;

/// Takes the owner's shared lock without waiting.
#[cfg(not(windows))]
pub fn lock_owner_shared(f: &File) -> std::result::Result<(), std::fs::TryLockError> {
    f.try_lock_shared()
}

#[cfg(windows)]
pub fn lock_owner_shared(f: &File) -> std::result::Result<(), std::fs::TryLockError> {
    lock_range(f, false, OWNER_LOCK_OFFSET, 1)
}

/// Releases the owner's lock explicitly (errors ignored: no lock held, or
/// a file system without locks).
#[cfg(not(windows))]
pub fn unlock_owner(f: &File) {
    let _ = f.unlock();
}

#[cfg(windows)]
pub fn unlock_owner(f: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    let mut ov = overlapped_at(OWNER_LOCK_OFFSET);
    // SAFETY: valid handle of `f`, `ov` lives across the synchronous call
    unsafe { UnlockFileEx(f.as_raw_handle() as _, 0, 1, 0, &mut ov) };
}

/// Exclusive lock over the whole file without waiting (`cleanup_results`);
/// collides with every owner lock. Released when `f` is closed.
#[cfg(not(windows))]
pub fn try_lock_exclusive_all(f: &File) -> std::result::Result<(), std::fs::TryLockError> {
    f.try_lock()
}

#[cfg(windows)]
pub fn try_lock_exclusive_all(f: &File) -> std::result::Result<(), std::fs::TryLockError> {
    lock_range(f, true, 0, u64::MAX)
}

#[cfg(windows)]
fn overlapped_at(off: u64) -> windows_sys::Win32::System::IO::OVERLAPPED {
    // SAFETY: OVERLAPPED is plain data, all-zero is its documented start state
    let mut ov: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.Anonymous.Anonymous.Offset = off as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (off >> 32) as u32;
    ov
}

#[cfg(windows)]
fn lock_range(f: &File, exclusive: bool, off: u64, len: u64) -> std::result::Result<(), std::fs::TryLockError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_LOCK_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY};
    let mut ov = overlapped_at(off);
    let flags = LOCKFILE_FAIL_IMMEDIATELY | if exclusive { LOCKFILE_EXCLUSIVE_LOCK } else { 0 };
    // SAFETY: valid handle of `f` (synchronous), `ov` lives across the call
    let ok = unsafe { LockFileEx(f.as_raw_handle() as _, flags, 0, len as u32, (len >> 32) as u32, &mut ov) };
    if ok != 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(c) if c == ERROR_LOCK_VIOLATION as i32 || c == ERROR_IO_PENDING as i32 => Err(std::fs::TryLockError::WouldBlock),
        _ => Err(std::fs::TryLockError::Error(e)),
    }
}

/// A file handle holding an owner lock; the lock is released explicitly
/// when it is dropped (not left to closing the handle).
#[derive(Debug)]
pub struct OwnerLock(File);

impl OwnerLock {
    pub fn new(f: File) -> OwnerLock {
        OwnerLock(f)
    }
    pub fn file(&self) -> &File {
        &self.0
    }
}

impl Drop for OwnerLock {
    fn drop(&mut self) {
        unlock_owner(&self.0);
    }
}

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

/// Partial override of the `[lock]` configuration by API parameters
/// (`lock=`, `lock_timeout_ms=`): only the given fields replace the
/// configured values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LockOverride {
    pub enabled: Option<bool>,
    pub timeout_ms: Option<u64>,
}

impl LockOverride {
    pub fn apply(self, base: LockConfig) -> LockConfig {
        LockConfig { enabled: self.enabled.unwrap_or(base.enabled), timeout_ms: self.timeout_ms.unwrap_or(base.timeout_ms) }
    }
}

/// Held process lock on `<db>.lock`. The lock file lives next to the DB and
/// is never removed, so the lock stays valid across an atomic replacement of
/// the DB file. Released on drop.
#[derive(Debug)]
pub struct DbLock {
    file: Option<File>,
    pub path: PathBuf,
    /// The lock is held by the caller (internal reopen during compaction,
    /// repair, create) – the handle may modify the file like a locked one.
    held_elsewhere: bool,
}

impl DbLock {
    pub fn lock_path(db_path: &Path) -> PathBuf {
        let mut s = db_path.as_os_str().to_owned();
        s.push(".lock");
        PathBuf::from(s)
    }

    /// No lock (disabled or already held internally).
    pub fn none(db_path: &Path) -> DbLock {
        DbLock { file: None, path: Self::lock_path(db_path), held_elsewhere: false }
    }

    /// No own lock, but the caller already holds the lock of this database.
    pub fn held_elsewhere(db_path: &Path) -> DbLock {
        DbLock { file: None, path: Self::lock_path(db_path), held_elsewhere: true }
    }

    pub fn is_held(&self) -> bool {
        self.file.is_some()
    }

    /// Whether writes are protected against other processes (own lock or a
    /// lock held by the caller).
    pub fn guards(&self) -> bool {
        self.file.is_some() || self.held_elsewhere
    }

    /// Acquires an exclusive write lock according to `cfg`.
    pub fn acquire(db_path: &Path, cfg: LockConfig) -> Result<DbLock> {
        if !cfg.enabled {
            return Ok(Self::none(db_path));
        }
        let path = Self::lock_path(db_path);
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create(true).truncate(false);
        // Never follow a symlink planted as `<db>.lock` (it would create or
        // lock an arbitrary target file).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.custom_flags(libc::O_NOFOLLOW);
        }
        let f = o.open(&path).map_err(|e| {
            #[cfg(unix)]
            if e.raw_os_error() == Some(libc::ELOOP) {
                return Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("lock file {} is a symbolic link; refusing to follow it", path.display()),
                ));
            }
            Error::Io(e)
        })?;
        let deadline = Instant::now() + Duration::from_millis(cfg.timeout_ms);
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(DbLock { file: Some(f), path, held_elsewhere: false }),
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

#[cfg(all(test, nhydb_internal_tests))]
#[path = "platform_tests.rs"]
mod tests;
