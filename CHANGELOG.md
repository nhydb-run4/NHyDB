# Changelog

All notable changes to this project are documented in this file.
The project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.28.0] – 2026-10-21

**File format:** the block format is unchanged (`NHYDB016`, superblocks,
block headers and data payloads), but the schema snapshot is now bound by a
keyed digest (see *Changed*); **databases created with 0.27.1 or earlier do
not open** (fail closed, no migration – export the data with 0.27.1 and
re-import it). The MATH layout state in the encrypted layout plan gains the
field `max_intermediate_bits`.

### Added

* `NHyDB(path, readonly=True)` (also `nhydb.open`): opens with `O_RDONLY`
  and without lock; the file is never changed (no truncation, no commit on
  close), write APIs and `commit()` raise `NHyDBError`, the view is the
  committed state at the time of opening. Attributes `readonly` and
  `open_warnings`; warnings on open (superblock fallback, key file readable
  by group/others) are also issued as `RuntimeWarning`.
* `NHyDB.restore_backup(backup, path)` and CLI `nhydb restore`: checks a
  backup of `repair_file`/`compact_file` and puts DB file and layout plan back
  in place (key file unchanged). Restore by hand: copy `<backup>` to the DB
  path and `<backup>.layout.enc` to its layout plan path.
* `repair_file(allow_data_loss=False, keep_uncommitted=False)`; result fields
  `action`, `plan_error`, `lost_blocks`, `discarded_uncommitted_blocks`,
  `kept_uncommitted_blocks`, `missing_committed_bytes`,
  `backup_layout_plan`. `inspect_tail()` also reports both superblocks, the
  commit boundary, `superblock_problem`, `uncommitted_bytes`,
  `damaged_blocks` and intact blocks behind a damaged spot (`later_valid_blocks`);
  `verify()` has `warnings`.
* `read_numpy(..., object_fallback=False)`: object column instead of
  `ValueError` for values that do not fit the field's dtype.
* Path placeholder `{db_path_hash}` for `[paths]` (several databases with the
  same file name sharing a key directory).
* `MathLayoutState.max_intermediate_bits` (bit limit frozen per MATH layout).
* Opt-in `with db.batch(commit=False, u_window=10_000) as b:` collects
  `b.put_cell`/`b.delete_cell` (validated at the call; every cell ends as
  with the same single calls, also for `None` values) and writes them on
  leaving the block with one `put_points` per realm/lane; no
  read-your-writes inside the block, nothing is written on an exception,
  and a write error rolls the whole batch back across all realm/lane
  groups. 20 000 single cells: ≈1.8× faster without commits, file 5×
  smaller.
* `db.rollback()`: discards all writes and schema changes since the last
  commit of the handle and restores the committed state.
* `nhydb._native.BUILD_SOURCE_HASH`: fingerprint of the Rust sources; the
  test suite warns if the installed module does not match the checkout.
* CLI: exit codes 0 (ok) / 1 (problems found) / 2 (usage or refused
  operation) / 3 (error), messages instead of tracebacks, `bytes` as
  `{"$bytes_b64": ...}`; read commands use `readonly=True`; `verify` works on
  damaged files; `repair --allow-data-loss/--keep-uncommitted`.

### Changed

* Durability: the commit boundary is now power-loss safe on macOS also in
  `fsync` mode (`F_BARRIERFSYNC`); this costs about 0.15–0.3 ms per small
  commit and more for large ones (≈0.7 ms with 2.3 MB of new data).
* `readonly=True` (and opens without lock) no longer fail with a false
  `CorruptionError` while a writer commits schema changes: the open catches up
  with the newer commits (a plan mismatch without a concurrent writer is still
  a `CorruptionError`).
* `close()` with pending changes leaves both superblocks on the committed
  state (a later bit error in one of them falls back transparently); closing
  an unchanged database still writes nothing – a handle that appended no
  blocks never writes a superblock.
* Backups made within the same second get a counter suffix
  (`<db>.before_compact_<ts>_2`, `<db>.bak_<ts>_2`, ...) instead of failing
  with `FileExistsError`; the path is returned as `backup`.
