"""Stabilisation of the Python API layer (cluster D).

Regression tests for F018, F024, F033 (T018), F040, F047, F048, F052, F055,
R003 (T010), R009 (T009), D006 and T024. Tests that could crash the
interpreter on a regression run in a subprocess.
"""
import array
import errno
import os
import subprocess
import sys
import textwrap
import threading

import numpy as np
import pytest

import nhydb

FIELDS = [
    {"v": 1, "name": "a", "type": "u32"},
    {"v": 2, "name": "s", "type": "utf8"},
    {"v": 3, "name": "f", "type": "f32"},
    {"v": 4, "name": "ok", "type": "bool"},
    {"v": 5, "name": "big", "type": "varuint"},
    {"v": 6, "name": "d", "type": "f64"},
    {"v": 7, "name": "i", "type": "varint"},
]


def _db(path):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", FIELDS)
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_column_layout(2, 1, "col", fields=[{"u": 10_000, "name": "x", "type": "varuint"},
                                                {"u": 10_001, "name": "y", "type": "f64"}])
    return db


def _cols(n):
    i = np.arange(1, n + 1)
    return {
        "a": i.astype(np.uint32),
        "s": np.array([f"s{k}" for k in i], dtype=object),
        "f": (i / 2).astype(np.float32),
        "ok": i % 2 == 0,
        "big": i.astype(np.uint64) * 10,
        "d": i / 4,
        "i": i.astype(np.int64) - 3,
    }


