# Changelog

All notable changes to this project are documented in this file.
The project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `benchmarks/kq10m/`: KQ-10M benchmark for your own machine (writing,
  indexing and reading 10 million records, NHyDB only), with a Markdown/JSON
  report and an issue template for sharing results. Requires NHyDB 0.28.2 or
  newer.
- `benchmarks/kq10m/results/`: reference results of NHyDB 0.28.2 (Apple M1 Max,
  Lenovo ThinkPad P52 with Windows 11 and an Ubuntu Hyper-V VM).

## [0.28.2] – 2026-11-11

**Engine 0.28.2, file format 0.28.2.** Files start with the signature
`NHYDB` followed by the version bytes 0, 28, 2 (the historical numbering
`NHYDB016` ends). New block types 14 `DIRECTORY` and 15 `SECONDARY_PAGE`,
the superblock field `dir_root`, a prefix checksum in DATA_DENSE (`D28D`)
and SPARSE_CELLS (`SP28`) payloads and SECONDARY indexes as key trees
(`SI28`); all other blocks and payloads are unchanged. Only format 0.28.2 is
read: files of NHyDB 0.28.0 and 0.27.1 (`NHYDB016`) and of an unreleased
development format are rejected with `CorruptionError` naming the format
(see *Removed*).

### Highlights

* **Block directory and file format 0.28.2:** opening a file with one
  million single-cell blocks takes ~5 ms / ~7 MB instead of ~0.3 s /
  ~230 MB; prefix checksums in the data payloads.
* **SECONDARY indexes as persistent key trees** with an index build of
  bounded memory (external sort): first lookup 0.2 ms instead of 200 ms,
  indexes 5.5x smaller on disk.
* **Automatic index maintenance:** indexes stay usable after writes and
  renew themselves; `build_indexes()` is only needed for new indexes.
* **Central RAM limiter** per process: large operations wait or spill
  instead of exhausting memory; last resort `ResourceLimitError`.
* **Large result sets:** faster query planning, streaming rows with
  `db.sql_iter()`, result files (`db.sql_to_file()`), background queries
  (`db.submit_sql()`) and bounded `ORDER BY` with sorted runs on disk.
* **Delete API:** `delete_cell`, `delete_area` and `delete_record`, also
  inside `db.batch()`.
* **The newest write wins** across dense and sparse storage on every path.
* **Fast queries on overwritten data:** `COUNT(*)`, field scans and `NOT`
  after overwrites, deletes or sparse cells (KQ-10M field scan 17.7–23.6 s →
  1.0–2.2 s).
* **`read_numpy(..., masks=True)`** tells live cells apart from deleted or
  missing ones.
* **`cleanup_compaction()`** removes the staging file of an interrupted
  in-place compaction.

### Upgrade notes

* **Files of NHyDB 0.28.0 and 0.27.x do not open** (`CorruptionError` naming
  the format; the file stays unchanged). There is no migration path: rebuild
  such databases from the source data or exports.
* **Python 3.12–3.15 only** (`requires-python = ">=3.12"`); Python 3.9–3.11
  are no longer supported.
* **Delete API:** `delete(realm_id, u)` and `delete_point(realm_id, u, v)` are
  removed without aliases. Use `delete_record` (whole record), `delete_area`
  (by realm coordinates) or `delete_cell` (former `delete_point`). Return
  values changed (see *Changed*).
* **DENSE fields reject `None`** with `ValueError`; store a null cell with
  `put_cell(..., None)`.
* **`put` replaces the BASE record as a whole**; a tombstoned BASE field is
  no longer reported as present.
* **New default bound `[query] max_result_bytes` = 4 GiB:** calls that return
  the whole result at once raise `QueryLimitError` beyond it (before: no
  bound). Use `sql_iter()`/`sql_to_file()` for larger results or set 0.
* `len(WriteBatch)` counts calls instead of distinct cells; a failed
  `WriteBatch.flush()` ends the batch. Negative administrative IDs raise
  `ValueError`.

### Known limitations

* `delete_area` costs time proportional to the live cells in the area (no
  O(1) range delete; about 15 s for 40 million cells).
* An index on a field missing from the current BASE shape of a realm reads
  only SPARSE cells; dense values of older shapes are not in the index.
