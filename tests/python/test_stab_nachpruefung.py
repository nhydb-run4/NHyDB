"""Stabilization phase 1, follow-up review (findings N1 … N5).

N1  read-only opens race with concurrent schema commits
N2  close() leaves both superblock slots current
N3  unique backup names within the same second
N4  put(get()) round trip with extension / v<n> keys
N5  nesting limit of Q expressions: clear message, explain within the limit
"""
import os
import struct
import subprocess
import sys
import textwrap
import time
import warnings

import pytest

import nhydb
from nhydb.query import Q

SB = 4096


def _classic(path):
    db = nhydb.NHyDB.create(path)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.commit()
    return db


# --------------------------------------------------------------------------
# N1
# --------------------------------------------------------------------------

WRITER = textwrap.dedent("""
    import sys, time, nhydb
    db = nhydb.NHyDB(sys.argv[1])
    print("ready", flush=True)
    t = time.time(); i = 0
    while time.time() - t < float(sys.argv[2]):
        i += 1
        db.put_dense_batch(1, i * 10, [[i]] * 10, shape_id=1, commit=True)
        if i % 2 == 0:
            db.create_realm(1000 + i, "x")
            db.commit()
    db.close()
    print("schema_commits", i // 2, flush=True)
""")


def test_n1_readonly_open_during_schema_commits(tmp_path):
    """A writer publishes a schema generation every second commit; read-only
    opens in a loop must never fail with a false CorruptionError (the plan
    only keeps two generations) and must see monotonically growing data."""
    p = str(tmp_path / "s.nhydb")
    _classic(p).close()
    # commits are slower on Windows: longer writer run, lower bar
    seconds, min_schema_commits = ("10.0", 10) if os.name == "nt" else ("3.0", 20)
    w = subprocess.Popen([sys.executable, "-c", WRITER, p, seconds], stdout=subprocess.PIPE, text=True)
    try:
        assert w.stdout.readline().strip() == "ready"
        failures, opens, last = [], 0, 0
        deadline = time.time() + 30
        while w.poll() is None and time.time() < deadline:
            try:
                with warnings.catch_warnings():
                    warnings.simplefilter("ignore")
                    r = nhydb.NHyDB(p, readonly=True)
                n = r.sql("SELECT COUNT(*) FROM c")["rows"][0]["count_all"]
                r.close()
            except Exception as e:  # noqa: BLE001 - every failure counts
                failures.append(f"{type(e).__name__}: {e}")
                continue
            opens += 1
            assert n >= last, "read-only snapshots must be monotonic"
            last = n
        out = w.communicate(timeout=60)[0]
    finally:
        if w.poll() is None:
            w.kill()
    assert w.returncode == 0, out
    schema_commits = int(out.split()[-1])
    assert schema_commits >= min_schema_commits, out
    assert opens >= 5
    assert failures == [], failures[:5]
    r = nhydb.NHyDB(p, readonly=True)
    assert r.verify()["ok"]
    r.close()


def test_n1_real_plan_mismatch_stays_corruption(tmp_path):
    """Without a concurrent writer, a layout plan newer than the DB file
    (old DB file restored without its plan) is still reported."""
    p = str(tmp_path / "s.nhydb")
    _classic(p).close()
    old = open(p, "rb").read()
    db = nhydb.NHyDB(p)
    for r in (7, 8):
        db.create_realm(r, "x")
        db.commit()
    db.close()
    open(p, "wb").write(old)
    for kw in ({"readonly": True}, {}):
        t = time.time()
        with pytest.raises(nhydb.CorruptionError, match="no layout plan"):
            nhydb.NHyDB(p, **kw)
        assert time.time() - t < 5


# --------------------------------------------------------------------------
# N2
# --------------------------------------------------------------------------

def _gens(raw):
    return [struct.unpack_from("<Q", raw, s * SB + 16)[0] for s in range(2)]


@pytest.mark.parametrize("variant", ["put+close", "put(commit=True)+close", "context manager"])
def test_n2_close_keeps_both_superblocks_current(tmp_path, variant):
    p = str(tmp_path / "a.nhydb")
    _classic(p).close()
    if variant == "context manager":
        with nhydb.NHyDB(p) as db:
            db.put(1, 5, [5])
    else:
        db = nhydb.NHyDB(p)
        db.put(1, 5, [5], commit=variant.startswith("put(commit"))
        db.close()
    raw = bytearray(open(p, "rb").read())
    g = _gens(raw)
    assert abs(g[0] - g[1]) == 1
    # unchanged open + close writes nothing (F012)
    nhydb.NHyDB(p).close()
    assert open(p, "rb").read() == bytes(raw)
    # a bit error in the newest slot falls back transparently
    act = 0 if g[0] > g[1] else 1
    raw[act * SB + 100] ^= 0xFF
    open(p, "wb").write(raw)
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        db = nhydb.NHyDB(p)
    assert db.get(1, 5)["a"] == 5
    db.close()


# --------------------------------------------------------------------------
# N3
# --------------------------------------------------------------------------

