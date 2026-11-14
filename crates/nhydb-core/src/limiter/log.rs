//! RUN¹⁰ AP10.7: statistics file `limiter.txt`.
//!
//! * The limiter keeps its counters in memory all the time. Every
//!   `interval_min` minutes (default 30) its monitor thread takes one
//!   consistent record (UTC time stamp; counters as differences to the last
//!   record, peaks as maxima of the interval, which start anew) and hands it
//!   over with `try_send` into a bounded buffer (`buffer_records`): the
//!   limiter never blocks; a full buffer drops the record and counts it.
//! * A separate helper thread only does the file work: it appends each record
//!   as one JSON line (human- and machine-readable) to `limiter.txt` and,
//!   **after** writing, checks the size: from `max_bytes` on (default
//!   60 MB, a rotation threshold, not a hard limit) the file is archived as
//!   `limiter_<UTC time>.zip` and a new `limiter.txt` begins. A write error
//!   is only reported in the status; it never touches the limiter.
//! * Location: `path` (a directory, or a file name ending in `.txt`);
//!   default the directory of the database whose handle defined the limiter.
//!   The limiter is per process: every line names the process (`pid`) and
//!   that database (`db`); processes writing into the same directory share
//!   the file (each line is one appending write).
//! * When the last handle of the process closes, the started interval is
//!   written as well (`final: true`).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `[limiter_log]` of `nhydb_config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogConfig {
    pub enabled: bool,
    /// directory or `*.txt` file; `None` = directory of the database
    pub path: Option<String>,
    pub interval_min: u64,
    pub max_bytes: u64,
    pub buffer_records: u64,
}

pub const FILE_NAME: &str = "limiter.txt";

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig { enabled: true, path: None, interval_min: 30, max_bytes: 60_000_000, buffer_records: 16 }
    }
}

/// Upper bound of `[limiter_log] interval_min`: one year in minutes.
pub const MAX_INTERVAL_MIN: u64 = 366 * 24 * 60;

impl LogConfig {
    pub fn check(&self) -> std::result::Result<(), String> {
        // R9D-04: bounded, so that `interval_min * 60` seconds can never
        // overflow the duration arithmetic (one year at most)
        if !(1..=MAX_INTERVAL_MIN).contains(&self.interval_min) {
            return Err(format!("'limiter_log.interval_min' must be between 1 and {MAX_INTERVAL_MIN} (one year)"));
        }
        if self.max_bytes < 1024 {
            return Err("'limiter_log.max_bytes' must be at least 1024".into());
        }
        if !(1..=10_000).contains(&self.buffer_records) {
            return Err("'limiter_log.buffer_records' must be between 1 and 10000".into());
        }
        Ok(())
    }

    /// The file for a database at `db` (the configuration file decides the
    /// relative base: the database directory).
    pub fn file_for(&self, db: Option<&Path>) -> Option<PathBuf> {
        let base = db.map(crate::platform::parent_dir);
        match &self.path {
            None => Some(base?.join(FILE_NAME)),
            Some(p) => {
                let p = PathBuf::from(p);
                let p = if p.is_relative() { base.map(|b| b.join(&p)).unwrap_or(p) } else { p };
                Some(if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("txt")) { p } else { p.join(FILE_NAME) })
            }
        }
    }
}

pub(crate) enum Msg {
    Line(String),
    /// all lines before this one are written: notify the sender
    Flush(Arc<(Mutex<bool>, Condvar)>),
}

/// State shared by the limiter and its helper (status only).
#[derive(Default)]
pub(crate) struct LogState {
    pub file: Option<PathBuf>,
    pub last_error: Option<String>,
    pub lines_written: u64,
    pub rotations: u64,
}

/// The sending side kept by the limiter.
pub(crate) struct Logger {
    pub(crate) tx: SyncSender<Msg>,
    pub state: Arc<Mutex<LogState>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn lock_state(m: &Mutex<LogState>) -> std::sync::MutexGuard<'_, LogState> {
    lock(m)
}

impl Logger {
    pub(crate) fn tx_clone(&self) -> SyncSender<Msg> {
        self.tx.clone()
    }

    /// Starts the helper thread for `file`. `None` if the thread cannot start.
    pub(crate) fn start(file: PathBuf, cfg: &LogConfig) -> Option<Logger> {
        let (tx, rx) = sync_channel(cfg.buffer_records as usize);
        let state = Arc::new(Mutex::new(LogState { file: Some(file.clone()), ..Default::default() }));
        let st2 = state.clone();
        let max = cfg.max_bytes;
        std::thread::Builder::new()
            .name("nhydb-limiter-log".into())
            // default stack: the deflate compressor of a rotation needs it
            .spawn(move || helper(rx, file, max, st2))
            .ok()?;
        Some(Logger { tx, state })
    }

    /// Hands a record over; never blocks. `false`: the buffer is full (or
    /// the helper ended) and the record is dropped.
    pub(crate) fn offer(&self, line: String) -> bool {
        !matches!(self.tx.try_send(Msg::Line(line)), Err(TrySendError::Full(_) | TrySendError::Disconnected(_)))
    }