* A `batch()` holds every collected call in RAM until `flush()` (about 105 B
  per cell call, about 86 B per record delete): flush in between and use
  `db.delete_area()` for mass deletes.
* RAM limiter on Windows: with a limiter budget that cannot be reached, a
  cache relief can keep the cache budget capped for several seconds up to
  about two minutes (measured 4–131 s) until it is judged ineffective
  (intentional policy).
* A FULLTEXT conjunction on an index with pending changes is about 4× slower
  until its rebuild; every index rebuild leaves the old trees as dead space
  until a compaction.

### Added

* **Delete API.**
  - `delete_area(realm_id, u_from, u_to, v_from, v_to, *, lane=None,
    commit=False)` deletes every live cell of a rectangle of a realm (bounds
    inclusive), independent of layout, BASE/extension and dense/sparse
    storage, and returns the number of live cells deleted. The call is
    atomic: a size limit, I/O error or RAM-limiter cancel leaves nothing of
    it. Memory stays bounded (macOS M1 Max: peak RSS +41 MB for 3.2 million
    deleted cells).
  - `WriteBatch.delete_record(layout_id, object_id)` and
    `WriteBatch.delete_area(...)` next to `put_cell`/`delete_cell`. The batch
    keeps the call order and is written as one unit. `flush()` reports
    `"deleted"` (the sum the single calls would return); `blocks` and
    `bytes` are what the batch appended.
* **`read_numpy(..., masks=True)`** adds `"_cells"`: field name → bool array,
  `True` where the cell lives (a stored `None` included). Without `masks` the
  output is unchanged.
* **`cleanup_compaction()` and the health check `compaction_leftover`.** A
  `<db>.compacting` staging file left by a killed in-place compaction is
  reported by `health()` (path, size, `removable`) and removed by
  `cleanup_compaction()` (own lock required) or when a writer opens the
  database.
* **Automatic index maintenance.** After writes an index answers from its
  built part plus an in-memory overlay of the changed records (VALUE, TOKEN,
  TRIGRAM, FULLTEXT incl. phrases and BM25 ranking); results equal a full
  rebuild. After a commit the writer renews stale indexes by a merge rebuild
  without a data scan (KQ-10M 1.4–2.3 s instead of 4.3–6.7 s; FULLTEXT over
  10 million documents 12.6 s instead of 36 s), deferred during bulk commits
  and while the RAM limiter is not green; an index that was never built is
  built at the next commit. A failed rebuild never touches committed data.
  - New `[index]` settings: `maintenance` (`"auto"`, or `"manual"` = only
    `build_indexes()`), `rebuild_ratio` (0.02), `rebuild_min_changes`
    (1024), `bulk_commit_ratio` (0.002), `crisis_ratio` (0.25),
    `overlay_max_bytes` (256 MiB).
* **Central RAM limiter.** One limiter per process compares the process
  footprint (macOS `phys_footprint`, Linux RSS within the cgroup limit,
  Windows commit) with `(100 − reserve_percent) %` of the RAM. Large
  operations (queries, conversions, sorts, index builds, write calls,
  deletes, batch flushes, `compact_file`) check it at chunk boundaries;
  single operations are not affected. Yellow zone: caches are emptied,
  sorts and index builds spill early, new large operations wait; red: all but
  the oldest wait. As a last resort (`cancel_over_limit`, default on) the
  operation that keeps the process above its limit is cancelled with the new
  `ResourceLimitError` (`reason`; a write call is rolled back). The GIL is
  never held while waiting.
  - New: `nhydb.resource_status()`, `nhydb.cancel_job(id)`, section
    `[limiter]` (`enabled`, `reserve_percent`, `warn_percent`, `tick_ms`,
    `notify_wait_s`, `wait_timeout_s`, `cancel_over_limit`, `probe_bytes`,
    `ram_bytes`, `dry_run`).
  - Statistics file `limiter.txt` (`[limiter_log]`, default on): one JSON
    line per interval (default 30 minutes; `interval_min` 1 … 527,040),
    archived as ZIP from 60 MB; default location is the directory of the
    first database of the process.
