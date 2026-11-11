"""Crash simulation (T002).

(a) A sub process writes with ``commit=True`` in a loop and is killed at
    random moments (SIGKILL / TerminateProcess). After every kill the
    database must open, pass ``verify(deep=True)`` and contain every commit
    the writer had acknowledged.
(b) File-state simulator: from snapshots of a real commit sequence, every
    intermediate state that a crash between the write steps of a commit can
    leave behind is materialized (data partially appended, superblock torn
    inside its 4 KiB slot, plan published but superblock old, temporary
    plan file left over, both superblocks damaged, commit boundary beyond
    the end of the file) and the defined behaviour is checked:
    - unambiguous state  -> opens with the last committed state;
    - ambiguous state    -> fail closed (CorruptionError, nothing changed),
                            ``readonly=True`` reads the last usable state,
                            ``repair_file`` restores an openable database;
    never wrong data, never an exception outside ``NHyDBError``.

Scale: ``NHYDB_CRASH_ROUNDS`` (default 6 kill rounds). Large runs, e.g.
``NHYDB_CRASH_ROUNDS=500`` (several minutes), are meant for local soak tests.
fsync fault injection lives in the Rust tests (``Storage::fail_fsync``).
"""
import hashlib
import json
import os
import random
import shutil
import struct
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import pytest

import nhydb

SB = 4096
DATA_START = 8192
SUPER_STRUCT = 68
ROUNDS = int(os.environ.get("NHYDB_CRASH_ROUNDS", "6"))
NAME = "c.nhydb"
SIDECARS = (".layout.enc", ".layout.key")


# ----------------------------------------------------------------------
# helpers
# ----------------------------------------------------------------------

def _setup(path):
    db = nhydb.NHyDB.create(str(path))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=10)
    db.create_realm(2, "sparse")
    db.commit()
    return db


def _content(db):
    """Complete logical content of the test databases."""
    recs = {}
    for u in range(1, 400):
        r = db.get(1, u)
        if r is not None:
            recs[u] = [r["a"], r["b"]]
    cells = {}
    for u in range(1, 40):
        for v in range(1, 4):
            c = db.get_cell(2, u, v)
            if c is not None:
                cells[f"{u},{v}"] = c["value"]
    return {"realms": sorted(db.realms), "recs": recs, "cells": cells}


def _blocks(raw):
    out, off = [], DATA_START
    while off + 32 <= len(raw):
        magic, _v, bt, _f, bid, plen, _pc, _hc = struct.unpack_from("<4sBBHQQII", raw, off)
        if magic != b"NHBL":
            break
        out.append((off, bt, plen))
        off += (32 + plen + 7) & ~7
    return out


