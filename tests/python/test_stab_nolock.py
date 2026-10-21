"""G2-1 / G2-4: handles opened with ``lock=False`` next to a locked writer.

A handle that changed nothing never writes a superblock (its stale state
would hide a newer, acknowledged commit of the writer and a later writer
crash would cut that commit as "uncommitted tail"). A ``lock=False`` handle
with changes detects the concurrent writer and commits nothing
(``DatabaseLockedError``). ``commit()`` without new data writes nothing.
Crashes are simulated in a sub process only.
"""
import hashlib
import signal
import subprocess
import sys
import textwrap

import pytest

import nhydb


def _setup(p):
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.commit()
    db.put(1, 1, [1], commit=True)
    return db


def _sha(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()


def test_unchanged_nolock_handle_writes_nothing(tmp_path):
    p = tmp_path / "n.nhydb"
    w = _setup(p)
    w.put(1, 2, [2], commit=True)
    r = nhydb.NHyDB(str(p), lock=False)
    w.put(1, 3, [3], commit=True)  # writer commits while r is open
    h = _sha(p)
    r.commit()
    r.close()
    assert _sha(p) == h, "unchanged lock=False handle must not write"
    x = nhydb.NHyDB(str(p), readonly=True)
    assert x.sql("SELECT COUNT(*) FROM c")["rows"][0]["count_all"] == 3
    x.close()
    w.close()


def test_nolock_writer_detects_concurrent_writer(tmp_path):
    p = tmp_path / "n.nhydb"
    w = _setup(p)
    r = nhydb.NHyDB(str(p), lock=False)
    w.put(1, 2, [2], commit=True)
    r.put(1, 9, [9])
    with pytest.raises(nhydb.DatabaseLockedError, match="concurrent writer"):
        r.commit()
    with pytest.raises(nhydb.NHyDBError):
        r.close()  # poisoned: handle released, error reported
    w.put(1, 3, [3], commit=True)
    w.close()
    x = nhydb.NHyDB(str(p), readonly=True)
    assert [x.get(1, u) is not None for u in (1, 2, 3, 9)] == [True, True, True, False]
    assert x.verify()["ok"]
    x.close()


def test_empty_commit_writes_nothing(tmp_path):
    p = tmp_path / "n.nhydb"
    w = _setup(p)
    h = _sha(p)
    assert w.commit() == w.commit()
    assert _sha(p) == h
    w.close()  # N2 mirror of the own commit
    h = _sha(p)
    db = nhydb.NHyDB(str(p))
    db.commit()
    db.close()
    assert _sha(p) == h


def test_nolock_close_then_writer_crash_keeps_acknowledged_commit(tmp_path):
    p = tmp_path / "c.nhydb"
    _setup(p).close()
    code = textwrap.dedent(f"""
        import nhydb, sys, time
        W = nhydb.NHyDB({str(p)!r}); W.put(1, 2, [2], commit=True); print('ack2', flush=True)
        sys.stdin.readline()
        W.put(1, 3, [3], commit=True); print('ack3', flush=True); time.sleep(100)
    """)
    w = subprocess.Popen([sys.executable, "-c", code], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert w.stdout.readline().strip() == "ack2"
        r = nhydb.NHyDB(str(p), lock=False)  # opened after commit 2
        w.stdin.write("\n")
        w.stdin.flush()
        assert w.stdout.readline().strip() == "ack3"
        r.close()  # reader-like handle without changes
    finally:
        w.send_signal(signal.SIGKILL)
        w.wait()
    x = nhydb.NHyDB(str(p))  # locked open after the writer crash
    assert x.get(1, 2) is not None
    assert x.get(1, 3) is not None, "acknowledged commit lost"
    assert x.recovered_tail_bytes == 0
    assert x.verify()["ok"]
    x.close()