* **Streaming rows:** `db.sql_iter()` yields the rows of a SELECT in chunks
  (same rows and order as `sql()`); `sql_numpy()` builds its columns from
  streamed rows (KQ 5 million rows x 11 fields: 21.8 s / 24.3 GB → 8.9 s /
  0.95 GB; `sql_iter` 11.0 s / 0.35 GB, first row after 87 ms).
* **Result files and background queries:** `db.sql_to_file()` streams a
  SELECT into a CRC-protected result file (`.nhyc`, layout in the technical
  documentation, section 14.5) and returns a `nhydb.ResultFile` (iterate,
  `to_numpy()`, `fetchall()`, `pin()`/`unpin()`, `delete()`);
  `db.submit_sql()` runs it in a background thread on a read-only snapshot
  (`nhydb.ResultJob`). `nhydb.cleanup_results()` removes leftovers of ended
  processes (no expiry by time). KQ 5 million rows x 11 fields: 8.9 s,
  477 MB (82 MB with zlib).
  - `[results]` settings: `temp_dir` (default `<system temp>/nhydb-results`),
    `budget_bytes` (0 = none), `compress`, `sort_memory_bytes` (beyond it a
    streaming `ORDER BY` sorts in runs on disk).
* **`[query] max_result_rows`** (default 0 = no bound): queries whose
  estimated result rows exceed it are rejected before they run with the new
  `QueryLimitError` (subclass of `QueryError`); `explain()` and
  `sql("EXPLAIN …")` report `hints` for large estimates.
* **`[query] max_result_bytes`** (default 4 GiB, 0 = none): `sql()`,
  `QueryEngine.execute()`/`search()`, `sql_numpy()`, `find_u()`, the `find()`
  iterator and `ResultFile.fetchall()`/`to_numpy()` raise `QueryLimitError`
  beyond it. Iterating a `ResultFile` stays unbounded.
* **Process-wide cache budget** `[cache] total_bytes` (default 4 GiB) for all
  open database objects of a process (defined by the first object opened;
  a different later value is reported as a warning). New pools
  `[cache] posting_cache_bytes` (256 MiB), `sparse_cache_bytes` (128 MiB)
  and `index_page_cache_bytes` (32 MiB); `[cache] vindex_max_marks`
  (4 000 000 ≈ 192 MB) and `vindex_max_indexes` (4 096) are now used.
  `stats()["cache"]` reports the budget and every pool.
* **SECONDARY indexes as persistent sorted key trees** (pages of about
  4 KiB, read on demand): with 1 million keys per index the first lookup
  takes 0.2 ms instead of 200 ms, memory after 15 000 mixed queries is 75 MB
  instead of 722 MB, the index is 16.8 instead of 92 MB on disk. FULLTEXT
  document lengths are paged as well (0.26 MB heap after a rare-word search
  over 900 000 documents; 0.28.0 kept 49 bytes per document).
  `stats()["block_cache"]` reports the `index_page_*` counters.
* **Index build with bounded memory:** beyond `[index] build_memory_bytes`
  (512 MiB) the keys are spilled to an anonymous temporary file and merged;
  `build_indexes()` reports `spill_rounds` and `spill_bytes`, `passes` is
  always 1. The refusal of indexes larger than the physical memory is gone.
* **Persistent block directory:** written by `commit()` once
  `[directory] flush_blocks` (65 536) blocks or `flush_bytes` (256 MiB) were
  added; opening reads only the directory root and the blocks behind it;
  `[directory] cache_entries` bounds its page cache. Reported in
  `stats()["directory"]` (incl. `last_flush_error`, `failed_flushes`,
  `damaged_blocks_skipped`, `degraded`); `verify(deep=True)` checks it,
  `inspect_tail()` reports `damaged_directory_blocks` and
  `directory_damaged_in_use`. A damaged directory falls back to a full scan;
  `repair_file` drops it losslessly.
* **RD-08: Prefix checksum** for DATA_DENSE and SPARSE_CELLS payloads: the
  open checks the routing prefix instead of whole payloads; the full payload
  CRC is checked before first use.
