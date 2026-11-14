//! Result files (0.28.2 round 4, stage C): query results written to result
//! containers (`container.rs`) in a result directory independent of the
//! database (`[results] temp_dir`, default `<system temp>/nhydb-results`),
//! with an optional byte budget (`[results] budget_bytes`, 0 = none).
//!
//! Managed files (written into the result directory without an explicit
//! path) are named `nhydb-<pid>-<time>-<seq>.nhyc`. Their owner holds a
//! shared lock on the file while it is open; an unpinned managed file is
//! deleted when its owner closes it (or the process ends and a later
//! `cleanup_results` finds it unlocked). `pin` renames it to
//! `….pinned.nhyc`: pinned files stay until `delete`. No expiry by time.
//! Files with an explicit path belong to the caller (never deleted, no
//! budget).

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::config::Config;
use crate::container::{Budget, ContainerInfo, ContainerReader, ContainerWriter, KIND_ROWS};
use crate::db::{Db, OpenOptions};
use crate::error::{Error, Result};
use crate::query::{Providers, QueryEngine};
use crate::sql::{self, Params};
use crate::val::Val;
use crate::vmap;
use crate::Lane;

pub const MANAGED_PREFIX: &str = "nhydb-";
pub const EXT: &str = ".nhyc";
pub const PINNED_EXT: &str = ".pinned.nhyc";

/// The result directory of a configuration.
pub fn result_dir(cfg: &Config) -> PathBuf {
    match &cfg.results_temp_dir {
        Some(d) => PathBuf::from(d),
        None => std::env::temp_dir().join("nhydb-results"),
    }
}

// ----------------------------------------------------------------------
// Budget of a result directory (per process)
// ----------------------------------------------------------------------

pub struct DirBudget {
    dir: PathBuf,
    limit: u64,
    used: AtomicU64,
}

impl Budget for DirBudget {
    fn reserve(&self, n: u64) -> Result<()> {
        if self.limit == 0 {
            self.used.fetch_add(n, Ordering::Relaxed);
            return Ok(());
        }
        let prev = self.used.fetch_add(n, Ordering::Relaxed);
        if prev + n > self.limit {
            self.used.fetch_sub(n, Ordering::Relaxed);
            return Err(Error::SizeLimit(format!(
                "result directory {}: [results] budget_bytes = {} exceeded ({} bytes in use, {n} more needed); \
                 delete or unpin result files, run nhydb.cleanup_results() or raise the budget",
                self.dir.display(),
                self.limit,
                prev
            )));
        }
        Ok(())
    }

    fn release(&self, n: u64) {
        let _ = self.used.try_update(Ordering::Relaxed, Ordering::Relaxed, |u| Some(u.saturating_sub(n)));
    }
}

impl DirBudget {
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
    pub fn limit(&self) -> u64 {
        self.limit
    }
}

static BUDGETS: Mutex<Option<HashMap<PathBuf, Arc<DirBudget>>>> = Mutex::new(None);

/// Process id of a managed name `nhydb-<pid>-<hex time>-<seq>` followed by
/// `.nhyc`, `.pinned.nhyc` or `.nhyc.part` (RD4-07: only exactly this form
/// is managed; other files – also caller files starting with `nhydb-` – are
/// never touched).
fn managed_pid(name: &str) -> Option<u32> {
    let stem = name
        .strip_suffix(".nhyc.part")
        .or_else(|| name.strip_suffix(PINNED_EXT))
        .or_else(|| name.strip_suffix(EXT))?
        .strip_prefix(MANAGED_PREFIX)?;
    let mut it = stem.split('-');
    let (pid, t, seq) = (it.next()?, it.next()?, it.next()?);
    let digits = |x: &str, hex: bool| !x.is_empty() && x.bytes().all(|c| if hex { c.is_ascii_hexdigit() && !c.is_ascii_uppercase() } else { c.is_ascii_digit() });
    if it.next().is_some() || !digits(pid, false) || !digits(t, true) || !digits(seq, false) {
        return None;
    }
    pid.parse().ok()
}

fn managed_name(name: &str) -> bool {
    managed_pid(name).is_some()
}

