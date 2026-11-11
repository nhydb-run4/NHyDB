"""Review 2026-11-09 (reviewers B and D, counter checks V2): crash, rollback
and concurrency checks of the review, reduced to a permanent test.

1. Kill probes: a child process runs one engine phase (large batch flush
   with deletes, delete_area over whole records and a v part, in-place
   compact_file with backup, merge rebuild of the automatic index
   maintenance + build_indexes, directory flushes of single writes) and is
   killed (SIGKILL / TerminateProcess) after a seeded random time inside
   that phase. After every kill: open, verify(deep=True) ok, the content
   (checksum over all live cells) is exactly one committed state (before or
   after), index answers = scan. A killed compaction leaves at most
   ``<db>.compacting``: readonly health() reports it, opening a writer
   removes it (E6).
2. Rollback sequences with delete_area, delete_record, put with shape
   change, batches and automatic maintenance: every read path and index
   query equals the state before the transaction; commit + reopen equal.
3. Failed WriteBatch (E2) with earlier uncommitted writes and an active
   overlay: size limit, realm removed by rollback() in the block, exception
   in the block. State before the batch (uncommitted writes included), end
   of file unchanged, the batch is consumed afterwards.
4. Limiter: cancel_job() ends delete_area, batch flush with deletes,
   compact_file and build_indexes atomically (E5), in a sub process.
5. Threads on one handle: only RuntimeError, never PanicException, no hang,
   no data change by a rejected call (R9D-01/R9D-02); two readers of a
   stale index in the yellow zone do not block each other (R9D-03).
6. R9B-02: the cell cursor after an ineffective cache relief is not
   slowed down by a factor ~140 any more.

Scale: ``NHYDB_REVIEW_KILLS`` (default 6 kills per scenario).
"""
import hashlib
import json
import os
import random
import shutil
import subprocess
import sys
import textwrap
import time

import numpy as np
import pytest

import nhydb

KILLS = int(os.environ.get("NHYDB_REVIEW_KILLS", "6"))
R = 1
N = 20_000
TRIAD = ("k.nhydb", "k.nhydb.layout.enc", "k.nhydb.layout.key")
SHAPE_A = [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}, {"v": 3, "name": "y", "type": "utf8"}]
SHAPE_B = [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "x", "type": "varint"}]
V_EXT = 30
# automatic maintenance at every commit, directory flushes very often (also
# inside a transaction)
CFG_AGGRESSIVE = """
[index]
maintenance = "auto"
rebuild_ratio = 0.000001
rebuild_min_changes = 1
crisis_ratio = 0.000001
bulk_commit_ratio = 1.0

[directory]
flush_blocks = 4
flush_bytes = 32768
"""
CFG_PLAIN = "[directory]\nflush_blocks = 64\n"


# ----------------------------------------------------------------------
# helpers
# ----------------------------------------------------------------------

def _schema(db):
    db.create_realm(R, "r")
    db.create_shape(1, R, "A", SHAPE_A)
    db.create_shape(2, R, "B", SHAPE_B)
    db.create_classic_layout(1, R, "main", shape_id=1, base_reserve_to=20)
    db.define_extension(R, V_EXT, "varuint", name="ext")
    db.create_index(1, R, "ix", "x")
    db.create_index(2, R, "iy", "y")
    db.commit()


def _rows(lo, hi, shape_id=1, salt=0):
    ids = np.arange(lo, hi + 1, dtype=np.uint64)
    cols = {"id": ids, "x": ((ids + salt) % 7).astype(np.int64) - 3}
    if shape_id == 1:
        cols["y"] = np.array(["abc"[(int(i) + salt) % 3] for i in ids], dtype=object)
    return cols


def _fill(db, n=N):
    db.put_numpy_batch(R, 1, _rows(1, n), shape_id=1)
    db.put_points(R, [(u, 2, (u % 5) - 2, "varint") for u in range(1, n + 1, 3)])
    db.put_points(R, [(u, V_EXT, u % 7, "varuint") for u in range(1, n + 1, 5)])
    db.commit()
    db.build_indexes(commit=True)


def _digest(db):
    """(number, sha256) over all live cells of realm R (cell cursor)."""
    h = hashlib.sha256()
    rows = sorted((c["u"], c["v"], repr(c["value"])) for c in db.iter_materialized_cells(R))
    for r in rows:
        h.update(repr(r).encode())
    return f"{len(rows)}:{h.hexdigest()}"


def _index_problems(db):
    """Index answers (ix on x, iy on y) and COUNT(*) against the cell scan."""
    recs, xs, ys = set(), {}, {}
    for c in db.iter_materialized_cells(R):
        recs.add(c["u"])
        if c["v"] == 2:
            xs[c["u"]] = c["value"]
        elif c["v"] == 3:
            ys[c["u"]] = c["value"]
    probs = []
    for name, vals, keys in (("ix", xs, (-3, 0, 2, 3)), ("iy", ys, ("a", "b", "c"))):
        for k in keys:
            want = sorted(u for u in recs if vals.get(u, "-") == k)
            got = sorted(db.find_u(R, {name: k}))
            if got != want:
                probs.append((name, k, len(got), len(want)))
    cnt = db.sql("SELECT COUNT(*) FROM main")["rows"][0]["count_all"]
    if cnt != len(recs):
        probs.append(("count", cnt, len(recs)))
    return probs


def _run_py(code, cwd, timeout=150):
    r = subprocess.run([sys.executable, "-c", textwrap.dedent(code)], cwd=cwd, capture_output=True, text=True, timeout=timeout)
    assert r.returncode == 0, r.stdout[-2000:] + r.stderr[-4000:]
    return json.loads(r.stdout.strip().splitlines()[-1])


# ----------------------------------------------------------------------
# 1. kill probes
# ----------------------------------------------------------------------

