"""Stabilization phase 1, cluster A: persistence, opening, repair, backups,
paths, locking and durability (findings F001 … F054, R002 … R014, T002 … T025)."""
import hashlib
import json
import os
import shutil
import signal
import struct
import subprocess
import sys
import textwrap
import time
import warnings
from pathlib import Path

import pytest

import nhydb

SB = 4096
DATA_START = 8192


def _classic(path, **kw):
    db = nhydb.NHyDB.create(path, **kw)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=10)
    return db


def _sha(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


def _blocks(p):
    """(offset, type, block_id, payload_len) of all blocks."""
    raw = Path(p).read_bytes()
    out, off = [], DATA_START
    while off + 32 <= len(raw):
        magic, ver, bt, flags, bid, plen, _pc, _hc = struct.unpack_from("<4sBBHQQII", raw, off)
        if magic != b"NHBL":
            break
        out.append((off, bt, bid, plen))
        off += (32 + plen + 7) & ~7
    return out


def _flip(p, pos, mask=0x01):
    raw = bytearray(Path(p).read_bytes())
    raw[pos] ^= mask
    Path(p).write_bytes(bytes(raw))


def _active_slot(p):
    sbs = nhydb.NHyDB.inspect_tail(p)["superblocks"]
    return max((s for s in sbs if s["valid"]), key=lambda s: s["generation"])["slot"]


def _five_commits(p):
    db = _classic(p)
    db.create_realm(2, "sparse")
    for u in range(1, 6):
        db.put(1, u, [u, f"rec{u}"], commit=True)
    db.put_points(2, [(7, 3, "sparse", "utf8")], commit=True)
    db.delete_record(1, 4)
    db.commit()
    db.put(1, 6, [6, "rec6"], commit=True)
    db.close()


# ----------------------------------------------------------------------
# F001 / T006: damage in the committed range
# ----------------------------------------------------------------------

def _damage_targets(p):
    """One block per type (not the last block): (name, header pos, payload pos)."""
    bl = _blocks(p)
    names = {13: "SYSTEM", 4: "DATA", 10: "SPARSE", 5: "DELETE", 12: "DB_CONFIG"}
    seen = {}
    for off, bt, bid, plen in bl[:-1]:
        if bt in names and names[bt] not in seen and plen > 0:
            seen[names[bt]] = (off + 9, off + 32 + plen - 1)
    return seen


@pytest.mark.parametrize("kind", ["SYSTEM", "DATA", "SPARSE", "DELETE", "DB_CONFIG"])
@pytest.mark.parametrize("where", ["header", "payload"])
def test_repair_never_cuts_committed_blocks_silently(tmp_path, kind, where):
    p = tmp_path / "f001.nhydb"
    _five_commits(p)
    hpos, ppos = _damage_targets(p)[kind]
    _flip(p, hpos if where == "header" else ppos)
    before = _sha(p)
    mtime = os.stat(p).st_mtime_ns
    dry = nhydb.NHyDB.repair_file(p)  # default: dry run
    assert dry["action"] == "abort", dry
    assert _sha(p) == before and os.stat(p).st_mtime_ns == mtime, "dry run must not touch the file"
    if kind == "SYSTEM":
        # damaged R0,u1 bootstrap: identity/key cannot be verified → no repair at all
        assert "bootstrap" in dry["plan_error"]
        for kw in ({}, {"allow_data_loss": True}):
            with pytest.raises(nhydb.CorruptionError):
                nhydb.NHyDB.repair_file(p, apply=True, **kw)
            assert _sha(p) == before
        return
    assert dry["lost_blocks"], dry
    with pytest.raises(nhydb.CorruptionError, match="allow_data_loss"):
        nhydb.NHyDB.repair_file(p, apply=True)
    assert _sha(p) == before, "file unchanged without consent"
    assert not list(tmp_path.glob("*.bak_*")), "no backup written for a refused repair"
    rep = nhydb.NHyDB.repair_file(p, apply=True, allow_data_loss=True)
    assert rep["applied"] and rep["lost_blocks"] and rep["cut_reason"]
    assert rep["after"]["clean"]
    # the backup is byte-identical to the damaged original
    assert _sha(rep["backup"]) == before
    nhydb.NHyDB(p).close()


def test_repair_backup_restore(tmp_path):
    p = tmp_path / "bak.nhydb"
    _five_commits(p)
    good = p.read_bytes()
    hpos, ppos = _damage_targets(p)["DATA"]
    _flip(p, ppos)
    rep = nhydb.NHyDB.repair_file(p, apply=True, allow_data_loss=True)
    assert Path(rep["backup_layout_plan"]).exists()
    # undo the flip in the backup → restored database has all records again
    b = Path(rep["backup"])
    raw = bytearray(b.read_bytes())
    raw[ppos] ^= 1
    b.write_bytes(bytes(raw))
    assert raw == good
    res = nhydb.NHyDB.restore_backup(b, p)
    assert res["ok"]
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 6)["b"] == "rec6" and db.get(1, 2)["b"] == "rec2"