/// The budget of `dir` in this process. The first use counts the managed
/// files already in the directory; the limit of the first use applies.
pub fn dir_budget(dir: &Path, limit: u64) -> Result<Arc<DirBudget>> {
    std::fs::create_dir_all(dir).map_err(|e| Error::IoPath(e, dir.into()))?;
    let key = crate::platform::absolute(dir)?;
    let mut g = BUDGETS.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    if let Some(b) = map.get(&key) {
        return Ok(b.clone());
    }
    let mut used = 0u64;
    if let Ok(rd) = std::fs::read_dir(&key) {
        for e in rd.flatten() {
            if managed_name(&e.file_name().to_string_lossy()) {
                used += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    let b = Arc::new(DirBudget { dir: key.clone(), limit, used: AtomicU64::new(used) });
    map.insert(key, b.clone());
    Ok(b)
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn new_managed_path(dir: &Path) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let s = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{MANAGED_PREFIX}{}-{t:x}-{s}{EXT}", std::process::id()))
}

/// Removes managed files of the result directory that nobody holds:
/// `.part` files of interrupted writers and unpinned results of ended
/// processes. A file is removed only while this call holds its exclusive
/// lock; owners (writers, open results) hold a shared lock from the moment
/// the name exists (RD4-02). Sort runs have no name (RD4-01). Pinned files
/// and files not named exactly like managed files stay (RD4-07). Returns
/// the number of removed files.
pub fn cleanup_results(dir: &Path) -> Result<u64> {
    let mut n = 0;
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::IoPath(e, dir.into())),
    };
    let budget = BUDGETS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| crate::platform::absolute(dir).ok().and_then(|k| m.get(&k).cloned()));
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = e.path();
        // RY-03: an anonymous sort run of a process killed between creating
        // and unlinking it (`.nhydb-sortrun_<pid>_<n>_<t>.tmp`)
        if let Some(pid) = name.strip_prefix(".nhydb-sortrun_").and_then(|x| x.split('_').next()).and_then(|x| x.parse::<u32>().ok()) {
            if name.ends_with(".tmp") && pid != std::process::id() && !crate::platform::process_alive(pid) && std::fs::remove_file(&p).is_ok() {
                n += 1;
            }
            continue;
        }
        // a writer that died between creating and naming its part file
        if let Some(pid) = name.strip_prefix('.').and_then(|x| x.strip_suffix(".init")).and_then(managed_pid) {
            if pid != std::process::id() && !crate::platform::process_alive(pid) && std::fs::remove_file(&p).is_ok() {
                n += 1;
            }
            continue;
        }
        if !managed_name(&name) || name.ends_with(PINNED_EXT) {
            continue;
        }
        let Ok(f) = File::open(&p) else { continue };
        if crate::platform::try_lock_exclusive_all(&f).is_ok() {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            let removed = std::fs::remove_file(&p).is_ok();
            drop(f);
            if removed {
                n += 1;
                if let Some(b) = &budget {
                    b.release(len);
                }
            }
        }
    }
    Ok(n)
}

// ----------------------------------------------------------------------
// Result file handle
// ----------------------------------------------------------------------

/// An open result file (rows of a query).
pub struct ResultFile {
    path: PathBuf,
    managed: bool,
    pinned: bool,
    lock: Option<crate::platform::OwnerLock>,
    budget: Option<Arc<DirBudget>>,
    pub info: ContainerInfo,
    deleted: bool,
}

impl ResultFile {
    /// Opens an existing result file (all checksums of header and footer
    /// are checked; chunks when read). Not managed: never deleted
    /// automatically.
    pub fn open(path: &Path) -> Result<ResultFile> {
        let r = ContainerReader::open(path)?;
        if r.info.kind != KIND_ROWS {
            return Err(Error::Value(format!("{} is no query result (payload kind {:?})", path.display(), r.info.kind)));
        }
        Ok(ResultFile { path: path.to_path_buf(), managed: false, pinned: false, lock: None, budget: None, info: r.info, deleted: false })
    }