# The child opens the database and prints "GO" right before the engine
# phase, "DONE <seconds>" after it. The work is deterministic (fixed seed);
# an undisturbed calibration run ("cal") prints "STATE <digest>" after each
# of its commits: these and the base state are the committed states a
# killed run may leave (the digest time is not counted in the phase).
CHILD = textwrap.dedent(r'''
    import hashlib, os, random, sys, time
    import nhydb
    scen, d, N, cal = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4] == "cal"
    p, cfg = os.path.join(d, "k.nhydb"), os.path.join(d, "c.toml")
    R, V_EXT = 1, 30
    rng = random.Random(4711)
    spent = [0.0]
    def commit(db):
        db.commit()
        if cal:
            t = time.perf_counter()
            rows = sorted((c["u"], c["v"], repr(c["value"])) for c in db.iter_materialized_cells(R))
            h = hashlib.sha256()
            for r in rows:
                h.update(repr(r).encode())
            print("STATE %d:%s" % (len(rows), h.hexdigest()), flush=True)
            spent[0] += time.perf_counter() - t
    db = None if scen == "compact" else nhydb.NHyDB(p, config=cfg)
    if scen == "batch":
        b = db.batch(u_window=5000)
        for _ in range(6000):
            k, u = rng.random(), rng.randint(1, N)
            if k < 0.45:
                b.put_cell(R, u, 2, rng.randint(-3, 3), type="varint")
            elif k < 0.65:
                b.delete_cell(R, u, rng.choice((2, 3, V_EXT)))
            elif k < 0.93:
                b.delete_record(1, u)
            else:
                b.delete_area(R, u, u + 50, 1, 40)
    elif scen == "dirflush":
        ops = [(rng.random(), rng.randint(1, N), rng.randint(-3, 3)) for _ in range(4000)]
    elif scen == "index":
        us = [rng.sample(range(1, N + 1), 6000) for _ in range(3)]
    print("GO", flush=True)
    t0 = time.perf_counter()
    if scen == "batch":
        b.flush()
        commit(db)
    elif scen == "area":
        db.delete_area(R, 1, N // 2, 1, 100)          # whole records (v 1..100 covers every cell)
        db.delete_area(R, N // 2 + 1, N, 2, 2)        # one v of the other records
        commit(db)
    elif scen == "compact":
        nhydb.NHyDB.compact_file(p, make_backup=True, config=cfg)
    elif scen == "index":
        for part in us:
            db.put_points(R, [(u, 2, rng.randint(-3, 3), "varint") for u in part])
            commit(db)                                # automatic maintenance (merge rebuild) inside
        db.create_index(3, R, "iext", "ext")
        db.commit()
        db.build_indexes(commit=True)
    elif scen == "dirflush":
        for i, (k, u, x) in enumerate(ops):           # every single write is a block, flush every 4
            if k < 0.6:
                db.put_cell(R, u, 2, x)
            elif k < 0.8:
                db.delete_cell(R, u, 3)
            elif k < 0.9:
                db.delete_record(1, u)
            else:
                db.put_dense_batch(R, u, [[u, x]], shape_id=2)
            if i % 2000 == 1999:
                commit(db)
    print("DONE %.6f" % (time.perf_counter() - t0 - spent[0]), flush=True)
''')

SCENARIOS = {
    "batch": CFG_PLAIN,
    "area": CFG_PLAIN,
    "compact": CFG_PLAIN,
    "index": CFG_AGGRESSIVE,
    "dirflush": CFG_AGGRESSIVE,
}


def _copy_db(src, dst):
    if dst.exists():
        shutil.rmtree(dst)
    dst.mkdir(parents=True)
    for f in TRIAD + ("c.toml",):
        shutil.copy(src / f, dst / f)