# ----------------------------------------------------------------------
# F002 / R007: damaged superblock
# ----------------------------------------------------------------------

def _two_commits_no_close(p):
    db = _classic(p)
    db.put(1, 1, [1, "eins"], commit=True)
    db.put(1, 2, [2, "zwei"], commit=True)
    db.close(commit=False)


@pytest.mark.parametrize("pos", [16, 17, 24, 40, 63, 64, 100, 2000, 4091])
def test_damaged_active_superblock_fails_closed(tmp_path, pos):
    p = tmp_path / "f002.nhydb"
    _two_commits_no_close(p)
    _flip(p, _active_slot(p) * SB + pos)
    before, size = _sha(p), p.stat().st_size
    with pytest.raises(nhydb.CorruptionError, match="readonly=True"):
        nhydb.NHyDB(p)
    assert _sha(p) == before and p.stat().st_size == size
    with pytest.warns(RuntimeWarning):
        ro = nhydb.NHyDB(p, readonly=True)
    assert ro.get(1, 1)["b"] == "eins" and ro.get(1, 2) is None and ro.open_warnings
    ro.close()
    assert _sha(p) == before
    with pytest.raises(nhydb.CorruptionError, match="keep_uncommitted"):
        nhydb.NHyDB.repair_file(p, apply=True)
    rep = nhydb.NHyDB.repair_file(p, apply=True, keep_uncommitted=True, make_backup=False)
    gens = sorted(s["generation"] for s in rep["after"]["superblocks"])
    assert rep["after"]["clean"] and gens[0] > 2  # R007: new, higher generations
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 1)["b"] == "eins" and db.get(1, 2)["b"] == "zwei"


def test_damaged_inactive_superblock_loses_nothing(tmp_path):
    p = tmp_path / "f002b.nhydb"
    _two_commits_no_close(p)
    _flip(p, (1 - _active_slot(p)) * SB + 30)
    with pytest.warns(RuntimeWarning):
        db = nhydb.NHyDB(p)
    assert db.get(1, 2)["b"] == "zwei"
    db.put(1, 3, [3, "drei"], commit=True)  # rewrites the damaged slot
    db.close()
    assert nhydb.NHyDB.inspect_tail(p)["clean"]


def test_superblock_fallback_without_data_behind(tmp_path):
    """After close() both slots describe the same state: losing one costs nothing."""
    p = tmp_path / "fb.nhydb"
    db = _classic(p)
    db.put(1, 1, [1, "a"], commit=True)
    db.close()
    size = p.stat().st_size
    _flip(p, _active_slot(p) * SB + 20)
    with pytest.warns(RuntimeWarning, match="superblock"):
        db = nhydb.NHyDB(p)
    assert db.get(1, 1)["b"] == "a" and db.verify()["ok"] and p.stat().st_size == size
    db.close()


def test_both_superblocks_damaged(tmp_path):
    p = tmp_path / "both.nhydb"
    _two_commits_no_close(p)
    _flip(p, 20)
    _flip(p, SB + 20)
    with pytest.raises(nhydb.CorruptionError, match="both superblocks damaged"):
        nhydb.NHyDB(p, readonly=True)
    assert not nhydb.NHyDB.inspect_tail(p)["clean"]


# ----------------------------------------------------------------------
# F019 / F020
# ----------------------------------------------------------------------