* **File format – keyed schema digest:** the layout plan entry
  (`sha256` field) and R0,u1,v6 hold
  `HMAC-SHA256(SHA256("NHYDB027-SCHEMA-DIGEST:" ‖ key), snapshot)` instead of
  a plain SHA-256 of the canonical schema snapshot, so a guessed schema can no
  longer be confirmed without the key. Databases created before this change
  do not open (no migration); the `CorruptionError` names NHyDB 0.27.1 and
  the export/re-import route, and the `repair_file` dry run reports it as
  `action: "abort"` with `plan_error` (`inspect_tail` checks only the block
  structure).
* OS errors on the layout plan or key file name that file in
  `OSError.filename` (previously always the database path).
* **Opening never cuts committed data.** The uncommitted tail is only
  discarded if the commit boundary is unambiguous (both superblocks valid and
  consistent) and the lock is held, and only after key, plan and schema were
  verified. If a superblock is damaged or inconsistent and complete blocks
  follow the usable boundary, or the file is shorter than the boundary,
  opening fails with `CorruptionError` and changes nothing; `readonly=True`
  reads the state of the usable superblock. Without lock (`lock=False`) an
  uncommitted tail is never cut (`DatabaseLockedError`). A plain process kill
  never causes this state, only a power/system failure while a superblock is
  written.
* `repair_file` takes the lock first, never discards intact committed blocks
  without `allow_data_loss=True` (otherwise `CorruptionError`), discards
  uncommitted blocks unless `keep_uncommitted=True`, asks for one of the two
  switches when the boundary is ambiguous, and writes both superblocks with
  new generations. The dry run reports the planned action.
* Backups of `repair_file`/`compact_file` include the layout plan
  (`<backup>.layout.enc`) and are made durable.
* `create(overwrite=True)` replaces the old database only after the new one
  was created, and never deletes a plan/key file that belongs to another
  database (`FileExistsError`).
* All paths are anchored absolutely on open/create (`db.path` is absolute; a
  later `chdir` cannot redirect a schema commit). Empty, identical or
  directory side-file paths and `db_max_size_bytes` below 64 KiB are rejected.
* `lock=`/`lock_timeout_ms=` override the `[lock]` configuration field by
  field. The lock file never follows a symlink.
* Commit: a failed fsync or superblock write poisons the handle (reopen
  required); `close()` on such a handle releases file and lock. macOS mode
  `fsync` issues `F_BARRIERFSYNC` before the superblock. A commit without
  new blocks writes nothing and issues no barrier.
* `verify()` reports every damaged block; a stale index is a warning
  (`warnings`, `health()` status `warning`), no longer an error.
* The context manager raises the commit error of `__exit__` chained to the
  original exception and releases handle and lock; `with` on a closed
  database raises `NHyDBError`. (It still commits after an exception, as in
  v0.14.)
* `create_realm`, `create_shape` and `create_index` with an occupied ID raise
  `SchemaError` instead of replacing the definition (change a realm's default
  shape with `set_realm_default_shape`).
* Shape, layout (incl. MATH), index and default-shape IDs are limited to
  2^63−1 (`ValueError`).
* Coordinates, realm, lane and IDs are no longer truncated: non-integral
  floats (`u=2.7`) raise `TypeError`; integral values (`3.0`) are accepted.
* Writing: `bytes(n)` from an integer is limited to 1 GiB; integers that do
  not fit a finite `f32`/`f64` raise `ValueError`; `bytes` for a `utf8` field
  raise `TypeError`; unknown keys in dict rows raise `SchemaError`, also
  extension names and `v<n>` keys in a plain dict (pass extensions via
  `extensions=`). `_` metadata keys are ignored; in a `get()` record (a dict
  with `_shape`, `_u` or `_realm`) extension names and `v<n>` labels are
  ignored as well, so `put(realm, u, get(...))` round-trips (only the shape
  fields are written).
* MATH: nesting is limited (200 brackets/levels as in CPython, syntax tree
  depth 1000) with `MathSyntaxError`; long operator chains are evaluated
  iteratively. Inputs follow Python `int()` exactly (large floats,
  underscores, NaN/inf → `ValueError`); identifiers must be NFC/NFKC
  normalized; number literals follow CPython. `math_max_intermediate_bits`
  of the configuration is frozen per MATH layout at creation and used for
  writing and reading; `set_math_max_intermediate_bits` only affects
  standalone expressions (value ≥ 1).
