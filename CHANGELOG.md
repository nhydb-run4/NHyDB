# Changelog

All notable changes to this project are documented in this file.
The project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