def test_repair_discards_uncommitted_transaction(tmp_path):
    p = tmp_path / "f019.nhydb"
    db = _classic(p)
    db.put(1, 1, [1, "c"], commit=True)
    db.put(1, 2, [2, "tx-teil-1"])
    db.put(1, 3, [3, "tx-teil-2"])
    db.sync()
    db.close(commit=False)
    with open(p, "ab") as f:
        f.write(b"NHBL\x10\x04" + b"\x00" * 40)  # torn block
    q = tmp_path / "copy.nhydb"
    rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False)
    assert len(rep["discarded_uncommitted_blocks"]) == 2
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 2) is None and db.get(1, 3) is None and db.get(1, 1)["b"] == "c"


def test_repair_keep_uncommitted_opt_in(tmp_path):
    p = tmp_path / "keep.nhydb"
    db = _classic(p)
    db.put(1, 1, [1, "c"], commit=True)
    db.put(1, 2, [2, "tx-teil-1"])
    db.sync()
    db.close(commit=False)
    with open(p, "ab") as f:
        f.write(b"\x01\x02\x03")
    rep = nhydb.NHyDB.repair_file(p, apply=True, keep_uncommitted=True, make_backup=False)
    assert rep["kept_uncommitted_blocks"]
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 2)["b"] == "tx-teil-1"


def test_truncated_at_block_boundary(tmp_path):
    p = tmp_path / "f020.nhydb"
    db = _classic(p)
    db.put(1, 1, [1, "a"], commit=True)
    first = db.committed_end
    for u in (2, 3):
        db.put(1, u, [u, "x"], commit=True)
    db.close()
    for cut in [b[0] for b in _blocks(p) if b[0] > first]:
        q = tmp_path / "t.nhydb"
        for suffix in ("", ".layout.enc", ".layout.key"):
            shutil.copy(str(p) + suffix, str(q) + suffix)
        # the file name is bound in R0,u1: rename by compaction is not needed,
        # the copy is checked under the original name in a sub directory
        sub = tmp_path / f"cut{cut}"
        sub.mkdir()
        for suffix in ("", ".layout.enc", ".layout.key"):
            shutil.move(str(q) + suffix, str(sub / p.name) + suffix)
        r = sub / p.name
        os.truncate(r, cut)
        with pytest.raises(nhydb.CorruptionError, match="outside the file"):
            nhydb.NHyDB(r)
        dry = nhydb.NHyDB.repair_file(r)
        assert not dry["clean"] and dry["action"] == "repair" and dry["missing_committed_bytes"] > 0
        rep = nhydb.NHyDB.repair_file(r, apply=True, make_backup=False)
        assert rep["applied"] and rep["after"]["clean"]
        with nhydb.NHyDB(r) as db:
            assert db.get(1, 1)["b"] == "a"


# ----------------------------------------------------------------------
# F012 / D001: read-only mode
# ----------------------------------------------------------------------

def test_readonly_never_writes(tmp_path):
    p = tmp_path / "ro.nhydb"
    db = _classic(p)
    db.put(1, 1, [1, "a"], commit=True)
    db.put(1, 2, [2, "tail"])
    db.sync()
    db.close(commit=False)
    before = _sha(p)
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.readonly and db.get(1, 1)["b"] == "a" and db.get(1, 2) is None
        assert db.verify(deep=True)["ok"] and db.stats() and db.health()["ok"]
        with pytest.raises(nhydb.NHyDBError, match="read-only"):
            db.put(1, 3, [3, "x"])
        with pytest.raises(nhydb.NHyDBError, match="read-only"):
            db.create_realm(5, "x")
        with pytest.raises(nhydb.NHyDBError, match="read-only"):
            db.commit()
    assert _sha(p) == before


def test_readonly_on_write_protected_file(tmp_path):
    p = tmp_path / "ro444.nhydb"
    _classic(p).close()
    os.chmod(p, 0o444)
    try:
        if os.access(p, os.W_OK):
            pytest.skip("running with privileges that ignore file modes")
        with nhydb.NHyDB(p, readonly=True) as db:
            assert db.get_system_cell(2) == "ro444.nhydb"
    finally:
        os.chmod(p, 0o644)


