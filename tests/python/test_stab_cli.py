"""CLI smoke test (T013/F034/F012): exit codes, no tracebacks, JSON output
(also for bytes), read commands leave the file unchanged."""
import base64
import hashlib
import json
import subprocess
import sys
from pathlib import Path

import pytest

import nhydb


def _run(*args):
    r = subprocess.run([sys.executable, "-m", "nhydb", *map(str, args)], capture_output=True, text=True)
    assert "Traceback" not in r.stderr, r.stderr
    return r


def _sha(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


@pytest.fixture
def cli_db(tmp_path):
    p = tmp_path / "cli.nhydb"
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "bytes"},
                                {"v": 3, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.create_index(1, 1, "a_idx", 1)
    db.put_dense_batch(1, 1, [[i, bytes([i, 255]), f"t{i}"] for i in range(1, 6)], shape_id=1)
    db.build_indexes()
    db.commit()
    db.put(1, 9, [9, b"x", "uncommitted"])  # uncommitted tail must survive read commands
    db.sync()
    db.close(commit=False)
    return p


def _n(realm, u, v):
    return f"N{nhydb.dynadron_n(realm, u, v, 'NL')}"


READ_COMMANDS = [
    ["stats", "{f}"], ["stats", "{f}", "--json"], ["info", "{f}"],
    ["health", "{f}"], ["health", "{f}", "--deep", "--json"],
    ["verify", "{f}"], ["verify", "{f}", "--deep"],
    ["layout", "{f}"], ["layout", "{f}", "--json"], ["layout-check", "{f}", "--json"],
    ["layout-plan", "{f}", "--layout-id", "1", "--priority", "3", "--json"],
    ["route", "{f}", "--realm", "1", "--u", "2", "--json"],
    ["describe", "{f}", "{n}", "--json"], ["occupied", "{f}", "{n}"],
    ["logical-address", "{f}", "--layout-id", "1", "--object", "2", "--field", "t"],
    ["get", "{f}", "{n}", "--json"], ["get", "{f}", "R1,u2,v2", "--json"],
    ["get", "{f}", "--realm", "1", "--u", "2"],
    ["address", "{f}", "--realm", "1", "--u", "2", "--v", "2"],
    ["cell", "{f}", "--realm", "1", "--u", "2", "--v", "2"], ["cell-n", "{f}", "{n}"],
    ["find", "{f}", "--realm", "1", "--eq", "a_idx=3", "--show", "1"],
    ["repair", "{f}"],
]


@pytest.mark.parametrize("cmd", READ_COMMANDS, ids=lambda c: " ".join(c[:3]))
def test_read_commands_do_not_modify_the_file(cli_db, cmd):
    before = _sha(cli_db)
    args = [c.format(f=cli_db, n=_n(1, 2, 2)) for c in cmd]
    r = _run(*args)
    assert r.returncode == 0, (r.stdout, r.stderr)
    assert r.stdout.strip()
    if "--json" in cmd or cmd[0] in {"verify", "occupied", "cell", "cell-n", "find", "address", "repair",
                                      "logical-address"} or (cmd[0] == "get" and "--u" in cmd):
        json.loads(r.stdout)
    assert _sha(cli_db) == before, f"{cmd[0]} changed the file"


def test_bytes_are_serialized(cli_db):
    r = _run("get", cli_db, "--realm", "1", "--u", "3")
    assert r.returncode == 0, r.stderr
    rec = json.loads(r.stdout)
    assert base64.b64decode(rec["b"]["$bytes_b64"]) == bytes([3, 255])


def test_from_n_and_to_n():
    r = _run("to-n", "--realm", "1", "--u", "2", "--v", "3", "--json")
    n = json.loads(r.stdout)["N_ref"]
    r2 = _run("from-n", n, "--json")
    assert r2.returncode == 0 and json.loads(r2.stdout)["u"] == 2


def test_errors_without_traceback(tmp_path, cli_db):
    r = _run("stats", tmp_path / "missing.nhydb")
    assert r.returncode == 3 and "nhydb: error: FileNotFoundError" in r.stderr
    r = _run("stats")
    assert r.returncode == 2  # argparse usage error
    r = _run("get", cli_db)
    assert r.returncode == 2 and "GET needs" in r.stderr


def test_verify_damaged_file_exit_code(cli_db):
    raw = bytearray(cli_db.read_bytes())
    raw[8192 + 9] ^= 0xFF  # first block header → cannot be opened
    cli_db.write_bytes(bytes(raw))
    before = _sha(cli_db)
    r = _run("verify", cli_db)
    assert r.returncode == 1
    out = json.loads(r.stdout)
    assert out["ok"] is False and "open_error" in out and out["inspect"]["error"]
    r = _run("repair", cli_db)
    assert r.returncode == 1 and json.loads(r.stdout)["action"] == "abort"
    assert _sha(cli_db) == before


def test_verify_payload_damage_exit_code(tmp_path):
    p = tmp_path / "v.nhydb"
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "t", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put(1, 1, ["wert-eins"], commit=True)
    db.put(1, 2, ["wert-zwei"], commit=True)
    db.close()
    raw = bytearray(p.read_bytes())
    raw[raw.find(b"wert-eins")] ^= 1
    p.write_bytes(bytes(raw))
    r = _run("verify", p)
    assert r.returncode == 1 and json.loads(r.stdout)["error_count"] == 1
    r = _run("health", p, "--json")
    assert r.returncode == 1 and json.loads(r.stdout)["status"] == "unhealthy"


def test_write_commands(cli_db, tmp_path):
    r = _run("rebuild-indexes", cli_db)
    assert r.returncode == 0, r.stderr
    r = _run("evolve-layout", cli_db, "--layout-id", "1", "--description", "neu", "--json")
    assert r.returncode == 0, r.stderr
    r = _run("compact", cli_db)
    assert r.returncode == 0, r.stderr
    backup = json.loads(r.stdout)["backup"]
    r = _run("compact", cli_db, "--output", tmp_path / "export.nhydb", "--no-backup")
    assert r.returncode == 0, r.stderr
    r = _run("restore", backup, cli_db)
    assert r.returncode == 0, r.stderr
    raw = bytearray(cli_db.read_bytes())
    cli_db.write_bytes(bytes(raw) + b"\x01\x02\x03")
    r = _run("repair", cli_db, "--apply", "--no-backup")
    assert r.returncode == 0 and json.loads(r.stdout)["applied"]
    r = _run("verify", cli_db, "--deep")
    assert r.returncode == 0, r.stdout