def _child(scen, d, kill_after=None):
    """Runs the child; kill_after = seconds after "GO" (None: undisturbed
    calibration run). Returns (killed inside the phase, phase seconds or
    None, committed states printed by a calibration run)."""
    mode = "cal" if kill_after is None else "kill"
    proc = subprocess.Popen([sys.executable, "-c", CHILD, scen, str(d), str(N), mode],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    first = proc.stdout.readline()
    assert first.strip() == "GO", (first, proc.stderr.read())
    if kill_after is not None:
        time.sleep(kill_after)
        proc.kill()
    out, err = proc.communicate(timeout=120)
    done = [ln for ln in out.splitlines() if ln.startswith("DONE")]
    if kill_after is None:
        assert proc.returncode == 0 and done, err[-3000:]
    states = [ln.split()[1] for ln in out.splitlines() if ln.startswith("STATE")]
    return not done, (float(done[0].split()[1]) if done else None), states


@pytest.fixture(scope="module")
def kill_template(tmp_path_factory):
    """Per scenario: database with the base state, the committed states
    (base first) of an undisturbed run of the phase and its duration."""
    root = tmp_path_factory.mktemp("kill")
    base = root / "base"
    base.mkdir()
    (base / "c.toml").write_text(CFG_PLAIN)
    db = nhydb.NHyDB.create(str(base / "k.nhydb"), config=str(base / "c.toml"))
    _schema(db)
    _fill(db)
    before = _digest(db)
    assert _index_problems(db) == []
    db.close()
    out = {}
    for scen, cfg in SCENARIOS.items():
        t = root / f"{scen}_tpl"
        _copy_db(base, t)
        (t / "c.toml").write_text(cfg)
        cal = root / f"{scen}_cal"
        _copy_db(t, cal)
        _, dur, states = _child(scen, cal)
        with nhydb.NHyDB(str(cal / "k.nhydb"), config=str(cal / "c.toml")) as db:
            assert db.verify(deep=True)["ok"]
            assert _index_problems(db) == []
            assert _digest(db) == (states[-1] if states else before)
        out[scen] = {"tpl": t, "states": [before] + states, "dur": dur}
    return root, out


@pytest.mark.parametrize("scen", list(SCENARIOS))
def test_kill_inside_engine_phase(kill_template, scen):
    root, tpls = kill_template
    tpl = tpls[scen]
    allowed = tpl["states"]
    if scen != "compact":
        assert len(set(allowed)) == len(allowed) > 1, "every commit of the phase changes the content"
    rng = random.Random(f"review-kill-{scen}")
    hits, states, leftovers = 0, [], 0
    for k in range(KILLS):
        d = root / f"{scen}_{k}"
        _copy_db(tpl["tpl"], d)
        wait = rng.uniform(0.02, 0.8) * tpl["dur"]
        killed, _, _ = _child(scen, d, kill_after=wait)
        hits += killed
        p, cfg = str(d / "k.nhydb"), str(d / "c.toml")
        staging = d / "k.nhydb.compacting"
        if scen == "compact":
            # E6: readonly reports the leftover, does not remove it
            with nhydb.NHyDB(p, readonly=True) as ro:
                names = [c["name"] for c in ro.health()["checks"]]
                assert ("compaction_leftover" in names) == staging.exists(), (names, staging.exists())
                if staging.exists():
                    leftovers += 1
                    with pytest.raises(nhydb.NHyDBError):
                        ro.cleanup_compaction()
                    assert staging.exists()
        with nhydb.NHyDB(p, config=cfg) as db:
            assert not staging.exists(), "E6: a writer with its own lock removes <db>.compacting"
            assert "compaction_leftover" not in [c["name"] for c in db.health()["checks"]]
            v = db.verify(deep=True)
            assert v["ok"], (scen, k, wait, v)
            dig = _digest(db)
            assert dig in allowed, ("not a committed state", scen, k, wait, killed)
            states.append(allowed.index(dig))
            assert _index_problems(db) == [], (scen, k, wait)
        shutil.rmtree(d)
    print(f"kill {scen}: phase {tpl['dur']:.3f} s, killed inside {hits}/{KILLS}, states {states}, leftovers {leftovers}")
    # the kills land inside the phase (timing calibrated by the undisturbed run)
    assert hits >= max(1, KILLS // 2), (scen, hits, states, tpl["dur"])
    if scen == "compact":
        assert leftovers >= 1, "no kill left <db>.compacting: E6 only checked by test_compaction_leftover_e6"


def test_compaction_leftover_e6(tmp_path):
    """E6 deterministic: a staging file of a killed compaction (simulated)
    is reported by health() of a readonly handle, removed by
    cleanup_compaction() only with an own lock, and removed on writer open."""
    p = tmp_path / "e.nhydb"
    db = nhydb.NHyDB.create(str(p))
    _schema(db)
    db.put_numpy_batch(R, 1, _rows(1, 500), shape_id=1, commit=True)
    assert "compaction_leftover" not in [c["name"] for c in db.health()["checks"]]
    staging = tmp_path / "e.nhydb.compacting"
    staging.write_bytes(b"\0" * 5000)
    chk = [c for c in db.health()["checks"] if c["name"] == "compaction_leftover"]
    assert len(chk) == 1 and chk[0]["details"]["bytes"] == 5000 and chk[0]["details"]["removable"] is True
    assert db.cleanup_compaction() == 5000 and not staging.exists()
    assert db.cleanup_compaction() == 0
    staging.write_bytes(b"\0" * 77)
    db.close()
    with nhydb.NHyDB(str(p), readonly=True) as ro:
        chk = [c for c in ro.health()["checks"] if c["name"] == "compaction_leftover"]
        assert len(chk) == 1 and chk[0]["details"]["removable"] is False
        with pytest.raises(nhydb.NHyDBError):
            ro.cleanup_compaction()
    assert staging.exists()
    with nhydb.NHyDB(str(p)) as db:
        assert not staging.exists()
        assert db.verify(deep=True)["ok"]
        assert db.get(R, 500)["id"] == 500


# ----------------------------------------------------------------------
# 2. rollback sequences
# ----------------------------------------------------------------------

SMALL = 300


def _view(db):
    """Every read path of the small test database (plus index answers)."""
    us = range(1, SMALL + 30)
    recs = {u: db.get(R, u) for u in us}
    cells = {(u, v): db.get_cell(R, u, v) for u in range(1, SMALL + 30, 7) for v in (1, 2, 3, 15, V_EXT, 40)}
    sql = sorted(repr(r) for r in db.sql("SELECT * FROM main")["rows"])
    num = db.read_numpy(R, 1, SMALL + 30, masks=True)
    num = {k: (v.tolist() if hasattr(v, "tolist") else {f: m.tolist() for f, m in v.items()}) for k, v in num.items()}
    idx = {(name, k): sorted(db.find_u(R, {name: k})) for name, keys in (("ix", (-3, 0, 2, 3)), ("iy", ("a", "b", "c"))) for k in keys}
    sqlx = {k: sorted(repr(r) for r in db.sql(f"SELECT * FROM main WHERE x = {k}")["rows"]) for k in (-3, 2)}
    return {"digest": _digest(db), "recs": recs, "cells": cells, "sql": sql, "numpy": num, "idx": idx, "sqlx": sqlx}


def _random_op(db, rng, b=None):
    """One random write (shape change, single cells, the three deletes)."""
    k, u = rng.random(), rng.randint(1, SMALL)
    w = b if b is not None else db
    if b is None and k < 0.22:
        sid = rng.choice((1, 2))
        n = rng.choice((1, 2, 5))
        db.put_numpy_batch(R, u, _rows(u, u + n - 1, sid, salt=rng.randrange(7)), shape_id=sid)
    elif b is None and k < 0.28:
        db.put(R, u, [u, rng.randint(-3, 3)], shape_id=2)            # put replaces the record (shape B: no y)
    elif k < 0.5:
        v = rng.choice((2, 3, 15, V_EXT, 40))
        x = {2: rng.randint(-3, 3), 3: rng.choice("abc"), 15: rng.randint(-9, 9), V_EXT: rng.randrange(7), 40: "p"}[v]
        typ = {2: "varint", 3: "utf8", 15: "varint", V_EXT: "varuint", 40: "utf8"}[v]
        w.put_cell(R, u, v, x, type=typ)
    elif k < 0.65:
        w.delete_cell(R, u, rng.choice((1, 2, 3, V_EXT, 40)))
    elif k < 0.78:
        w.delete_record(1, u)
    else:
        u1 = u + rng.choice((0, 1, 4, 20, 100))
        v0 = rng.choice((1, 2, 3, 16, 21))
        w.delete_area(R, u, u1, v0, v0 + rng.choice((0, 1, 13, 60)))


def _fresh_small(path, cfg=CFG_AGGRESSIVE):
    cf = path.parent / (path.name + ".toml")
    cf.write_text(cfg)
    db = nhydb.NHyDB.create(str(path), config=str(cf))
    _schema(db)
    _fill(db, SMALL)
    return db, str(cf)


@pytest.mark.parametrize("seed", range(4))
def test_rollback_sequences(tmp_path, seed):
    """rollback() in the middle of sequences of writes, deletes, batches,
    build_indexes(commit=False) and commits with automatic maintenance at
    every commit and directory flushes inside the transaction: afterwards
    every read path and index answer equals the state before the
    transaction; the final state survives reopen and compaction."""
    rng = random.Random(9100 + seed)
    p = tmp_path / "rb.nhydb"
    db, cfg = _fresh_small(p)
    committed = _view(db)
    n_rb = n_cm = 0
    for step in range(300):
        k = rng.random()
        if k < 0.70:
            _random_op(db, rng)
        elif k < 0.76:
            with db.batch(u_window=rng.choice((1, 10, 10_000))) as b:
                for _ in range(rng.randint(1, 8)):
                    _random_op(db, rng, b)
        elif k < 0.79:
            db.build_indexes(commit=False)
        elif k < 0.88:
            db.commit()
            n_cm += 1
            committed = _view(db)
            assert _index_problems(db) == [], (seed, step)
        else:
            db.rollback()
            n_rb += 1
            now = _view(db)
            for key in committed:
                assert now[key] == committed[key], (seed, step, "rollback", key)
            assert db.verify(deep=True)["ok"]
    assert n_rb >= 5 and n_cm >= 5
    db.commit()
    final = _view(db)
    assert _index_problems(db) == []
    db.close()
    with nhydb.NHyDB(str(p), config=cfg) as db:
        assert _view(db) == final
        assert db.verify(deep=True)["ok"]
    nhydb.NHyDB.compact_file(str(p), make_backup=False)
    with nhydb.NHyDB(str(p), config=cfg) as db:
        assert _view(db) == final


# ----------------------------------------------------------------------
# 3. failed WriteBatch (E2)
# ----------------------------------------------------------------------

def _batch_ops(b, rng, n):
    for _ in range(n):
        _random_op(None, rng, b)


@pytest.mark.parametrize("mode", ["size", "size_with", "realm", "exception"])
def test_failed_batch_is_consumed_and_leaves_nothing(tmp_path, mode):
    rng = random.Random(f"e2-{mode}")
    p = tmp_path / "b.nhydb"
    db, cfg = _fresh_small(p)
    db.create_realm(2, "side")
    db.commit()
    # uncommitted writes before the batch; the index is stale: overlay
    for _ in range(25):
        _random_op(db, rng)
    db.put_points(R, [(u, 2, 3, "varint") for u in range(1, SMALL, 4)])
    assert _index_problems(db) == []
    if mode == "realm":
        db.commit()                       # rollback() in the block drops every uncommitted write
        db.create_realm(9, "tmp")
    pre = _view(db) if mode != "realm" else None
    st0 = db.stats()
    if mode.startswith("size"):
        db.set_max_db_size(st0["current_end"] + 1500, commit=False)
        st0 = db.stats()
    end0, committed_end0 = st0["current_end"], st0["committed_end"]
    caught = None
    b = db.batch(commit=True, u_window=rng.choice((1, 10_000)))
    try:
        with b:
            _batch_ops(b, rng, 400)
            for u in range(1, 400):
                b.put_cell(2, u, 1, "z" * 40, type="utf8")
            if mode == "size_with":
                # the failed explicit flush inside the block: the block end
                # writes and commits nothing (E2)
                with pytest.raises(nhydb.DatabaseSizeLimitError):
                    b.flush()
                with pytest.raises(RuntimeError):
                    b.put_cell(2, 1, 1, 1, type="varint")
            elif mode == "realm":
                b.put_cell(9, 1, 1, 1, type="varint")
                db.rollback()             # removes realm 9 under the batch
                pre = _view(db)
                end0 = committed_end0 = db.stats()["current_end"]
            elif mode == "exception":
                raise LookupError("application error inside the block")
    except (nhydb.NHyDBError, LookupError) as e:
        caught = type(e)
    want = {"size": nhydb.DatabaseSizeLimitError, "size_with": None, "realm": nhydb.SchemaError, "exception": LookupError}[mode]
    assert caught is want or (want is not None and caught is not None and issubclass(caught, want)), (mode, caught)
    st = db.stats()
    assert st["current_end"] == end0, (mode, st["current_end"], end0)
    assert st["committed_end"] == committed_end0, "nothing committed by the failed batch"
    now = _view(db)
    for key in pre:
        assert now[key] == pre[key], (mode, key)
    assert all(db.get_cell(2, u, 1) is None or not db.get_cell(2, u, 1)["present"] for u in (1, 2, 399))
    assert db.verify(deep=True)["ok"]
    # the batch is consumed
    for call in (lambda: b.flush(), lambda: b.put_cell(R, 1, 2, 1), lambda: b.delete_record(1, 1), b.__enter__):
        with pytest.raises(RuntimeError):
            call()
    if mode != "exception":               # see test_len_after_exception_exit
        assert len(b) == 0
    assert db.stats()["current_end"] == end0
    # the handle goes on: write, commit, reopen
    if mode.startswith("size"):
        db.set_max_db_size(1 << 32, commit=False)
    for _ in range(20):
        _random_op(db, rng)
    db.commit()
    final = _view(db)
    assert _index_problems(db) == []
    db.close()
    with nhydb.NHyDB(str(p), config=cfg) as db:
        assert _view(db) == final
        assert db.verify(deep=True)["ok"]


def test_len_after_exception_exit(tmp_path):
    """Found while writing these tests: __exit__ on an exception discards
    the collected operations, so len() reports 0 (as after discard())."""
    db = nhydb.NHyDB.create(str(tmp_path / "l.nhydb"))
    _schema(db)
    b = db.batch()
    with pytest.raises(LookupError):
        with b:
            b.put_cell(R, 1, 2, 1)
            raise LookupError
    assert db.get_cell(R, 1, 2) is None or not db.get_cell(R, 1, 2)["present"]
    with pytest.raises(RuntimeError):
        b.flush()
    assert len(b) == 0


# ----------------------------------------------------------------------
# 4. limiter: cancel_job() on the delete and batch paths (E5)
# ----------------------------------------------------------------------

CANCEL = r'''
import glob, hashlib, json, os, threading, time
import numpy as np
import nhydb
N = 300_000
P = "c.nhydb"
db = nhydb.NHyDB.create(P)
db.create_realm(1, "r")
db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"}, {"v": 3, "name": "k", "type": "varuint"}])
db.create_classic_layout(1, 1, "c", shape_id=1)
db.create_index(5, 1, "ix_k", 3)
ids = np.arange(1, N + 1, dtype=np.uint64)
db.put_numpy_batch(1, 1, {"id": ids, "body": np.array(["b%07d" % u + "y" * 40 for u in range(N)], dtype=object), "k": ids % 1000}, shape_id=1)
db.commit()
db.build_indexes(commit=True)

def sha():
    h = hashlib.sha256()
    with open(P, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

def snap(h):
    return [h.sql("SELECT COUNT(*) FROM c")["rows"][0]["count_all"], h.sql("SELECT SUM(k) FROM c")["rows"][0],
            h.get(1, 1), h.get(1, N // 2), h.get(1, N), sorted(h.find_u(1, {"ix_k": 7}))[:5], len(h.find_u(1, {"ix_k": 7})),
            len(h.sql("SELECT id FROM c WHERE k = 9")["rows"]), h.stats()["current_end"]]

def batch_delete():
    with db.batch() as b:
        for u in range(1, N + 1):
            b.delete_record(1, u)
        b.delete_area(1, 1, N, 2, 2)

def batch_mixed():
    with db.batch() as b:
        for u in range(1, N + 1, 2):
            b.put_cell(1, u, 3, 4242)
            b.delete_cell(1, u + 1, 2)

def compact():
    nhydb.NHyDB.compact_file(P, make_backup=True)

OPS = {
    "delete_area_records": lambda: db.delete_area(1, 1, N, 1, 3),
    "delete_area_v": lambda: db.delete_area(1, 1, N, 2, 2),
    "batch_delete": batch_delete,
    "batch_mixed": batch_mixed,
    "build_indexes": lambda: db.build_indexes(commit=False),
    "compact_file": compact,
}
db.put_cell(1, N, 3, 555)            # uncommitted write before: kept by a cancel
base, sha0 = snap(db), sha()
assert base[4]["k"] == 555
res = {}
for name, fn in OPS.items():
    r = {"cancelled": 0, "missed": 0, "bad": [], "paths": []}
    if name == "compact_file":
        db.rollback()
        db.close()                       # a compaction needs the file closed
        sha0 = sha()
        with nhydb.NHyDB(P, readonly=True) as h:
            base = snap(h)
    for attempt in range(4):
        box = {}
        def run():
            try:
                box["r"] = fn()
            except BaseException as e:
                box["e"] = e
        th = threading.Thread(target=run)
        th.start()
        hit = None
        while th.is_alive() and hit is None:
            for j in nhydb.resource_status()["jobs"]:
                hit = (j["path"], nhydb.cancel_job(j["id"]))
                break
            time.sleep(0.0002)
        th.join(120)
        if th.is_alive():
            r["bad"].append("HANG")
            break
        e = box.get("e")
        st = nhydb.resource_status()
        if st["jobs"] or st["active_jobs"]:
            r["bad"].append(["register not empty", str(st["jobs"])])
        if hit:
            r["paths"].append(hit[0])
        if name == "compact_file":
            h = nhydb.NHyDB(P, readonly=True)
        else:
            h = db
        now = snap(h)
        if e is None:
            r["missed"] += 1
            if name != "compact_file":
                db.rollback()
                db.put_cell(1, N, 3, 555)
                if snap(db) != base:
                    r["bad"].append("rollback did not restore")
            else:
                base, sha0 = now, sha()      # compacted: the new base
            if name == "compact_file":
                h.close()
            for f in glob.glob(P + ".before_compact_*"):
                os.remove(f)
            continue
        if not (isinstance(e, nhydb.ResourceLimitError) and e.reason == "cancelled_by_app"):
            r["bad"].append([type(e).__name__, str(e)[:200]])
        r["cancelled"] += 1
        if now != base:
            r["bad"].append(["changed after cancel", repr([(i, now[i], base[i]) for i in range(len(now)) if now[i] != base[i]])])
        # nothing of the cancelled call stays behind the logical end (the file
        # is cut back; an uncommitted earlier write may still sit in the write
        # buffer, so the size can be below current_end, never above)
        if os.path.getsize(P) > h.stats()["current_end"]:
            r["bad"].append(["file grew past current_end", os.path.getsize(P), h.stats()["current_end"]])
        if name == "compact_file":
            if sha() != sha0:
                r["bad"].append("file bytes changed by a cancelled compaction")
            left = sorted(os.path.basename(f) for f in glob.glob(P + ".*") if not f.endswith((".layout.enc", ".layout.key", ".lock")))
            if any(".compacting" in f for f in left):
                r["bad"].append(["leftover", left])
            for f in glob.glob(P + ".before_compact_*"):
                os.remove(f)
        v = h.verify(deep=True)
        if not v["ok"]:
            r["bad"].append(["verify", str(v.get("errors"))[:300]])
        if name == "compact_file":
            h.close()
        if r["cancelled"] >= 2:
            break
    res[name] = r
db = nhydb.NHyDB(P)
db.put_cell(1, 5, 3, 77)
db.rollback()
v = db.verify(deep=True)["ok"]
db.close()
print(json.dumps({"res": res, "verify": v, "jobs": nhydb.resource_status()["jobs"]}))
'''


def test_cancel_job_rolls_back_deletes_batches_compaction_and_build(tmp_path):
    """E5: delete_area (whole records and one v over 300 000 records),
    batch flushes with deletes, compact_file and build_indexes register with
    the RAM limiter; cancel_job() ends them with ResourceLimitError
    (cancelled_by_app), data and file unchanged, verify ok, register empty.
    In a sub process: the limiter is process wide."""
    out = _run_py(CANCEL, tmp_path, timeout=300)
    print(json.dumps(out["res"]))
    for name, r in out["res"].items():
        assert r["bad"] == [], (name, r)
        assert r["cancelled"] >= 1, (name, r)
    assert out["verify"] is True and out["jobs"] == []


# ----------------------------------------------------------------------
# 5. threads on one handle (R9D-01, R9D-02, R9D-03)
# ----------------------------------------------------------------------

THREADS = r'''
import json, threading, time, traceback
import numpy as np
import nhydb
from nhydb.query import Q, QueryEngine
N = 100_000
db = nhydb.NHyDB.create("t.nhydb")
db.create_realm(1, "r")
db.create_realm(2, "side")
db.create_shape(1, 1, "s", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "body", "type": "utf8"}, {"v": 3, "name": "k", "type": "varuint"}])
db.create_classic_layout(1, 1, "c", shape_id=1)
db.create_index(5, 1, "ix_k", 3)
ids = np.arange(1, N + 1, dtype=np.uint64)
cols = {"id": ids, "body": np.array(["b%07d" % u for u in range(N)], dtype=object), "k": ids % 1000}
db.put_numpy_batch(1, 1, cols, shape_id=1)
db.commit()
db.build_indexes(commit=True)
COUNT7 = len(db.find_u(1, {"ix_k": 7}))
qe = QueryEngine(db)
b_pre = db.batch()
b_pre.put_cell(2, 5, 1, 4242, type="varuint")       # collected before the writer starts
stop = threading.Event()
busy = threading.Event()
seen = {}
bad = []
outcome = {"flush_ok": 0, "flush_rejected": 0, "put_ok": 0, "batch_put_ok": 0}

def note(name, e):
    k = name + ": " + (type(e).__module__ + "." + type(e).__name__ if e is not None else "ok")
    seen[k] = seen.get(k, 0) + 1
    if e is not None and not isinstance(e, RuntimeError):
        bad.append([name, type(e).__module__ + "." + type(e).__name__, str(e)[:200]])

def writer():
    i = 0
    while not stop.is_set():
        busy.set()
        i += 1
        if i % 2:
            db.build_indexes(commit=False)
        else:
            db.put_numpy_batch(1, 1, cols, shape_id=1)    # same content: reads stay comparable
        time.sleep(0.003)               # short gaps: some calls of the others get through

def calls():
    def it_next():
        it = db.sql_iter("SELECT id FROM c WHERE k = 7")
        next(it)
    def cells_next():
        next(iter(db.iter_materialized_cells(1)))
    def query():
        n = qe.execute(Q.field("k") == 7, realm=1, return_mode="n")["count"]
        if n != COUNT7:
            bad.append(["query result", n])
    def sql():
        n = db.sql("SELECT COUNT(*) FROM c WHERE k = 7")["rows"][0]["count_all"]
        if n != COUNT7:
            bad.append(["sql result", n])
    def find():
        n = len(db.find_u(1, {"ix_k": 7}))
        if n != COUNT7:
            bad.append(["find_u result", n])
    def enter():
        db.__enter__()
    def put():
        db.put_cell(2, 6, 1, 99, type="varuint")
        outcome["put_ok"] += 1
    b = db.batch()
    def bput():
        b.put_cell(2, 7, 1, 11, type="varuint")
        outcome["batch_put_ok"] += 1
    def bdel():
        b.delete_area(2, 100, 200, 1, 1)
    return {"next(sql_iter)": it_next, "next(cells)": cells_next, "QueryEngine.execute": query, "sql": sql, "find_u": find,
            "with db (__enter__)": enter, "put_cell": put, "WriteBatch.put_cell": bput, "WriteBatch.delete_area": bdel}, b

CALLS, b_side = calls()

def caller(name, fn):
    end = time.time() + 2.5
    while time.time() < end:
        try:
            fn()
            note(name, None)
        except BaseException as e:      # PanicException does not derive from Exception
            note(name, e)
        time.sleep(0.0005)

def flusher():
    end = time.time() + 2.5
    while time.time() < end:
        try:
            b_pre.flush()
            outcome["flush_ok"] += 1
            note("WriteBatch.flush", None)
            break
        except BaseException as e:
            note("WriteBatch.flush", e)
            if isinstance(e, RuntimeError):
                outcome["flush_rejected"] += 1
                break                   # E2: the rejected flush consumed the batch
        time.sleep(0.0005)
    try:
        b_pre.flush()
        bad.append("flush after a rejected flush did not raise")
    except RuntimeError:
        pass
    except BaseException as e:
        bad.append(["flush again", type(e).__name__])

wt = threading.Thread(target=writer, daemon=True)
wt.start()
busy.wait()
time.sleep(0.05)
ths = [threading.Thread(target=caller, args=(n, f), daemon=True) for n, f in CALLS.items()] + [threading.Thread(target=flusher, daemon=True)]
for t in ths:
    t.start()
hang = []
for t in ths:
    t.join(30)
    if t.is_alive():
        hang.append(t.name)
stop.set()
wt.join(60)
if wt.is_alive():
    hang.append("writer")
db.rollback()
cell = lambda u: (db.get_cell(2, u, 1) or {}).get("value") if (db.get_cell(2, u, 1) or {}).get("present") else None
res = {"seen": seen, "bad": bad, "hang": hang, "outcome": outcome, "len_side": len(b_side),
       "c5": cell(5), "c6": cell(6), "count7": len(db.find_u(1, {"ix_k": 7})), "COUNT7": COUNT7,
       "verify": db.verify(deep=True)["ok"], "jobs": nhydb.resource_status()["jobs"]}
print(json.dumps(res))
'''


def test_threads_on_one_handle_only_runtime_error(tmp_path):
    """R9D-01/R9D-02: while one thread writes on a handle (build_indexes /
    put_numpy_batch, GIL released), calls from other threads on the same
    handle (iterator next, QueryEngine, WriteBatch methods, with db:, sql,
    find_u, put_cell) succeed or raise RuntimeError – never PanicException,
    no hang; a rejected call changes nothing; a rejected flush consumes the
    batch without writing it (E2)."""
    out = _run_py(THREADS, tmp_path, timeout=120)
    print(json.dumps(out["seen"]))
    assert out["hang"] == [], out
    assert out["bad"] == [], out["bad"]
    assert not any("Panic" in k for k in out["seen"]), out["seen"]
    rejected = sum(n for k, n in out["seen"].items() if k.endswith("RuntimeError"))
    assert rejected > 0, ("the writer never held the handle", out["seen"])
    o = out["outcome"]
    # data changes only by calls that succeeded
    assert out["c5"] == (4242 if o["flush_ok"] else None), o
    assert out["c6"] is None, "uncommitted writes were rolled back at the end"
    assert out["len_side"] == o["batch_put_ok"] + sum(n for k, n in out["seen"].items() if k == "WriteBatch.delete_area: ok")
    assert out["count7"] == out["COUNT7"]
    assert out["verify"] is True and out["jobs"] == []


OVERLAY = r'''
import json, threading, time
import numpy as np
import nhydb
# yellow zone without much RAM: ram_bytes = 512 MiB, yellow from ~25 MiB
open("y.toml", "w").write("[limiter]\nram_bytes = 536870912\nreserve_percent = 10\nwarn_percent = 85\n"
                          "cancel_over_limit = false\nprobe_bytes = 0\n[limiter_log]\nenabled = false\n")
def mk(p, n):
    db = nhydb.NHyDB.create(p, config="y.toml")
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "varuint"}, {"v": 2, "name": "y", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_index(1, 1, "ix", "x")
    db.create_index(2, 1, "iy", "y")
    u = np.arange(1, n + 1, dtype=np.uint64)
    db.put_numpy_batch(1, 1, {"x": u % 50_000, "y": u})
    db.commit()
    db.build_indexes(commit=True)
    return db
N1 = 600_000
h1, h2 = mk("h1.nhydb", N1), mk("h2.nhydb", 2_000_000)
# both indexes of h1 stale: overwritten records (uncommitted), queries need overlays
u = np.arange(1, 240_001, dtype=np.uint64)
h1.put_numpy_batch(1, 1, {"x": (u * 7) % 50_000, "y": u + 10})
def jobs():
    return [(j["id"], j["path"], j["state"]) for j in nhydb.resource_status()["jobs"]]
gaps, stop = [], threading.Event()
def ticker():
    last = time.perf_counter()
    while not stop.is_set():
        time.sleep(0.001)
        t = time.perf_counter()
        gaps.append(t - last)
        last = t
threading.Thread(target=ticker, daemon=True).start()
T0 = time.perf_counter()
res = {}
def C():          # older job on another handle
    h2.build_indexes()
    res["C_end"] = time.perf_counter() - T0
def A():          # reader 1 on h1, no GIL: overlay of iy, younger job -> waits in yellow
    r = h1.sql("SELECT COUNT(*) FROM c WHERE y = 77")["rows"][0]["count_all"]
    res["A"] = [r, time.perf_counter() - T0]
def B2():         # reader 2 on h1, no GIL: overlay of ix
    r = h1.sql("SELECT COUNT(*) FROM c WHERE x = 5")["rows"][0]["count_all"]
    res["B2"] = [r, time.perf_counter() - T0]
tc = threading.Thread(target=C, daemon=True)
tc.start()
while not jobs() and tc.is_alive():
    time.sleep(0.001)
ta = threading.Thread(target=A, daemon=True)
ta.start()
waiting = None
end = time.time() + 5
while time.time() < end and ta.is_alive():
    js = jobs()
    if any(s == "waiting" for _, _, s in js):
        waiting = js
        break
    time.sleep(0.001)
zone = nhydb.resource_status()["zone"]
tb2 = threading.Thread(target=B2, daemon=True)
tb2.start()
t = time.perf_counter()
nb = len(h1.find_u(1, {"ix": 5}))            # GIL held: before R9D-03 blocked on the overlay mutex
res["B"] = [nb, time.perf_counter() - T0, time.perf_counter() - t]
for th in (ta, tb2, tc):
    th.join(60)
stop.set()
res["hang"] = [n for n, th in (("A", ta), ("B2", tb2), ("C", tc)) if th.is_alive()]
res["waiting"] = waiting
res["zone"] = zone
res["max_gap"] = max(gaps) if gaps else None
cols = h1.read_numpy(1, 1, N1)                # scan, no index
res["want"] = [int((cols["y"] == 77).sum()), int((cols["x"] == 5).sum())]
h1.rollback()
print(json.dumps(res))
'''


def test_overlay_readers_do_not_block_each_other(tmp_path):
    """R9D-03: two readers of stale indexes on one handle in the yellow
    zone, an older job (build_indexes on another handle) running: the
    younger overlay computation waits in the limiter; a GIL-holding find_u
    and a second reader on the same handle do not wait for it (the overlay
    mutex is not held across the limiter wait). Without a limiter wait
    (zone not yellow) this is only a smoke test."""
    out = _run_py(OVERLAY, tmp_path, timeout=150)
    print(json.dumps(out))
    assert out["hang"] == [], out
    assert out["A"][0] == out["want"][0] and out["B2"][0] == out["want"][1] and out["B"][0] == out["want"][1]
    if out["waiting"] and out["zone"] in ("yellow", "red"):
        # find_u (GIL) and reader B2 finish while the older job still runs;
        # before the correction both stood until it ended
        assert out["B"][1] < out["C_end"], out
        # the interpreter stops only for find_u's own overlay computation:
        # no stall is longer than that call (no GIL-holding limiter wait in
        # A or B2). Not a ratio to C_end: find_u's own computation is ~0.3 of
        # the job on macOS but ~0.65 on Windows (P52, 2026-11-10).
        assert out["max_gap"] <= out["B"][2] + 0.1, out


# ----------------------------------------------------------------------
# 6. R9B-02: cell cursor after an ineffective cache relief
# ----------------------------------------------------------------------

RELIEF = r'''
import itertools, json, sys, threading, time
import numpy as np
import nhydb
on = sys.argv[1] == "on"
# configuration of rev_B/p7_red_iter.py: 64 MiB, red zone as soon as a large job runs
open("c.toml", "w").write("[limiter]\n" + ("" if on else "enabled = false\n") +
                          "ram_bytes = 67108864\nprobe_bytes = 0\ncancel_over_limit = false\n[limiter_log]\nenabled = false\n")
db = nhydb.NHyDB.create("c6.nhydb", config="c.toml")
db.create_realm(1, "r")
db.create_shape(1, 1, "s", [{"v": 1, "name": "x", "type": "varuint"}])
db.create_classic_layout(1, 1, "c", shape_id=1)
db.put_numpy_batch(1, 1, {"x": np.arange(1, 1_000_001, dtype=np.uint64)})
for s in range(0, 100_000, 1000):               # 100 000 sparse cells in 100 blocks
    cu = np.arange(s + 1, s + 1001, dtype=np.uint64)
    db.put_cells_numpy(1, cu, np.full(1000, 50, dtype=np.uint64), cu.astype(np.float64), type="f64")
db.commit()
db.close()
db = nhydb.NHyDB("c6.nhydb", config="c.toml")
def cursor():
    t = time.perf_counter()
    n = sum(1 for _ in itertools.islice(db.iter_materialized_cells(1), 100_000))
    return n, time.perf_counter() - t
c1 = cursor()
# when the limiter first judged a relief ineffective (sampled every 10 ms)
judged, stop = [], threading.Event()
def sampler():
    while not stop.is_set() and not judged:
        if nhydb.resource_status()["counters"].get("cache_relief_ineffective"):
            judged.append(time.perf_counter())
        time.sleep(0.01)
th = threading.Thread(target=sampler, daemon=True)
t = time.perf_counter()
th.start()
rows = len(db.sql("SELECT x FROM c WHERE x > 0")["rows"])     # large job: monitor thread, relief
t_end = time.perf_counter()
sql_s = t_end - t
stop.set()
th.join()
s = nhydb.resource_status()
c2 = cursor()
s2 = nhydb.resource_status()
print(json.dumps({"c1": c1, "c2": c2, "rows": rows, "sql_s": sql_s, "zone": s["zone"], "zone2": s2["zone"],
                  "relief": s2["counters"].get("cache_relief_requests"), "ineffective": s2["counters"].get("cache_relief_ineffective"),
                  "judged_at": (judged[0] - t) if judged else None, "after_judged_s": (t_end - judged[0]) if judged else None}))
'''


def test_cursor_after_ineffective_relief_is_not_slowed_down(tmp_path):
    """R9B-02: an ineffective cache relief ends; before the correction the
    pot stayed capped near 0 until the zone was green again and the cell
    cursor over 100 000 cells was ~140x slower (0.2 s -> 28-34 s), the
    collecting query ~60x. Now within 10x of enabled = false.

    The test runs a deliberately unreachable budget (64 MiB against a
    process of ~440 MiB): the zone stays red whatever the limiter does. It
    is a stress test of the relief logic, not a performance benchmark.

    Platform dependence of the collecting query (decision W3, 2026-11-10):
    on macOS the first relief is judged ineffective at once (freed cache
    stays in the footprint). On Windows the first relief is effective (the
    level falls by the freed ~37 MB), the zone stays red, further reliefs
    free nothing while the query's handle is busy and are therefore not
    judged; the pot stays capped until a relief frees bytes again and is
    judged ineffective (measured 4-20 s, once 91 s with dir3.toml; old code
    308-327 s, enabled = false 2.9 s). That phase is the limiter saving RAM
    under an unreachable budget and is accepted. What R9B-02 guarantees is
    checked: once a relief is judged ineffective, it ends - the rest of the
    query runs at the speed of enabled = false (before the correction it
    stayed slow until green)."""
    (tmp_path / "off").mkdir()
    (tmp_path / "on").mkdir()
    off = _run_py(RELIEF.replace("sys.argv[1]", "'off'"), tmp_path / "off")
    on = _run_py(RELIEF.replace("sys.argv[1]", "'on'"), tmp_path / "on")
    print(json.dumps({"off": off, "on": on}))
    assert off["c2"][0] == on["c2"][0] == 100_000 and off["rows"] == on["rows"] == 1_000_000
    if on["zone2"] == "green" or not on["relief"] or not on["ineffective"]:
        pytest.skip(f"no ineffective cache relief in the yellow/red zone reached here: {on}")
    # cell cursor after the query: the relief has ended (all platforms)
    assert on["c2"][1] <= 10 * max(off["c2"][1], 0.05), (on, off)
    # collecting query: after the ineffective relief is judged, it runs at
    # the speed of enabled = false; the time before that judgement depends
    # on the platform (see docstring) and is not bounded here
    assert on["judged_at"] is not None, ("ineffective relief not seen during the query", on)
    assert on["after_judged_s"] <= 10 * max(off["sql_s"], 0.2), (on, off)