def test_close_without_changes_keeps_file(tmp_path):
    p = tmp_path / "noop.nhydb"
    db = _classic(p)
    db.put(1, 1, [1, "a"], commit=True)
    db.close()
    before = _sha(p)
    for _ in range(3):
        nhydb.NHyDB(p).close()
    assert _sha(p) == before


# ----------------------------------------------------------------------
# F005 / F006 / F051: create(overwrite=True)
# ----------------------------------------------------------------------

def test_overwrite_never_deletes_foreign_key(tmp_path):
    one, two = tmp_path / "eins.nhydb", tmp_path / "zwei.nhydb"
    _classic(one).close()
    _classic(two).close()
    with pytest.raises(FileExistsError, match="does not belong"):
        nhydb.NHyDB.create(two, overwrite=True, key_path=str(one) + ".layout.key")
    nhydb.NHyDB(one).close()
    nhydb.NHyDB(two).close()


def test_overwrite_with_shared_key_template(tmp_path):
    keys = tmp_path / "secure"
    for d in ("A", "B"):
        (tmp_path / d).mkdir()
        (tmp_path / d / "nhydb_config.toml").write_text(f'[paths]\nkey_path = "{keys.as_posix()}/{{db}}.layout.key"\n')
    _classic(tmp_path / "A" / "kunden.nhydb").close()
    with pytest.raises(FileExistsError):
        nhydb.NHyDB.create(tmp_path / "B" / "kunden.nhydb")
    with pytest.raises(FileExistsError, match="does not belong"):
        nhydb.NHyDB.create(tmp_path / "B" / "kunden.nhydb", overwrite=True)
    nhydb.NHyDB(tmp_path / "A" / "kunden.nhydb").close()
    # unique template placeholder separates the two databases
    for d in ("A", "B"):
        (tmp_path / d / "nhydb_config.toml").write_text(
            f'[paths]\nkey_path = "{keys.as_posix()}/{{db}}.{{db_path_hash}}.layout.key"\n')
    nhydb.NHyDB.create(tmp_path / "B" / "kunden.nhydb").close()
    assert len(list(keys.glob("kunden.nhydb.*.layout.key"))) == 1


def test_overwrite_failure_keeps_old_database(tmp_path):
    p = tmp_path / "a.nhydb"
    db = _classic(p)
    db.put(1, 1, [1, "alt"], commit=True)
    db.close()
    blocker = tmp_path / "datei"
    blocker.write_text("x")
    with pytest.raises(OSError):
        nhydb.NHyDB.create(p, overwrite=True, layout_path=str(blocker / "plan.enc"))
    cfg = tmp_path / "small.toml"
    cfg.write_text("[database]\ndb_max_size_bytes = 70000\n")
    db = nhydb.NHyDB(p)
    assert db.get(1, 1)["b"] == "alt"
    db.close()
    names = sorted(x.name for x in tmp_path.iterdir())
    assert not [n for n in names if "creating" in n or "replaced" in n], names
    # successful overwrite: new empty database, old files replaced
    nhydb.NHyDB.create(p, overwrite=True).close()
    with nhydb.NHyDB(p) as db:
        assert 1 not in db.realms
    assert not [x for x in tmp_path.iterdir() if "creating" in x.name or "replaced" in x.name]


@pytest.mark.parametrize("kw,exc,match", [
    ({"key_path": ""}, ValueError, "must not be empty"),
    ({"layout_path": ""}, ValueError, "must not be empty"),
    ({"key_path": "SAME", "layout_path": "SAME"}, ValueError, "must be different"),
    ({"key_path": "DB"}, ValueError, "must be different"),
    ({"key_path": "DIR"}, IsADirectoryError, "Is a directory"),  # F051/F048: OSError subclass with errno
])
def test_path_validation(tmp_path, kw, exc, match):
    p = tmp_path / "v.nhydb"
    subst = {"SAME": str(tmp_path / "same"), "DB": str(p), "DIR": str(tmp_path)}
    kw = {k: subst.get(v, v) for k, v in kw.items()}
    with pytest.raises(exc, match=match):
        nhydb.NHyDB.create(p, **kw)
    assert not p.exists()


