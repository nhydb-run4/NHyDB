# NHyDB

**A database in which every value has a mathematical address.**

NHyDB is a general-purpose storage engine built on the **Dynadron**, a
geometric arrangement of the natural numbers. Instead of tables and primary
keys, every value lives in a cell of a structured address space; the
structure itself provides order, and each cell has an exact natural number
`N` that is computed – never stored. The engine is written in native Rust and
used from Python.

[![PyPI](https://img.shields.io/pypi/v/nhydb)](https://pypi.org/project/nhydb/)
· Python 3.9–3.14 · Linux, macOS, Windows ·
[License: source-available, non-production](#license)

**→ [Python Cookbook](https://github.com/nhydb/NHyDB/blob/main/docs/cookbook/PYTHON_COOKBOOK.md)** –
task-oriented guide with tested examples for the complete Python API.

**→ Interactive Jupyter Notebooks — [English](https://github.com/nhydb/NHyDB/blob/main/notebooks/en/README_v1.md) | [Deutsch](https://github.com/nhydb/NHyDB/blob/main/notebooks/de/README_v1.md)** –
22 runnable notebooks per language, including all 57 Python Cookbook examples.

## The addressing idea

A cell is addressed by `(R, u, v, lane)`: realm `R`, record `u`, field or
position `v`, and one of two lanes (`NL`, `NR`). In realm 1 its number is

```text
G = u + v        N = G² − u  (lane NL)        N = G² + v  (lane NR)
```

so `(u, v) = (11, 13)` is `N = 565` on NL and `N = 589` on NR. Every further
realm occupies its own range, defined by a fixed realm span of 10¹⁰⁰. The
mapping is exact in both directions (`N ↔ (R, u, v, lane)`), which lets you
reach data by coordinates, by record or directly by number.

## Installation

```bash
pip install nhydb
pip install numpy        # optional, for the NumPy batch API
```

Prebuilt wheels are available for CPython 3.9–3.14 on Linux (x86_64,
aarch64), macOS (x86_64, arm64) and Windows (x64). No Rust toolchain is
needed; building from source is described in
[`BUILDING.md`](https://github.com/nhydb/NHyDB/blob/main/BUILDING.md).

## A first example

Create a database, write two records, close it, open it again and read:

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "cities.nhydb")

# Create: realm 1, a record shape with two typed fields, a CLASSIC layout.
with nhydb.create(path) as db:
    db.create_realm(1, "main")
    db.create_shape(1, 1, "city", [{"v": 1, "name": "name", "type": "utf8"},
                                   {"v": 2, "name": "population", "type": "varuint"}])
    db.create_classic_layout(1, 1, "cities", shape_id=1)
    db.put_dense_batch(1, 1, [["Berlin", 3_685_000], ["Hamburg", 1_862_000]], shape_id=1)
# Leaving the block commits and closes the database.

with nhydb.open(path) as db:
    print(db.get(1, 2))
    print(db.sql("SELECT name FROM cities WHERE population > 2000000")["rows"])
print(sorted(os.listdir(os.path.dirname(path))))
```

```text
{'_realm': 1, '_u': 2, '_lane': 0, '_lane_name': 'NL', '_shape': 1, 'name': 'Hamburg', 'population': 1862000}
[{'name': 'Berlin'}]
['cities.nhydb', 'cities.nhydb.layout.enc', 'cities.nhydb.layout.key', 'cities.nhydb.lock']
```

A database consists of **three files that belong together**: the data file
(`.nhydb`), the encrypted layout plan (`.layout.enc`) and its key
(`.layout.key`). Back them up and move them together, and protect the key
file – without it the database cannot be opened. The `.lock` file is only
the writer lock. Details:
[Cookbook, section 21](https://github.com/nhydb/NHyDB/blob/main/docs/cookbook/PYTHON_COOKBOOK.md#21-database-files-backup-restore-and-moving).

## What NHyDB offers

* **Storage model:** realms, typed record shapes, dense records and sparse
  cells, two lanes, null values and tombstones; layouts CLASSIC, COLUMN,
  PACKED and CUSTOM (Python plugins), routing across layouts, layout
  evolution.
* **MATH_MAP:** place values by formulas in a safe expression language with
  resource limits.
* **Indexes and queries:** VALUE, TOKEN, TRIGRAM and FULLTEXT indexes (BM25),
  the `QueryEngine` with a cost-based planner and aggregation, and NHySQL
  (read-only `SELECT`).
* **NumPy batch API:** write and read whole columns; the loops run in Rust.
* **Persistence and integrity:** append-only file with an explicit commit
  boundary, crash recovery, verify, health, repair and compaction;
  durability `fsync` or `full`; one writer with an exclusive lock.
* **Protected schema:** the layout plan is stored outside the data file,
  encrypted (AES-256-GCM, HMAC-SHA256); opening fails closed on any
  inconsistency.

Measured throughput figures and their exact test conditions are documented
in the
[technical documentation, section 18.5](https://github.com/nhydb/NHyDB/blob/main/docs/en/NHyDB_Technical_Documentation.md#185-benchmarks).

## Learn NHyDB interactively — Jupyter Notebooks

NHyDB comes with a complete interactive notebook series in
**[English](https://github.com/nhydb/NHyDB/blob/main/notebooks/en/README_v1.md)** and **[Deutsch](https://github.com/nhydb/NHyDB/blob/main/notebooks/de/README_v1.md)**.
Each language contains 15 guided notebooks and 7 reference notebooks.

The guided notebooks connect the mathematics of the Dynadron with real
database experiments. Each idea is explained briefly and then shown with code
you can run; the diagrams and visualizations are drawn from data the engine
actually stored. Topics include mathematical addressing, records and layouts,
MATH_MAP, storage on disk and persistence, indexes and queries, NumPy batches,
the encrypted layout plan and its key (ENC/KEY), benchmarks on your own
machine and a complete small application. You can execute every example,
change its parameters and run your own experiments.

The seven reference notebooks contain all 57 tested examples of the
[Python Cookbook](https://github.com/nhydb/NHyDB/blob/main/docs/cookbook/PYTHON_COOKBOOK.md)
as runnable cells, each followed by the output documented in the cookbook.
The cookbook remains the authoritative reference; the notebooks let you run
and explore its examples step by step.

The notebooks were written for NHyDB 0.27.1 and are stored with their outputs,
so they can also be read directly on GitHub. To run them locally, clone the
repository and start JupyterLab in its `notebooks/` folder; it opens in your
browser:

```bash
git clone https://github.com/nhydb/NHyDB.git
cd NHyDB/notebooks
python -m pip install "nhydb==0.27.1" numpy pandas matplotlib jupyterlab ipykernel
python -m jupyterlab
```

Step-by-step instructions are in the notebook READMEs.

## Documentation

* **[Python Cookbook](https://github.com/nhydb/NHyDB/blob/main/docs/cookbook/PYTHON_COOKBOOK.md)** –
  installation, records and cells, addresses, types, indexes, QueryEngine,
  NHySQL, NumPy, layouts, MATH_MAP, configuration, maintenance, backups,
  errors and locking, complete application examples and an API reference.
  All examples are executed by the test suite.
* **Jupyter notebooks:** [English](https://github.com/nhydb/NHyDB/blob/main/notebooks/en/README_v1.md) ·
  [Deutsch](https://github.com/nhydb/NHyDB/blob/main/notebooks/de/README_v1.md) – 15 guided notebooks
  and 7 reference notebooks per language. The reference notebooks present the
  57 cookbook examples as runnable cells with the cookbook's expected output.
* [Technical documentation (English)](https://github.com/nhydb/NHyDB/blob/main/docs/en/NHyDB_Technical_Documentation.md) ·
  [Technische Dokumentation (Deutsch)](https://github.com/nhydb/NHyDB/blob/main/docs/de/NHyDB_Technische_Dokumentation.md) –
  mathematics, architecture, file format, guarantees.
* [Example programs](https://github.com/nhydb/NHyDB/tree/main/examples/python) ·
  [example configuration](https://github.com/nhydb/NHyDB/blob/main/config/nhydb_config.example.toml) ·
  [`CHANGELOG.md`](https://github.com/nhydb/NHyDB/blob/main/CHANGELOG.md)
* Command line: `nhydb --help` (or `python -m nhydb`); query CLI:
  `python -m nhydb.query --help`.

## Sibling project: dNHyDB

[**dNHyDB**](https://github.com/nhydb/dNHyDB) (`pip install dnhydb`) applies
the same addressing principle in a deliberately reduced form: one realm,
direct `u64` coordinates and a single record layout, aimed at simple and
fast record storage with snapshot readers in other processes. Choose **NHyDB** for the
full model – realms, several layouts, MATH_MAP, text indexes and SQL – and
**dNHyDB** for a compact record store.

## Scientific background

The mathematical foundations (in German):

* *Das Dynadron als mathematisches Datenbankprinzip* –
  [doi:10.5281/zenodo.23438748](https://doi.org/10.5281/zenodo.23438748)
* *Das Dynadron der natürlichen Zahlen* –
  [doi:10.5281/zenodo.22549415](https://doi.org/10.5281/zenodo.22549415)
* *Geometrie und Arithmetik im Dynadron* –
  [doi:10.5281/zenodo.22694525](https://doi.org/10.5281/zenodo.22694525)

## Known limitations

* **One writer at a time** per database (exclusive lock); there are no
  multi-process transactions.
* **The key file is required.** A lost `*.layout.key` cannot be recovered.
* **NHySQL is read-only** (`SELECT`); data is written through the API.
* **File format:** only files with the identifier `NHYDB016` are read; there
  is no converter for older format revisions.
* **Durability `full`:** macOS uses `F_FULLFSYNC` and reports an error if the
  file system does not support it; Linux uses `fsync(2)`, Windows
  `FlushFileBuffers`.
* **Configuration:** the `[cache]` keys are accepted but not yet used; the
  open parameter `strict` currently has no effect.
* **Status:** alpha – the API may still change between minor versions.

## License

NHyDB is licensed under the **NHyDB / dNHyDB Experimental Software License
1.0** ([`LICENSE.md`](https://github.com/nhydb/NHyDB/blob/main/LICENSE.md)).
The source code is available, but this is **not an open-source license**:
free use is limited to non-production purposes (unlimited for private
individuals, time-limited for teaching, research and businesses), and
**production use is not permitted** without a separate written license.
Please read the license text before using the software.

The compiled extension module contains third-party Rust components under
MIT/Apache-2.0-style licenses; their notices are in
[`THIRD-PARTY-LICENSES.txt`](https://github.com/nhydb/NHyDB/blob/main/THIRD-PARTY-LICENSES.txt),
which is also shipped inside the wheel.

## Development

Building from source: [`BUILDING.md`](https://github.com/nhydb/NHyDB/blob/main/BUILDING.md) ·
release procedure: [`RELEASING.md`](https://github.com/nhydb/NHyDB/blob/main/RELEASING.md)

```bash
cargo test --workspace --release        # Rust core
pip install ".[test]"
python -m pytest tests/python           # Python package, cookbook and examples
```

## Contact

Achim Meissner – achim.meissner@nhydb.de