* **Fast paths on overwritten data:** after overwrites, deletes or sparse
  cells, `COUNT(*)`, `NOT`, field predicates without index and the ORDER-BY
  index window no longer fall back to the slow cell cursor until the next
  compaction (KQ-10M, macOS M1 Max: field scan 17.7–23.6 s → 1.0–2.2 s,
  `COUNT(*)` 16.5–21.9 s → ≤ 0.22 s, `NOT` 34.7–45.4 s → 1.0–2.6 s). New
  settings `[query] overlay_view` (default true) and
  `[query] overlay_view_max_bytes` (256 MiB), `stats()["overlay_view"]`.
* **`[write] max_block_bytes`** (default 8 MiB, 0 = one block per call):
  `put_dense_batch`, `put_numpy_batch` and `compact_file` split large calls
  into several blocks, atomic like one; calls above 4 GiB of record data are
  now accepted.
* Prebuilt wheels for CPython 3.15 (Linux, macOS, Windows).
* Documentation: Windows paths in `nhydb_config.toml` belong in single
  quotes or use `/`.

### Changed

* **Delete API: exactly three functions.** `delete_cell`, `delete_area` and
  `delete_record` work on logical cells; BASE/extension, dense/sparse
  storage and field names make no difference. `delete(realm_id, u)` (hid
  only the dense record) and `delete_point` (now `delete_cell`) are removed.
  `delete_cell` returns `bool` (the cell lived); `delete_area` and
  `delete_record` return the number of distinct live cells deleted
  (`delete_record` no longer counts the dense record as an extra element).
  `delete_cell`/`delete_area` on an exclusive MATH realm raise
  `SchemaError`; `delete_record` on a MATH layout says so in its error.
* **`put` replaces the BASE record, `put_cell` changes one cell.** Fields a
  new shape lacks are no longer part of the record, and an older hidden value
  cannot show again. Cells outside the shape keep following their own writes.
* **Cell existence.** A tombstoned BASE field is no cell (`get_cell(...)
  ["present"]` is `False`; not listed by `get_internal`). A field of the
  current shape written with `put_cell` on a record of an older shape reads
  under its field name and is indexed. `get()` shows unnamed BASE cells also
  with `include_extensions=False`.
* **R9E-01: DENSE fields reject `None`** for every type in `put`,
  `put_dense_batch`, `put_numpy_batch` and `put_record` (`ValueError`);
  before, `utf8` stored `"None"` and `bool` stored `False`.
* **R9D-02/R9B-01: a failed `WriteBatch.flush()` ends the batch:** nothing
  of it is written, every further call raises `RuntimeError` (before, a
  second `flush()` reported success with zero cells).
* **R9E-05:** `len(WriteBatch)` counts the calls since the last flush.
* **R9E-02:** `read_numpy` `_present`: a record whose cells are all deleted
  is absent, a pure sparse record is present with its values.
* **R9E-06:** negative administrative IDs raise `ValueError` (before,
  `create_shape(-5, …)` created shape 0).
* **R9E-07:** `put_math_batch` raises `TypeError` naming a malformed row.
* **R9C-H1:** `index_candidates(TRIGRAM, "contains", p)` with `p` shorter
  than `ngram` raises `ValueError` instead of returning `[]`.
* **Indexes with pending changes** (`index_candidates`, `find_u`, `find`,
  `posting_count` – now exact –, FULLTEXT functions, QueryEngine) answer
  instead of raising `StaleIndexError`; it remains for indexes without a
  built part and beyond `overlay_max_bytes`. `health()` lists them under
  `indexes.details.overlay`, `verify()` warns.
* **Bounded `ORDER BY`:** `sql_iter()`, `sql_numpy()`, `sql_to_file()` and
  `submit_sql()` sort in memory up to `[results] sort_memory_bytes` and in
  runs on disk beyond it, in every realm and layout; the order is always that
  of `sql()`. NaN in a sort column beyond the budget raises `QueryLimitError`.
* `verify(deep=True)` also walks every SECONDARY key tree and the block
  directory; a CRC-valid block found at the wrong place is rejected as
  corruption instead of being read.
* Directory blocks count against `db_max_size_bytes`; opening and closing a
  handle without changes writes nothing.
* `checkpoint()`, `sync()` and `inspect_tail()` release the GIL like
  `commit()`.
* All iterators raise `NHyDBError` ("iterator invalidated") on the next step
  after the same database object discarded data (`rollback()` with pending
  changes, or a concurrent writer detected with `lock=False`); restart the
  query.