def test_minimum_db_size_and_exists_message(tmp_path):
    cfg = tmp_path / "c.toml"
    cfg.write_text("[database]\ndb_max_size_bytes = 1\n")
    with pytest.raises(ValueError, match="at least"):
        nhydb.NHyDB.create(tmp_path / "x.nhydb", config=str(cfg))
    p = tmp_path / "y.nhydb"
    _classic(p).close()
    with pytest.raises(FileExistsError, match="y.nhydb"):
        nhydb.NHyDB.create(p)


# ----------------------------------------------------------------------
# F007: relative paths and chdir
# ----------------------------------------------------------------------

@pytest.mark.parametrize("sidecars", [False, True])
def test_relative_paths_survive_chdir(tmp_path, monkeypatch, sidecars):
    home, other = tmp_path / "rel", tmp_path / "anderswo"
    home.mkdir()
    other.mkdir()
    monkeypatch.chdir(home)
    kw = {"layout_path": "plan.enc", "key_path": "plan.key"} if sidecars else {}
    db = nhydb.NHyDB.create("rel.nhydb", **kw)
    monkeypatch.chdir(other)
    db.create_realm(1, "r")
    db.commit()
    db.close()
    assert list(other.iterdir()) == []
    monkeypatch.chdir(home)
    with nhydb.NHyDB("rel.nhydb", **kw) as db:
        assert 1 in db.realms
        assert Path(db.path).is_absolute()


# ----------------------------------------------------------------------
# F013 / T007: compaction backups
# ----------------------------------------------------------------------

def test_compaction_backup_restorable_after_schema_commits(tmp_path):
    p = tmp_path / "c.nhydb"
    db = _classic(p)
    for u in range(1, 6):
        db.put(1, u, [u, f"v{u}"], commit=True)
    db.close()
    original = p.read_bytes()
    res = nhydb.NHyDB.compact_file(p)
    assert Path(res["backup"]).read_bytes() == original
    assert Path(res["backup_layout_plan"]).exists()
    with nhydb.NHyDB(p) as db:
        for rid in (5, 6, 7):  # three schema commits
            db.create_realm(rid, f"r{rid}")
            db.commit()
    nhydb.NHyDB.restore_backup(res["backup"], p)
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 5)["b"] == "v5" and 5 not in db.realms


# ----------------------------------------------------------------------
# F032: file name binding
# ----------------------------------------------------------------------

def test_name_binding_case_insensitive(tmp_path):
    p = tmp_path / "Case.nhydb"
    _classic(p).close()
    lower = tmp_path / "case.nhydb"
    if not lower.exists():
        pytest.skip("case-sensitive file system")
    with nhydb.NHyDB(lower, layout_path=str(p) + ".layout.enc", key_path=str(p) + ".layout.key") as db:
        assert db.get_system_cell(2) == "Case.nhydb"


def test_name_binding_message(tmp_path):
    p = tmp_path / "A.nhydb"
    _classic(p).close()
    q = tmp_path / "B.nhydb"
    shutil.copy(p, q)
    with pytest.raises(nhydb.CorruptionError, match="must keep the name"):
        nhydb.NHyDB(q, layout_path=str(p) + ".layout.enc", key_path=str(p) + ".layout.key")


# ----------------------------------------------------------------------
# F037 / T022: small and foreign files
# ----------------------------------------------------------------------

@pytest.mark.parametrize("size", [0, 1, 4095, 4096, 8191])
def test_small_files(tmp_path, size):
    p = tmp_path / "small.nhydb"
    p.write_bytes(b"\x00" * size)
    with pytest.raises(nhydb.CorruptionError, match="shorter than the superblock area"):
        nhydb.NHyDB(p, readonly=True)
    rep = nhydb.NHyDB.inspect_tail(p)
    assert not rep["clean"] and rep["trailing_bad_bytes"] >= 0 and rep["last_good_end"] >= 0
    assert nhydb.NHyDB.repair_file(p)["action"] == "abort"


def test_foreign_and_old_format(tmp_path):
    p = tmp_path / "f.nhydb"
    p.write_bytes(os.urandom(9000))
    with pytest.raises(nhydb.CorruptionError, match="not an NHyDB"):
        nhydb.NHyDB(p, readonly=True)
    q = tmp_path / "old.nhydb"
    _classic(q).close()
    raw = bytearray(q.read_bytes())
    raw[0:8] = b"NHYDB014"
    raw[SB:SB + 8] = b"NHYDB014"
    q.write_bytes(bytes(raw))
    with pytest.raises(nhydb.CorruptionError, match="NHYDB014"):
        nhydb.NHyDB(q, readonly=True)