    fn open_managed(path: &Path, budget: Arc<DirBudget>) -> Result<ResultFile> {
        let lock = File::open(path).map_err(|e| Error::IoPath(e, path.into()))?;
        lock_shared(&lock, path)?;
        let mut rf = ResultFile::open(path)?;
        rf.managed = true;
        rf.lock = Some(crate::platform::OwnerLock::new(lock));
        rf.budget = Some(budget);
        Ok(rf)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn managed(&self) -> bool {
        self.managed
    }
    pub fn pinned(&self) -> bool {
        self.pinned || self.path.to_string_lossy().ends_with(PINNED_EXT)
    }
    pub fn rows(&self) -> u64 {
        self.info.items
    }
    pub fn columns(&self) -> Vec<String> {
        match self.info.header_meta.get("columns") {
            Some(Val::List(l)) => l.iter().map(|c| c.py_str()).collect(),
            _ => Vec::new(),
        }
    }

    /// A reader over the rows (checks every chunk).
    pub fn reader(&self) -> Result<ContainerReader> {
        if self.deleted {
            return Err(Error::NotFound(format!("result file {} was deleted", self.path.display())));
        }
        ContainerReader::open(&self.path)
    }

    /// Keeps a managed file beyond its owner (renamed to `….pinned.nhyc`,
    /// synced). No effect on unmanaged files.
    pub fn pin(&mut self) -> Result<()> {
        if !self.managed || self.pinned() || self.deleted {
            return Ok(());
        }
        let s = self.path.to_string_lossy().into_owned();
        let to = PathBuf::from(format!("{}{PINNED_EXT}", s.strip_suffix(EXT).unwrap_or(&s)));
        // RD4-02: the lock stays across the rename (same file)
        crate::platform::rename_no_replace(&self.path, &to).map_err(|e| Error::IoPath(e, self.path.as_path().into()))?;
        crate::platform::sync_renamed(&to, crate::platform::Durability::Fsync)?;
        self.path = to;
        self.pinned = true;
        Ok(())
    }

    /// Makes a pinned managed file temporary again (deleted on close).
    pub fn unpin(&mut self) -> Result<()> {
        if !self.managed || !self.pinned() || self.deleted {
            return Ok(());
        }
        let s = self.path.to_string_lossy().into_owned();
        let to = PathBuf::from(format!("{}{EXT}", s.strip_suffix(PINNED_EXT).unwrap_or(&s)));
        // RD4-02: locked before it gets the name `cleanup_results` looks at
        if self.lock.is_none() {
            let lock = File::open(&self.path).map_err(|e| Error::IoPath(e, self.path.as_path().into()))?;
            lock_shared(&lock, &self.path)?;
            self.lock = Some(crate::platform::OwnerLock::new(lock));
        }
        crate::platform::rename_no_replace(&self.path, &to).map_err(|e| Error::IoPath(e, self.path.as_path().into()))?;
        self.path = to;
        self.pinned = false;
        Ok(())
    }

    /// Deletes the file (any file, also unmanaged and pinned ones).
    pub fn delete(&mut self) -> Result<()> {
        if self.deleted {
            return Ok(());
        }
        std::fs::remove_file(&self.path).map_err(|e| Error::IoPath(e, self.path.as_path().into()))?;
        self.lock = None;
        self.deleted = true;
        if let Some(b) = &self.budget {
            b.release(self.info.file_bytes);
        }
        Ok(())
    }

    /// Closes the handle: an unpinned managed file is deleted.
    pub fn close(&mut self) {
        if self.managed && !self.pinned() && !self.deleted {
            let _ = self.delete();
        }
        self.lock = None;
    }
}

impl Drop for ResultFile {
    fn drop(&mut self) {
        self.close();
    }
}

// ----------------------------------------------------------------------
// Writing a query result
// ----------------------------------------------------------------------

/// Options of a result file write.
#[derive(Clone, Debug, Default)]
pub struct ResultTarget {
    /// explicit file (caller-owned); `None` = managed file in the result directory
    pub path: Option<PathBuf>,
    /// zlib-compressed chunks (default `[results] compress`)
    pub compress: Option<bool>,
}

/// Progress and cancellation of a write.
#[derive(Default)]
pub struct Progress {
    pub rows: AtomicU64,
    pub cancel: AtomicBool,
}

/// Streams the rows of a SELECT into a result file and opens it.
pub fn write_sql(engine: &QueryEngine<'_>, text: &str, params: &Params, lane: Lane, target: &ResultTarget, progress: &Progress) -> Result<ResultFile> {
    let db = engine.db;
    let cfg = &db.config;
    let mut cur = sql::open_sql(engine, text, params, lane, false)?;
    let columns: Vec<Val> = cur.columns().iter().map(|c| Val::str(c.as_str())).collect();
    let compress = target.compress.unwrap_or(cfg.results_compress);
    let (path, budget) = match &target.path {
        Some(p) if p.exists() => return Err(Error::Exists(format!("result file {} already exists", p.display()))),
        Some(p) if is_reserved(p, &result_dir(cfg)) => {
            return Err(Error::Value(format!("{}: names nhydb-<pid>-<hex>-<seq>.nhyc in the result directory are reserved for managed result files", p.display())))
        }
        Some(p) => (p.clone(), None),
        None => {
            let dir = result_dir(cfg);
            let b = dir_budget(&dir, cfg.results_budget_bytes)?;
            (new_managed_path(&dir), Some(b))
        }
    };
    let meta = vmap! {"columns" => Val::List(columns), "query" => text, "created" => crate::platform::local_timestamp()};
    let durability = if target.path.is_some() { Some(db.config.durability) } else { None };
    // the writer holds a shared lock on its part file from its creation to
    // the end of `finish_locked` (also across the rename): `cleanup_results`
    // keeps it (RD4-02)
    let mut w = ContainerWriter::create(&path, KIND_ROWS, &meta, compress, durability, budget.clone().map(|b| b as Arc<dyn Budget>))?;
    let mut item = Vec::new();
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err(Error::Query("result write cancelled".into()));
        }
        let batch = cur.next_batch(engine, 4096, None)?;
        if batch.is_empty() {
            break;
        }
        for row in &batch {
            item.clear();
            crate::valcodec::encode(row, &mut item);
            w.push_raw(&item)?;
        }
        progress.rows.fetch_add(batch.len() as u64, Ordering::Relaxed);
    }
    let rows = w.items();
    let (writer_lock, _) = w.finish_locked(&vmap! {"rows" => rows as i64})?;
    // the result handle locks the file before the writer's lock ends
    let rf = match budget {
        Some(b) => ResultFile::open_managed(&path, b),
        None => ResultFile::open(&path),
    };
    drop(writer_lock);
    rf
}