### Performance

* Query planning: AND/OR/NOT of indexed predicates (OR of two 5-million
  postings: COUNT 7.9 s → 0.25 s), `ORDER BY … LIMIT` with top-k windows and
  MIN/MAX/COUNT/SUM/AVG/GROUP BY from a VALUE index (KQ-10M: `ORDER BY tq
  DESC LIMIT 10` 12.3 s → 0.04 s, `GROUP BY lage` 11.9 s → 0.18 s), ranked
  FULLTEXT top-k (7.5 s / 2.7 GB → 2.0 s / 1.15 GB), field predicates
  without index (scan of 10 million records 17.7 s → 2.1 s).
* Lower memory in `sql()` (`SELECT *` over 5 million rows: peak 12.2 →
  8.6 GiB), TRIGRAM verification (460 → 55 MiB), NOT (372 → 220 MiB), index
  builds with spilled runs and the first access to large blocks (+239 MB →
  +1 MB).
* Compaction and sparse iteration with bounded memory (~55–60 MB independent
  of the data; 40 M cells: 57 MB, was ~1.4 GB for 5 M cells); a compaction
  with backup checks the free disk space first.
* Sparse cells: one million versions of one cell cost ~0 MB instead of
  ~300 MB, point read ~0.3 µs instead of ~3 ms; out-of-order single-cell
  writes no longer slow down quadratically (one million blocks: 269 s →
  seconds); `COUNT(*)` over 600 000 blocks 0.9 s → 0.1 s.
* RR6-08: block cache without thrashing under random access (sorted output
  of 3 million rows on a 64 MiB cache: 157.8 s → 10.1 s).

### Fixed

* **F1: the newest write wins across dense and sparse.** An older
  `put_cell`/`delete_cell` of a BASE field overrode a later `put` of the same
  field (`put(x=10)`, `put_cell(x=11)`, `put(x=12)` read 11; also in Python
  v0.14/v0.27 – a deliberate deviation from the reference). Now fixed on
  every read path, in the index build and in compaction. No format change;
  existing files read differently at the affected cells.
* `delete_cell` on a cell with a stored `None` now deletes it (before:
  returned `False` and wrote nothing, also inside a batch).
* `build_indexes()` indexed an old value when a newer `put` used a shape
  without the indexed field.
* R9A-04: `get_n_cell`, `get_n(mode="cell"/"auto")`, `get_address` and
  `describe_n` reported deleted or shape-less BASE values as present.
* R9D-01: concurrent use of one handle from two threads raised
  `PanicException` in iterators, `QueryEngine`, `WriteBatch` and a few other
  places; now `RuntimeError` like every other method.
* N2: writes that append several blocks (`put_points`, `put_dense_batch`
  with `extensions_by_u`, `put_extensions(merge=False)`,
  `put_objects_batch`, `put_objects_numpy`, `delete_record`) are undone
  completely when a later block fails; before, a partial write could be
  committed.
* N1: `ORDER BY … LIMIT` in `sql()` outside the CLASSIC fast path keeps only
  the top-k rows instead of collecting every candidate row.
* N3: `python -m nhydb.query` opens the database read-only and no longer
  blocks or is blocked by writers.
* Iterators started before `rollback()` could return discarded or wrong
  values, or raise a false `CorruptionError`; the file was never affected.
* A module built from the sdist no longer warns about a different source
  state.
* Documentation: existence rule per read path, batch memory, `cancel_job`
  return value, `ResourceLimitError`, `sql(..., lane=None)`, index
  maintenance.

### Removed

* **Python 3.9, 3.10 and 3.11.** Supported are CPython 3.12–3.15
  (`requires-python = ">=3.12"`). Existing releases stay installable on older
  Python versions.
* **Reading file format 0.16.** Files of NHyDB 0.28.0 and 0.27.1
  (`NHYDB016`) fail with `CorruptionError` "unsupported file format NHYDB016
  (expected NHYDB 0.28.2)"; the file stays unchanged. Blocks with the tags
  `D10D`, `SP10` and `SI10` are rejected. There is no migration path: rebuild
  such databases from the source data or exports.
* `delete(realm_id, u)` and `delete_point(realm_id, u, v)` (see *Changed*).

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
