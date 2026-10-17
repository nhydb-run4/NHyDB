//! Portable Betriebssystem-Schnittstelle.
//!
//! Einzige Stelle mit betriebssystemspezifischem Code: positionsbasiertes
//! Lesen/Schreiben, Dauerhaftigkeit (`fsync`/`full`), Verzeichnis-Sync,
//! atomarer Dateiaustausch und Dateisperren. Zielplattformen: macOS, Linux,
//! Windows.
//!
//! Garantien der Durability-Modi (siehe `docs/PLATFORM.md`):
//!
//! | Modus | macOS | Linux | Windows |
//! |---|---|---|---|
//! | `fsync` | `fsync(2)` | `fsync(2)` | `FlushFileBuffers` |
//! | `full`  | `fcntl(F_FULLFSYNC)` | `fsync(2)` (leert den Gerätecache) | `FlushFileBuffers` (leert den Gerätecache) |
//!
//! Kann ein Modus nicht garantiert werden, wird ein Fehler gemeldet; es gibt
//! keinen stillen Rückfall auf eine schwächere Garantie.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Dauerhaftigkeitsmodus für Commit/Sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Durability {
    /// Normale Betriebssystem-Synchronisation (Python `os.fsync`).
    #[default]
    Fsync,
    /// Zusätzlich Leeren des Laufwerks-Caches (stärkste Garantie).
    Full,
}

impl Durability {
    pub fn parse(s: &str) -> Result<Durability> {
        match s.to_ascii_lowercase().as_str() {
            "fsync" | "default" | "normal" => Ok(Durability::Fsync),
            "full" | "fullfsync" => Ok(Durability::Full),
            _ => Err(Error::Value("durability muss 'fsync' oder 'full' sein".into())),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Durability::Fsync => "fsync",
            Durability::Full => "full",
        }
    }
    /// Prüft, ob die Plattform den Modus garantieren kann.
    pub fn check_supported(self) -> Result<()> {
        if self == Durability::Full && !FULL_SUPPORTED {
            return Err(Error::Value(format!(
                "durability='full' wird auf dieser Plattform ({}) nicht garantiert",
                std::env::consts::OS
            )));
        }
        Ok(())
    }
}

const FULL_SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "ios", target_os = "linux", target_os = "android", windows));

// ----------------------------------------------------------------------
// Positionsbasiertes I/O
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
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Dateiende erreicht")),
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
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "Schreiben fehlgeschlagen")),
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
// Dauerhaftigkeit
// ----------------------------------------------------------------------

/// Synchronisiert Inhalt und Metadaten einer Datei gemäß `mode`.
pub fn sync_file(f: &File, mode: Durability) -> Result<()> {
    match mode {
        Durability::Fsync => plain_sync(f),
        Durability::Full => full_sync(f),
    }
}

/// `fsync(2)` direkt. Achtung: Rusts `File::sync_all` ruft auf Apple-
/// Plattformen `F_FULLFSYNC` auf und wäre damit bereits der Modus `full`.
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
            format!("F_FULLFSYNC fehlgeschlagen (durability='full' auf diesem Dateisystem nicht garantiert): {e}"),
        )));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android", windows))]
fn full_sync(f: &File) -> Result<()> {
    // Linux: fsync(2) leert laut Kernel-Vertrag auch den flüchtigen
    // Gerätecache (Write-Barrier/FUA). Windows: FlushFileBuffers ebenso.
    f.sync_all()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux", target_os = "android", windows)))]
fn full_sync(_f: &File) -> Result<()> {
    Err(Error::Value("durability='full' wird auf dieser Plattform nicht garantiert".into()))
}

/// Macht Verzeichniseinträge (Anlegen/Umbenennen) dauerhaft.
#[cfg(unix)]
pub fn sync_dir(dir: &Path, mode: Durability) -> Result<()> {
    let d = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let f = File::open(d)?;
    sync_file(&f, mode)
}

/// Windows: Verzeichnis-Metadaten sind über das NTFS-Journal geschützt;
/// Umbenennungen erfolgen mit `MOVEFILE_WRITE_THROUGH` (siehe
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

/// Ersetzt `dst` atomar durch `src` (gleiches Verzeichnis/Dateisystem) und
/// macht den Austausch dauerhaft.
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

/// Setzt die Zugriffsrechte einer geheimen Datei auf „nur Besitzer“.
#[cfg(unix)]
pub fn restrict_to_owner(f: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Windows: Dateien im Benutzerprofil erben die Besitzer-ACL; keine
/// zusätzliche Einschränkung über Standardmittel.
#[cfg(windows)]
pub fn restrict_to_owner(_f: &File) -> Result<()> {
    Ok(())
}

/// Legt eine neue Datei exklusiv an (Fehler, falls vorhanden); bei `secret`
/// mit Besitzer-Rechten.
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
// Lokale Zeit (Backup-Namen wie Pythons `time.strftime("%Y%m%d_%H%M%S")`)
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
// Dateisperren
// ----------------------------------------------------------------------

/// Sperrverhalten (aus `nhydb_config.toml`, Abschnitt `[lock]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockConfig {
    pub enabled: bool,
    /// 0 = sofort fehlschlagen, sonst maximale Wartezeit.
    pub timeout_ms: u64,
}

impl Default for LockConfig {
    fn default() -> Self {
        LockConfig { enabled: true, timeout_ms: 0 }
    }
}

/// Gehaltene Prozess-Sperre auf `<db>.lock`. Die Sperrdatei liegt neben der
/// DB und bleibt bestehen; dadurch gilt die Sperre auch über einen atomaren
/// Austausch der DB-Datei hinweg. Freigabe beim Drop.
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

    /// Keine Sperre (deaktiviert oder intern bereits gehalten).
    pub fn none(db_path: &Path) -> DbLock {
        DbLock { file: None, path: Self::lock_path(db_path) }
    }

    pub fn is_held(&self) -> bool {
        self.file.is_some()
    }

    /// Erwirbt eine exklusive Schreibsperre gemäß `cfg`.
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
                            "Datenbank wird bereits von einem anderen Prozess geschrieben (Sperre {} nach {} ms nicht erhalten)",
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