    /// Waits up to `timeout` until everything handed over is written (last
    /// handle closed; not called by the monitor thread).
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        if self.tx.try_send(Msg::Flush(pair.clone())).is_err() {
            return false;
        }
        let (m, cv) = &*pair;
        let g = lock(m);
        let (g, _) = cv.wait_timeout_while(g, timeout, |done| !*done).unwrap_or_else(|e| e.into_inner());
        *g
    }
}

fn helper(rx: Receiver<Msg>, file: PathBuf, max_bytes: u64, state: Arc<Mutex<LogState>>) {
    for msg in rx {
        match msg {
            Msg::Line(line) => {
                let r = append(&file, &line).and_then(|_| rotate_if_due(&file, max_bytes));
                let mut st = lock(&state);
                match r {
                    Ok(rotated) => {
                        st.lines_written += 1;
                        st.rotations += rotated as u64;
                    }
                    Err(e) => st.last_error = Some(format!("{}: {e}", file.display())),
                }
            }
            Msg::Flush(pair) => {
                let (m, cv) = &*pair;
                *lock(m) = true;
                cv.notify_all();
            }
        }
    }
}

fn append(file: &Path, line: &str) -> std::io::Result<()> {
    if let Some(d) = file.parent() {
        if !d.as_os_str().is_empty() {
            std::fs::create_dir_all(d)?;
        }
    }
    let mut f = OpenOptions::new().create(true).append(true).open(file)?;
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    f.write_all(&buf) // one write: lines of several processes do not interleave
}

/// After a write: archive the file once it reached the threshold.
fn rotate_if_due(file: &Path, max_bytes: u64) -> std::io::Result<bool> {
    if std::fs::metadata(file)?.len() < max_bytes {
        return Ok(false);
    }
    let data = std::fs::read(file)?;
    let stamp = utc_stamp(SystemTime::now()).replace([':', '-'], "");
    let dir = crate::platform::parent_dir(file);
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("limiter");
    let mut zip = dir.join(format!("{stem}_{stamp}.zip"));
    let mut n = 1;
    while zip.exists() {
        zip = dir.join(format!("{stem}_{stamp}_{n}.zip"));
        n += 1;
    }
    let name = file.file_name().and_then(|s| s.to_str()).unwrap_or(FILE_NAME);
    let tmp = zip.with_extension("zip.part");
    std::fs::write(&tmp, zip_one(name, &data))?;
    std::fs::rename(&tmp, &zip)?;
    std::fs::remove_file(file)?;
    Ok(true)
}

/// A ZIP archive with one deflated entry (no dependency beyond miniz/crc32).
pub(crate) fn zip_one(name: &str, data: &[u8]) -> Vec<u8> {
    let comp = miniz_oxide::deflate::compress_to_vec(data, 6);
    let crc = crc32fast::hash(data);
    let (time, date) = dos_time(SystemTime::now());
    let n = name.as_bytes();
    let mut out = Vec::with_capacity(comp.len() + 2 * n.len() + 128);
    let le16 = |o: &mut Vec<u8>, v: u16| o.extend_from_slice(&v.to_le_bytes());
    let le32 = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());
    // local file header
    le32(&mut out, 0x0403_4b50);
    for v in [20u16, 0, 8, time, date] {
        le16(&mut out, v);
    }
    for v in [crc, comp.len() as u32, data.len() as u32] {
        le32(&mut out, v);
    }
    le16(&mut out, n.len() as u16);
    le16(&mut out, 0);
    out.extend_from_slice(n);
    out.extend_from_slice(&comp);
    // central directory
    let cd = out.len() as u32;
    le32(&mut out, 0x0201_4b50);
    for v in [20u16, 20, 0, 8, time, date] {
        le16(&mut out, v);
    }
    for v in [crc, comp.len() as u32, data.len() as u32] {
        le32(&mut out, v);
    }
    for v in [n.len() as u16, 0, 0, 0, 0] {
        le16(&mut out, v);
    }
    le32(&mut out, 0);
    le32(&mut out, 0);
    out.extend_from_slice(n);
    let cd_len = out.len() as u32 - cd;
    // end of central directory
    le32(&mut out, 0x0605_4b50);
    for v in [0u16, 0, 1, 1] {
        le16(&mut out, v);
    }
    le32(&mut out, cd_len);
    le32(&mut out, cd);
    le16(&mut out, 0);
    out
}

/// Civil date from days since 1970-01-01 (proleptic Gregorian).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2026-11-02T15:04:05Z`.
pub fn utc_stamp(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, mo, d) = civil(s.div_euclid(86_400));
    let r = s.rem_euclid(86_400);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", r / 3600, r % 3600 / 60, r % 60)
}

fn dos_time(t: SystemTime) -> (u16, u16) {
    let s = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, mo, d) = civil(s.div_euclid(86_400));
    let r = s.rem_euclid(86_400);
    let time = (((r / 3600) << 11) | ((r % 3600 / 60) << 5) | ((r % 60) / 2)) as u16;
    let date = (((y - 1980).max(0) << 9) | (mo as i64) << 5 | d as i64) as u16;
    (time, date)
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "log_tests.rs"]
mod tests;
