"""System layer: R0,u1, external layout plan, fail-closed opening, locking,
configuration, durability and repair."""
import json
import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

import nhydb


def _classic(path, **kw):
    db = nhydb.NHyDB.create(path, **kw)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "u32"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.set_realm_default_shape(1, 1)
    db.create_classic_layout(1, 1, "c", shape_id=1)
    return db


def test_triad_r0_and_fixed_math_constants(tmp_path):
    p = tmp_path / "A.nhydb"
    db = _classic(p)
    db.put_dense_batch(1, 1, [(1, "x")], shape_id=1, commit=True)
    assert db.get_system_cell(2) == "A.nhydb"
    assert db.get_system_cell(5) == 1
    assert db.get_system_cell(9) == {"lane_NL": 0, "lane_NR": 1, "realm_span": 10**100}
    assert db.get_system_cell(10) is None
    with pytest.raises(nhydb.AddressError):
        db.get_system_cell(0)
    meta = db.get_system_metadata()
    assert meta["realm"] == 0 and meta["u"] == 1 and meta["cells"]["8"] == "AES-256-GCM + HMAC-SHA256"
    db.close()
    assert {f.name for f in tmp_path.iterdir()} >= {"A.nhydb", "A.nhydb.layout.enc", "A.nhydb.layout.key"}
    assert b'"classic"' not in p.read_bytes()  # no plaintext schema in the DB file
    if os.name == "posix":
        assert (Path(str(p) + ".layout.key").stat().st_mode & 0o777) == 0o600


def test_fail_closed_and_renamed_db(tmp_path):
    p = tmp_path / "A.nhydb"
    _classic(p).close()
    q = tmp_path / "B.nhydb"
    p.rename(q)
    with pytest.raises(nhydb.CorruptionError):  # file name does not match R0,u1
        nhydb.NHyDB(q, layout_path=str(p) + ".layout.enc", key_path=str(p) + ".layout.key")
    q.rename(p)
    key = Path(str(p) + ".layout.key")
    raw = key.read_bytes()
    key.unlink()
    with pytest.raises(OSError):
        nhydb.NHyDB(p)
    key.write_bytes(raw)
    plan = Path(str(p) + ".layout.enc")
    env = json.loads(plan.read_bytes())
    # tamper with the ciphertext and recompute the MAC correctly → AES-GCM tag check fails
    import base64
    k = base64.b64decode(json.loads(raw)["key_b64"])
    ct = bytearray(base64.b64decode(env["entries"][-1]["ciphertext"]))
    ct[0] ^= 1
    env["entries"][-1]["ciphertext"] = base64.b64encode(bytes(ct)).decode()
    plan.write_bytes(nhydb._native._layout_envelope(env["db_id"], env["entries"], k))
    with pytest.raises(nhydb.CorruptionError, match="AES-GCM"):
        nhydb.NHyDB(p)


LOCK_HOLDER = textwrap.dedent("""
    import sys, time, nhydb
    db = nhydb.NHyDB(sys.argv[1])
    print("locked", flush=True)
    time.sleep(float(sys.argv[2]))
    db.close()
""")


def test_write_lock_between_processes(tmp_path):
    p = tmp_path / "L.nhydb"
    _classic(p).close()
    proc = subprocess.Popen([sys.executable, "-c", LOCK_HOLDER, str(p), "1.5"], stdout=subprocess.PIPE, text=True)
    try:
        assert proc.stdout.readline().strip() == "locked"
        with pytest.raises(nhydb.DatabaseLockedError):
            nhydb.NHyDB(p)
        with pytest.raises(nhydb.DatabaseLockedError):
            nhydb.NHyDB.compact_file(p, make_backup=False)
        db = nhydb.NHyDB(p, lock_timeout_ms=10_000)  # waits until the other process closes
        db.close()
    finally:
        proc.wait(timeout=30)
    db = nhydb.NHyDB(p, lock=False)  # locking can be disabled
    db.close()


def test_config_file_defaults_and_persisted_values(tmp_path):
    cfg = tmp_path / "nhydb_config.toml"
    cfg.write_text("[database]\ndb_max_size_bytes = 300000\ndurability = \"full\"\n[lock]\ntimeout_ms = 50\n")
    p = tmp_path / "C.nhydb"
    db = nhydb.NHyDB.create(p)  # configuration next to the DB is found
    assert db.config_source == cfg and db.durability == "full" and db.max_db_size_bytes == 300000
    db.create_realm(1, "r")
    with pytest.raises(nhydb.DatabaseSizeLimitError):
        db.put_points(1, [(u, 1, b"x" * 1000, "bytes") for u in range(1, 400)])
    db.close()
    cfg.write_text("[database]\ndb_max_size_bytes = 999999999\n")
    db = nhydb.NHyDB(p)
    assert db.max_db_size_bytes == 300000  # persisted value wins
    assert db.durability == "fsync"
    db.close()
    cfg.write_text("[math]\nrealm_span = 10\n")
    with pytest.raises(ValueError, match="fixed mathematical"):
        nhydb.NHyDB(p)
    cfg.unlink()


def test_full_durability_commit_and_reopen(tmp_path):
    p = tmp_path / "F.nhydb"
    db = _classic(p, durability="full")
    assert db.durability == "full"
    db.put_dense_batch(1, 1, [(i, "f") for i in range(100)], shape_id=1, commit=True)
    db.close()
    db = nhydb.NHyDB(p, durability="full")
    assert db.get(1, 100)["a"] == 99
    db.close()


def test_repair_cuts_blocks_without_published_schema(tmp_path):
    """Data blocks whose realm only existed in memory are not kept;
    repair works with a relocated key file."""
    p = tmp_path / "R.nhydb"
    db = _classic(p)
    db.put_dense_batch(1, 1, [(1, "ok")], shape_id=1, commit=True)
    committed = db.committed_end
    db.create_realm(7, "nie veroeffentlicht")
    db.create_shape(7, 7, "s7", [{"v": 1, "name": "z", "type": "u8"}])
    db.put_dense_batch(7, 1, [(5,)], shape_id=7)
    db.put_dense_batch(1, 2, [(2, "tail")], shape_id=1)
    db.sync()
    db.close(commit=False)
    with open(p, "ab") as f:
        f.write(b"\x01\x02\x03")  # incomplete tail
    key = Path(str(p) + ".layout.key")
    moved = tmp_path / "geheim" / key.name
    moved.parent.mkdir()
    key.rename(moved)
    rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False, key_path=str(moved))
    assert rep["applied"] and rep["cut_reason"] and "unpublished" in rep["cut_reason"]
    assert rep["safe_end"] == committed
    db = nhydb.NHyDB(p, key_path=str(moved))
    assert db.verify(deep=True)["ok"] and db.get(1, 1)["b"] == "ok" and 7 not in db.realms
    db.close()