def test_n3_backups_within_the_same_second_get_unique_names(tmp_path):
    p = str(tmp_path / "a.nhydb")
    db = _classic(p)
    db.put(1, 1, [1], commit=True)
    db.close()
    backups = []
    for i in range(3):
        r = nhydb.NHyDB.compact_file(p, make_backup=True)
        backups.append(r["backup"])
        db = nhydb.NHyDB(p)
        db.put(1, 2 + i, [10 + i], commit=True)
        db.close()
    assert len(set(backups)) == 3, backups
    for b in backups:
        assert os.path.isfile(b) and os.path.isfile(b + ".layout.enc")
    # the newest backup has the most data; restoring an older one works
    first = backups[0]
    res = nhydb.NHyDB.restore_backup(first, p)
    assert res["ok"]
    db = nhydb.NHyDB(p)
    assert db.get(1, 1)["a"] == 1 and db.get(1, 2) is None
    db.close()


# --------------------------------------------------------------------------
# N4
# --------------------------------------------------------------------------

def _ext_db(p):
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.define_extension(1, 20, "utf8", name="note")
    db.define_extension(1, 21, "varint")
    db.put(1, 1, {"a": 1, "b": "x"})
    db.put_extensions(1, 1, {"note": "hello", 21: 4})
    db.put_cell(1, 1, 12, 7, type="varint")
    db.commit()
    return db


def test_n4_put_of_get_record_round_trip(tmp_path):
    db = _ext_db(str(tmp_path / "a.nhydb"))
    rec = db.get(1, 1)
    assert rec["note"] == "hello" and rec["v12"] == 7 and rec["v21"] == 4
    db.put(1, 2, rec)
    db.put_dense_batch(1, 3, [rec], shape_id=1)
    for u in (2, 3):
        got = db.get(1, u)
        assert (got["a"], got["b"]) == (1, "x")
        # only the shape fields are written; extensions via extensions=
        assert "note" not in got and "v12" not in got and "v21" not in got
    db.put(1, 4, rec, extensions={"note": "copied"})
    assert db.get(1, 4)["note"] == "copied"
    db.close()


@pytest.mark.parametrize("key", ["nte", "c", "v", "v01", "vx", "note2", "B"])
def test_n4_unknown_keys_stay_schema_errors(tmp_path, key):
    db = _ext_db(str(tmp_path / "a.nhydb"))
    with pytest.raises(nhydb.SchemaError, match="unknown field"):
        db.put(1, 2, {"a": 1, "b": "x", key: 1})
    with pytest.raises(nhydb.SchemaError, match="unknown field"):
        db.put_dense_batch(1, 3, [{"a": 1, "b": "x", key: 1}], shape_id=1)
    db.close()


@pytest.mark.parametrize("extra", [{"note": "x"}, {"v7": 99}, {"v20": "y"}, {"v12": 7}])
def test_g2_3_extension_keys_in_plain_dict_are_schema_errors(tmp_path, extra):
    """G2-3: extension names and v<n> keys are only ignored in a get() record;
    in a user dict they are a SchemaError with a hint to extensions=."""
    db = _ext_db(str(tmp_path / "a.nhydb"))
    row = {"a": 1, "b": "x", **extra}
    with pytest.raises(nhydb.SchemaError, match="extensions="):
        db.put(1, 5, row)
    with pytest.raises(nhydb.SchemaError, match="unknown field"):
        db.put_dense_batch(1, 5, [row], shape_id=1)
    assert db.get(1, 5) is None
    # the same keys in a get() record are ignored (round trip)
    db.put(1, 6, {**db.get(1, 1), **extra})
    assert db.get(1, 6)["a"] == 1 and "note" not in db.get(1, 6)
    db.close()


def test_n4_extension_names_of_other_realms_are_unknown(tmp_path):
    db = _ext_db(str(tmp_path / "a.nhydb"))
    db.create_realm(2, "r2")
    db.create_shape(2, 2, "s2", [{"v": 1, "name": "a", "type": "varint"}])
    db.create_classic_layout(2, 2, "c2", shape_id=2)
    with pytest.raises(nhydb.SchemaError, match="unknown field 'note'"):
        db.put(2, 1, {"a": 1, "note": "x"})
    db.close()


# --------------------------------------------------------------------------
# N5
# --------------------------------------------------------------------------

N5_CODE = textwrap.dedent("""
    import os, sys, nhydb
    from nhydb.query import Q
    db = nhydb.NHyDB.create(os.path.join(sys.argv[1], "q.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "i64"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[1], [2], [3]], shape_id=1)
    db.commit()
    def alt(n):
        q = Q.field("x") == 1
        for i in range(n):
            q = (q & (Q.field("x") > -i)) | (Q.field("x") == 2)
        return q
    r = db.query(alt(127), realm=1, return_mode="count", explain=True)
    print("explain_ok", r["count"])
    try:
        db.query(alt(129), realm=1, return_mode="count")
    except nhydb.QueryPlanError as e:
        print("limit", str(e))
""")


def test_n5_alternating_depth_limit_and_explain(tmp_path):
    proc = subprocess.run([sys.executable, "-c", N5_CODE, str(tmp_path)], capture_output=True, text=True, timeout=300)
    assert proc.returncode == 0, proc.stderr[-2000:]
    lines = proc.stdout.strip().splitlines()
    assert lines[0] == "explain_ok 2"
    assert lines[1].startswith("limit query expression nested deeper than 256 levels")
    assert "flat AND/OR" in lines[1]
