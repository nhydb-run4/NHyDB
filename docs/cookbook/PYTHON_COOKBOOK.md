# NHyDB Python Cookbook

Practical, copy-and-paste recipes for the Python package
[`nhydb`](https://pypi.org/project/nhydb/) (version 0.28.0). The recipes
need no knowledge of the Rust core.

Every Python block in this cookbook is a complete program: it has its own
imports, creates its databases in a fresh temporary directory and closes
them again. The blocks are executed automatically by the test suite
(`tests/python/test_cookbook.py`), so what you read here is what the
published package does. Blocks marked `# requires: numpy` need the optional
NumPy package.

## Contents

1. [Installation, import and version](#1-installation-import-and-version)
2. [Concepts in five minutes](#2-concepts-in-five-minutes)
3. [Your first database](#3-your-first-database)
4. [Opening databases and options](#4-opening-databases-and-options)
5. [Commit, sync, close and crash behavior](#5-commit-sync-close-and-crash-behavior)
6. [Records (CLASSIC layout)](#6-records-classic-layout)
7. [Single cells, lanes, null and delete](#7-single-cells-lanes-null-and-delete)
8. [Extensions: additional fields per record](#8-extensions-additional-fields-per-record)
9. [Addresses and the number N](#9-addresses-and-the-number-n)
10. [Reading by N and inspecting addresses](#10-reading-by-n-and-inspecting-addresses)
11. [Data types](#11-data-types)
12. [Reading many records: ranges and iterators](#12-reading-many-records-ranges-and-iterators)
13. [Secondary indexes](#13-secondary-indexes)
14. [QueryEngine and the Q builder](#14-queryengine-and-the-q-builder)
15. [NHySQL](#15-nhysql)
16. [NumPy batch API](#16-numpy-batch-api)
17. [Layouts: COLUMN, PACKED, CUSTOM, routing and evolution](#17-layouts-column-packed-custom-routing-and-evolution)
18. [MATH_MAP: placing values by formulas](#18-math_map-placing-values-by-formulas)
19. [Configuration and limits](#19-configuration-and-limits)
20. [Maintenance: stats, health, verify, repair, compaction](#20-maintenance-stats-health-verify-repair-compaction)
21. [Database files, backup, restore and moving](#21-database-files-backup-restore-and-moving)
22. [Errors, locks and concurrent access](#22-errors-locks-and-concurrent-access)
23. [System space R0,u1](#23-system-space-r0u1)
24. [Command line](#24-command-line)
25. [Complete application examples](#25-complete-application-examples)
26. [Known limitations](#26-known-limitations)
27. [API reference](#27-api-reference)

---

## 1. Installation, import and version

```bash
python -m pip install nhydb            # prebuilt wheels: CPython 3.9–3.14 on Linux, macOS, Windows
python -m pip install numpy            # optional, for the NumPy batch API (section 16)
```

```python
import nhydb

print(nhydb.ENGINE_VERSION)        # product version of the installed package
print(nhydb.FILE_FORMAT_VERSION)   # identifier of the on-disk format
assert nhydb.ENGINE_VERSION == "0.28.0"
assert nhydb.FILE_FORMAT_VERSION == "0.16"
```

```text
0.28.0
0.16
```

`FILE_FORMAT_VERSION` identifies the block format (file identifier
`NHYDB016`); it is not a product version. NHyDB 0.28.0 keeps this
identifier but protects the schema digest with the key: databases created
with NHyDB 0.27.1 or earlier do not open (section 21).

---

## 2. Concepts in five minutes

* **Cell.** Every value lives at a cell `(R, u, v, lane)`: realm `R`,
  record coordinate `u`, field/position `v` and lane `NL` (0) or `NR` (1).
  All coordinates are natural numbers ≥ 1 and may be arbitrarily large.
* **N.** Each cell has a unique natural number `N`, computed from the
  address (section 9). `N` is never stored; it is a mathematical name for
  the cell that you can use to read it.
* **Realm.** A numbered address range (`create_realm`). A realm groups
  shapes, layouts, extensions and indexes.
* **Shape.** The BASE fields of a record (`v = 1 … k`) with names and types
  (`create_shape`).
* **Layout.** How logical records map onto cells. **CLASSIC** (record =
  `u`, field = `v`) is the common case; COLUMN, PACKED, CUSTOM and MATH
  are covered in sections 17 and 18.
* **Dense and sparse.** CLASSIC records (`put`, `put_record`,
  `put_dense_batch`, `put_numpy_batch`) are stored *dense*; single cells,
  extensions and overrides are stored *sparse*. Reading combines both; the
  newest write wins.
* **Commit boundary.** The file is append-only. Data becomes durable with
  `commit()`. Everything after the last commit is discarded when the
  database is opened again.
* **Files.** A database consists of `X.nhydb` (data),
  `X.nhydb.layout.enc` (encrypted schema), `X.nhydb.layout.key` (key – keep
  it safe!) and `X.nhydb.lock` (lock file). See section 21.

---

## 3. Your first database

Create a database, define a realm, a shape and a CLASSIC layout, write two
records, close, reopen and read.

```python
import os
import tempfile

import nhydb

folder = tempfile.mkdtemp()
path = os.path.join(folder, "shop.nhydb")

db = nhydb.create(path)
db.create_realm(1, "shop")
db.create_shape(1, 1, "order", [
    {"v": 1, "name": "id", "type": "varuint"},
    {"v": 2, "name": "customer", "type": "utf8"},
    {"v": 3, "name": "amount", "type": "f64"},
])
db.create_classic_layout(1, 1, "orders", shape_id=1)   # layout 1 in realm 1 uses shape 1

db.put_record(1, 1, {"id": 1, "customer": "Alice", "amount": 19.90})
db.put_record(1, 2, {"id": 2, "customer": "Bob", "amount": 5.50})
db.commit()                                            # make it durable
db.close()

db = nhydb.open(path)
print(db.get_record(1, 1))
print(db.get_value(1, 2, 2))                           # realm 1, u=2, v=2 -> "customer"
db.close()
```

```text
{'_realm': 1, '_u': 1, '_lane': 0, '_lane_name': 'NL', '_shape': 1, 'id': 1, 'customer': 'Alice', 'amount': 19.9}
Bob
```

Fields starting with `_` describe where the record lives. The same program
with a context manager (`with` commits and closes automatically):

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "notes.nhydb")

with nhydb.create(path) as db:
    db.create_realm(1, "notes")
    db.create_shape(1, 1, "note", [{"v": 1, "name": "text", "type": "utf8"}])
    db.create_classic_layout(1, 1, "notes", shape_id=1)
    db.put_record(1, 1, {"text": "buy milk"})

with nhydb.open(path) as db:
    assert db.get_record(1, 1)["text"] == "buy milk"
    print(db.closed)
print(db.closed)
```

```text
False
True
```

> **Note:** `__exit__` calls `close(commit=True)` even if an exception
> occurred inside the `with` block. Use explicit `close(commit=False)` if
> you want to discard changes after an error (section 5).

---

## 4. Opening databases and options

`nhydb.open(path, **options)` is the same as `nhydb.NHyDB(path, **options)`;
`nhydb.create(path, **options)` is the same as `nhydb.NHyDB.create(...)`.
All options are keyword arguments.

| Option | `create` | `open` | Meaning |
|---|:-:|:-:|---|
| `overwrite=False` | ✓ | | replace an existing database |
| `durability="fsync"` | ✓ | ✓ | `"fsync"` or `"full"` (section 5) |
| `lock=True` | ✓ | ✓ | exclusive lock file `<db>.lock` (section 22) |
| `lock_timeout_ms=0` | ✓ | ✓ | wait for the lock instead of failing immediately |
| `layout_path`, `key_path` | ✓ | ✓ | other locations of the schema and key files (section 21) |
| `config` | ✓ | ✓ | path of a `nhydb_config.toml` (section 19) |
| `readonly=True` | | ✓ | open without changing the file (no lock, no commit; write APIs raise `NHyDBError`) |

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "app.nhydb")
nhydb.create(path, durability="full").close()

try:
    nhydb.create(path)                       # already exists
except FileExistsError as e:
    print("FileExistsError")

db = nhydb.create(path, overwrite=True)      # start from scratch
print(str(db.path).endswith("app.nhydb"), db.durability)   # db.path is a pathlib.Path
db.close()

try:
    nhydb.open(os.path.join(os.path.dirname(path), "missing.nhydb"))
except FileNotFoundError:
    print("FileNotFoundError")
```

```text
FileExistsError
True fsync
FileNotFoundError
```

Useful attributes of an open database:

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "info.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "main")
    db.commit()
    print(os.path.basename(db.path), os.path.basename(db.layout_path), os.path.basename(db.key_path))
    print(db.durability, db.max_db_size_bytes)
    print(db.committed_end > 0, db.recovered_tail_bytes, db.config_source)
    print(db.realms)
```

```text
info.nhydb info.nhydb.layout.enc info.nhydb.layout.key
fsync 53687091200
True 0 None
{1: {'realm_id': 1, 'name': 'main', 'description': '', 'default_shape_id': None, 'base_reserve_to': 0}}
```

`db.durability` can also be changed while the database is open
(`db.durability = "full"`).

---

## 5. Commit, sync, close and crash behavior

* `commit()` writes all pending data, syncs the file and moves the commit
  boundary. It returns the new boundary (a byte offset).
* Every write method has `commit=False` by default; `commit=True` commits
  right after the write.
* `sync(hard=False)` writes buffers to the file (`hard=True` also syncs the
  file) but does **not** commit.
* `close(commit=True)` commits and releases the file and the lock.
  `close(commit=False)` closes without committing.
* After `close()` the file handle and the lock are released immediately.
  Every read, write, query or index call and every open iterator then
  raises `NHyDBError("database is closed")`, also for data that was read
  before; calling `close()` again does nothing.
* Uncommitted data is discarded when the database is opened again;
  `recovered_tail_bytes` tells you how many bytes were cut off. This only
  happens when the commit boundary is unambiguous: if one of the two
  superblocks is damaged and data follows the boundary, opening fails with
  `CorruptionError` and changes nothing (open with `readonly=True` to read,
  use `repair_file` to resolve – section 20). Committed data is never cut
  off silently.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "crash.nhydb")
db = nhydb.create(path)
db.create_realm(1, "r")
db.create_shape(1, 1, "s", [{"v": 1, "name": "n", "type": "varuint"}])
db.create_classic_layout(1, 1, "c", shape_id=1)
db.put_record(1, 1, {"n": 100}, commit=True)   # durable
db.put_record(1, 2, {"n": 200})                # not committed
db.close(commit=False)                         # like a crash before commit

db = nhydb.open(path)
print(db.recovered_tail_bytes > 0)             # the uncommitted tail was cut off
print(db.get_record(1, 1)["n"], db.get_record(1, 2))
db.close()
db.close()                                     # no effect

try:
    db.put_record(1, 3, {"n": 300})
except nhydb.NHyDBError as e:
    print(e)
```

```text
True
100 None
database is closed
```

**Durability modes.** `"fsync"` (default) uses the operating system's file
sync (on macOS with an `F_BARRIERFSYNC` write barrier before the
superblock). `"full"` additionally flushes the drive cache on macOS: the
data are ordered before the superblock with `F_BARRIERFSYNC`, and the
superblock sync is an `F_FULLFSYNC` that flushes data and superblock to the
medium before `commit()` returns; on Linux and Windows both modes perform
the same call.
`"full"` is slower; on macOS it raises an error if the file system does not
support it. The aliases `"default"`/`"normal"` (= `"fsync"`) and
`"fullfsync"` (= `"full"`) are accepted. If a sync fails during a commit,
the object refuses further commits; reopen the database.

**Rollback.** `rollback()` discards every write and every schema change of
this object since its last commit and returns the commit boundary. The
object stays open and usable; session settings (registered plugins,
`durability`) are kept. Without pending changes it does nothing. A
`readonly=True` object raises `NHyDBError`; with `lock=False` a commit of
another writer is detected first and raises `DatabaseLockedError` (nothing
is cut). Closing with `commit=False` (or ending the process) before a
commit discards the work as well. A single call that writes several blocks
(for example `put_points` across many `u` windows) is not atomic as a
whole – use `rollback()` after an error, or rely on `put_math_batch`, which
rolls back by itself.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "rollback.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "n", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_record(1, 1, {"n": 100}, commit=True)
    boundary = db.committed_end

    db.put_record(1, 1, {"n": 111})                 # overwrite, not committed
    db.put_record(1, 2, {"n": 200})
    db.create_realm(2, "scratch")                   # schema change, not committed
    print(db.get_record(1, 1)["n"], sorted(db.realms))

    print(db.rollback() == boundary)                # back to the last commit
    print(db.get_record(1, 1)["n"], db.get_record(1, 2), sorted(db.realms))
    db.put_record(1, 3, {"n": 300}, commit=True)    # the handle stays usable

with nhydb.open(path, readonly=True) as db:
    print(db.get_record(1, 3)["n"])
    try:
        db.rollback()
    except nhydb.NHyDBError as e:
        print(e)
```

```text
111 [1, 2]
True
100 None [1]
300
database is opened read-only (readonly=True); rollback is not possible
```

---

## 6. Records (CLASSIC layout)

In a CLASSIC layout a record is the cell row `(R, u, v = 1 … k)`: the
record id is `u`, the fields are the shape fields. There are several ways
to write records:

| Method | Writes | Addressed by |
|---|---|---|
| `put_record(layout_id, object_id, values_dict)` | one record | layout + object id |
| `put(realm_id, u, values_list)` | one record | realm + `u` |
| `put_dense_batch(realm_id, u_start, rows)` | consecutive records `u_start, u_start+1, …` | realm + `u` |
| `put_object` / `put_objects_batch` | one / many records of **any** layout (section 17) | layout + object id |

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "orders.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "shop")
    db.create_shape(1, 1, "order", [
        {"v": 1, "name": "id", "type": "varuint"},
        {"v": 2, "name": "customer", "type": "utf8"},
        {"v": 3, "name": "amount", "type": "f64"},
    ])
    db.create_classic_layout(1, 1, "orders", shape_id=1)

    db.put_record(1, 1, {"id": 1, "customer": "Alice", "amount": 19.9})      # dict by field name
    db.put(1, 2, [2, "Bob", 5.5], shape_id=1)                                  # list in field order
    db.put_dense_batch(1, 3, [[3, "Carol", 7.25], [4, "Dave", 120.0]], shape_id=1)  # u = 3, 4
    db.commit()

    print(db.get_record(1, 4))       # via layout 1, object 4
    print(db.get(1, 2))              # via realm 1, u = 2
    print(db.get_object(1, 3)["customer"])
```

```text
{'_realm': 1, '_u': 4, '_lane': 0, '_lane_name': 'NL', '_shape': 1, 'id': 4, 'customer': 'Dave', 'amount': 120.0}
{'_realm': 1, '_u': 2, '_lane': 0, '_lane_name': 'NL', '_shape': 1, 'id': 2, 'customer': 'Bob', 'amount': 5.5}
Carol
```

`shape_id` can be omitted when the realm has a default shape:
`create_classic_layout` sets the layout's shape as default if the realm has
none yet, and `set_realm_default_shape(realm_id, shape_id)` changes it.

**Overwrite and delete.** Writing the same `u` again replaces the record
(the newest write wins). `delete_record(layout_id, object_id)` is the
standard way to delete a record completely: it hides the dense record and
deletes all single cells stored for it (extension values and fields written
as single cells with `put_cell`, `put_cells_numpy` or `batch()`). It
returns the number of deleted items (the dense record counts as one, plus
one per deleted cell). `delete(realm_id, u)` only hides the dense record as
written by `put`/`put_dense_batch` and returns `True`/`False`; fields
written afterwards as single cells and extension values stay visible
through `get_value`/`get_cell`, queries and indexes (section 7). Deleted
data stays in the append-only file until `compact_file()` removes it
physically (section 20).

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "people.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "people")
    db.create_shape(1, 1, "person", [
        {"v": 1, "name": "name", "type": "utf8"},
        {"v": 2, "name": "age", "type": "u8"},
    ])
    db.create_classic_layout(1, 1, "people", shape_id=1)
    db.put_dense_batch(1, 1, [["Ann", 31], ["Ben", 45], ["Cid", 27]], commit=True)

    db.put_record(1, 2, {"name": "Ben", "age": 46}, commit=True)   # birthday: replace u=2
    print(db.get_record(1, 2)["age"])

    print(db.delete_record(1, 1, commit=True))                      # number of deleted items
    print(db.get_record(1, 1))
    print(db.delete(1, 3, commit=True), db.get(1, 3))
```

```text
46
1
None
True None
```

**Enums.** A field can map labels to integer codes. You write and read
labels; the file stores the codes.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "tickets.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "tickets")
    db.create_shape(1, 1, "ticket", [
        {"v": 1, "name": "title", "type": "utf8"},
        {"v": 2, "name": "state", "type": "u8", "enum": {"open": 0, "in_progress": 1, "done": 2}},
    ])
    db.create_classic_layout(1, 1, "tickets", shape_id=1)
    db.put_record(1, 1, {"title": "Fix login", "state": "in_progress"}, commit=True)
    print(db.get_record(1, 1)["state"])
    try:
        db.put_record(1, 2, {"title": "x", "state": "unknown"})
    except ValueError as e:
        print("ValueError:", e)
```

```text
in_progress
ValueError: state: unknown enum value 'unknown'
```

**Default shape, raw views and file positions.** A realm can have several
shapes; `set_realm_default_shape` selects the one used when `shape_id` is
omitted. `get_internal` shows the raw values by `v`; `committed_end`,
`last_valid_end` (end of the written data, including uncommitted blocks)
and `next_block_id` describe the container state.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "internal.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "small", [{"v": 1, "name": "x", "type": "u8"}])
    db.create_shape(2, 1, "large", [{"v": 1, "name": "x", "type": "u8"}, {"v": 2, "name": "y", "type": "u8"}])
    db.set_realm_default_shape(1, 2)
    db.create_classic_layout(1, 1, "c", shape_id=2)
    db.put(1, 1, [1, 2])                               # uses the default shape 2
    print(db.realms[1]["default_shape_id"], db.get_internal(1, 1)["by_v"])
    db.sync()                                          # written to the file, not committed
    print(db.last_valid_end > db.committed_end, db.next_block_id)
```

```text
2 {1: 1, 2: 2}
True 4
```

---

## 7. Single cells, lanes, null and delete

Any cell `(R, u, v, lane)` can be written directly, also outside a shape.
If the cell is not described by a shape field or an extension, pass its
type with `type=` (the type is then stored with the value).

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "cells.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "grid")
    db.put_cell(1, 14, 744, "left", type="utf8")                # lane NL (default)
    db.put_cell(1, 14, 744, "right", type="utf8", lane="NR")    # same (u, v), other lane
    db.put_cell(1, 14, 745, None, type="utf8")                  # explicit null
    db.commit()

    print(db.get_value(1, 14, 744), db.get_value(1, 14, 744, lane="NR"))
    print(db.get_value(1, 14, 746))                              # never written -> None
    cell = db.get_cell(1, 14, 745, dyn_address=False)
    print(cell["present"], cell["value"])                        # null: present, value None
    print(db.get_cell(1, 14, 746, dyn_address=False))            # missing -> None

    db.delete_cell(1, 14, 744, commit=True)                      # tombstone in lane NL only
    print(db.get_value(1, 14, 744), db.get_value(1, 14, 744, lane="NR"))
```

```text
left right
None
True None
None
None right
```

`delete_cell` returns `True` if the cell had a value and `False` otherwise;
for an empty cell it writes nothing. `delete(realm, u)` returns `True`/
`False` in the same way, `delete_record` the number of deleted elements.
Deleting a single field of a dense record with `delete_cell` makes it read
as `None`; the other fields stay.

**Which delete to use.** A record can consist of a dense part (written by
`put`/`put_dense_batch`) and sparse cells on top of it (fields written
later with `put_cell`, `put_cells_numpy` or `batch().put_cell`, and
extensions). `delete_record(layout_id, object_id)` removes both and is the
way to delete a record completely. `delete(realm, u)` hides only the dense
part: the sparse cells stay visible to `get_value`, `get_cell`, queries and
indexes, while `get(realm, u)` returns `None`. Use `delete_cell`/
`delete_point` to remove individual cells, for example to drop one field
but keep the extensions. `delete_point` always writes a tombstone and
returns the write report of `put_points`.
Coordinates must be integers: `int`, NumPy integers and integral floats such
as `3.0` are accepted, `2.7` raises `TypeError` (no silent truncation).

Lanes can be given as `"NL"`/`"NR"`, `0`/`1` (also lower case). Without
`type=` and without a declared field the write is rejected:

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "t.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    try:
        db.put_cell(1, 1, 1, 42)
    except nhydb.SchemaError as e:
        print("SchemaError:", e)
```

```text
SchemaError: R1,u1,v1: cannot derive a data type; specify type=...
```

**Many cells at once.** `put_points(realm_id, cells)` writes a list of
tuples `(u, v, value)` or `(u, v, value, type)`. `delete_point` is the
realm-level counterpart of `delete_cell`.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "points.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "sensors")
    report = db.put_points(1, [
        (1, 1, 20.5, "f64"),
        (1, 2, 21.0, "f64"),
        (2, 1, "offline", "utf8"),
        (10**30, 7, b"\x00\x01", "bytes"),       # coordinates may be arbitrarily large
    ], commit=True)
    print(report["cells"], report["u_groups"])
    print(db.get_value(1, 1, 2), db.get_value(1, 10**30, 7))
    db.delete_point(1, 2, 1, commit=True)
    print(db.get_value(1, 2, 1))
```

```text
4 3
21.0 b'\x00\x01'
None
```

**Collecting single cells: `batch()`.** Many single `put_cell` calls each
write a small block. `with db.batch(commit=False, u_window=10_000) as b:`
collects `b.put_cell(...)` and `b.delete_cell(...)` (same arguments as on
the database) and writes them when the block ends, with one `put_points`
per realm and lane – fewer, larger blocks and a smaller file. Every call is
checked immediately (same errors as `put_cell`/`delete_cell`); the last
call per cell wins; reads inside the block do **not** see the collected
cells; if the block ends with an exception, nothing is written.
`commit=True` commits after writing. `b.flush()` writes early,
`b.discard()` drops the collected cells, `len(b)` counts them.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "batch.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "sensors")
    db.put_cell(1, 5, 1, 99.0, type="f64", commit=True)

    with db.batch(commit=True) as b:               # collect, write once at the end
        for u in range(1, 1001):
            b.put_cell(1, u, 1, u / 10, type="f64")
        b.put_cell(1, 7, 1, -1.0, type="f64")      # last call per cell wins
        b.delete_cell(1, 5, 1)                     # written as a tombstone at the end
        print(len(b), db.get_value(1, 7, 1))       # no read-your-writes inside the block
    print(db.get_value(1, 7, 1), db.get_value(1, 5, 1), db.get_value(1, 1000, 1))

    try:
        with db.batch() as b:
            b.put_cell(1, 2000, 1, 1.0, type="f64")
            b.put_cell(1, 2001, 1, "text", type="f64")   # checked at the call
    except (TypeError, ValueError) as e:
        print(type(e).__name__)
    print(db.get_value(1, 2000, 1))                # an exception writes nothing
```

```text
1000 None
-1.0 None 100.0
ValueError
None
```

> **Note:** `get(realm, u)` and `get_record(...)` return **dense**
> records. A `u` that only has sparse cells (written with `put_cell`/
> `put_points`) returns `None` there; read such cells with `get_value` or
> `get_cell`, or iterate them with `iter_materialized_cells` (section 12).

---

## 8. Extensions: additional fields per record

Extensions are named, typed fields of a realm at a `v` above the BASE
fields. They are stored sparsely, so only records that have a value pay for
it. Reserve a `v` range for future BASE fields with `base_reserve_to`.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "ext.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "products", base_reserve_to=20)
    db.create_shape(1, 1, "product", [
        {"v": 1, "name": "sku", "type": "utf8"},
        {"v": 2, "name": "price", "type": "f64"},
    ])
    db.create_classic_layout(1, 1, "products", shape_id=1)
    db.define_extension(1, 30, "utf8", name="color")
    db.define_extensions(1, [{"v": 31, "name": "weight_kg", "type": "f64"},
                             {"v": 32, "name": "tags", "type": "utf8"}])
    db.put_dense_batch(1, 1, [["A-100", 9.99], ["B-200", 24.5], ["C-300", 3.2]])

    db.put_extensions(1, 1, {"color": "red", "weight_kg": 0.4})       # by name
    db.put_extension_list(1, 2, [30, 31], ["blue", 1.25])             # by v
    db.put_extension_range(1, 3, 30, ["green", 0.1, "sale"])          # v = 30, 31, 32
    db.commit()

    print(db.get(1, 1))
    print(db.get(1, 3, include_extensions=False))
    print([e["name"] for e in db.extension_catalog(1)], db.resolve_v(1, "tags"))

    db.put_extensions(1, 1, {"tags": "new"}, merge=False, commit=True)  # replace all extensions
    print({k: v for k, v in db.get(1, 1).items() if not k.startswith("_")})
```

```text
{'_realm': 1, '_u': 1, '_lane': 0, '_lane_name': 'NL', '_shape': 1, 'sku': 'A-100', 'price': 9.99, 'color': 'red', 'weight_kg': 0.4}
{'_realm': 1, '_u': 3, '_lane': 0, '_lane_name': 'NL', '_shape': 1, 'sku': 'C-300', 'price': 3.2}
['color', 'weight_kg', 'tags'] 32
{'sku': 'A-100', 'price': 9.99, 'tags': 'new'}
```

With `merge=True` (default) only the given extensions are written; with
`merge=False` the other live extensions of the record are deleted.
`put_record` and `put_dense_batch` (via `extensions_by_u`) can write
extensions together with the BASE fields:

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "ext2.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "name", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.define_extension(1, 10, "utf8", name="note")
    db.put_record(1, 1, {"name": "x", "note": "via put_record"})
    db.put(1, 2, ["y"], extensions={"note": "via put"})
    db.put_dense_batch(1, 3, [["z"]], extensions_by_u={3: {"note": "via batch"}})
    db.commit()
    print([db.get_value(1, u, 10) for u in (1, 2, 3)])
```

```text
['via put_record', 'via put', 'via batch']
```

---

## 9. Addresses and the number N

The module-level functions work without a database.

* `dynadron_n(realm, u, v, lane)` – address → `N`.
* `n_to_dynadron_address(N)` – `N` (int or `"N…"` text) → address.
* `cell_address(realm, u, v, lane)` / `dynadron_address(realm, u, v)` –
  address with all derived indicators.

```python
import nhydb

n_left = nhydb.dynadron_n(1, 11, 13, "NL")
n_right = nhydb.dynadron_n(1, 11, 13, "NR")
print(n_left, n_right)

a = nhydb.n_to_dynadron_address(589)
print(a["realm"], a["u"], a["v_local"], a["lane_name"], a["G"])
print(nhydb.n_to_dynadron_address("N565")["lane_name"])
print(nhydb.parse_n_reference("N589"))

c = nhydb.cell_address(1, 11, 13, 1)
print(c["N_ref"], c["G"], c["NL"], c["NR"])
```

```text
565 589
1 11 13 NR 24
NL
589
N589 24 565 589
```

For realm 1 the formula is `G = u + v`, `N = G² − u` (NL) or `N = G² + v`
(NR); for example `24² − 11 = 565`. Larger realms use the global
coordinate `v_global = (R − 1)·10^100 + v`, so `N` grows very large –
Python integers handle this exactly:

```python
import nhydb

n = nhydb.dynadron_n(23, 14, 744, "NL")
print(len(str(n)))                                   # number of digits
a = nhydb.n_to_dynadron_address(n)
print(a["realm"], a["u"], a["v_local"], a["lane_name"])

print(nhydb.REALM_SPAN == 10**100)
print(nhydb.global_v(2, 5) == 10**100 + 5, nhydb.split_global_v(10**100 + 5))
lo, hi = nhydb.realm_v_bounds(2)
print(lo == 10**100 + 1, hi == 2 * 10**100)
print(nhydb.validate_local_v(5))
print(nhydb.normalize_lane("nr"), nhydb.lane_name(0))
print(nhydb.dynadron_address(1, 11, 13)["G"])        # indicators without lane
```

```text
203
23 14 744 NL
True
True (2, 5)
True True
5
1 NL
24
```

Not every natural number is a storage cell: numbers on the "GZ" and "N0"
lines of the Dynadron are excluded and raise `AddressError`:

```python
import nhydb

for n in (4, 576, 589):
    try:
        print(n, nhydb.n_to_dynadron_address(n)["u"])
    except nhydb.AddressError as e:
        print(n, "AddressError:", e)
```

```text
4 AddressError: N4 is a GZ point, not an NHyDB storage cell
576 AddressError: N576 is a GZ point, not an NHyDB storage cell
589 11
```

Coordinates outside the valid range are rejected as well (`v` must stay
within its realm: `1 ≤ v ≤ 10^100`):

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "limits.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    try:
        db.put_cell(1, 14, 10**100 + 1, 5, type="u8")
    except nhydb.RealmLimitError as e:
        print("RealmLimitError")
    print(db.realm_bounds(1)["local_v_max"] == 10**100)
```

```text
RealmLimitError
True
```

The low-level codecs `enc_vu`/`dec_vu` (VarUInt) and `enc_vi`/`dec_vi`
(ZigZag VarInt) are also exported:

```python
import nhydb

data = nhydb.enc_vu(300)
print(data, nhydb.dec_vu(data))        # (value, bytes consumed)
print(nhydb.dec_vi(nhydb.enc_vi(-3)))
```

```text
b'\xac\x02' (300, 2)
(-3, 1)
```

---

## 10. Reading by N and inspecting addresses

A number `N` names exactly one cell. How it is read depends on the mode:

| Mode | Result |
|---|---|
| `"cell"` | exactly the addressed cell |
| `"record"` | the whole logical record the cell belongs to (via its layout) |
| `"auto"` (default) | record if `N` is the record's anchor cell, otherwise cell |

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "byn.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "shop")
    db.create_shape(1, 1, "order", [
        {"v": 1, "name": "id", "type": "varuint"},
        {"v": 2, "name": "customer", "type": "utf8"},
        {"v": 3, "name": "amount", "type": "f64"},
    ])
    db.create_classic_layout(1, 1, "orders", shape_id=1)
    db.put_dense_batch(1, 1, [[1, "Alice", 19.9], [2, "Bob", 5.5]], commit=True)

    n = nhydb.dynadron_n(1, 2, 3, "NL")                 # amount of order 2
    cell = db.get_n(n, mode="cell")
    print(cell["N"], cell["field"], cell["value"], cell["record_N"])

    rec = db.get_n(cell["record_N"])                    # anchor -> whole record
    print(rec["effective_mode"] if "effective_mode" in rec else rec["result"], rec["data"])

    print(db.get_address(1, 2, 3, mode="cell")["value"])
    print(db.get_n_cell(n)["value"], db.get_n_record(n)["object"])
    print(db.get_cell_n(n, dyn_address=False)["value"])
```

```text
N23 amount 5.5 N7
record {'id': 2, 'customer': 'Bob', 'amount': 5.5}
5.5
5.5 2
5.5
```

More tools to understand an address or a record:

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "inspect.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "shop")
    db.create_shape(1, 1, "order", [
        {"v": 1, "name": "id", "type": "varuint"},
        {"v": 2, "name": "customer", "type": "utf8"},
    ])
    db.create_classic_layout(1, 1, "orders", shape_id=1)
    db.put_dense_batch(1, 1, [[1, "Alice"], [2, "Bob"]], commit=True)

    d = db.describe_n(14)                               # N14 = realm 1, u=2, v=2, NL
    print(d["u"], d["v"], d["lane_name"], d["layout"]["name"])
    print(db.resolve_n("N14")["realm_defined"])
    print(db.record_n_for_address(1, 2, 2), db.record_n_from_n(14))
    print(db.record_anchor(1, 2)["N_ref"], db.logical_address(1, 2, "customer")["N_ref"])
    print(db.occupied_n(1, 2))
    print(db.occupied_from_n(14, output="N")["cells"])
    view = db.get_record_view(1, 2)
    print(view["cell_count"], [c["N"] for c in view["cells"]])
    print(db.cell_address(1, 2, 2)["N"], db.dyn_address(1, 2, 2)["NR"])
```

```text
2 2 NL orders
True
N7 N7
N7 N14
['N7', 'N14']
['N7', 'N14']
2 ['N7', 'N14']
14 18
```

---

## 11. Data types

| Type | Python value | Range / note |
|---|---|---|
| `u8`, `u16`, `u32`, `u64` | `int` | unsigned, checked |
| `i8`, `i16`, `i32`, `i64` | `int` | signed, checked |
| `varuint` | `int` ≥ 0 | arbitrarily large |
| `varint` | `int` | arbitrarily large, negative allowed |
| `f32`, `f64` | `float` | `f32`: values that would overflow are rejected |
| `bool` | `bool` | |
| `utf8` | `str` | |
| `bytes` | `bytes` | |

`nhydb.TYPE_CODES` maps the names to the codes stored in the file.

```python
import os
import tempfile

import nhydb

print(nhydb.TYPE_CODES["utf8"], len(nhydb.TYPE_CODES))
fields = [("u8", 255), ("u16", 65535), ("u32", 4_000_000_000), ("u64", 2**64 - 1),
          ("i8", -128), ("i16", -30000), ("i32", -2**31), ("i64", -2**63),
          ("f32", 1.5), ("f64", 3.141592653589793), ("bool", True),
          ("varuint", 10**40), ("varint", -10**40), ("utf8", "Grüße ✓"), ("bytes", b"\x00\xff")]

path = os.path.join(tempfile.mkdtemp(), "types.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "types")
    db.create_shape(1, 1, "all", [{"v": i + 1, "name": t, "type": t} for i, (t, _) in enumerate(fields)])
    db.create_classic_layout(1, 1, "all", shape_id=1)
    db.put_record(1, 1, {t: value for t, value in fields}, commit=True)
    rec = db.get_record(1, 1)
    print(all(rec[t] == value for t, value in fields))
    try:
        db.put_record(1, 2, {**{t: value for t, value in fields}, "u8": 256})
    except ValueError:
        print("u8 out of range -> ValueError")
```

```text
14 15
True
u8 out of range -> ValueError
```

---

## 12. Reading many records: ranges and iterators

`range`, `select`, `select_us`, `find` (section 13) and the
`iter_materialized_*` methods return **lazy iterators**: records are loaded
block by block, so memory use does not depend on the result size. Gaps in
`u` are skipped efficiently. After `close()` an iterator raises
`NHyDBError("database is closed")`. `find` takes its list of matching `u`
from the index at the first `next()` and keeps it: later writes neither
raise `StaleIndexError` nor show up in that iterator – create a new one for
the current state.

```python
import itertools
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "iter.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "log")
    db.create_shape(1, 1, "entry", [{"v": 1, "name": "level", "type": "utf8"},
                                    {"v": 2, "name": "code", "type": "u16"}])
    db.create_classic_layout(1, 1, "log", shape_id=1)
    db.put_dense_batch(1, 1, [["info" if u % 3 else "warn", u] for u in range(1, 1001)])
    db.put(1, 10**15, ["error", 999])                   # far away: the gap is skipped
    db.commit()

    first = list(itertools.islice(db.range(1, 1, 10**15), 2))
    print([r["code"] for r in first])
    print(sum(1 for _ in db.range(1, 1, 10**15)))
    print(list(db.select(1, 998, 1000, ["code"])))
    print([r["level"] for r in db.select_us(1, [3, 10**15], ["level"])])
```

```text
[1, 2]
1001
[{'_u': 998, '_lane': 0, '_lane_name': 'NL', 'code': 998}, {'_u': 999, '_lane': 0, '_lane_name': 'NL', 'code': 999}, {'_u': 1000, '_lane': 0, '_lane_name': 'NL', 'code': 1000}]
['warn', 'error']
```

To list everything that is physically stored (dense and sparse):

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "walk.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "u8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[10], [20]])
    db.put_cell(1, 5, 9, "loose", type="utf8")
    db.commit()

    for cell in db.iter_materialized_cells(1):
        print(cell["N"], cell["u"], cell["v"], cell["value"])
    print(list(db.iter_materialized_sparse_points(1)))
    print(list(db.iter_materialized_record_ns(1)))
```

```text
N3 1 1 10
N7 2 1 20
N191 5 9 loose
[{'realm': 1, 'lane': 0, 'u': 5, 'v': 9, 'value': 'loose', 'type': 'utf8'}]
['N3', 'N7', 'N31']
```

**Address indicators in results.** `get`, `range` and `find` can add the
mathematical indicators of a field (`dyn_address="once"` with `dyn_v`) or
of every field (`dyn_address="auto"`). `set_dyn_address_output(mode, v=…)`
sets a default for the database object (`"off"`, `"once"`, `"auto"`).

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "dyn.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "u8"}, {"v": 2, "name": "b", "type": "u8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put(1, 1, [7, 8], commit=True)

    r = db.get(1, 1, dyn_address="once", dyn_v=2)
    print(r["_dyn_v"], r["_dyn_NL"], r["_dyn_NR"])
    r = db.get(1, 1, dyn_address="auto")
    print(sorted(r["_dyn_addresses"]), r["_dyn_addresses"]["a"]["NL"])
    db.set_dyn_address_output("once", v=1)
    print(db.get(1, 1)["_dyn_NL"])
    db.set_dyn_address_output("off")
    print("_dyn_NL" in db.get(1, 1))
```

```text
2 8 11
['a', 'b'] 3
3
False
```

---

## 13. Secondary indexes

| Kind | Create | Field types | Answers |
|---|---|---|---|
| VALUE | `create_index(index_id, realm_id, name, field)` | any | equality, ranges |
| TOKEN | `create_token_index(...)` or `create_index(..., kind="TOKEN")` | `utf8` | contains word(s) |
| TRIGRAM | `create_trigram_index(..., ngram=3, casefold=False)` | `utf8` | contains substring |
| FULLTEXT | `create_fulltext_index(...)` | `utf8` | words, phrases, BM25 ranking |

The field can be given by name or by `v`. Indexes are **not** maintained
automatically: call `build_indexes()` after writing. An index that is older
than the data is *stale*; the direct index APIs then raise
`StaleIndexError`, the QueryEngine falls back to scanning, and `verify()`
lists it under `warnings`.

An index returns exactly what a scan returns:

* Only records of **CLASSIC** layouts are indexed. In a realm that also has
  COLUMN, PACKED or CUSTOM layouts the QueryEngine scans instead.
* `== NaN` never matches; ranges skip `None` and NaN; values that cannot be
  compared with the bounds (text against numbers) simply do not match.
* `delete(realm, u)` hides the dense fields of a record; extensions and
  single cells that survive it stay visible to `get_value`/`get_cell`, the
  QueryEngine and the index (`delete_record` removes them as well,
  section 7).
* **After updating NHyDB, rebuild existing indexes** with
  `build_indexes(commit=True)`.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "idx.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "staff")
    db.create_shape(1, 1, "person", [
        {"v": 1, "name": "name", "type": "utf8"},
        {"v": 2, "name": "dept", "type": "utf8"},
        {"v": 3, "name": "salary", "type": "u32"},
    ])
    db.create_classic_layout(1, 1, "staff", shape_id=1)
    db.put_dense_batch(1, 1, [["Ann", "IT", 5200], ["Ben", "Sales", 4100],
                              ["Cid", "IT", 6100], ["Dee", "HR", 3900]])
    db.create_index(1, 1, "by_dept", "dept")
    db.create_index(2, 1, "by_salary", "salary")
    report = db.build_indexes(commit=True)
    print([(i["name"], i["kind"], i["distinct_values"]) for i in report["indexes"]])

    print(list(db.find_u(1, {"by_dept": "IT"})))                       # equality -> u
    print(list(db.find_u(1, {"by_salary": (4000, 5500)})))             # range (inclusive)
    print([r["name"] for r in db.find(1, {"by_dept": "IT", "by_salary": (6000, 9999)})])
    print(db.posting_count(1, "IT"))                                   # count without reading records
    print(list(db.index_candidates(2, "between", 4000, 6000)))
    print(db.index_candidate_estimate(1, "eq", "IT"))

    db.put(1, 5, ["Eve", "IT", 4800], commit=True)                     # index is now stale
    try:
        list(db.find_u(1, {"by_dept": "IT"}))
    except nhydb.StaleIndexError:
        print("stale -> rebuild")
    db.build_indexes([1, 2], commit=True)
    print(list(db.find_u(1, {"by_dept": "IT"})))
```

```text
[('by_dept', 'VALUE', 3), ('by_salary', 'VALUE', 4)]
[1, 3]
[1, 2]
['Cid']
2
[1, 2]
{'estimated_rows': 2, 'exact': True, 'key_count': 1}
stale -> rebuild
[1, 3, 5]
```

Text indexes (`index_candidates` operations per index kind: TOKEN
`token`/`tokens_all`/`tokens_any`, TRIGRAM `contains`/`icontains`,
FULLTEXT `fulltext`/`phrase`):

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "text.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "docs")
    db.create_shape(1, 1, "doc", [{"v": 1, "name": "title", "type": "utf8"},
                                  {"v": 2, "name": "body", "type": "utf8"}])
    db.create_classic_layout(1, 1, "docs", shape_id=1)
    db.put_dense_batch(1, 1, [
        ["Rust storage", "an append only storage engine in Rust"],
        ["Python tips", "NumPy arrays and Python speed"],
        ["Rust and Python", "bindings between Rust and Python via PyO3"],
    ])
    db.create_token_index(1, 1, "tok_title", "title")
    db.create_trigram_index(2, 1, "tri_title", "title", casefold=True)
    db.create_fulltext_index(3, 1, "ft_body", "body")
    db.build_indexes(commit=True)

    print(list(db.index_candidates(1, "token", "rust")))              # case-insensitive words
    print(list(db.index_candidates(1, "tokens_all", "rust python")))
    print(list(db.index_candidates(1, "tokens_any", "storage tips")))
    print(list(db.index_candidates(2, "icontains", "PYTH")))           # substring
    print(list(db.index_candidates(3, "fulltext", "rust python")))     # all terms
    print(list(db.index_candidates(3, "phrase", "storage engine")))    # exact word sequence
    print(db.fulltext_corpus(3))
    print(db.fulltext_term_posting(3, "rust"))
    postings, corpus = db.fulltext_payloads(3, ["rust", "python"])
    print(sorted(postings), corpus.total_docs)
    print({i: d.kind for i, d in db.index_defs.items()}, db.index_defs[2].to_dict()["options"])
```

```text
[1, 3]
[3]
[1, 2]
[2, 3]
[3]
[1]
FullTextCorpus(total_docs=3, total_tokens=19)
(FullTextPosting(u=1, positions=(6,)), FullTextPosting(u=3, positions=(2,)))
['python', 'rust'] 3
{1: 'TOKEN', 2: 'TRIGRAM', 3: 'FULLTEXT'} {'ngram': 3, 'casefold': True}
```

`fulltext_payloads(index_id, terms)` returns the postings of several terms
plus the corpus statistics in one call. Ranked BM25 search is part of the
QueryEngine (section 14) and NHySQL (section 15).

---

## 14. QueryEngine and the Q builder

`QueryEngine(db)` evaluates expressions built with `Q`. It uses fresh
indexes where possible and scans otherwise.

| Builder | Meaning |
|---|---|
| `Q.field("x") == 5`, `!=`, `<`, `<=`, `>`, `>=` | comparisons |
| `Q.field("x").between(a, b)` | inclusive range |
| `.contains(s)`, `.icontains(s)`, `.token(w)`, `.tokens_all(s)`, `.tokens_any(s)` | text |
| `.fulltext(s)`, `.phrase(s)`, `Q.fulltext(field, s)`, `Q.phrase(field, s)` | full text |
| `.exists()` | field is present |
| `Q.u`, `Q.v`, `Q.realm`, `Q.lane`, `Q.N`, `Q.record_N` | structural predicates |
| `Q.n(*numbers)` | a set of cell numbers |
| `Q.all()` | everything |
| `&`, `|`, `~` | and, or, not |

Return modes: `record` (default), `fields`, `count`, `n`, `cell`,
`aggregate`; `search()` returns `ranked`.

Comparisons follow Python (`1 == 1.0 == True`); comparing incompatible
types (text with a number) is simply `False`, `== NaN` never matches, and
rows with a NaN group value form groups of their own. Unknown columns in
`select` yield `None`. Expressions may be nested up to 256 levels
(`QueryPlanError` beyond); long `&`/`|` chains are fine.

```python
import os
import tempfile

import nhydb
from nhydb.query import Q, QueryEngine

path = os.path.join(tempfile.mkdtemp(), "q.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "sales")
    db.create_shape(1, 1, "sale", [
        {"v": 1, "name": "id", "type": "varuint"},
        {"v": 2, "name": "region", "type": "utf8"},
        {"v": 3, "name": "amount", "type": "f64"},
    ])
    db.create_classic_layout(1, 1, "sales", shape_id=1)
    regions = ["north", "south", "east"]
    db.put_dense_batch(1, 1, [[u, regions[u % 3], float(u * 10)] for u in range(1, 31)])
    db.create_index(1, 1, "by_region", "region")
    db.build_indexes(commit=True)

    q = QueryEngine(db)
    print(q.execute(Q.field("region") == "north", realm=1, return_mode="count")["count"])
    r = q.execute((Q.field("region") == "south") & (Q.field("amount") >= 200), realm=1,
                  return_mode="fields", select=["id", "amount"], order_by=[("amount", "desc")], limit=3)
    print([(row["id"], row["amount"]) for row in r["rows"]], r["count"])
    r = q.execute(Q.all(), realm=1, aggregate={"n": ("count", "*"), "total": ("sum", "amount")},
                  group_by=["region"])
    print(sorted((row["region"], row["n"], row["total"]) for row in r["rows"]))
    print(q.execute(Q.u.between(5, 7), realm=1, return_mode="n")["refs"])
    rec = q.execute(Q.field("id") == 4, realm=1, limit=1)["rows"][0]
    print(rec["data"])
    plan = q.explain(Q.field("region") == "east", realm=1)["plan"]
    print(plan["access"], plan["index_name"])
```

```text
10
[(28, 280.0), (25, 250.0), (22, 220.0)] 3
[('east', 10, 1550.0), ('north', 10, 1650.0), ('south', 10, 1450.0)]
['N31', 'N43', 'N57']
{'id': 4, 'region': 'south', 'amount': 40.0}
VALUE_INDEX by_region
```

> **Tip:** In 0.28.0 `order_by` of `QueryEngine.execute` only takes
> effect for fields that are also listed in `select`. Include the sort
> fields in `select` (NHySQL `ORDER BY` works without this restriction).

Aggregate operations are `count`, `sum`, `min`, `max`, `avg`. The same
queries are available directly on the database as `db.query(expr, ...)`
and `db.explain_query(expr, ...)`.

**Full-text search with BM25 ranking** (needs a fresh FULLTEXT index):

```python
import os
import tempfile

import nhydb
from nhydb.query import Q, QueryEngine

path = os.path.join(tempfile.mkdtemp(), "search.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "kb")
    db.create_shape(1, 1, "article", [{"v": 1, "name": "title", "type": "utf8"},
                                      {"v": 2, "name": "body", "type": "utf8"}])
    db.create_classic_layout(1, 1, "articles", shape_id=1)
    db.put_dense_batch(1, 1, [
        ["Backups", "copy all three database files while the database is closed"],
        ["Indexes", "build indexes after writing; stale indexes are detected"],
        ["Files", "the key file must be kept safe; without the key file the database cannot be opened"],
    ])
    db.create_fulltext_index(1, 1, "ft_body", "body")
    db.build_indexes(commit=True)

    hits = QueryEngine(db).search(Q.fulltext("body", "key file"), realm=1, select=["title"], limit=5)
    print([(h["title"], round(h["relevance"], 3)) for h in hits["rows"]])
    print(db.query(Q.field("body").phrase("database files"), realm=1, return_mode="count")["count"])
```

```text
[('Files', 2.417)]
1
```

**Queries as JSON-like specifications** (useful for configuration files or
web APIs): `execute_spec(dict)` and `nhydb.query.expr_from_spec(dict)`.

```python
import os
import tempfile

import nhydb
from nhydb.query import QueryEngine, expr_from_spec

path = os.path.join(tempfile.mkdtemp(), "spec.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "year", "type": "u16"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[2022], [2024], [2024]], commit=True)

    q = QueryEngine(db)
    print(q.execute_spec({"where": {"field": "year", "op": "eq", "value": 2024}, "realm": 1, "return": "count"})["count"])
    expr = expr_from_spec({"field": "year", "op": "ge", "value": 2023})
    print(expr, q.execute(expr, realm=1, return_mode="count")["count"])
```

```text
2
FieldPredicate(name='year', op='ge', value=2023, value2=None, options={}) 2
```

**Math providers.** `Q.math(provider, **params)` produces candidate cells
from a computation instead of stored data. Built in are `formula`,
`n_sequence` and `math_layout_inputs` (section 18); your own providers are
plain Python functions `function(engine, context, params)` that return cell
numbers:

```python
import os
import tempfile

import nhydb
from nhydb.query import Q, QueryEngine

path = os.path.join(tempfile.mkdtemp(), "prov.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "squares")
    for k in range(1, 6):
        db.put_cell(1, k, 1, k * k, type="varuint")
    db.commit()

    def even_rows(engine, context, params):
        """Cells (realm 1, u, v=1) for even u up to params['limit']."""
        return [nhydb.dynadron_n(1, u, 1, 0) for u in range(2, params["limit"] + 1, 2)]

    q = QueryEngine(db)
    q.register_math_provider("even_rows", even_rows, description="even u in realm 1")
    print(sorted(q.math_providers))
    r = q.execute(Q.math("even_rows", limit=5), return_mode="cell")
    print([(row["u"], row["value"]) for row in r["rows"]])
    seq = q.execute(Q.math("n_sequence", start=3, stop=20, step=4), return_mode="cell")
    print([(row["N"], row["value"]) for row in seq["rows"]])     # computed cells, stored or not
```

```text
['even_rows', 'formula', 'math_layout_inputs', 'n_sequence']
[(2, 4), (4, 16)]
[('N3', 1), ('N7', 4), ('N11', None), ('N15', None), ('N19', None)]
```

---

## 15. NHySQL

NHySQL is a read-only `SELECT` dialect over CLASSIC layouts. The source is
the layout name (optionally `realm.layout`). Parameters: `?` (positional,
pass a list) and `:name` (pass a dict).

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "sql.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "shop")
    db.create_shape(1, 1, "order", [
        {"v": 1, "name": "id", "type": "varuint"},
        {"v": 2, "name": "status", "type": "utf8"},
        {"v": 3, "name": "amount", "type": "f64"},
        {"v": 4, "name": "note", "type": "utf8"},
    ])
    db.create_classic_layout(1, 1, "orders", shape_id=1)
    db.put_dense_batch(1, 1, [
        [1, "paid", 19.9, "express delivery"], [2, "new", 5.5, None],
        [3, "paid", 120.0, "gift wrap"], [4, "shipped", 7.25, "express delivery requested"],
    ], commit=True)

    r = db.sql("SELECT status, COUNT(*) AS n, SUM(amount) AS total FROM orders GROUP BY status ORDER BY n DESC")
    print(r["columns"], r["rows"])
    print(db.sql("SELECT id FROM orders WHERE amount BETWEEN ? AND ? ORDER BY id", [5, 20])["rows"])
    print(db.sql("SELECT id FROM orders WHERE status = :s AND note IS NOT NULL", {"s": "paid"})["rows"])
    print(db.sql("SELECT id FROM orders WHERE note CONTAINS 'express' ORDER BY id DESC LIMIT 1")["rows"])
    print(db.sql("SELECT id FROM shop.orders WHERE @u BETWEEN 2 AND 3")["rows"])   # structural u
    print(db.sql("EXPLAIN SELECT COUNT(*) FROM orders WHERE status = 'paid'")["plan"]["access"])
```

```text
['status', 'n', 'total'] [{'status': 'paid', 'n': 2, 'total': 139.9}, {'status': 'new', 'n': 1, 'total': 5.5}, {'status': 'shipped', 'n': 1, 'total': 7.25}]
[{'id': 1}, {'id': 2}, {'id': 4}]
[{'id': 1}, {'id': 3}]
[{'id': 4}]
[{'id': 2}, {'id': 3}]
MATERIALIZED_SCAN
```

Full-text relevance and errors:

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "sql2.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "kb")
    db.create_shape(1, 1, "doc", [{"v": 1, "name": "id", "type": "varuint"},
                                  {"v": 2, "name": "text", "type": "utf8"}])
    db.create_classic_layout(1, 1, "docs", shape_id=1)
    db.put_dense_batch(1, 1, [[1, "quick brown fox"], [2, "quick quick fox"], [3, "lazy dog"]])
    db.create_fulltext_index(1, 1, "ft_text", "text")
    db.build_indexes(commit=True)

    r = db.sql("SELECT id, relevance FROM docs WHERE FULLTEXT(text, :q) ORDER BY relevance DESC LIMIT 5", {"q": "quick"})
    print([row["id"] for row in r["rows"]])
    for bad in ["SELEC id FROM docs", "SELECT DISTINCT id FROM docs", "DELETE FROM docs"]:
        try:
            db.sql(bad)
        except nhydb.SQLSyntaxError as e:
            print("SQLSyntaxError")
    compiled = db.compile_sql("SELECT id FROM docs WHERE id > ?", [1])
    print(compiled.layout_name, compiled.return_mode, compiled.expr)
```

```text
[2, 1]
SQLSyntaxError
SQLSyntaxError
SQLSyntaxError
docs fields FieldPredicate(name='id', op='gt', value=1, value2=None, options={})
```

Structural references (`@u` or `STRUCT.u`, also `realm`, `v`, `lane`,
`n`, `record_n`, `layout_id`, `layout_kind`, `layout_name`, `object`) are
allowed in `WHERE`, not in the select list. Not supported: `DISTINCT`,
`HAVING`, joins and all writing statements.

`FROM <layout>` covers only the records routed to that layout; in a realm
with several CLASSIC layouts each layout sees its own `u` range.
`x IS NULL` is true only when the field is **missing** in a record (for
example an extension that was never set); a stored null value counts as
present. Brackets and `NOT` may be nested up to 256 levels
(`SQLSyntaxError` beyond).
The same SQL functions exist on the QueryEngine (`execute_sql`,
`compile_sql`, `execute_sql_numpy`); `nhydb.query.parse_sql(text)` only
parses.

More query helpers: SQL through the QueryEngine, `db.explain_query`, the
structural builder `Q.struct(name)`, the parser and the version constants:

```python
import os
import tempfile

import nhydb
from nhydb.query import Q, QUERY_VERSION, SQL_LANGUAGE_VERSION, QueryEngine, parse_sql

path = os.path.join(tempfile.mkdtemp(), "more_q.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "u8"}])
    db.create_classic_layout(1, 1, "items", shape_id=1)
    db.put_dense_batch(1, 1, [[1], [2], [3]])
    db.create_index(1, 1, "by_x", "x")
    db.build_indexes(commit=True)

    q = QueryEngine(db)
    print(q.execute_sql("SELECT x FROM items WHERE x >= ?", [2])["rows"])
    print(db.explain_query(Q.field("x") == 1, realm=1)["plan"]["access"])
    print(q.execute(Q.struct("layout_name") == "items", realm=1, return_mode="count")["count"])

print(parse_sql("SELECT x FROM items WHERE x > 1").where)
print(QUERY_VERSION, SQL_LANGUAGE_VERSION)
```

```text
[{'x': 2}, {'x': 3}]
VALUE_INDEX
3
FieldPredicate(name='x', op='gt', value=1, value2=None, options={})
0.27 NHySQL-0.24
```

---

## 16. NumPy batch API

The NumPy methods copy whole columns into the engine; encoding and the
write loops run in Rust. All examples in this section need NumPy.

```python
# requires: numpy
import os
import tempfile

import numpy as np

import nhydb

path = os.path.join(tempfile.mkdtemp(), "ts.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "measurements")
    db.create_shape(1, 1, "sample", [
        {"v": 1, "name": "t", "type": "u64"},
        {"v": 2, "name": "temp", "type": "f32"},
        {"v": 3, "name": "ok", "type": "bool"},
        {"v": 4, "name": "station", "type": "utf8"},
    ])
    db.create_classic_layout(1, 1, "samples", shape_id=1)

    n = 100_000
    t = np.arange(n, dtype=np.uint64) * 60
    cols = {
        "t": t,
        "temp": (20 + np.sin(t / 3600.0)).astype(np.float32),
        "ok": t % 7 != 0,
        "station": np.array(["A", "B"] * (n // 2), dtype=object),
    }
    db.put_numpy_batch(1, 1, cols, commit=True)        # records u = 1 … 100000

    out = db.read_numpy(1, 99_999, 3, ["t", "temp", "station"])
    print(out["t"], out["temp"].dtype, list(out["station"]), out["_present"])
```

```text
[5999880 5999940       0] float32 ['A', 'B', None] [ True  True False]
```

`read_numpy` returns one array per field plus `_present`; missing records
have `_present=False` (values 0, NaN or `None`). Integer types come back as
`int64` (`u64` and `varuint` as `uint64`), `utf8`/`bytes`/enums as object arrays. The
dtype depends only on the field type; a stored value that does not fit
(for example a `varuint` above 2^64 − 1) raises `ValueError` unless you pass
`object_fallback=True`. A `count` too large for the memory raises
`MemoryError`.

Objects of any layout, sparse cells and query results:

```python
# requires: numpy
import os
import tempfile

import numpy as np

import nhydb
from nhydb.query import Q, QueryEngine

path = os.path.join(tempfile.mkdtemp(), "np2.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "f64"}])
    db.create_classic_layout(1, 1, "points", shape_id=1)

    ids = np.array([10, 11, 12, 50], dtype=np.uint64)
    db.put_objects_numpy(1, ids, {"id": ids, "x": ids / 4})                  # any layout, by object id
    db.put_cells_numpy(1, np.full(3, 500, dtype=np.uint64), np.array([1, 2, 3], dtype=np.uint64),
                       np.array([1.5, 2.5, 3.5]), type="f64", lane="NR")   # sparse cells
    db.commit()

    print(db.get_record(1, 50)["x"], db.get_value(1, 500, 3, lane="NR"))
    print(db.sql_numpy("SELECT id, x FROM points WHERE id < ? ORDER BY id", [50]))
    r = QueryEngine(db).execute_numpy(Q.field("x") > 2.8, realm=1, select=["id"])
    print(r["id"], QueryEngine(db).execute_sql_numpy("SELECT x FROM points WHERE id = 11")["x"])
```

```text
12.5 3.5
{'id': array([10, 11, 12]), 'x': array([2.5 , 2.75, 3.  ])}
[12 50] [2.75]
```

Accepted column types: integer arrays of any width and byte order (ranges
are checked) for integer fields, float or integer arrays for `f32`/`f64`,
bool or integer arrays for `bool`, object arrays or lists for text, bytes
and enum labels (integer arrays for enum codes). Columns must be
one-dimensional and of equal length; bool or float arrays for integer
fields, numeric arrays for text fields and dtypes such as float16, complex
or datetime raise `TypeError`. `u`/`v`/object-id arrays are limited to
`uint64`; use lists of Python `int` for larger coordinates; a scalar `u`/`v`
is broadcast to all rows. Invalid input raises `TypeError`, `ValueError`
(wrong length, out of range) or `SchemaError` (missing field) before
anything is written. A batch becomes one block; it is read and checked
completely once on first use, after that single accesses read only the
bytes they need, so large batches stay fast to read (10,000–100,000
records per batch remain a good default). `read_numpy`/`sql_numpy` return
writable arrays that share the engine's result memory (no extra copy).
`put_math_numpy` is shown in section 18.

---

## 17. Layouts: COLUMN, PACKED, CUSTOM, routing and evolution

| Layout | Record id | Field | Use it for |
|---|---|---|---|
| CLASSIC | `u` | `v` (shape field) | ordinary tables (sections 3–8) |
| COLUMN | `v` | one fixed `u` per field | column-oriented data: all values of one field share a `u` |
| PACKED | `⌊(v−1)/span⌋ + 1` | offset in a block of `span` cells | many small records packed along `v` of one `u` |
| CUSTOM | your plugin | your plugin | any mapping you implement in Python |

`put_object`, `put_objects_batch`, `get_object` and `delete_record` work
for every layout; `put_record`/`get_record` are the same calls with
simpler names.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "layouts.nhydb")
with nhydb.create(path) as db:
    db.create_realm(2, "columns")
    db.create_column_layout(10, 2, "metrics", fields=[
        {"u": 1, "name": "temp", "type": "f64"},
        {"u": 2, "name": "humidity", "type": "u8"},
    ])
    db.put_object(10, 42, {"temp": 21.5, "humidity": 40})
    print(db.get_object(10, 42))
    print(db.get_object(10, 42, include_addresses=True)["_addresses"]["temp"]["u"])

    db.create_realm(3, "packed")
    db.create_packed_layout(11, 3, "pairs", u=1, span=8, fields=[
        {"offset": 0, "name": "a", "type": "varuint"},
        {"offset": 1, "name": "b", "type": "utf8"},
    ])
    print(db.put_objects_batch(11, [(1, {"a": 1, "b": "x"}), (2, {"a": 2, "b": "y"})])["objects"])
    anchor = db.record_anchor(11, 2)
    print(anchor["u"], anchor["v_local"])            # record 2 starts at v = (2-1)*8 + 1
    print(db.delete_record(11, 1), db.get_object(11, 1), db.get_object(11, 2)["b"])
    db.commit()
```

```text
{'_layout': 10, '_layout_name': 'metrics', '_kind': 'COLUMN', '_object': 42, '_lane': 0, '_lane_name': 'NL', 'temp': 21.5, 'humidity': 40}
1
2
1 9
2 None y
```

**Several layouts in one realm (routing).** Layouts can share a realm with
different `u` ranges. For a given `u` the layout with the higher
`priority` wins, then the narrower range, then the higher id.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "routing.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "mixed")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "u32"}])
    db.create_classic_layout(1, 1, "all", shape_id=1)                          # u = 1 … ∞
    db.create_layout(2, 1, "special", "CLASSIC", shape_id=1, u_start=100, u_end=199, priority=5)

    print(db.layout_at(1, 50).name, db.layout_at(1, 150).name)
    print([l.name for l in db.layout_candidates(1, 150)])
    print(db.route_address(1, 150, 1)["layout_name"])
    print(db.layout_diagnostics(1)["summary"])
    print(db.get_layout(layout_id=2)["u_range"])
    lay = db.get_layout(layout_id=2)
    print(db.layout_at(1, 150).to_dict()["priority"], db.layout_at(1, 150).contains_u(150))
```

```text
all special
['special', 'all']
special
{'realms': 1, 'overlaps': 1, 'conflicts': 0, 'gaps': 0}
{'from': 100, 'to': 199}
5 True
```

**Changing a layout safely.** `plan_layout_evolution` shows the effect of a
change without applying it; `evolve_layout` applies it. Changes that would
give already stored data a different layout owner are rejected with
`SchemaError`.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "evolve.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "u32"}])
    db.create_layout(1, 1, "first", "CLASSIC", shape_id=1, u_start=1, u_end=1000)
    db.put(1, 5, [7], commit=True)

    plan = db.plan_layout_evolution(1, u_end=None)
    print(plan["status"], plan["routing_changes"][0]["from"])
    print(db.evolve_layout(1, u_end=None, description="open ended", commit=True))
    try:
        db.evolve_layout(1, u_start=10)                  # would orphan the record at u=5
    except nhydb.SchemaError:
        print("rejected")
```

```text
safe 1001
LayoutDef(layout_id=1, realm_id=1, name="first", kind='CLASSIC', u_start=1, u_end=None, priority=0)
rejected
```

`evolve_layout(..., add_fields=[...])` adds fields to a COLUMN/PACKED/CUSTOM
layout; `spacemap_update` applies to CUSTOM layouts only.

**CUSTOM layouts** delegate the mapping to a Python object that you
register at runtime (only the plugin name is stored). The plugin needs
`logical_address(layout, object_id, field, lane)` and, for reading,
`type_of(layout, address)` and `describe(layout, address)`. Optional:
`spacemap` and `occupied_addresses(db, layout, object_id, lane)`.

```python
import os
import tempfile

import nhydb


class KeyValuePairs:
    """Object k stores 'key' at v = 2k-1 and 'val' at v = 2k, all in u = 100."""

    def logical_address(self, layout, object_id, field, lane):
        offset = {None: 0, "key": 0, "val": 1}[field]
        return {"u": 100, "v": (object_id - 1) * 2 + offset + 1, "lane": lane}

    def type_of(self, layout, address):
        return "utf8"

    def describe(self, layout, address):
        v = address["v_local"]
        return {"record": (v - 1) // 2 + 1, "field": ("key", "val")[(v - 1) % 2], "anchor": (v - 1) % 2 == 0}


path = os.path.join(tempfile.mkdtemp(), "custom.nhydb")
with nhydb.create(path) as db:
    db.create_realm(50, "kv")
    db.create_custom_layout(9, 50, "pairs", plugin="kv-pairs", u_start=100, u_end=100,
                            params={"fields": [{"name": "key"}, {"name": "val"}]})
    db.register_layout_plugin("kv-pairs", KeyValuePairs())   # needed again after every open
    db.put_object(9, 3, {"key": "color", "val": "blue"}, commit=True)
    print(db.get_object(9, 3)["val"], db.logical_address(9, 3, "val")["v_local"])
    print(db.get_n(db.logical_address(9, 3, "key")["N"])["data"])
```

```text
blue 6
{'key': 'color', 'val': 'blue'}
```

Without a registered plugin, operations that need the mapping raise
`SchemaError("CUSTOM layout requires registered plugin '…'")`. A plugin
must not write into the same database from inside a callback.
`occupied_addresses(db, layout, object_id, lane)` is also called during
`delete_record`, while `db` is locked (`RuntimeError: Already mutably
borrowed`); compute the addresses from `layout`, `object_id` and `lane`
only and do not call `db` there.

`create_layout(...)` is the generic form of all `create_*_layout` methods
(`kind="CLASSIC" | "COLUMN" | "PACKED" | "CUSTOM"`, `replace=True` to
redefine a layout whose address space is still empty).

---

## 18. MATH_MAP: placing values by formulas

A MATH layout stores a value at a cell **computed** from input values:
`inputs → (u, V, side)` with integer expressions. `V` is a global
coordinate; the engine maps it to realms automatically and owns these
realms exclusively.

```python
from nhydb.math import MathExpression, MathMap

e = MathExpression.compile("gcd(a, b) + a // 2", ("a", "b"))
print(e.evaluate(a=12, b=18))

grid = MathMap.compile("grid", ("x", "y"), u="x + 1", V="y", side="x % 2")
t = grid.target(3, 5)
print(t.u, t.V_global, t.logical_realm, t.v_local, t.side)
print(grid.target(3, 10**100 + 1).logical_realm)          # V beyond 10^100 -> next logical realm
print([t.u for t in grid.targets([(1, 1), (2, 2)])])
print(MathMap.from_dict(grid.to_dict()).fingerprint == grid.fingerprint)

from nhydb.math import DEFAULT_REALM_AUTO_LIMIT, join_global_v, normalize_side, split_global_v
print(split_global_v(10**100 + 5), join_global_v(2, 5) == 10**100 + 5)
print(normalize_side("NR"), normalize_side(False), DEFAULT_REALM_AUTO_LIMIT)
```

```text
12
4 5 1 5 1
2
[2, 3]
True
(2, 5) True
1 0 4096
```

Expressions use Python syntax for integers: `+ - * // % **`, comparisons,
`and`/`or`/`not` and the functions `abs`, `gcd`, `lcm`, `isqrt`, `min`,
`max`. Not allowed: `/`, bit operators, attributes, conditional
expressions. Errors raise `MathSyntaxError` or `MathDomainError` (both
subclasses of `MathError`, which is an `NHyDBError`). Brackets may be
nested 200 levels deep as in CPython; long `+` chains are fine.

**Logical and physical realms.** `V` is split into a *logical* realm
(`⌊(V−1)/10^100⌋ + 1`, unbounded – 10^5000 is fine) and a local `v`. The
engine stores the cell under the next free *physical* realm (small numbers
from 1 upward) and keeps the mapping in the encrypted layout plan. The
`N` of a stored MATH cell is therefore computed from the physical realm;
`get_math_cell` also reports `_logical_realm` and `_V_global`.
`realm_auto_limit` limits the **number** of logical realms a layout may
use, not their size.

Check a mapping before writing with `dry_run` (no database needed):

```python
from nhydb.math import MathMap, dry_run

grid = MathMap.compile("grid", ("x", "y"), u="x + 1", V="y", side="NL")
report = dry_run(grid, [(1, 1), (1, 1), (2, 10**100 + 3), (-5, 1)])
print(report.total, report.valid, report.invalid, report.collisions, report.unique_cells)
print(report.logical_realms, report.invalid_examples)
```

```text
4 3 1 1 2
(1, 2) (((-5, 1), 'u must be a positive integer'),)
```

Write and read MATH data:

```python
import os
import tempfile

import nhydb
from nhydb.math import MathMap, MathMapLayoutSpec

path = os.path.join(tempfile.mkdtemp(), "math.nhydb")
with nhydb.create(path) as db:
    grid = MathMap.compile("grid", ("x", "y"), u="x + 1", V="y", side="x % 2")
    state = db.create_math_layout(MathMapLayoutSpec("grid", grid, 64), layout_id=7, payload_type="utf8")
    print(state)

    print(db.put_math(7, (1, 7), "first"))                           # one value
    report = db.put_math_batch(7, [((x, y), f"{x},{y}") for x in (2, 3) for y in (1, 10**100 + 1)],
                               commit=True)
    print(report["cells"], report["new_realms"], report["realm_extents"])
    print(db.get_math(7, (3, 10**100 + 1)), db.get_math(7, (9, 9)))
    cell = db.get_math_cell(7, (3, 10**100 + 1))
    print(cell["_realm"], cell["_u"], cell["_lane_name"], cell["_logical_realm"])
    info = db.describe_math(7)
    print(info["frozen"], info["payload_type"], info["mapped_realms"])
    try:
        db.put_math(7, (2, 1), "again")                              # cell already used
    except nhydb.MathCellConflictError:
        print("conflict")
    print(db.put_math(7, (2, 1), "replaced", overwrite=True)["u"], db.get_math(7, (2, 1)))
```

```text
MathLayoutState(layout_id=7, name='grid', frozen=False, payload_type='utf8', mapped_realms=0)
{'layout_id': 7, 'physical_realm': 1, 'u': 2, 'v': 7, 'lane': 1, 'lane_name': 'NR'}
4 1 [{'logical_start': 1, 'logical_end': 2, 'physical_start': 1, 'physical_end': 2}]
3,10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001 None
2 4 NR 2
True utf8 2
conflict
3 replaced
```

A whole `put_math_batch` is rolled back if any row fails. The short form
`create_math_map_layout` takes the expressions directly; the mapping can be
replaced with `replace_math_spec` only **before** the first write
(afterwards `MappingFrozenError`):

```python
import os
import tempfile

import nhydb
from nhydb.math import MathMap, MathMapLayoutSpec

path = os.path.join(tempfile.mkdtemp(), "squares.nhydb")
with nhydb.create(path) as db:
    db.create_math_map_layout("squares", ("k",), u="k", V="k * k", side="NR",
                              payload_type="varint", layout_id=1)
    new = MathMap.compile("squares", ("k",), u="k", V="k * k + 1", side="NR")
    db.replace_math_spec(1, MathMapLayoutSpec("squares", new, 64))    # allowed: nothing written yet
    db.put_math(1, (12,), 144, commit=True)
    print(db.get_math(1, (12,)), db.get_math_cell(1, (12,))["_V_global"])
    try:
        db.replace_math_spec(1, MathMapLayoutSpec("squares", new, 64))
    except nhydb.MappingFrozenError:
        print("frozen")
    print(list(db.math_layouts))
```

```text
144 145
frozen
[1]
```

NumPy input (2-D array, 1-D array or list of columns):

```python
# requires: numpy
import os
import tempfile

import numpy as np

import nhydb
from nhydb.math import MathMap, MathMapLayoutSpec

path = os.path.join(tempfile.mkdtemp(), "mathnp.nhydb")
with nhydb.create(path) as db:
    m = MathMap.compile("g", ("x", "k"), u="x // 100 + 1", V="k * 10**100 + x % 100 + 1", side="x % 2")
    db.create_math_layout(MathMapLayoutSpec("g", m, 64), layout_id=1, payload_type="f64")
    x = np.arange(1000, dtype=np.int64)
    report = db.put_math_numpy(1, np.stack([x, x % 3], axis=1), x * 0.5, commit=True)
    print(report["cells"], db.get_math(1, (999, 0)))
```

```text
1000 499.5
```

Find MATH cells by input tuples with the QueryEngine provider
`math_layout_inputs`, and limit the size of intermediate results
(default 2^20 bits):

```python
import os
import tempfile

import nhydb
from nhydb.math import MathExpression, MathMap, MathMapLayoutSpec, get_math_max_intermediate_bits, set_math_max_intermediate_bits
from nhydb.query import Q, QueryEngine

path = os.path.join(tempfile.mkdtemp(), "mathq.nhydb")
with nhydb.create(path) as db:
    grid = MathMap.compile("grid", ("x", "y"), u="x + 1", V="y", side="x % 2")
    db.create_math_layout(MathMapLayoutSpec("grid", grid, 64), layout_id=1, payload_type="utf8")
    db.put_math_batch(1, [((x, y), f"{x}:{y}") for x in (1, 2) for y in (1, 2)], commit=True)
    r = QueryEngine(db).execute(Q.math("math_layout_inputs", layout_id=1, inputs=[(1, 2), (2, 2), (7, 7)]),
                                return_mode="cell")
    print([(row["N"], row["value"]) for row in r["rows"]])

print(get_math_max_intermediate_bits())
e = MathExpression.compile("2 ** x", ("x",))
old = get_math_max_intermediate_bits()
set_math_max_intermediate_bits(64)
try:
    e.evaluate(x=100)
except nhydb.MathDomainError:
    print("too large")
finally:
    set_math_max_intermediate_bits(old)
```

```text
[('N18', '1:2'), ('N22', '2:2'), ('N232', None)]
1048576
too large
```

There are two independent bit limits. Inside a database the limit comes
from `[math] math_max_intermediate_bits` of the configuration (default
2^20) and is **frozen into each MATH layout when it is created**
(`db.math_layouts[id].max_intermediate_bits`). `set_math_max_intermediate_bits`
changes only the process-wide limit for standalone `MathExpression`,
`MathMap` and `dry_run` objects, as in the example above. Neither limit
applies to values that a formula only passes through (`u="x"`): such
inputs may have any size.

**Very large numbers in Python output.** Python refuses to convert integers
with more than 4,300 decimal digits to text (`ValueError: Exceeds the limit
… for integer string conversion`). NHyDB stores and returns such numbers
exactly; only printing them needs `sys.set_int_max_str_digits(0)` (or the
environment variable `PYTHONINTMAXSTRDIGITS=0`, also for the command line).

---

## 19. Configuration and limits

Settings can come from a TOML file. The engine looks in this order:
`config=` argument, environment variable `NHYDB_CONFIG`,
`nhydb_config.toml` next to the database, built-in defaults. A commented
template is `config/nhydb_config.example.toml` in the repository.

```python
import os
import tempfile

import nhydb

folder = tempfile.mkdtemp()
with open(os.path.join(folder, "nhydb_config.toml"), "w", encoding="utf-8") as f:
    f.write('[database]\ndb_max_size_bytes = 300000\ndurability = "full"\n\n[lock]\ntimeout_ms = 2000\n')

path = os.path.join(folder, "configured.nhydb")
with nhydb.create(path) as db:                       # the file next to the database is found
    print(os.path.basename(db.config_source), db.durability, db.max_db_size_bytes)
    db.create_realm(1, "r")
    try:
        db.put_points(1, [(u, 1, b"x" * 1000, "bytes") for u in range(1, 400)])
    except nhydb.DatabaseSizeLimitError:
        print("size limit reached")
```

```text
nhydb_config.toml full 300000
size limit reached
```

| Section / key | Meaning |
|---|---|
| `[database] db_max_size_bytes` | size limit for **new** databases (default 50 GiB, at least 64 KiB) |
| `[database] durability` | `"fsync"` or `"full"` (aliases `"default"`/`"normal"`, `"fullfsync"`) |
| `[math] realm_auto_limit` | realm budget for new MATH layouts (default 4096) |
| `[math] math_max_intermediate_bits` | bit limit of MATH expressions, frozen into each new MATH layout |
| `[lock] enabled`, `timeout_ms` | lock file and wait time |
| `[paths] layout_path`, `key_path` | path templates, `{db}` = database file name, `{db_path_hash}` = hash of the absolute database path (for equally named databases sharing a key folder) |
| `[cache] block_cache_bytes` | budget of the block cache per open object (default 64 MiB, LRU; `0` = no cache); blocks up to max(8 MiB, budget/8) are cached |
| `[cache] vindex_max_marks`, `vindex_max_indexes` | accepted and checked, not used yet (fixed values) |

Explicit arguments win over the file (`lock=`/`lock_timeout_ms=` override
the `[lock]` keys one by one). Values stored in the database (the size
limit, the realm budget and bit limit of a MATH layout) are never
overridden when opening. Unknown keys are errors. Change the size limit of an existing
database with `set_max_db_size`:

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "limit.nhydb")
with nhydb.create(path) as db:
    db.set_max_db_size(10 * 1024**2, commit=True)     # 10 MiB
with nhydb.open(path) as db:
    print(db.max_db_size_bytes)
```

```text
10485760
```

**Block cache.** Every block is read completely and CRC-checked once per
open object; then it is either kept in the block cache or, if it does not
fit, later read only in the parts that are needed. `stats()["block_cache"]`
shows the budget, the largest cacheable block, the cached and verified
blocks and the counters `hits`, `loads`, `partial_reads` and `evictions`:

```python
import os
import tempfile

import nhydb

folder = tempfile.mkdtemp()
with open(os.path.join(folder, "nhydb_config.toml"), "w", encoding="utf-8") as f:
    f.write("[cache]\nblock_cache_bytes = 16777216\n")      # 16 MiB block cache

path = os.path.join(folder, "cached.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "u32"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 10_001)], commit=True)

with nhydb.open(path, readonly=True) as db:
    for u in (1, 5_000, 10_000):
        db.get(1, u)
    bc = db.stats()["block_cache"]
    print(bc["budget_bytes"], bc["max_item_bytes"])
    print(bc["loads"], bc["hits"], bc["cached_blocks"], bc["verified_blocks"])
```

```text
16777216 8388608
1 2 1 1
```

---

## 20. Maintenance: stats, health, verify, repair, compaction

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "maint.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "u32"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    for u in range(1, 201):
        db.put(1, u, [u], commit=True)                # many small commits -> many blocks

    st = db.stats()
    print(st["format"], st["storage"]["physical_dense_records"], st["file_bytes"] > 0)
    h = db.health(deep=True)
    print(h["status"], [(c["name"], c["status"]) for c in h["checks"] if c["status"] != "ok"])
    offset = db.checkpoint()                          # writes a CHECKPOINT block and commits
    print(offset == db.checkpoint_offset, db.health()["status"])
    v = db.verify(deep=True)
    print(v["ok"], v["error_count"])

print(nhydb.NHyDB.inspect_tail(path)["clean"])
res = nhydb.NHyDB.compact_file(path, make_backup=False)   # rewrite the live state compactly
print(res["ok"], res["records_written"], res["after_bytes"] < res["before_bytes"])
with nhydb.open(path) as db:
    print(db.get(1, 200)["x"])
```

```text
0.16 200 True
warning [('checkpoint', 'warning')]
True healthy
True 0
True
True 200 True
200
```

* `verify(deep=False)` checks the block structure (`deep=True` also the
  payload checksums), continues after damage and lists every damaged
  block; stale indexes appear under `warnings` (they are not errors).
* `health(deep=False)` runs the checks `container`, `layouts`, `spacemap`,
  `layout_routing`, `math_layouts`, `definitions`, `indexes` and
  `checkpoint`; status `healthy`, `warning` or `unhealthy` (a stale index
  is a `warning`). A database without a recent checkpoint reports a
  `checkpoint` warning;
  `checkpoint()` writes one (a diagnostic snapshot of the segment
  structure; it does not replace older blocks).
* `stats()` reports file, format, realms, shapes, layouts, extensions,
  indexes and storage statistics, and under `block_cache` the block cache
  of this object (section 19).
* `compact_file(path, output=None, make_backup=True, ...)` is static and
  needs the database to be closed. In place it keeps the identity and side
  files and writes a backup `<db>.before_compact_<timestamp>` plus
  `<db>.before_compact_<timestamp>.layout.enc` (unless
  `make_backup=False`). With `output=` it exports to a **new** database
  with its own side files. A failed compaction leaves the source unchanged
  and removes only the files it created. Memory use is bounded by one
  batch (`batch_records`), not by the size of the database: records are
  copied as raw bytes and sparse cells in chunks.

**Repairing a damaged tail.** `inspect_tail` checks the end of the file
without opening it; `repair_file(path, apply=False)` only reports,
`apply=True` cuts off the damaged tail (with a backup `<db>.bak_<timestamp>`
by default):

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "damaged.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "r")
    db.put_cell(1, 1, 1, "kept", type="utf8", commit=True)

with open(path, "ab") as f:
    f.write(b"NHBL" + b"\x07" * 40)                  # simulate an incomplete write

tail = nhydb.NHyDB.inspect_tail(path)
print(tail["clean"], tail["trailing_bad_bytes"])
print(nhydb.NHyDB.repair_file(path)["clean"])        # report only
rep = nhydb.NHyDB.repair_file(path, apply=True, make_backup=False)
print(rep["applied"], nhydb.NHyDB.inspect_tail(path)["clean"])
with nhydb.open(path) as db:
    print(db.get_value(1, 1, 1))
```

```text
False 44
False
True True
kept
```

`repair_file` never sacrifices committed data silently:

* If intact committed blocks lie behind a damaged spot, it raises
  `CorruptionError` and changes nothing. `allow_data_loss=True` cuts them
  anyway and lists them in `lost_blocks`.
* Uncommitted blocks after the commit boundary are discarded
  (`discarded_uncommitted_blocks`); `keep_uncommitted=True` keeps intact
  ones (`kept_uncommitted_blocks`).
* If one superblock is damaged and complete blocks follow the boundary, you
  must decide explicitly with one of these two switches.
* The dry run (`apply=False`) reports the planned `action` (`none`,
  `repair` or `abort`) and, if it would refuse, `plan_error`.
* The backup consists of `<db>.bak_<timestamp>` and
  `<db>.bak_<timestamp>.layout.enc`.

Opening a database with a damaged tail also works – the writer cuts off
everything after the commit boundary automatically, as long as both
superblocks agree. If they do not (only possible after a power or system
failure while a superblock was written – a killed process never causes
it), opening fails without changing the file: `nhydb.open(path,
readonly=True)` still reads the previous committed state, and
`repair_file(path, apply=True, keep_uncommitted=True)` keeps the intact
blocks after the boundary (`allow_data_loss=True` discards them).

To go back to a backup of `repair_file` or `compact_file`, call
`nhydb.NHyDB.restore_backup(backup_path, path)` (command line:
`nhydb restore BACKUP DB`). It checks that the backup belongs to the
database and opens with its key, then replaces the data file and the layout
plan; the key file stays as it is.

---

## 21. Database files, backup, restore and moving

| File | Content | Note |
|---|---|---|
| `X.nhydb` | data (append-only container) | not encrypted |
| `X.nhydb.layout.enc` | schema (realms, shapes, layouts, indexes, MATH), encrypted | |
| `X.nhydb.layout.key` | 32-byte key for the schema file | **without it the database cannot be opened**; Unix permissions `0600` |
| `X.nhydb.lock` | lock file, empty | may be deleted while the database is closed |

Rules:

* **Back up all three files together** (`.nhydb`, `.layout.enc`,
  `.layout.key`) while the database is **closed**. Keep a copy of the key
  file in a safe place; a lost key cannot be recovered.
* The file name is part of the database identity: a renamed `.nhydb` does
  not open (also not a change of upper/lower case only). Restore backups
  under the **same file name** (another folder is fine). To create a copy
  under a new name use `compact_file(path, output=new_path)`.
* Only the schema is encrypted. The data in `X.nhydb` is stored in plain
  form; protect the files with file-system permissions or disk encryption.
* The key file is created with permissions `0600` on Unix; if it is
  readable by group or others, opening issues a `RuntimeWarning` (also in
  `db.open_warnings`). **Windows:** the key file inherits the ACL of its
  folder – NHyDB sets no ACL of its own. Keep key files in a folder only you
  can access (for example below `%USERPROFILE%`) or point `key_path` there.
* A backup copy on read-only media can be inspected with
  `nhydb.open(path, readonly=True)`.
* **Databases of NHyDB 0.27.1 or earlier do not open** with 0.28.0, also
  not with `readonly=True`: opening fails closed with `CorruptionError`
  (the message names 0.27.1) and changes nothing. There is no migration and
  `repair_file` cannot convert such a file (its dry run reports
  `action: "abort"`). Export the data with NHyDB 0.27.1 and re-import it
  into a new database with 0.28.0.

```python
import os
import shutil
import tempfile

import nhydb

live = tempfile.mkdtemp()
path = os.path.join(live, "crm.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "crm")
    db.put_cell(1, 1, 1, "customer 1", type="utf8")

# backup: copy the three files of the closed database
backup = tempfile.mkdtemp()
for suffix in ("", ".layout.enc", ".layout.key"):
    shutil.copy2(path + suffix, os.path.join(backup, "crm.nhydb" + suffix))

# restore into another folder, same file name
restored = tempfile.mkdtemp()
for suffix in ("", ".layout.enc", ".layout.key"):
    shutil.copy2(os.path.join(backup, "crm.nhydb" + suffix), os.path.join(restored, "crm.nhydb" + suffix))
with nhydb.open(os.path.join(restored, "crm.nhydb")) as db:
    print(db.get_value(1, 1, 1))

# a renamed file is rejected ...
os.rename(os.path.join(restored, "crm.nhydb"), os.path.join(restored, "renamed.nhydb"))
try:
    nhydb.open(os.path.join(restored, "renamed.nhydb"),
               layout_path=os.path.join(restored, "crm.nhydb.layout.enc"),
               key_path=os.path.join(restored, "crm.nhydb.layout.key"))
except nhydb.CorruptionError:
    print("renamed file rejected")

# ... use compact_file(output=...) for a copy under a new name
copy = os.path.join(tempfile.mkdtemp(), "crm_copy.nhydb")
nhydb.NHyDB.compact_file(path, output=copy)
print(sorted(os.listdir(os.path.dirname(copy))))
with nhydb.open(copy) as db:
    print(db.get_value(1, 1, 1))
```

```text
customer 1
renamed file rejected
['crm_copy.nhydb', 'crm_copy.nhydb.layout.enc', 'crm_copy.nhydb.layout.key', 'crm_copy.nhydb.lock']
customer 1
```

**Keep the key file somewhere else.** Pass `key_path` (and optionally
`layout_path`) when creating and every time you open the database – or put
the path templates into the configuration file (section 19):

```python
import os
import tempfile

import nhydb

data_dir, key_dir = tempfile.mkdtemp(), tempfile.mkdtemp()
path = os.path.join(data_dir, "secure.nhydb")
key = os.path.join(key_dir, "secure.nhydb.layout.key")

with nhydb.create(path, key_path=key) as db:
    db.create_realm(1, "r")
print(sorted(os.listdir(data_dir)), os.listdir(key_dir))

try:
    nhydb.open(path)                                    # key not found next to the database
except OSError:
    print("key missing -> cannot open")
with nhydb.open(path, key_path=key) as db:
    print(1 in db.realms)
```

```text
['secure.nhydb', 'secure.nhydb.layout.enc', 'secure.nhydb.lock'] ['secure.nhydb.layout.key']
key missing -> cannot open
True
```

---

## 22. Errors, locks and concurrent access

All engine errors derive from `nhydb.NHyDBError`, including the MATH
expression errors (`MathError`), the MATH realm planner (`LayoutError`)
and the query layer (`QueryError`). Only `IndexConfigError` is a
`ValueError` instead.

```text
NHyDBError
├── CorruptionError, SchemaError, StaleIndexError, RealmLimitError, AddressError
├── DatabaseLockedError, DatabaseSizeLimitError
├── MathLayoutError ── MathLayoutNotFound, MappingFrozenError, MathCellConflictError
├── MathError ── MathSyntaxError, MathDomainError
├── LayoutError ── LayoutStateError, RealmBudgetExceeded, StalePlanError, AllocationConflictError
└── QueryError ── QueryPlanError, QueryDomainError, SQLSyntaxError
ValueError ── IndexConfigError
```

The engine also raises built-in exceptions:

* `ValueError`, `TypeError`, `KeyError` for invalid arguments (for example
  `TypeError` for `u=2.7`, `ValueError` for IDs above 2^63 − 1 or values
  nested deeper than 256 levels);
* `OverflowError` when a Python integer does not fit a 64-bit parameter
  (for example `u_window=2**64`), `UnicodeEncodeError` for text with lone
  surrogates;
* `MemoryError` when a result cannot fit into memory;
* `OSError` subclasses with `errno` and, where known, `filename`:
  `FileNotFoundError`, `FileExistsError`, `PermissionError`,
  `IsADirectoryError` …;
* `RuntimeError` when one object is used by two threads at the same time,
  or when a plugin calls back into a database that is writing.

Damaged files always raise `CorruptionError`.

```python
import nhydb

print(issubclass(nhydb.CorruptionError, nhydb.NHyDBError))
print(issubclass(nhydb.MathSyntaxError, nhydb.NHyDBError), issubclass(nhydb.MathSyntaxError, nhydb.MathError))
print(issubclass(nhydb.SQLSyntaxError, nhydb.QueryError), issubclass(nhydb.IndexConfigError, ValueError))
try:
    nhydb.n_to_dynadron_address(576)
except nhydb.NHyDBError as e:
    print(type(e).__name__)
```

```text
True
True True
True True
AddressError
```

**One writing object per file.** Opening a database for writing takes an
exclusive lock on `<db>.lock`. A second such open (in the same or in
another process) raises `DatabaseLockedError`, unless it waits with
`lock_timeout_ms`. Readers that must not block can open with
`readonly=True`: no lock, nothing in the file is changed, write calls and
`commit()` raise `NHyDBError`, and the object sees the committed state at
the time it was opened (later commits of the writer are not reloaded).

```python
import os
import subprocess
import sys
import tempfile
import textwrap

import nhydb

path = os.path.join(tempfile.mkdtemp(), "shared.nhydb")
nhydb.create(path).close()

with nhydb.open(path) as db:
    try:
        nhydb.open(path)                                # same process, second object
    except nhydb.DatabaseLockedError:
        print("locked (same process)")
    with nhydb.open(path, readonly=True) as reader:     # readers need no lock
        print("read-only:", reader.readonly)

holder = textwrap.dedent("""
    import sys, time, nhydb
    db = nhydb.open(sys.argv[1])
    print("locked", flush=True)
    time.sleep(1.0)
    db.close()
""")
proc = subprocess.Popen([sys.executable, "-c", holder, path], stdout=subprocess.PIPE, text=True)
assert proc.stdout.readline().strip() == "locked"
try:
    nhydb.open(path)
except nhydb.DatabaseLockedError:
    print("locked (other process)")
db = nhydb.open(path, lock_timeout_ms=10_000)           # waits until the other process closes
print("opened after waiting")
db.close()
proc.wait(timeout=30)
proc.stdout.close()
```

```text
locked (same process)
read-only: True
locked (other process)
opened after waiting
```

`lock=False` disables the lock file. Only use it when you can guarantee
that no other process opens the database at the same time; with
`lock=False` an uncommitted tail is never cut off (`DatabaseLockedError`).
The lock file never follows a symbolic link.

**Threads.** Long calls release the GIL while the Rust core works: queries
(`query`, `sql`, `sql_numpy`, `explain_query`, `QueryEngine` methods),
`stats`, `health`, `verify`, `read_numpy`, the batch writers, `commit`,
`rollback` and the static file operations. Python callbacks (query
providers, layout plugins) take the GIL back for their call. Short calls
such as `get` and `find_u` keep the GIL on purpose (handing it over would
cost more than the call). Several threads may run read-only queries on the
same object at the same time; a write, `commit` or `rollback` while another
call holds the object fails immediately with `RuntimeError`
("Already borrowed") – it does not wait and changes nothing. For parallel
work give each thread its own object (readers with `readonly=True`; four
threads with own objects run SQL scans about 2.9 times faster than one) or
protect a shared object with a `threading.Lock`.

---

## 23. System space R0,u1

Every database stores its identity in the reserved cells `(0, 1, v)`:
database id, file names, schema generation and hash, creation time,
encryption scheme and the fixed constants.

```python
import os
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "sys.nhydb")
with nhydb.create(path) as db:
    meta = db.get_system_metadata()
    print(meta["realm"], meta["u"], sorted(meta["cells"], key=int))
    print(db.get_system_cell(2), db.get_system_cell(8))
    print(db.get_system_cell(9)["realm_span"] == 10**100)
```

```text
0 1 ['1', '2', '3', '4', '5', '6', '7', '8', '9']
sys.nhydb AES-256-GCM + HMAC-SHA256
True
```

---

## 24. Command line

The package installs the command `nhydb` (also `python -m nhydb`) and the
query tool `python -m nhydb.query`. `--help` shows all options; `--json`
gives machine-readable output.

```bash
nhydb to-n --realm 1 --u 11 --v 13 --lane NR
nhydb from-n N589
nhydb get shop.nhydb u2,v3 --realm 1 --mode cell
nhydb stats shop.nhydb
nhydb health shop.nhydb --deep
nhydb verify shop.nhydb
python -m nhydb.query shop.nhydb --sql "SELECT COUNT(*) AS n FROM orders" --json
python -m nhydb.query shop.nhydb --realm 1 --eq "status=paid" --return count
```

Commands: `to-n`, `from-n`, `layout`, `layout-check`, `layout-plan`,
`evolve-layout`, `route`, `describe`, `logical-address`, `occupied`,
`stats`, `health`, `get`, `address`, `cell`, `cell-n`, `find`, `verify`,
`rebuild-indexes`, `repair`, `restore`, `compact`. The CLI uses the default
side-file locations or the configuration file. Read commands open the
database with `readonly=True` and never change the file; `verify` also works
on damaged files. `repair` is a dry run unless `--apply` is given
(`--allow-data-loss`, `--keep-uncommitted` as in `repair_file`). Exit codes:
`0` success, `1` a check found problems, `2` usage error or refused
operation, `3` error (message on stderr, no traceback). In JSON output
`bytes` appear as `{"$bytes_b64": "…"}`. The same calls from Python:

```python
import json
import os
import subprocess
import sys
import tempfile

import nhydb

path = os.path.join(tempfile.mkdtemp(), "cli.nhydb")
with nhydb.create(path) as db:
    db.create_realm(1, "shop")
    db.create_shape(1, 1, "order", [{"v": 1, "name": "id", "type": "varuint"},
                                    {"v": 2, "name": "status", "type": "utf8"}])
    db.create_classic_layout(1, 1, "orders", shape_id=1)
    db.put_dense_batch(1, 1, [[1, "paid"], [2, "new"], [3, "paid"]])


def run(*args):
    return subprocess.run([sys.executable, *args], capture_output=True, text=True, check=True).stdout


print(json.loads(run("-m", "nhydb", "to-n", "--realm", "1", "--u", "11", "--v", "13", "--lane", "NR", "--json"))["N"])
print(json.loads(run("-m", "nhydb", "get", path, "u2,v2", "--realm", "1", "--mode", "cell", "--json"))["value"])
out = json.loads(run("-m", "nhydb.query", path, "--sql", "SELECT COUNT(*) AS n FROM orders WHERE status = ?",
                     "--params", '["paid"]', "--json"))
print(out["rows"])
```

```text
589
new
[{'n': 2}]
```

---

## 25. Complete application examples

Runnable programs in [`examples/python/`](../../examples/python/); each
creates its database in a temporary directory:

| Program | Shows |
|---|---|
| [`sensor_timeseries.py`](../../examples/python/sensor_timeseries.py) | NumPy batch writes of a day of sensor data, time-window reads with `read_numpy`, NHySQL aggregation (needs NumPy) |
| [`document_store.py`](../../examples/python/document_store.py) | records with extensions, VALUE and FULLTEXT indexes, BM25 search, phrase queries, NHySQL grouping |
| [`math_map_grid.py`](../../examples/python/math_map_grid.py) | MATH_MAP planning with `dry_run`, writing and reading cells by input tuples, lookup through the QueryEngine |

---

## 26. Known limitations

* **One writing object per database.** The exclusive lock is taken for
  every read-write open; further readers use `readonly=True` (no lock,
  state at the time of opening). There are no multi-process transactions.
* **One writing call per object.** A write on an object that another
  thread is using raises `RuntimeError`; parallel read-only queries on one
  object are allowed, separate objects per thread scale best.
* **Rollback per object.** `rollback()` returns to the last commit of the
  object; multi-block calls are not atomic by themselves (exception:
  `put_math_batch`). There are no nested or multi-object transactions.
* **`batch()`** has no read-your-writes inside the block.
* **Context manager commits** on exit, even after an exception.
* **Key file.** Without the matching `*.layout.key` the database cannot be
  opened; a lost key cannot be recovered. Only the schema is encrypted, not
  the data.
* **Renamed database files** do not open (use `compact_file(output=...)`).
* **No upgrade of older files.** Databases created with NHyDB 0.27.1 or
  earlier raise `CorruptionError`; export them with 0.27.1 and re-import
  the data (section 21).
* **Indexes** are not maintained automatically (`build_indexes`), cover
  CLASSIC records only and should be rebuilt after updating NHyDB.
* **NHySQL** is read-only and works on CLASSIC layouts; no `DISTINCT`,
  `HAVING` or joins.
* **QueryEngine `order_by`** only takes effect for fields listed in
  `select` (0.28.0).
* **Large batches:** the first access to a block reads and checks the
  whole block once (a block of 1 million records is about 25 MB); later
  accesses read only the needed bytes.
* **CUSTOM layouts** need their plugin registered after every open;
  plugins must not write to the same database during a callback.
* **`[cache]` configuration:** only `block_cache_bytes` is used; the
  `vindex_*` keys are accepted but not used yet.
* **NumPy coordinate arrays** (`u`, `v`, object ids) are limited to
  `uint64`; use Python `int` lists for larger values.
* **Durability `full`** raises an error on macOS file systems without
  `F_FULLFSYNC` support.

---

## 27. API reference

Section numbers refer to the recipes above. Parameters in *italics* are
keyword-only. All write methods accept *`commit=False`*; reading and
writing methods with a lane accept *`lane=None`* (`NL`).

### 27.1 Module `nhydb`

| Name | Purpose | Key parameters | See |
|---|---|---|---|
| `create(path, **options)` / `NHyDB.create` | create a database | *overwrite, durability, lock, lock_timeout_ms, layout_path, key_path, config* | 3, 4 |
| `open(path, **options)` / `NHyDB(path, ...)` | open a database | as `create` plus *readonly* (without *overwrite*) | 3, 4, 22 |
| `dynadron_n(realm, u, v, lane)` | address → N | | 9 |
| `n_to_dynadron_address(n)` | N (int or `"N…"`) → address | | 9 |
| `parse_n_reference(ref)` | `"N589"` → 589 | | 9 |
| `cell_address(realm, u, v, lane)` | address with all indicators | | 9 |
| `dynadron_address(realm, u, v)` | indicators without lane | | 9 |
| `global_v`, `split_global_v`, `realm_v_bounds`, `validate_local_v` | realm ↔ global `v` | | 9 |
| `normalize_lane`, `lane_name` | lane conversion | | 9 |
| `enc_vu`, `dec_vu`, `enc_vi`, `dec_vi` | VarUInt/VarInt codecs | | 9 |
| `ENGINE_VERSION`, `FILE_FORMAT_VERSION`, `REALM_SPAN`, `TYPE_CODES` | constants | | 1, 9, 11 |
| `LayoutDef`, `IndexDef`, `MathLayoutState` | definition objects returned by the API (`to_dict()`) | | 13, 17, 18 |
| `QueryEngine` | query engine (also `nhydb.query.QueryEngine`) | | 14 |
| exceptions | see the hierarchy | | 22 |

### 27.2 `NHyDB` – life cycle, attributes and maintenance

| Method / attribute | Purpose | Key parameters | See |
|---|---|---|---|
| `commit()` | make data durable; returns the commit boundary | | 5 |
| `rollback()` | discard all changes since the last commit; returns the commit boundary | | 5 |
| `sync()` | write buffers (no commit) | *hard=False* | 6 |
| `close()` | release file and lock | *commit=True* | 5 |
| `checkpoint()` | write a checkpoint block and commit | | 20 |
| `closed`, `path`, `layout_path`, `key_path`, `config_source`, `readonly`, `open_warnings` | state, file locations (absolute) and warnings on open | | 4, 21, 22 |
| `durability` | `"fsync"`/`"full"`, writable | | 4, 5 |
| `max_db_size_bytes`, `set_max_db_size(size)` | size limit | | 19 |
| `committed_end`, `last_valid_end`, `checkpoint_offset`, `next_block_id`, `recovered_tail_bytes` | container state | | 4, 5, 6, 20 |
| `stats()`, `health()`, `verify()` | statistics (incl. `block_cache`) and checks | *deep=False* | 19, 20 |
| `NHyDB.inspect_tail(path)` | check the file (superblocks, tail, damaged blocks) without opening | | 20 |
| `NHyDB.repair_file(path)` | restore a consistent state; never cuts committed data silently | *apply=False, make_backup=True, rebuild_indexes=True, allow_data_loss=False, keep_uncommitted=False*, side-file options | 20 |
| `NHyDB.restore_backup(backup, path)` | put a backup of repair/compaction back in place | side-file options | 20 |
| `NHyDB.compact_file(path)` | rewrite in place or export | *output=None, make_backup=True, rebuild_indexes=True, batch_records=10000*, side-file options | 20, 21 |
| `get_system_metadata()`, `get_system_cell(v)` | system space R0,u1 | | 23 |

### 27.3 `NHyDB` – schema and layouts

| Method / attribute | Purpose | Key parameters | See |
|---|---|---|---|
| `create_realm(realm_id, name)` | define a realm (an occupied ID raises `SchemaError`; same for shapes, layouts and indexes) | *description, default_shape_id, base_reserve_to* | 3, 8 |
| `realms`, `realm_bounds(realm_id)` | realm definitions, `v` range | | 4, 9 |
| `create_shape(shape_id, realm_id, name, fields)` | BASE fields `[{v, name, type, enum?}]` | *description* | 3, 6 |
| `set_realm_default_shape(realm_id, shape_id)` | default shape | | 6 |
| `define_extension(realm_id, v, type)`, `define_extensions(realm_id, fields)` | extension fields | *name, enum, description, replace* | 8 |
| `extension_catalog(realm_id)`, `resolve_v(realm_id, column)` | list extensions, name → `v` | | 8 |
| `create_classic_layout(layout_id, realm_id, name, *, shape_id)` | CLASSIC layout | *base_reserve_to, u_start, u_end, priority* | 3, 6 |
| `create_column_layout(..., *, fields)` | COLUMN layout `[{u, name, type}]` | *anchor_u, priority* | 17 |
| `create_packed_layout(..., *, u, span, fields)` | PACKED layout `[{offset, name, type}]` | *priority* | 17 |
| `create_custom_layout(..., *, plugin, u_start, u_end)` | CUSTOM layout | *params, spacemap, priority* | 17 |
| `register_layout_plugin(name, plugin)` | attach the Python plugin | | 17 |
| `create_layout(layout_id, realm_id, name, kind)` | generic form | *u_start, u_end, priority, shape_id, params, spacemap, plugin, replace* | 17 |
| `get_layout(...)`, `layout_at(realm, u)`, `layout_candidates(realm, u)`, `route_address(realm, u, v)`, `layout_diagnostics(realm)` | routing | | 17 |
| `plan_layout_evolution(layout_id, ...)`, `evolve_layout(layout_id, ...)` | change a layout safely | *name, description, u_start, u_end, priority, add_fields, spacemap_update* | 17 |
| `logical_address(layout_id, object_id, field)`, `record_anchor(layout_id, object_id)` | object → cell address | | 10, 17 |

### 27.4 `NHyDB` – writing

| Method | Purpose | Key parameters | See |
|---|---|---|---|
| `put_record(layout_id, object_id, values)` | one record (dict) | | 3, 6 |
| `put(realm_id, u, values)` | one record (list) | *shape_id, extensions* | 6, 8 |
| `put_dense_batch(realm_id, u_start, rows)` | consecutive records | *shape_id, extensions_by_u* | 6, 8 |
| `put_object(layout_id, object_id, values)`, `put_objects_batch(layout_id, objects)` | records of any layout | *u_window* | 17 |
| `put_cell(realm_id, u, v, value)` | one cell | *type* | 7 |
| `put_points(realm_id, cells)` | many cells `(u, v, value[, type])` | *u_window* | 7 |
| `batch()` → `WriteBatch` | context manager collecting `put_cell`/`delete_cell`; `flush()`, `discard()`, `len()` | *commit=False, u_window=10000* | 7 |
| `put_extensions(realm_id, u, mapping)`, `put_extension_list(realm_id, u, vs, values)`, `put_extension_range(realm_id, u, start_v, values)` | extension values | *merge=True* | 8 |
| `delete_record(layout_id, object_id)` | delete a record completely (dense record plus its single cells and extensions); returns the number of deleted items | | 6, 7 |
| `delete(realm_id, u)` | hide only the dense record; later single cells and extensions stay visible; returns `True`/`False` | | 6, 7 |
| `delete_cell(realm_id, u, v)`, `delete_point(realm_id, u, v)` | delete single cells; `delete_cell` returns `True`/`False`, `delete_point` the write report | | 7 |

### 27.5 `NHyDB` – reading and addresses

| Method | Purpose | Key parameters | See |
|---|---|---|---|
| `get_record(layout_id, object_id)`, `get_object(...)` | one record | *include_addresses* (`get_object`) | 3, 6, 17 |
| `get(realm_id, u)` | dense record with extensions | *include_extensions, dyn_address, dyn_v* | 6, 12 |
| `get_value(realm_id, u, v)` | value of one cell (`v` also as a column name) | | 3, 7 |
| `get_cell(realm_id, u, v)` | cell with metadata | *dyn_address=True* | 7 |
| `get_internal(realm_id, u)` | raw values by `v` | | 6 |
| `get_record_view(layout_id, object_id)` | record with all cell numbers | *request_n* | 10 |
| `range(realm_id, u_from, u_to)`, `select(realm_id, u_from, u_to, columns)`, `select_us(realm_id, us, columns)` | lazy iterators | *dyn_address, dyn_v, include_extensions* | 12 |
| `iter_materialized_cells`, `iter_materialized_sparse_points`, `iter_materialized_record_ns` | everything stored | *realm_id* | 12 |
| `set_dyn_address_output(mode)` | default address indicators | *v* | 12 |
| `get_n(n)`, `get_n_cell(n)`, `get_n_record(n)`, `get_cell_n(n)`, `get_address(realm, u, v)` | read by N / address | *mode="auto"* | 10 |
| `describe_n(n)`, `resolve_n(n)` | explain an N | | 10 |
| `record_n_for_address(realm, u, v)`, `record_n_from_n(n)` | record anchor N | | 10 |
| `occupied_n(layout_id, object_id)`, `occupied_from_n(n)` | occupied cells of an object | *output="N"\|"ADDRESS"\|"BOTH"* | 10 |
| `cell_address(realm, u, v)`, `dyn_address(realm, u, v)` | address indicators | | 10 |

### 27.6 `NHyDB` – indexes, queries, NumPy, MATH

| Method / attribute | Purpose | Key parameters | See |
|---|---|---|---|
| `create_index(index_id, realm_id, name, field)` | VALUE (or other) index | *kind, options, tokenizer, casefold, ngram* | 13 |
| `create_token_index`, `create_trigram_index`, `create_fulltext_index` | text indexes | *tokenizer, casefold, ngram* | 13 |
| `build_indexes(index_ids=None)` | build / refresh | | 13 |
| `index_defs` | `{id: IndexDef}` | | 13 |
| `find_u(realm_id, filters)`, `find(realm_id, filters)` | index filter → `u` / records | *u_from, u_to* | 13 |
| `posting_count(index_id, value)`, `index_candidates(index_id, op, value, value2)`, `index_candidate_estimate(...)` | direct index access | | 13 |
| `fulltext_term_posting`, `fulltext_corpus`, `fulltext_payloads` | full-text internals | | 13 |
| `query(expr)`, `explain_query(expr)` | QueryEngine shortcuts | *realm, return_mode, select* | 14, 15 |
| `sql(text, params)`, `sql_numpy(text, params)`, `compile_sql(text, params)` | NHySQL | | 15, 16 |
| `put_numpy_batch(realm_id, u_start, columns)` | CLASSIC records from arrays | *shape_id* | 16 |
| `put_objects_numpy(layout_id, object_ids, columns)` | objects of any layout | *u_window* | 16 |
| `put_cells_numpy(realm_id, u, v, values)` | sparse cells | *type, u_window* | 16 |
| `read_numpy(realm_id, u_start, count, fields=None)` | columns + `_present` | *lane, object_fallback=False* | 16 |
| `create_math_layout(spec)`, `create_math_map_layout(name, parameters, *, u, V)` | MATH layouts | *layout_id, payload_type, side, realm_auto_limit* | 18 |
| `replace_math_spec(layout_id, spec)` | only before the first write | *payload_type* | 18 |
| `put_math(layout_id, inputs, payload)`, `put_math_batch(layout_id, rows)`, `put_math_numpy(layout_id, inputs, payloads)` | write MATH cells | *overwrite=False* | 18 |
| `get_math(layout_id, inputs)`, `get_math_cell(layout_id, inputs)` | read MATH cells | *dyn_address* | 18 |
| `describe_math(layout_id)`, `math_layouts` | MATH state | | 18 |

### 27.7 `nhydb.query`

| Name | Purpose | See |
|---|---|---|
| `QueryEngine(db)` | `execute`, `explain`, `search`, `execute_spec`, `execute_numpy`, `compile_sql`, `execute_sql`, `execute_sql_numpy`, `register_math_provider`, `math_providers`, `db` | 14, 15, 16 |
| `Q.field`, `Q.u`, `Q.v`, `Q.realm`, `Q.lane`, `Q.N`, `Q.record_N`, `Q.struct`, `Q.n`, `Q.math`, `Q.fulltext`, `Q.phrase`, `Q.all` | expression builder | 14 |
| `expr_from_spec(spec)`, `parse_sql(text)` | build expressions from data / parse SQL | 14, 15 |
| `QUERY_VERSION`, `SQL_LANGUAGE_VERSION` | version identifiers | 15 |

### 27.8 `nhydb.math`

| Name | Purpose | See |
|---|---|---|
| `MathExpression.compile(text, parameters)` / `.evaluate(**values)` | safe integer expressions | 18 |
| `MathMap.compile(name, parameters, *, u, V, side)` | mapping; `target`, `targets`, `to_dict`, `from_dict`, `fingerprint` | 18 |
| `MathMapLayoutSpec(name, mapping, realm_auto_limit)` | specification for `create_math_layout` | 18 |
| `dry_run(mapping, rows)` → `MathDryRunReport` | check a mapping without a database | 18 |
| `get_math_max_intermediate_bits()`, `set_math_max_intermediate_bits(bits)` | resource limit (process-wide, standalone objects only; databases use the configuration value frozen per MATH layout) | 18 |
| `split_global_v`, `join_global_v`, `normalize_side`, `NL`, `NR`, `REALM_SPAN`, `DEFAULT_*` | helpers and constants | 18 |

### 27.9 Deliberately not covered by recipes

| Name | Reason |
|---|---|
| `nhydb.kernel`, `nhydb.index`, `nhydb.mathlayout` | stateless building blocks of the engine (codecs, tokenizers, BM25 kernel, realm planner); they exist for conformance tests and tooling. Use the database methods above instead. |
| `NHyDB.f` | read-only low-level file view (`seek`/`tell`) used by tests |
| expression and SQL classes in `nhydb.query` (`FieldPredicate`, `BoolExpr`, `SQLCompiledQuery`, `QueryContext`, `CandidateSet`, …) | data classes returned or consumed by the API; build expressions with `Q` instead |
| `MathMapTarget`, `MathDryRunReport`, `MathLayoutState` attributes | result objects; their fields are shown where they are returned (section 18) |
| `LayoutDef.contains_u`, `LayoutDef.to_dict`, `IndexDef.to_dict` | small helpers on result objects (sections 13, 17) |