# ----------------------------------------------------------------------
# F021: routing prefix plausibility
# ----------------------------------------------------------------------

def test_data_prefix_count_flip_detected_on_open(tmp_path):
    p = tmp_path / "pre.nhydb"
    db = _classic(p)
    db.put_dense_batch(1, 1, [[i, f"x{i}"] for i in range(1, 14)], commit=True)
    db.close()
    off = next(b[0] for b in _blocks(p) if b[1] == 4)
    raw = p.read_bytes()
    # payload: tag(4) realm shape u_start count → count is the 4th varint
    pos = off + 32 + 4 + 3
    assert raw[pos] == 13
    _flip(p, pos, 0x02)
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(p, readonly=True)


# ----------------------------------------------------------------------
# F049 / T025: context manager
# ----------------------------------------------------------------------

def test_context_manager_commits_after_exception(tmp_path):
    p = tmp_path / "cm.nhydb"
    _classic(p).close()
    with pytest.raises(KeyError):
        with nhydb.NHyDB(p) as db:
            db.put(1, 1, [1, "nach Ausnahme"])
            raise KeyError("x")
    assert db.closed
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 1)["b"] == "nach Ausnahme"  # v0.14 semantics: commit also after an exception
    with pytest.raises(nhydb.NHyDBError, match="closed"):
        with db:
            pass


@pytest.mark.skipif(os.name != "posix", reason="directory permissions")
def test_context_manager_commit_error_is_chained_and_releases(tmp_path):
    d = tmp_path / "plans"
    d.mkdir()
    p = tmp_path / "cmerr.nhydb"
    _classic(p, layout_path=str(d / "plan.enc")).close()
    os.chmod(d, 0o500)
    try:
        if os.access(d, os.W_OK):
            pytest.skip("running with privileges that ignore directory modes")
        with pytest.raises(OSError) as ei:
            with nhydb.NHyDB(p, layout_path=str(d / "plan.enc")) as db:
                db.create_realm(9, "neu")  # schema commit must write the plan
                raise ValueError("original")
        assert isinstance(ei.value.__cause__, ValueError)
        assert db.closed
    finally:
        os.chmod(d, 0o700)
    with nhydb.NHyDB(p, layout_path=str(d / "plan.enc")) as db:  # lock released
        assert 9 not in db.realms


# ----------------------------------------------------------------------
# R012: key file permissions
# ----------------------------------------------------------------------

@pytest.mark.skipif(os.name != "posix", reason="Unix permissions")
def test_key_permission_warning(tmp_path):
    p = tmp_path / "k.nhydb"
    _classic(p).close()
    os.chmod(str(p) + ".layout.key", 0o644)
    with pytest.warns(RuntimeWarning, match="chmod 600"):
        db = nhydb.NHyDB(p)
    assert any("group/others" in w for w in db.open_warnings)
    db.close()


# ----------------------------------------------------------------------
# F054: verify reports every damaged block
# ----------------------------------------------------------------------

def test_verify_reports_all_damage(tmp_path):
    p = tmp_path / "v.nhydb"
    db = _classic(p)
    for u in range(1, 5):
        db.put(1, u, [u, f"wert{u}"], commit=True)
    db.close()
    raw = p.read_bytes()
    for u in (1, 3):
        _flip(p, raw.find(f"wert{u}".encode()))
    with nhydb.NHyDB(p, readonly=True) as db:
        v = db.verify()
    assert not v["ok"] and v["error_count"] == 2, v["errors"]


# ----------------------------------------------------------------------
# R014 / T011: locking between processes
# ----------------------------------------------------------------------

HOLDER = textwrap.dedent("""
    import sys, time, nhydb
    db = nhydb.NHyDB(sys.argv[1])
    db.put(1, 1, [1, "halter"], commit=True)
    print("locked", flush=True)
    sys.stdin.readline()
    db.close()
""")