def _run(code, timeout=120):
    """Runs code in a fresh interpreter; returns (returncode, stdout+stderr)."""
    r = subprocess.run([sys.executable, "-c", textwrap.dedent(code)], capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout + r.stderr


# ----------------------------------------------------------------------
# F018: read_numpy(count) never aborts the process
# ----------------------------------------------------------------------

def test_read_numpy_huge_count_raises_memoryerror_in_subprocess(tmp_path):
    p = tmp_path / "r.nhydb"
    db = _db(p)
    db.put_numpy_batch(1, 1, _cols(5), commit=True)
    db.close()
    code, out = _run(f"""
        import nhydb
        db = nhydb.NHyDB({str(p)!r})
        for c in (2**40, 2**62, 10**17, 10**12, 2**64, 2**200):
            try:
                db.read_numpy(1, 1, c)
            except MemoryError:
                print("ok", c)
            else:
                print("NO ERROR", c)
        print("done")
    """)
    assert code == 0, out
    assert out.count("ok ") == 6 and "NO ERROR" not in out and "done" in out, out


def test_read_numpy_count_validation_and_exact_length(tmp_path):
    db = _db(tmp_path / "r.nhydb")
    db.put_numpy_batch(1, 1, _cols(5))
    with pytest.raises(ValueError):
        db.read_numpy(1, 1, -1)
    with pytest.raises(TypeError):
        db.read_numpy(1, 1, 2.5)
    out = db.read_numpy(1, 1, 8)
    assert all(len(v) == 8 for v in out.values())
    assert out["_present"].tolist() == [True] * 5 + [False] * 3
    assert len(db.read_numpy(1, 1, 0)["a"]) == 0
    assert db.read_numpy(1, 1, np.int64(3))["a"].tolist() == [1, 2, 3]


# ----------------------------------------------------------------------
# F024 / F033 / T018: NumPy column matrix
# ----------------------------------------------------------------------

UNSUPPORTED = [np.float16, np.longdouble, np.complex64, np.complex128, "datetime64[s]", "timedelta64[s]"]


@pytest.mark.parametrize("dtype", UNSUPPORTED, ids=str)
def test_unsupported_dtypes_raise_typeerror(tmp_path, dtype):
    db = _db(tmp_path / "t.nhydb")
    arr = np.zeros(2, dtype=dtype)
    with pytest.raises(TypeError):
        db.put_numpy_batch(1, 1, dict(_cols(2), f=arr))
    with pytest.raises(TypeError):
        db.put_cells_numpy(1, np.array([1, 2], dtype=np.uint64), 5, arr)
    with pytest.raises(TypeError):
        db.put_objects_numpy(1, np.array([1, 2], dtype=np.uint64), dict(_cols(2), f=arr))
    big_endian = np.zeros(2, dtype=np.dtype(dtype).newbyteorder(">")) if np.dtype(dtype).kind in "fc" else arr
    with pytest.raises(TypeError):
        db.put_numpy_batch(1, 1, dict(_cols(2), f=big_endian))
    assert db.stats()["storage"]["physical_dense_records"] == 0


def test_unsupported_dtype_no_recursion_in_subprocess(tmp_path):
    code, out = _run(f"""
        import sys, numpy as np, nhydb
        sys.setrecursionlimit(100000)
        db = nhydb.NHyDB.create({str(tmp_path / 's.nhydb')!r})
        db.create_realm(1, "r")
        for dt in (np.float16, np.longdouble, ">f2"):
            try:
                db.put_cells_numpy(1, 1, np.array([1], dtype=np.uint64), np.ones(1, dtype=dt))
            except TypeError:
                print("ok")
    """)
    assert code == 0 and out.count("ok") == 3, out


# (column, field) -> expected: "ok" or exception class
INT_FIELD_MATRIX = [
    (np.uint8, "ok"), (np.uint16, "ok"), (np.uint32, "ok"), (np.uint64, "ok"),
    (np.int8, "ok"), (np.int16, "ok"), (np.int32, "ok"), (np.int64, "ok"),
    (">u4", "ok"), (">i8", "ok"), ("<u2", "ok"),
    (np.bool_, TypeError), (np.float32, TypeError), (np.float64, TypeError),
]


@pytest.mark.parametrize("dtype,expect", INT_FIELD_MATRIX, ids=lambda x: str(x))
def test_dtype_matrix_u32_field(tmp_path, dtype, expect):
    db = _db(tmp_path / "m.nhydb")
    col = np.array([1, 0, 1], dtype=dtype)
    if expect == "ok":
        db.put_numpy_batch(1, 1, dict(_cols(3), a=col))
        assert db.read_numpy(1, 1, 3, ["a"])["a"].tolist() == [1, 0, 1]
    else:
        with pytest.raises(expect):
            db.put_numpy_batch(1, 1, dict(_cols(3), a=col))


@pytest.mark.parametrize("field,col,expect", [
    ("f", np.array([1.5, np.nan, np.inf], dtype=np.float32), "ok"),
    ("f", np.array([1.5, -np.inf, 2.0], dtype=">f8"), "ok"),
    ("f", np.array([1, 2, 3], dtype=np.int16), "ok"),
    ("f", np.array([1e300, 0, 0]), ValueError),
    ("d", np.array([0.25, np.nan, 3.0]), "ok"),
    ("d", np.array([True, False, True]), TypeError),
    ("ok", np.array([True, False, True]), "ok"),
    ("ok", np.array([1, 0, 1], dtype=np.uint8), "ok"),
    ("ok", np.array([1.0, 0.0, 1.0]), TypeError),
    ("s", np.array(["x", "yz", ""]), "ok"),
    ("s", np.array([1, 2, 3]), TypeError),
    ("s", ["x", "y", "z"], "ok"),
    ("big", np.array([2**64 - 1, 0, 5], dtype=np.uint64), "ok"),
    ("big", np.array([2**70, 1, 2], dtype=object), "ok"),
    ("i", np.array([-1, 0, 1], dtype=np.int8), "ok"),
], ids=lambda x: str(x)[:30])
def test_dtype_matrix_other_fields(tmp_path, field, col, expect):
    db = _db(tmp_path / "m.nhydb")
    if expect == "ok":
        db.put_numpy_batch(1, 1, dict(_cols(3), **{field: col}))
        got = db.read_numpy(1, 1, 3, [field], object_fallback=True)[field]
        exp = list(col)
        for g, e in zip(got.tolist(), exp):
            if isinstance(e, (float, np.floating)) and np.isnan(e):
                assert np.isnan(g)
            else:
                assert g == e
    else:
        with pytest.raises(expect):
            db.put_numpy_batch(1, 1, dict(_cols(3), **{field: col}))
        assert db.stats()["storage"]["physical_dense_records"] == 0


def test_nan_inf_consistent_with_single_item_api(tmp_path):
    """NaN/±Inf are valid IEEE values in f32/f64 fields on both paths; finite
    values outside the f32 range are rejected on both paths."""
    db = _db(tmp_path / "n.nhydb")
    row = [1, "x", float("nan"), True, 1, float("inf"), 0]
    db.put_dense_batch(1, 1, [row])
    db.put_numpy_batch(1, 2, dict(_cols(1), f=np.array([np.nan], dtype=np.float32), d=np.array([np.inf])))
    a, b = db.get(1, 1), db.get(1, 2)
    assert np.isnan(a["f"]) and np.isnan(b["f"]) and a["d"] == b["d"] == float("inf")
    with pytest.raises(ValueError):
        db.put_dense_batch(1, 3, [[1, "x", 1e300, True, 1, 0.0, 0]])
    with pytest.raises(ValueError):
        db.put_numpy_batch(1, 3, dict(_cols(1), f=np.array([1e300])))


def test_shape_and_container_checks(tmp_path):
    db = _db(tmp_path / "c.nhydb")
    c = _cols(4)
    with pytest.raises(ValueError):                                   # 2D is not flattened
        db.put_numpy_batch(1, 1, dict(c, a=np.ones((2, 2), dtype=np.uint32)))
    with pytest.raises(ValueError):
        db.put_cells_numpy(1, 1, np.array([1, 2]), np.ones((1, 2)), type="f64")
    with pytest.raises(TypeError):                                    # str is not a column
        db.put_numpy_batch(1, 1, dict(c, s="abcd"))
    with pytest.raises(TypeError):
        db.put_numpy_batch(1, 1, dict(c, s=b"abcd"))
    with pytest.raises(ValueError):                                   # 0-d array is not a column
        db.put_numpy_batch(1, 1, dict(c, a=np.array(5, dtype=np.uint32)))
    with pytest.raises(ValueError):                                   # empty first column, others not
        db.put_numpy_batch(1, 1, dict(c, a=np.array([], dtype=np.uint32)))
    with pytest.raises(ValueError):
        db.put_objects_numpy(1, 1, dict(c, a=np.array([], dtype=np.uint32)))
    assert db.stats()["storage"]["physical_dense_records"] == 0
    empty = {k: v[:0] for k, v in c.items()}
    assert db.put_numpy_batch(1, 1, empty)["records"] == 0           # all empty: no-op


def test_buffer_variants(tmp_path):
    db = _db(tmp_path / "b.nhydb")
    c = _cols(3)
    db.put_numpy_batch(1, 1, dict(c, a=np.arange(1, 7, dtype=">u4")[::2], ok=memoryview(np.array([True, False, True]))))
    db.put_numpy_batch(1, 4, dict(c, a=array.array("I", [7, 8, 9]), d=memoryview(np.array([0.5, 1.5, 2.5])[::-1].copy())))
    db.put_numpy_batch(1, 7, dict(c, ok=array.array("b", [1, 0, 1]), f=np.asfortranarray(np.array([1, 2, 3], dtype=np.float32))))
    out = db.read_numpy(1, 1, 9)
    assert out["a"].tolist() == [1, 3, 5, 7, 8, 9, 1, 2, 3]
    assert out["ok"].tolist()[:3] == [True, False, True] and out["ok"].tolist()[6:] == [True, False, True]
    assert out["d"].tolist()[3:6] == [2.5, 1.5, 0.5]


def test_numpy_scalars_broadcast(tmp_path):
    db = _db(tmp_path / "s.nhydb")
    for u in (7, np.uint64(7), np.int32(7), np.array(7)):
        db.put_cells_numpy(1, u, np.array([1, 2], dtype=np.uint64), np.array([1.5, 2.5]), type="f64")
    assert db.get_value(1, 7, 2) == 2.5
    db.put_cells_numpy(1, np.array([8, 9], dtype=np.uint64), np.uint64(3), np.array([1, 2]), type="varint")
    assert db.get_value(1, 9, 3) == 2
    r = db.put_objects_numpy(1, np.int64(20), _cols(2))
    assert r["objects"] == 2 and db.get(1, 21)["a"] == 2
    with pytest.raises(TypeError):
        db.put_cells_numpy(1, np.float64(7.5), np.array([1], dtype=np.uint64), np.array([1.0]), type="f64")


def test_read_numpy_dtype_is_deterministic(tmp_path):
    db = _db(tmp_path / "d.nhydb")
    expected = {"a": np.int64, "s": object, "f": np.float32, "ok": np.bool_, "big": np.uint64,
                "d": np.float64, "i": np.int64, "_present": np.bool_}
    empty = db.read_numpy(1, 1, 3)                                    # no data at all
    db.put_numpy_batch(1, 1, _cols(3))
    full = db.read_numpy(1, 1, 3)
    for out in (empty, full):
        assert {k: v.dtype for k, v in out.items()} == {k: np.dtype(t) for k, t in expected.items()}
    db.put_points(1, [(2, 5, 2**70)])                                 # varuint beyond uint64
    with pytest.raises(ValueError, match="object_fallback"):
        db.read_numpy(1, 1, 3)
    out = db.read_numpy(1, 1, 3, object_fallback=True)
    assert out["big"].dtype == object and out["big"].tolist() == [10, 2**70, 30]
    assert db.read_numpy(1, 3, 1)["big"].tolist() == [30]               # other rows unaffected


def test_read_numpy_result_is_writable(tmp_path):
    db = _db(tmp_path / "w.nhydb")
    db.put_numpy_batch(1, 1, _cols(2))
    out = db.read_numpy(1, 1, 2)
    out["a"][0] = 99
    assert out["a"].flags.writeable and out["a"][0] == 99


# ----------------------------------------------------------------------
# F040: no silent truncation of coordinates, realm and lane
# ----------------------------------------------------------------------

@pytest.mark.parametrize("bad", [2.7, np.float64(4.9), float("0.5"), __import__("decimal").Decimal("5.9"),
                                 __import__("fractions").Fraction(7, 2)], ids=repr)
def test_non_integral_coordinates_raise_typeerror(tmp_path, bad):
    db = _db(tmp_path / "f.nhydb")
    calls = [
        lambda: db.put_cell(1, bad, 10, 1, type="varint"),
        lambda: db.put_cell(1, 3, bad, 1, type="varint"),
        lambda: db.put_cell(bad, 3, 10, 1, type="varint"),
        lambda: db.put_cell(1, 3, 10, 1, type="varint", lane=bad),
        lambda: db.get(1, bad),
        lambda: db.get_value(1, 3, bad),
        lambda: db.delete_record(1, bad),
        lambda: db.put_points(1, [(bad, 10, 1, "varint")]),
        lambda: db.put_record(1, bad, {"a": 1}),
        lambda: db.read_numpy(1, bad, 1),
        lambda: list(db.select_us(1, [bad], ["a"])),
        lambda: nhydb.enc_vu(bad),
        lambda: nhydb.normalize_lane(bad),
    ]
    for k, call in enumerate(calls):
        with pytest.raises(TypeError):
            call()
    assert db.next_block_id == _db(tmp_path / "g.nhydb").next_block_id   # nothing written


def test_integral_coordinates_are_accepted_exactly(tmp_path):
    db = _db(tmp_path / "i.nhydb")
    for u in (3, np.uint64(4), np.int8(5), 6.0, np.float32(7.0), True, "8", __import__("decimal").Decimal(9)):
        db.put_cell(1, u, 10, 1, type="varint")
    assert [db.get_value(1, u, 10) for u in range(1, 10)] == [1, None, 1, 1, 1, 1, 1, 1, 1]
    assert nhydb.normalize_lane(1.0) == 1 and nhydb.enc_vu(2.0) == nhydb.enc_vu(2)
    assert db.get_value(1, 10**20 * 1.0, 10) is None                  # exact integral float is fine


# ----------------------------------------------------------------------
# F047: delete methods
# ----------------------------------------------------------------------

def test_delete_methods_return_values_and_no_phantom_writes(tmp_path):
    db = _db(tmp_path / "x.nhydb")
    db.put_cell(1, 14, 744, 5, type="varint")
    db.put_numpy_batch(1, 1, _cols(2))
    before = db.next_block_id
    assert db.delete_cell(1, 999, 1) is False                         # never existed: nothing written
    assert db.next_block_id == before
    assert db.delete_cell(1, 14, 744) is True
    assert db.get_value(1, 14, 744) is None
    assert db.delete_cell(1, 14, 744) is False                        # already deleted
    after = db.next_block_id
    assert after == before + 1
    assert db.delete_cell(1, 1, 1) is True                            # dense BASE field
    assert db.get_value(1, 1, 1) is None
    # number of live cells deleted (0.28.2/delete-api-f1): all 7 BASE fields
    assert db.delete_record(1, 2) == len(FIELDS)
    assert db.delete_record(1, 2) == 0                                 # already deleted
    before = db.next_block_id
    assert db.delete_record(1, 999) == 0                               # never existed: nothing written
    assert db.next_block_id == before
    with pytest.raises(nhydb.SchemaError):
        db.delete_cell(99, 1, 1)
    with pytest.raises(nhydb.RealmLimitError):
        db.delete_cell(1, 1, 0)
    db.commit()
    assert db.verify(deep=True)["ok"]


def test_delete_cell_commit_true_on_missing_cell_commits_pending(tmp_path):
    p = tmp_path / "y.nhydb"
    db = _db(p)
    db.put_cell(1, 5, 5, 1, type="varint")
    assert db.delete_cell(1, 6, 6, commit=True) is False
    db.close(commit=False)
    db = nhydb.NHyDB(str(p))
    assert db.get_value(1, 5, 5) == 1


# ----------------------------------------------------------------------
# F048: OSError subclasses with errno and filename
# ----------------------------------------------------------------------

def test_missing_file_has_errno_and_filename(tmp_path):
    p = tmp_path / "missing.nhydb"
    with pytest.raises(FileNotFoundError) as ei:
        nhydb.NHyDB(str(p))
    assert ei.value.errno == errno.ENOENT and ei.value.filename and "missing.nhydb" in ei.value.filename


def test_create_existing_has_errno(tmp_path):
    p = tmp_path / "e.nhydb"
    nhydb.NHyDB.create(str(p)).close()
    with pytest.raises(FileExistsError) as ei:
        nhydb.NHyDB.create(str(p))
    assert ei.value.errno == errno.EEXIST


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX errno semantics")
def test_directory_raises_isadirectoryerror(tmp_path):
    d = tmp_path / "dir.nhydb"
    d.mkdir()
    with pytest.raises(IsADirectoryError) as ei:
        nhydb.NHyDB(str(d))
    assert ei.value.errno == errno.EISDIR


@pytest.mark.skipif(sys.platform == "win32" or (hasattr(os, "geteuid") and os.geteuid() == 0), reason="needs POSIX permissions")
def test_permission_denied_raises_permissionerror(tmp_path):
    p = tmp_path / "p.nhydb"
    nhydb.NHyDB.create(str(p)).close()
    os.chmod(p, 0)
    try:
        with pytest.raises(PermissionError) as ei:
            nhydb.NHyDB(str(p))
        assert ei.value.errno == errno.EACCES
    finally:
        os.chmod(p, 0o600)


# ----------------------------------------------------------------------
# F052 / F055 / D006 / T024
# ----------------------------------------------------------------------

def test_star_import_does_not_shadow_open():
    ns = {}
    exec("from nhydb import *", ns)
    assert "open" not in ns and ns["NHyDB"] is nhydb.NHyDB and ns["create"] is nhydb.create
    assert all(hasattr(nhydb, n) for n in nhydb.__all__)
    assert callable(nhydb.open)


def test_get_value_accepts_column_names(tmp_path):
    db = _db(tmp_path / "n.nhydb")
    db.put_numpy_batch(1, 1, _cols(2))
    db.define_extension(1, 50, "utf8", name="note")
    db.put(1, 2, [2, "s2", 1.0, True, 20, 0.5, -1], extensions={"note": "hi"})
    db.put_objects_numpy(2, np.array([1], dtype=np.uint64), {"x": np.array([77], dtype=np.uint64)})
    assert db.get_value(1, 1, "s") == db.get_cell(1, 1, "s")["value"] == "s1"
    assert db.get_value(1, 1, "v2") == "s1"
    assert db.get_value(1, 2, "note") == "hi"
    cell = db.get_cell(1, 10_000, "x")                                # COLUMN field name, as get_cell
    assert cell["value"] == 77 and db.get_value(1, 10_000, "x") == 77
    assert db.get_value(1, 1, "nope") is None


def test_exception_hierarchy():
    for cls in (nhydb.CorruptionError, nhydb.SchemaError, nhydb.MathError, nhydb.MathSyntaxError,
                nhydb.LayoutError, nhydb.RealmBudgetExceeded, nhydb.QueryError, nhydb.SQLSyntaxError,
                nhydb.MathLayoutNotFound, nhydb.DatabaseLockedError):
        assert issubclass(cls, nhydb.NHyDBError), cls
    assert issubclass(nhydb.IndexConfigError, ValueError)


def test_build_fingerprint_present():
    h = nhydb._native.BUILD_SOURCE_HASH
    assert h == "unknown" or (h.startswith("crc32:") and len(h) == 14)


def test_build_fingerprint_ignores_the_sdist_license_rewrite(tmp_path):
    """P-08: ``maturin sdist`` rewrites ``license-file.workspace = true`` to
    ``license-file = "LICENSE.md"`` in the crate manifests; the fingerprint
    stays the same, any other manifest change still changes it."""
    import shutil
    import conftest
    root = conftest.ROOT
    if not (root / "crates/nhydb-core/src").is_dir():
        pytest.skip("no source checkout")
    for rel, src in conftest._source_files(root):
        dst = tmp_path / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, dst)
    base = conftest.source_hash(tmp_path)
    assert base == conftest.source_hash(root)
    for crate in ("nhydb-core", "nhydb-py"):
        m = tmp_path / "crates" / crate / "Cargo.toml"
        t = m.read_bytes()
        assert b"license-file.workspace = true" in t
        m.write_bytes(t.replace(b"license-file.workspace = true", b'license-file = "LICENSE.md"'))
    assert conftest.source_hash(tmp_path) == base
    m = tmp_path / "crates/nhydb-core/Cargo.toml"
    m.write_bytes(m.read_bytes() + b"\n# changed\n")
    assert conftest.source_hash(tmp_path) != base