/// Takes a shared lock (owners of managed files). A file system without
/// locks is no error; a held exclusive lock (`cleanup_results` right now)
/// is retried briefly.
fn lock_shared(f: &File, path: &Path) -> Result<()> {
    for _ in 0..100 {
        match crate::platform::lock_owner_shared(f) {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(2)),
            Err(std::fs::TryLockError::Error(_)) => return Ok(()),
        }
    }
    Err(Error::Value(format!("result file {} is locked exclusively", path.display())))
}

/// An explicit path with a managed name inside the result directory.
fn is_reserved(p: &Path, dir: &Path) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !managed_name(&name) {
        return false;
    }
    // RY-04: `..` and symbolic links resolved (a missing directory cannot
    // be the result directory)
    let parent = crate::platform::parent_dir(p);
    match (std::fs::canonicalize(&parent), std::fs::canonicalize(dir)) {
        (Ok(a), Ok(b)) => a == b,
        (Err(_), _) => false,
        (Ok(_), Err(_)) => true,
    }
}

// ----------------------------------------------------------------------
// Background jobs on a read-only snapshot
// ----------------------------------------------------------------------

/// State of a background result job.
pub struct JobState {
    pub progress: Progress,
    done: Mutex<Option<Result<ResultFile>>>,
    finished: AtomicBool,
    cv: Condvar,
}

impl JobState {
    pub fn is_done(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Waits up to `timeout` (None: without limit); `None` on timeout.
    pub fn wait(&self, timeout: Option<std::time::Duration>) -> Option<()> {
        let g = self.done.lock().unwrap_or_else(|e| e.into_inner());
        let fin = || self.finished.load(Ordering::Acquire);
        let _g = match timeout {
            None => self.cv.wait_while(g, |_| !fin()).unwrap_or_else(|e| e.into_inner()),
            Some(t) => self.cv.wait_timeout_while(g, t, |_| !fin()).unwrap_or_else(|e| e.into_inner()).0,
        };
        fin().then_some(())
    }

    /// Takes the result once the job is done (second call: error).
    pub fn take(&self) -> Option<Result<ResultFile>> {
        self.done.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Runs a SELECT in a background thread on a read-only snapshot of `db`
/// (a new read-only handle: the state of the last commit when the job
/// starts; uncommitted changes of `db` are not seen) and writes the rows to
/// a result file.
pub fn submit_sql(db: &Db, text: &str, params: &Params, lane: Lane, target: ResultTarget) -> Result<Arc<JobState>> {
    db.check_open()?;
    let opts = OpenOptions {
        readonly: true,
        layout_path: Some(db.sys.layout_path.clone()),
        key_path: Some(db.sys.key_path.clone()),
        config: Some(db.config.clone()),
        ..OpenOptions::default()
    };
    let path = db.path.clone();
    // open on the calling thread: errors of the snapshot surface at submit
    let snap = Db::open_with(&path, opts)?;
    let st = Arc::new(JobState { progress: Progress::default(), done: Mutex::new(None), finished: AtomicBool::new(false), cv: Condvar::new() });
    let st2 = st.clone();
    let (text, params) = (text.to_string(), params.clone());
    std::thread::Builder::new()
        .name("nhydb-result-job".into())
        .spawn(move || {
            let providers = Providers::default();
            let eng = QueryEngine::new(&snap, &providers);
            // RD4-14: a panic ends the job with an error, `wait()` returns
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write_sql(&eng, &text, &params, lane, &target, &st2.progress)))
                .unwrap_or_else(|p| {
                    let m = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
                    Err(Error::Value(format!("result job failed internally: {m}")))
                });
            drop(snap);
            let mut g = st2.done.lock().unwrap_or_else(|e| e.into_inner());
            *g = Some(r);
            st2.finished.store(true, Ordering::Release);
            drop(g);
            st2.cv.notify_all();
        })
        .map_err(Error::Io)?;
    Ok(st)
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "results_tests.rs"]
mod tests;