* `read_numpy`: result dtypes depend only on the field types; huge `count`
  values raise `MemoryError` instead of aborting the process; negative
  `count` raises `ValueError`.
* NumPy columns are checked strictly: only 1-D, equal lengths (also when the
  first column is empty), `str`/`bytes`/`dict` are no columns, float16,
  longdouble, complex and datetime arrays raise `TypeError`, bool/float
  arrays are rejected for integer fields and numeric arrays for utf8/bytes
  fields. Bool columns work via `memoryview` and `array.array`; NumPy
  scalars are broadcast like Python ints.
* `delete_cell` returns `True`/`False` and writes nothing for an empty cell.
* `get_value` accepts column names like `get_cell`.
* I/O errors are raised as the matching `OSError` subclass with `errno`
  (and `filename` where known).
* `MathError`, `LayoutError` and `QueryError` derive from `NHyDBError`.
* `from nhydb import *` no longer exports `open`.
* Nested values deeper than 256 levels raise `ValueError` (no stack overflow).
* NHySQL `FROM <layout>` covers only records routed to that layout.
* After `close()`, reads, queries, index APIs and open iterators raise
  "database is closed" independent of cached data.
* BM25 rejects non-finite `k1`; `@record_n` rejects fractional floats;
  `search(limit=)` accepts big ints.
* Documented: read-only queries may share one `NHyDB` object between
  threads; a write on an object that another thread is using raises
  `RuntimeError` (no data change); `IS NULL` means "field
  missing"; the `find` iterator works on the `u` list taken at its first
  `next()`; `delete(realm, u)` hides only the dense record (single cells and
  extensions written for that `u` stay visible), `delete_record(layout_id,
  object_id)` removes a record completely.

### Fixed

* Realm and u numbers and `varuint`/`varint` values of any size are read back
  after reopening (the fixed 16 384-bit decoder limit is gone; the length is
  bounded only by the block payload; allocations use `try_reserve`).
* Index paths now return exactly the scan result: sparse u above 2^32/2^64
  are stored exactly in postings; cells of COLUMN/PACKED/CUSTOM layouts are
  not indexed and the planner uses an index only where all layouts of the
  realm are CLASSIC and show the field under the queried name; range lookups
  ignore None/NaN/incomparable values (no `TypeError`); `== NaN` never
  matches; extensions that survive `delete(realm, u)` stay indexed.
  (Databases written by 0.27.1 must be re-imported anyway, see above.)
* SECONDARY index blocks are CRC-checked before first use (`CorruptionError`
  instead of silently wrong results); DATA prefixes are checked for
  plausibility on open.
* A read of an `u` that no DATA segment covers CRC-checks the next higher
  DATA block once per handle: a flipped `u_start` in a DATA prefix now
  raises `CorruptionError` instead of returning `None`; this also
  holds for `range`, `select` and `read_numpy` over such a gap.
* SPARSE blocks are CRC-checked during the open scan: a flipped realm,
  `u_start` or `u_end` in a SPARSE prefix previously hid the overlay and
  silently returned the older BASE value; it now raises `CorruptionError` on
  open. Cost: about 0.1 ms per MiB of sparse data per open.
* A DATA prefix whose realm does not match the realm of its shape raises
  `CorruptionError` on open (flipped realm varint).
* The CLASSIC_U fastpath returns the same row shapes as the general path
  (`record` envelopes, structural/unknown `select` columns); NaN group values
  form separate groups.
* Deeply nested query/SQL expressions raise `QueryPlanError`/`SQLSyntaxError`
  (limit 256 levels) instead of crashing the process; long AND/OR chains work.
  Every switch between AND, OR and NOT counts as one level (programmatic
  alternating `(q & a) | b` chains deeper than 128 rounds are rejected, the
  message explains how to flatten them); `explain=True` works up to the limit.
* Constant folding in MATH expressions respects `math_max_intermediate_bits`.
* Damaged or manipulated files raise `CorruptionError` instead of
  `IndexError`, panics or huge allocations (overflow-checked lengths, sparse
  group counts and posting counts checked against the payload, zlib size
  limit, JSON depth limit 256).
* `repair_file` refused databases with more than two schema generations.
* A failed `compact_file` removes the files it created (an existing export
  target leaves only its `.lock` file; a `<db>.compacting` left by a killed
  process is removed by the next compaction); an OS error at the export
  target names the output path in `OSError.filename`.