def _sha(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


# ----------------------------------------------------------------------
# (a) SIGKILL loop
# ----------------------------------------------------------------------

WRITER = textwrap.dedent(r'''
    import sys, nhydb
    path, start = sys.argv[1], int(sys.argv[2])
    db = nhydb.NHyDB(path)
    i = start
    while True:
        if i % 9 == 0:
            db.create_realm(1000 + i, f"realm{i}")          # schema commit (plan + R0 + superblock)
            db.put_points(2, [(i % 37 + 1, 1, f"s{i}", "utf8")])
            db.commit()
            print(f"S {i}", flush=True)
        elif i % 5 == 0:
            db.put_dense_batch(1, i, [[i, f"rec{i}"], [i + 1, f"rec{i + 1}"]], commit=True)
            print(f"B {i}", flush=True)
            i += 1
        else:
            db.put(1, i, [i, f"rec{i}"], commit=True)
            print(f"P {i}", flush=True)
        i += 1
''')


def _expected_ok(db, acked):
    for kind, i in acked:
        if kind == "S":
            assert 1000 + i in db.realms, f"acknowledged schema commit {i} lost"
        else:
            for u in ((i, i + 1) if kind == "B" else (i,)):
                r = db.get(1, u)
                assert r is not None and r["a"] == u and r["b"] == f"rec{u}", f"acknowledged record {u} lost: {r}"


def test_sigkill_during_commit_loop(tmp_path):
    p = tmp_path / NAME
    _setup(p).close()
    rnd = random.Random(20261020)
    acked_all = []
    start = 1
    for rnd_no in range(ROUNDS):
        proc = subprocess.Popen([sys.executable, "-c", WRITER, str(p), str(start)],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        # wait for the first acknowledgement, then a random time
        first = proc.stdout.readline()
        assert first, proc.stderr.read()
        time.sleep(rnd.uniform(0.0, 0.25))
        proc.kill()
        out, err = proc.communicate(timeout=30)
        lines = [first] + out.splitlines()
        acked = [(ln.split()[0], int(ln.split()[1])) for ln in lines if ln.strip() and len(ln.split()) == 2]
        acked_all += acked
        with nhydb.NHyDB(p) as db:
            assert db.verify(deep=True)["ok"], db.verify(deep=True)
            _expected_ok(db, acked_all)
            # continue after the highest committed record (an unacknowledged commit may be durable)
            u = max(i + (1 if k == "B" else 0) for k, i in acked_all) + 1
            while db.get(1, u) is not None or (1000 + u) in db.realms:
                u += 1
            start = u
    assert len(acked_all) >= ROUNDS


# ----------------------------------------------------------------------
# (b) file-state simulator
# ----------------------------------------------------------------------

def _record_history(d):
    """Runs a commit sequence and snapshots (db bytes, plan bytes, content) after each commit."""
    p = d / NAME
    db = _setup(p)
    snaps = []

    def snap(label):
        snaps.append({"label": label, "db": p.read_bytes(), "plan": Path(str(p) + ".layout.enc").read_bytes(),
                      "content": _content(db)})

    snap("setup")
    db.put(1, 1, [1, "eins"], commit=True)
    snap("put")
    db.put_dense_batch(1, 2, [[u, f"r{u}"] for u in range(2, 30)], shape_id=1, commit=True)
    snap("batch")
    db.create_realm(3, "neu")                       # schema commit
    db.put_points(2, [(u, v, f"c{u}.{v}", "utf8") for u in range(1, 8) for v in (1, 2)])
    db.commit()
    snap("schema+sparse")
    db.delete_record(1, 5)
    db.put(1, 7, [70, "overlay"])
    db.commit()
    snap("delete+overlay")
    db.create_index(10, 1, "b", 2)                  # schema commit
    db.build_indexes()
    db.commit()
    snap("index")
    db.put_points(2, [(9, 3, "late", "utf8")], commit=True)
    snap("late")
    key = Path(str(p) + ".layout.key").read_bytes()
    db.close(commit=False)
    return snaps, key


def _materialize(d, db_bytes, plan_bytes, key, extra=None):
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True)
    p = d / NAME
    p.write_bytes(db_bytes)
    Path(str(p) + ".layout.enc").write_bytes(plan_bytes)
    kp = Path(str(p) + ".layout.key")
    kp.write_bytes(key)
    if os.name == "posix":
        os.chmod(kp, 0o600)
    for name, data in (extra or {}).items():
        (d / name).write_bytes(data)
    return p


def _copy_triad(p, d):
    d.mkdir()
    for s in ("",) + SIDECARS:
        shutil.copyfile(str(p) + s, str(d / NAME) + s)
    if os.name == "posix":
        os.chmod(str(d / NAME) + ".layout.key", 0o600)
    return d / NAME


def _active(raw):
    gens = []
    for s in range(2):
        gens.append(struct.unpack_from("<Q", raw, s * SB + 16)[0])
    return 0 if gens[0] >= gens[1] else 1


@pytest.fixture(scope="module")
def history(tmp_path_factory):
    return _record_history(tmp_path_factory.mktemp("hist"))


def _open_content(p, **kw):
    with nhydb.NHyDB(p, **kw) as db:
        assert db.verify(deep=True)["ok"]
        return _content(db)


def test_history_is_append_only(history):
    snaps, _ = history
    for a, b in zip(snaps, snaps[1:]):
        assert b["db"][DATA_START:len(a["db"])] == a["db"][DATA_START:], "committed bytes rewritten"
        changed = [s for s in range(2) if a["db"][s * SB:(s + 1) * SB] != b["db"][s * SB:(s + 1) * SB]]
        assert len(changed) == 1, f"{b['label']}: exactly one superblock slot written per commit"


def _cut_points(a_len, b_raw):
    """Crash points inside the appended range: every block boundary plus torn blocks."""
    pts = {a_len}
    for off, _bt, plen in _blocks(b_raw):
        if off >= a_len:
            pts |= {off, off + 1, off + 31, off + 32, off + 32 + plen // 2, off + 32 + plen}
    return sorted(x for x in pts if a_len <= x < len(b_raw))


def test_crash_while_appending_keeps_last_commit(tmp_path, history):
    """Superblock not yet written: any prefix of the appended data (with old
    or new layout plan) opens as the previous commit; the tail is discarded."""
    snaps, key = history
    n = 0
    for a, b in zip(snaps, snaps[1:]):
        plans = {a["plan"], b["plan"]}
        for cut in _cut_points(len(a["db"]), b["db"]):
            raw = a["db"][:DATA_START] + b["db"][DATA_START:cut]
            for plan in plans:
                p = _materialize(tmp_path / "s", raw, plan, key)
                assert _open_content(p) == a["content"], (b["label"], cut)
                assert p.stat().st_size == len(a["db"]), "uncommitted tail discarded"
                n += 1
    assert n > 50


def test_crash_while_publishing_plan(tmp_path, history):
    """Schema commit: crash before the rename leaves a temporary plan file;
    crash after the rename but before the superblock leaves the new plan
    (two generations) next to the old database state."""
    snaps, key = history
    schema = [(a, b) for a, b in zip(snaps, snaps[1:]) if a["plan"] != b["plan"]]
    assert len(schema) >= 2
    for a, b in schema:
        tmpname = f".{NAME}.layout.enc.0123456789ab.tmp"
        for partial in (b"", b["plan"][: len(b["plan"]) // 2], b["plan"]):
            p = _materialize(tmp_path / "pl", a["db"], a["plan"], key, extra={tmpname: partial})
            assert _open_content(p) == a["content"]
        p = _materialize(tmp_path / "pl", a["db"], b["plan"], key)
        assert _open_content(p) == a["content"]
        # and the database continues normally after such a crash
        with nhydb.NHyDB(p) as db:
            db.create_realm(77, "after-crash")
            db.commit()
        with nhydb.NHyDB(p) as db:
            assert 77 in db.realms and db.verify(deep=True)["ok"]


TEAR = [8, 16, 17, 24, 32, 40, 48, 56, 64, 68, 512, 2048, 4091, 4092, 4095]


def _torn(a_raw, b_raw, slot, t, mode):
    """Superblock slot ``slot`` of state a, partially overwritten with b's slot."""
    sb = bytearray(a_raw[:DATA_START])
    lo = slot * SB
    if mode == "prefix":
        sb[lo:lo + t] = b_raw[lo:lo + t]
    else:  # the end of the sector reached the disk first
        sb[lo + t:lo + SB] = b_raw[lo + t:lo + SB]
    return bytes(sb) + b_raw[DATA_START:]


def _check_ambiguous(p, a_content, b_content, label):
    """Fail closed: nothing changed, readonly reads the last usable state,
    repair keeps (keep_uncommitted) or drops (allow_data_loss) the tail."""
    before = _sha(p)
    with pytest.raises(nhydb.CorruptionError, match="readonly=True"):
        nhydb.NHyDB(p)
    assert _sha(p) == before, f"{label}: fail-closed open must not modify the file"
    import warnings
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", RuntimeWarning)
        got = _open_content(p, readonly=True)
    assert got == a_content, label
    assert _sha(p) == before
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB.repair_file(p, apply=True, make_backup=False)
    assert _sha(p) == before, f"{label}: refused repair must not modify the file"
    keep = _copy_triad(p, p.parent / "keep")
    rep = nhydb.NHyDB.repair_file(keep, apply=True, make_backup=False, keep_uncommitted=True)
    assert rep["after"]["clean"], rep
    assert _open_content(keep) == b_content, f"{label}: keep_uncommitted must keep the complete commit"
    rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False, allow_data_loss=True)
    assert rep["after"]["clean"], rep
    assert _open_content(p) == a_content, f"{label}: allow_data_loss returns to the usable superblock"


def test_torn_superblock_write(tmp_path, history):
    snaps, key = history
    checked = {"new": 0, "old": 0, "ambiguous": 0}
    for a, b in zip(snaps, snaps[1:]):
        slot = _active(b["db"])
        assert slot != _active(a["db"])
        for mode in ("prefix", "suffix"):
            for t in TEAR:
                raw = _torn(a["db"], b["db"], slot, t, mode)
                p = _materialize(tmp_path / "torn", raw, b["plan"], key)
                label = f"{b['label']} {mode} {t}"
                if raw[:DATA_START] == b["db"][:DATA_START]:
                    assert _open_content(p) == b["content"], label   # the write was complete after all
                    checked["new"] += 1
                elif raw[:DATA_START] == a["db"][:DATA_START]:
                    assert _open_content(p) == a["content"], label   # only unchanged bytes written
                    checked["old"] += 1
                else:
                    _check_ambiguous(p, a["content"], b["content"], label)
                    checked["ambiguous"] += 1
    assert checked["ambiguous"] > 100


def test_both_superblocks_unusable(tmp_path, history):
    snaps, key = history
    last = snaps[-1]
    variants = {
        "zeroed": b"\0" * DATA_START + last["db"][DATA_START:],
        "crc": bytes(b ^ (0xFF if i in (SB - 1, 2 * SB - 1) else 0) for i, b in enumerate(last["db"][:DATA_START]))
               + last["db"][DATA_START:],
        "torn-both": _torn(_torn(last["db"], b"\x55" * len(last["db"]), 0, 40, "prefix"),
                           b"\x55" * len(last["db"]), 1, 40, "prefix"),
    }
    for name, raw in variants.items():
        p = _materialize(tmp_path / name, raw, last["plan"], key)
        before = _sha(p)
        for kw in ({}, {"readonly": True}):
            with pytest.raises(nhydb.CorruptionError):
                nhydb.NHyDB(p, **kw)
        insp = nhydb.NHyDB.inspect_tail(p)
        assert not insp["clean"], name
        try:
            nhydb.NHyDB.repair_file(p)  # dry run: report or refuse, never touch the file
        except nhydb.NHyDBError:
            pass
        assert _sha(p) == before, name


def test_commit_boundary_beyond_end_of_file(tmp_path, history):
    """File truncated inside the last committed transaction (not only at
    block boundaries, cf. test_truncated_at_block_boundary): fail closed,
    readonly reads the previous commit, repair returns to it."""
    snaps, key = history
    for a, b in zip(snaps[-3:], snaps[-2:]):
        for cut in _cut_points(len(a["db"]), b["db"]):
            if cut == len(a["db"]):
                continue
            p = _materialize(tmp_path / "cut", b["db"][:cut], b["plan"], key)
            before = _sha(p)
            with pytest.raises(nhydb.CorruptionError, match="outside the file"):
                nhydb.NHyDB(p)
            assert _sha(p) == before
            import warnings
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", RuntimeWarning)
                assert _open_content(p, readonly=True) == a["content"], (b["label"], cut)
            dry = nhydb.NHyDB.repair_file(p)
            assert not dry["clean"] and dry["missing_committed_bytes"] > 0, dry
            if dry["action"] == "abort":
                # complete blocks of the truncated commit survive: explicit decision required
                with pytest.raises(nhydb.CorruptionError, match="keep_uncommitted"):
                    nhydb.NHyDB.repair_file(p, apply=True, make_backup=False)
                assert _sha(p) == before
                keep = _copy_triad(p, p.parent / "keep")
                nhydb.NHyDB.repair_file(keep, apply=True, make_backup=False, keep_uncommitted=True)
                kept = _open_content(keep)
                assert all(kept["recs"].get(u) == r for u, r in a["content"]["recs"].items() if u not in b["content"]["recs"]
                           or b["content"]["recs"][u] == r), "keep_uncommitted must not lose the previous commit"
                rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False, allow_data_loss=True)
            else:
                rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False)
            assert rep["applied"] and rep["after"]["clean"], rep
            assert _open_content(p) == a["content"], (b["label"], cut)