def _holder(p):
    proc = subprocess.Popen([sys.executable, "-c", HOLDER, str(p)], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, text=True)
    assert proc.stdout.readline().strip() == "locked"
    return proc


def test_lock_released_when_holder_is_killed(tmp_path):
    p = tmp_path / "kill.nhydb"
    _classic(p).close()
    proc = _holder(p)
    try:
        with pytest.raises(nhydb.DatabaseLockedError):
            nhydb.NHyDB(p)
        with pytest.raises(nhydb.DatabaseLockedError):
            nhydb.NHyDB.repair_file(p, apply=True)  # R014: lock before inspection
        # reader next to the writer
        with nhydb.NHyDB(p, readonly=True) as ro:
            assert ro.get(1, 1)["b"] == "halter"
    finally:
        proc.kill()
        proc.wait(timeout=30)
    with nhydb.NHyDB(p) as db:  # immediately available after the kill
        assert db.get(1, 1)["b"] == "halter"


def test_lock_timeout_waits_for_release(tmp_path):
    p = tmp_path / "wait.nhydb"
    _classic(p).close()
    proc = _holder(p)
    try:
        t0 = time.monotonic()
        with pytest.raises(nhydb.DatabaseLockedError):
            nhydb.NHyDB(p, lock_timeout_ms=100)
        assert time.monotonic() - t0 >= 0.09
        proc.stdin.write("\n")
        proc.stdin.flush()
        with nhydb.NHyDB(p, lock_timeout_ms=30_000) as db:
            assert db.get(1, 1)["b"] == "halter"
    finally:
        proc.wait(timeout=30)


def test_lock_config_partial_override(tmp_path):
    p = tmp_path / "cfg.nhydb"
    _classic(p).close()
    cfg = tmp_path / "nhydb_config.toml"
    cfg.write_text("[lock]\nenabled = false\ntimeout_ms = 500\n")
    a = nhydb.NHyDB(p)
    b = nhydb.NHyDB(p, lock_timeout_ms=10)  # lock stays disabled
    b.close()
    a.close()
    cfg.unlink()


@pytest.mark.skipif(os.name != "posix", reason="symlinks")
def test_lock_file_symlink_is_refused(tmp_path):
    p = tmp_path / "s.nhydb"
    _classic(p).close()
    lock = Path(str(p) + ".lock")
    lock.unlink()
    victim = tmp_path / "victim"
    lock.symlink_to(victim)
    with pytest.raises(OSError, match="symbolic link"):
        nhydb.NHyDB(p)
    assert not victim.exists()


# ----------------------------------------------------------------------
# T012: layout plan replay, damaged key JSON
# ----------------------------------------------------------------------

def test_replay_of_older_plan_generation(tmp_path):
    p = tmp_path / "plan.nhydb"
    _classic(p).close()
    plan = Path(str(p) + ".layout.enc")
    old = plan.read_bytes()
    with nhydb.NHyDB(p) as db:
        db.create_realm(5, "x")
        db.commit()
        db.create_realm(6, "y")
        db.commit()
    plan.write_bytes(old)
    with pytest.raises(nhydb.CorruptionError, match="no layout plan matching"):
        nhydb.NHyDB(p, readonly=True)


def test_damaged_key_json(tmp_path):
    p = tmp_path / "key.nhydb"
    _classic(p).close()
    key = Path(str(p) + ".layout.key")
    key.write_text(key.read_text()[:-5])
    with pytest.raises(nhydb.CorruptionError, match="invalid JSON"):
        nhydb.NHyDB(p, readonly=True)


def test_repair_with_many_schema_generations(tmp_path):
    """R0 records older than the two plan generations are valid history
    (previously repair_file refused every DB with more than two generations)."""
    p = tmp_path / "gen.nhydb"
    db = _classic(p)
    db.commit()
    for rid in (5, 6, 7):
        db.create_realm(rid, f"r{rid}")
        db.commit()
    db.put(1, 1, [1, "a"], commit=True)
    db.close()
    with open(p, "ab") as f:
        f.write(b"\x01\x02\x03")
    rep = nhydb.NHyDB.repair_file(p, apply=True, make_backup=False)
    assert rep["applied"] and rep["after"]["clean"]
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 1)["b"] == "a" and 7 in db.realms