* A failed `create` removes the `<db>.lock` it created (a pre-existing lock
  file is kept).
* `lock=False`: closing or committing a handle that changed nothing no longer
  writes its stale superblock over a newer commit of another writer (which
  made an acknowledged commit invisible and let a later crash of that writer
  cut it). A `lock=False` handle with changes re-reads the superblocks before
  writing and raises `DatabaseLockedError` ("concurrent writer detected")
  instead of resetting the other writer's commit boundary; `lock=False` is
  meant for a single process – readers use `readonly=True`.
* `inspect_tail()` gives clear messages for files shorter than 8 KiB and for
  foreign files.
* Windows: atomic replacement retries briefly when another process holds the
  file (sharing violation). It keeps the POSIX rename semantics of the
  standard library (a file still open in a read-only handle can be replaced)
  and flushes the replaced file afterwards in the selected durability mode,
  so a replaced layout plan, a compacted or a restored database is durable
  once the call returns. Creating a database flushes its moved
  files the same way.

### Performance

* Index build memory: posting lists are collected as `u32` (widening
  to `u64` and exact BigInt values only when needed) and the scan reads the
  data blocks past the block cache. Three indexes over 10 M records: peak memory
  467–484 → 273–275 MiB, build time unchanged (≈ 4.2 s). Before the scan,
  `build_indexes` estimates the memory: if the selected indexes together
  would exceed half of the physical memory, they are built in several scans
  automatically (new result field `passes`); an index that alone exceeds the
  physical memory raises `MemoryError` before anything is written, instead
  of exhausting the memory or aborting the process. On-disk format and index
  size unchanged.
* Read path: every block is CRC-checked completely once per handle and then
  marked as verified; later accesses read only the needed byte range. Single
  accesses into large blocks (e.g. a batch of 1M records) are about 1 400×
  faster (`get` 4.4 ms → 3 µs), scattered reads beyond the cache 5–25×
  faster. `[cache] block_cache_bytes` is now effective (LRU eviction instead
  of clearing the whole cache); `stats()["block_cache"]` shows budget,
  occupancy and counters.
* Sparse writes no longer update every cached v index of the realm/lane (an
  index catches up lazily on its next read) and no longer clear the decoded
  sparse records: alternating `put_cell`/`get_value` with many cached u
  groups is about 60× faster.
* Opening reads block headers, DATA/SPARSE prefixes and the offset-table
  check through a read-ahead window instead of three reads per block: open
  of 1M records in 20 000 blocks 25 ms → 4.9 ms, in 1 000 blocks
  1.36 ms → 0.99 ms (the prefix plausibility check is unchanged).
* `get_value`/`get_cell` decode only the requested BASE field of a dense
  record when the realm/lane has no sparse overlays (`get_value` 2.6×,
  `get_cell` 1.25× faster).
* Queries: `range`/`select` find the next stored u in O(log segments)
  and skip gaps (also the Rust API `Db::range`/`Db::select`, which counted u by
  u); 80 000 single-record segments 3.6 s → 0.25 s (`select` 0.06 s).
* Queries: `Q.u.between(lo, hi)` over more than 4096 u visits only the
  stored u of the range instead of scanning the realm (1 M records: 274 →
  10 ms).
* Queries: for a realm with one plain CLASSIC layout and no overlays,
  `COUNT(*)` comes from the segment metadata after a one-time checksum
  check of the DATA blocks per handle (0.76 s → 1.9 ms, repeated 0.03 ms),
  `NOT`/`ALL`
  need no record decoding, `ORDER BY … LIMIT` builds only the window and
  `GROUP BY` streams the rows (1 M records: 2.6 s → 0.6 s and 2.1 s →
  0.56 s, less memory). Results are identical to the general path.
* Indexes: unions of several postings are sorted as `u64`
  (VALUE ranges 1.7–2.2× faster); the index build keeps u as `u64` until the
  first u > 2^64 (half the memory per reference). BigInt u keep their exact
  postings; no format change.
* Planner: nested AND/OR reuse the plan of `execute`; indexed
  `FIELD`/`AND` queries are planned only for `explain` (POSTING_COUNT
  3.1 → 2.0 µs).
