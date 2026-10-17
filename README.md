# NHyDB

**NHyDB** is a general-purpose database with mathematical cell addressing.
Every value lives at a cell `(R, u, v, lane)` – realm, record, field/position
and lane. The space itself provides order, so ordinary records (`u`) and
fields (`v`) work without tables or a primary index. The NHyDB number `N` of a
cell is computed arithmetically when needed; it is never a physical storage
key.

The storage engine is written in native Rust; the Python package `nhydb`
binds it via PyO3 and offers single operations as well as NumPy batch
operations.

Version: **0.27.0** · Platforms: macOS, Linux, Windows · License: see
[`LICENSE`](LICENSE)

## Features

* **Storage core:** dense BASE records, sparse cells (null ≠ tombstone ≠
  missing), two lanes, layouts CLASSIC / COLUMN / PACKED / CUSTOM,
  mixed-realm routing, layout evolution, geometric GET (`cell` / `record` /
  `auto`).
* **Persistence and integrity:** append-only container with commit boundary,
  checkpoints, verify, health, stats, repair, compaction (in place or export),
  durability modes `fsync` / `full`, portable write lock, database size limit.
* **System space and layout plan:** schema and layout plan are stored outside
  the data file, encrypted (AES-256-GCM + HMAC) with a two-generation commit;
  opening fails closed on any inconsistency.
* **Indexes:** VALUE, TOKEN, TRIGRAM and FULLTEXT (positions, BM25).
* **MATH_MAP:** a safe expression language with resource limits that maps
  values to cells mathematically; realm planner, exclusive MATH realms,
  rollback.
* **Queries:** `QueryEngine` (cost-based planner, aggregation, BM25 search)
  and NHySQL (read-only `SELECT`).
* **Python:** single-operation API and NumPy batch API (`put_numpy_batch`,
  `put_cells_numpy`, `put_objects_numpy`, `read_numpy`, `put_math_numpy`,
  `sql_numpy`, `QueryEngine.execute_numpy`); the loops run in Rust.
* **Rust:** the core crate `nhydb-core` has no Python dependency and can be
  used on its own.

## Installation

From a prebuilt wheel (see the *Releases* page of the repository):

```bash
pip install nhydb-0.27.0-<platform>.whl
pip install numpy          # optional, for the NumPy batch API
```

From source (requires a Rust toolchain, see [`BUILDING.md`](BUILDING.md)):

```bash
pip install .
```

## Quick start

```python
import nhydb
from nhydb.query import Q, QueryEngine

with nhydb.create("data.nhydb") as db:
    db.create_realm(1, "main")
    db.create_shape(1, 1, "row", [{"v": 1, "name": "id", "type": "varuint"},
                                  {"v": 2, "name": "status", "type": "utf8"}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[u, "idle" if u % 3 else "active"] for u in range(1, 1001)], shape_id=1)
    db.create_index(1, 1, "idx_status", "status")
    db.build_indexes(commit=True)
    print(db.sql("SELECT status, COUNT(*) AS n FROM records GROUP BY status ORDER BY n DESC")["rows"])
    print(QueryEngine(db).execute(Q.field("status") == "active", realm=1, return_mode="count")["count"])
```

A database consists of the data file (`data.nhydb`) plus the encrypted layout
plan (`data.nhydb.layout.enc`) and its key file (`data.nhydb.layout.key`).
Keep all three together and protect the key file.

Command line: `nhydb --help` (or `python -m nhydb`), query CLI:
`python -m nhydb.query --help`.

## Configuration

Operational settings (size limit, durability, lock, MATH limits, paths) can be
set via an optional `nhydb_config.toml`; see
[`config/nhydb_config.example.toml`](config/nhydb_config.example.toml).

## Known limitations

* **One writer at a time.** A writing process holds an exclusive lock
  (`<db>.lock`); there are no multi-process transactions.
* **Key file required.** Without the matching `*.layout.key` file a database
  cannot be opened (fail closed); there is no recovery of a lost key.
* **NHySQL is read-only** (`SELECT` only); data is written through the API.
* **File format:** only files with the identifier `NHYDB016` are read; there
  is no converter for older format revisions.
* **Durability `full`:** macOS uses `F_FULLFSYNC` and reports an error if the
  file system does not support it (no silent fallback); Linux uses `fsync(2)`,
  Windows `FlushFileBuffers`.
* **Configuration:** the `[cache]` keys are accepted but not yet used by the
  engine; the open parameter `strict` currently has no effect.
* **Platforms:** developed and tested primarily on macOS (Apple Silicon);
  Linux and Windows wheels are built and tested by CI.

## Documentation

* [Technical documentation (English)](docs/en/NHyDB_Technical_Documentation.md)
* [Technische Dokumentation (Deutsch)](docs/de/NHyDB_Technische_Dokumentation.md)
* [`BUILDING.md`](BUILDING.md) – building wheels from source
* [`RELEASING.md`](RELEASING.md) – versioning and release procedure
* [`CHANGELOG.md`](CHANGELOG.md)

## Tests

```bash
cargo test --workspace --release        # Rust core
pip install ".[test]"
python -m pytest tests/python           # Python package (against the installed wheel)
```

Some differential tests compare the engine against an external reference
implementation that is not part of this repository; they are skipped
automatically.

## Repository layout

```text
crates/nhydb-core   native Rust core (no Python dependency)
crates/nhydb-py     PyO3 module nhydb._native
python/nhydb        Python package: nhydb, nhydb.kernel, nhydb.index,
                    nhydb.math, nhydb.mathlayout, nhydb.query, CLI
tests/python        Python tests
config/             example configuration
docs/               documentation (de, en)
.github/workflows/  CI: tests and wheels for Linux, macOS, Windows
```

## Contact

Achim Meissner – achim.meissner@nhydb.de