# ----------------------------------------------------------------------
# R009 / T009: converter depth limits (subprocess)
# ----------------------------------------------------------------------

def test_value_nesting_limit_in_subprocess(tmp_path):
    code, out = _run(f"""
        import nhydb
        db = nhydb.NHyDB.create({str(tmp_path / 'deep.nhydb')!r})
        db.create_realm(1, "r")
        for depth in (300, 100_000):
            x = []
            for _ in range(depth):
                x = [x]
            try:
                db.put_cell(1, 1, 1, x)
            except ValueError as e:
                print("ok", depth, "nesting" in str(e))
        y = {{}}
        y["self"] = y
        try:
            db.put_cell(1, 1, 1, y)
        except ValueError:
            print("ok cyclic")
        x = []
        for _ in range(200):
            x = [x]
        try:
            db.put_cell(1, 1, 1, x)
        except ValueError as e:
            print("unexpected", e)
        except Exception:
            print("ok shallow")          # below the limit: normal (type) handling
    """)
    assert code == 0, out
    assert "ok 300 True" in out and "ok 100000 True" in out and "ok cyclic" in out and "ok shallow" in out, out


# ----------------------------------------------------------------------
# R003 / T010: concurrent use of one object from several threads
# ----------------------------------------------------------------------

def test_threads_on_one_object_defined_error_no_corruption(tmp_path):
    p = tmp_path / "t.nhydb"
    db = _db(p)
    db.put_numpy_batch(1, 1, _cols(100), commit=True)
    stop = threading.Event()
    errors, unexpected, written = [], [], []

    def guard(fn):
        def run():
            while not stop.is_set():
                try:
                    fn()
                except RuntimeError as e:
                    errors.append(str(e))
                except BaseException as e:  # pragma: no cover - would be a regression
                    # BaseException: a PyO3 PanicException does not derive from
                    # Exception (R9D-01, D H-3) and must show up here
                    unexpected.append(repr(e))
        return run

    # Each batch covers [u, u + BATCH - 1]; the step equals the batch size so
    # successful batches never overlap (an overlapping later batch would
    # legitimately overwrite the upper half of the earlier one).
    BATCH = 2000
    counter = iter(range(1000, 10**9, BATCH))

    def writer():
        u = next(counter)
        db.put_numpy_batch(1, u, _cols(BATCH))
        written.append(u)

    side = iter(range(5_000_000, 10**9))

    def batch_writer():
        # R9D-01/R9D-02: the batch borrows the handle at its calls and flush
        b = db.batch()
        b.put_cell(1, next(side), 2, 7, type="varint")
        b.flush()

    def iterate():
        it = db.range(1, 1, 60)
        next(it)
        for _ in it:
            pass

    from nhydb.query import Q, QueryEngine
    engine = QueryEngine(db)
    threads = [threading.Thread(target=guard(f)) for f in (
        writer,
        lambda: db.commit(),
        lambda: db.get(1, 5),
        lambda: db.read_numpy(1, 1, 50),
        batch_writer,
        iterate,
        lambda: engine.execute(Q.all(), realm=1, return_mode="count"),
    )]
    for t in threads:
        t.start()
    threading.Event().wait(1.5)
    stop.set()
    for t in threads:
        t.join()
    assert not unexpected, unexpected[:5]
    assert all("borrow" in e.lower() for e in errors), set(errors)
    db.commit()
    assert db.verify(deep=True)["ok"]
    db.close()
    db = nhydb.NHyDB(str(p))
    assert db.verify(deep=True)["ok"]
    for u in written:
        assert db.get(1, u)["a"] == 1
        assert db.get(1, u + BATCH // 2)["a"] == BATCH // 2 + 1
        assert db.get(1, u + BATCH - 1)["a"] == BATCH
    db.close()


def test_threads_with_lock_never_fail(tmp_path):
    db = _db(tmp_path / "l.nhydb")
    lock = threading.Lock()
    failures = []

    def work(k):
        try:
            for j in range(20):
                with lock:
                    db.put_cell(1, 1 + k, 1 + j, j, type="varint")
                    db.get_value(1, 1 + k, 1 + j)
        except Exception as e:  # pragma: no cover
            failures.append(e)

    ts = [threading.Thread(target=work, args=(k,)) for k in range(4)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    assert not failures
    assert [db.get_value(1, 1 + k, 20) for k in range(4)] == [19] * 4