* Layouts: `plan_layout_evolution` counts sparse u groups at risk in
  linear time.
* Compaction (`compact_file`) runs with memory bounded by one batch: BASE
  records are copied as raw bytes, tombstones are checked on demand, sparse
  cells are written in chunks (1 M records: 1.9 GiB → 67 MiB peak RSS,
  3.5 s → 1.2 s).
* `put_math` into already mapped realms skips the realm planner and the full
  state validation; its cost no longer grows with the number of realms
  (2 000 realms: 890 → ≈60 000 calls/s).
* `durability="full"` on macOS: the sync before the superblock is an
  `F_BARRIERFSYNC` (ordering), only the superblock sync is an `F_FULLFSYNC`
  (≈6.0 → ≈3.6 ms per commit; same guarantees).
* Long calls release the GIL while they run: `query`, `sql`, `sql_numpy`,
  `explain_query`, `QueryEngine` methods, `health`, `stats`, `verify`,
  `read_numpy`, the batch writers, `build_indexes`, `commit`, `rollback`,
  `close`, opening/creating and the file operations (`compact_file`,
  `repair_file`, `restore_backup`); Python providers and plugins re-acquire
  it; four threads with own handles run SQL scans ≈2.9×
  faster than one. `get`/`find_u` keep the GIL (calls of a few µs get slower
  when it is handed over per call).
* `read_numpy`/`sql_numpy` hand the result vectors to NumPy without a copy
  (writable arrays referencing a `ColumnBuffer`); 1 M records × 11 fields:
  peak RSS 309 → 240 MiB.
* `read_numpy` decodes numeric BASE values straight into the typed columns
  (records without sparse overlay or extensions); 1 M records × 11 fields:
  3.9 → 6.5 M records/s.

### Removed

* The open parameter `strict` (it had no effect); use `readonly=True` to
  open without changing the file. Passing `strict=` raises `TypeError`.

## [0.27.1] – 2026-10-18

### Added

* Prebuilt wheels for CPython 3.9–3.14 on Linux (x86_64, aarch64;
  manylinux_2_28), macOS (x86_64, arm64) and Windows (x64), published on
  PyPI: `pip install nhydb`.

### Changed

* `close()` releases the file handle and the write lock immediately, not only
  when the object is released. Afterwards every file access raises
  `NHyDBError` ("database is closed"); previously reads on a closed database
  could still succeed. Repeated calls have no effect.
* If the commit in `close(commit=True)` fails, the database stays open (retry,
  or discard with `close(commit=False)`). `close(commit=False)` releases handle
  and lock even if writing the buffer fails; uncommitted data never becomes
  visible, also not after reopening.

### Fixed

* Windows: `compact_file()` failed with "Access is denied (os error 5)" when
  the database had been closed but the object was still referenced.
* Windows: the Rust corruption test no longer uses Unix-only positional file
  I/O.
* Windows: the Python test for rolling back a failed append relies on the
  POSIX file size limit and is skipped on Windows; the same property is
  checked on all platforms by a Rust test with injected write errors.

## [0.27.0] – 2026-10-17

First public release under the NHyDB / dNHyDB Experimental Software License 1.0
(source-available, non-production use only; see `LICENSE.md`).

* Native Rust storage engine (`nhydb-core`) with cell identity
  `(R, u, v, lane)`; `N` is computed arithmetically, never stored as a key.
* Dense BASE records, sparse cells, two lanes, layouts CLASSIC / COLUMN /
  PACKED / CUSTOM, mixed-realm routing, layout evolution, geometric GET.
* Append-only container (file identifier `NHYDB016`) with commit boundary,
  checkpoints, verify, health, stats, repair and compaction; durability
  `fsync` / `full`; portable write lock; database size limit.
* System space R0,u1 and an encrypted external layout plan (AES-256-GCM +
  HMAC, two-generation commit), fail-closed opening.
* Secondary indexes VALUE / TOKEN / TRIGRAM / FULLTEXT (positions, BM25).
* MATH_MAP expression language with resource limits, realm planner.
* QueryEngine (cost-based planner, aggregation, BM25 search) and NHySQL
  (read-only `SELECT`).
* Python package `nhydb` (PyO3) with single-operation API and NumPy batch
  API; command line `nhydb`.
* Platforms: macOS, Linux, Windows.
