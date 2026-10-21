"""R013: keyed schema digest.

The layout plan entry (field ``sha256``) and R0,u1,v6 bind the active schema
snapshot with ``HMAC-SHA256(SHA256("NHYDB027-SCHEMA-DIGEST:" + key),
snapshot)`` instead of a plain SHA-256 – without the key a guessed schema can
not be confirmed from the DB or plan file. Manipulation fails closed.
"""
import hashlib
import hmac
import json
from pathlib import Path

import pytest

import nhydb


def _make(p):
    db = nhydb.NHyDB.create(p)
    db.create_realm(1, "main")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "u32"}])
    db.close()
    lp, kp = Path(str(p) + ".layout.enc"), Path(str(p) + ".layout.key")
    db = nhydb.NHyDB(p)
    db_id, gen, v6 = db._db_id, db.get_system_cell(5), db.get_system_cell(6)
    db.close()
    key = nhydb._native._layout_key_load(kp, db_id, p.name)
    return lp, kp, db_id, gen, v6, key


def test_digest_is_keyed_not_plain_sha256(tmp_path):
    p = tmp_path / "d.nhydb"
    lp, _, db_id, gen, v6, key = _make(p)
    entries = nhydb._native._layout_read_envelope(lp, db_id, key)
    active = [e for e in entries if e["generation"] == gen]
    assert len(active) == 1 and active[0]["sha256"] == v6
    snap = nhydb._native._layout_open_entry(db_id, active[0], key)
    raw = nhydb._native._json_dumps(snap).encode("utf-8")
    # the plain hash of the snapshot (what an attacker can compute for a
    # guessed schema) appears neither in the plan nor in R0,u1,v6
    plain = hashlib.sha256(raw).hexdigest()
    assert v6 != plain
    assert plain not in lp.read_text() and plain.encode() not in p.read_bytes()
    mk = hashlib.sha256(b"NHYDB027-SCHEMA-DIGEST:" + key).digest()
    assert v6 == hmac.new(mk, raw, hashlib.sha256).hexdigest()
    # another key gives another digest
    other = hmac.new(hashlib.sha256(b"NHYDB027-SCHEMA-DIGEST:" + bytes(32)).digest(), raw, hashlib.sha256).hexdigest()
    assert other != v6


def test_plain_sha256_entry_is_rejected(tmp_path):
    """A plan whose entry carries the plain SHA-256 (v0.27 style) – even with a
    correctly recomputed envelope MAC – does not match R0,u1,v6: fail closed."""
    p = tmp_path / "d.nhydb"
    lp, _, db_id, gen, _, key = _make(p)
    entries = nhydb._native._layout_read_envelope(lp, db_id, key)
    for e in entries:
        snap = nhydb._native._layout_open_entry(db_id, e, key)
        e["sha256"] = hashlib.sha256(nhydb._native._json_dumps(snap).encode("utf-8")).hexdigest()
    nhydb._native._atomic_layout_replace(lp, nhydb._native._layout_envelope(db_id, entries, key))
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(p)


def test_manipulated_digest_or_snapshot_fails_closed(tmp_path):
    p = tmp_path / "d.nhydb"
    lp, _, db_id, gen, _, key = _make(p)
    original = lp.read_bytes()
    env = json.loads(original)
    # digest flipped (MAC not recomputed) -> integrity check fails
    d = env["entries"][-1]["sha256"]
    env["entries"][-1]["sha256"] = ("0" if d[0] != "0" else "1") + d[1:]
    lp.write_text(json.dumps(env, sort_keys=True, separators=(",", ":")))
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(p)
    # digest flipped with a valid MAC -> no plan entry matches R0,u1,v6
    lp.write_bytes(original)
    entries = nhydb._native._layout_read_envelope(lp, db_id, key)
    for e in entries:
        e["sha256"] = "0" * 64
    nhydb._native._atomic_layout_replace(lp, nhydb._native._layout_envelope(db_id, entries, key))
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(p)
    # other snapshot sealed for the same generation (valid MAC and
    # encryption) -> its keyed digest does not match R0,u1,v6
    lp.write_bytes(original)
    entries = nhydb._native._layout_read_envelope(lp, db_id, key)
    snap = nhydb._native._layout_open_entry(db_id, [e for e in entries if e["generation"] == gen][0], key)
    snap["realms"] = []
    forged = nhydb._native._layout_seal_entry(db_id, gen, snap, key)
    nhydb._native._atomic_layout_replace(lp, nhydb._native._layout_envelope(db_id, [forged], key))
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(p)
    # untouched plan opens again
    lp.write_bytes(original)
    nhydb.NHyDB(p).close()


def _as_v0271(p, lp, db_id, key):
    """Synthetic NHyDB 0.27.1 database: every plan entry carries the plain
    SHA-256 of its snapshot (bound via AES-GCM AAD, valid plan MAC) and the
    R0,u1 records reference these plain digests (block CRCs recomputed)."""
    import base64
    import os
    import struct
    import zlib
    aesgcm = pytest.importorskip("cryptography.hazmat.primitives.ciphers.aead")
    entries = nhydb._native._layout_read_envelope(lp, db_id, key)
    mapping, new = {}, []
    for e in entries:
        raw = nhydb._native._json_dumps(nhydb._native._layout_open_entry(db_id, e, key)).encode("utf-8")
        plain = hashlib.sha256(raw).hexdigest()
        nonce = os.urandom(12)
        aad = f"NHYDB027:{db_id}:{e['generation']}:{plain}".encode()
        ct = aesgcm.AESGCM(bytes(key)).encrypt(nonce, raw, aad)
        mapping[e["sha256"].encode()] = plain.encode()
        new.append({"generation": e["generation"], "sha256": plain,
                    "nonce": base64.b64encode(nonce).decode(), "ciphertext": base64.b64encode(ct).decode()})
    nhydb._native._atomic_layout_replace(lp, nhydb._native._layout_envelope(db_id, new, key))
    data = bytearray(p.read_bytes())
    off, patched = 8192, 0
    while off + 32 <= len(data):
        plen = struct.unpack_from("<Q", data, off + 16)[0]
        start, end = off + 32, off + 32 + plen
        payload = bytes(data[start:end])
        for old, plain in mapping.items():
            if old in payload:
                payload = payload.replace(old, plain)
                patched += 1
        data[start:end] = payload
        struct.pack_into("<I", data, off + 24, zlib.crc32(payload))
        struct.pack_into("<I", data, off + 28, 0)
        struct.pack_into("<I", data, off + 28, zlib.crc32(bytes(data[off:off + 32])))
        off = (end + 7) & ~7
    assert patched >= 1
    p.write_bytes(bytes(data))


def test_v0271_plan_gets_version_message(tmp_path):
    """G2-2: a database of NHyDB 0.27.1 (plain schema digest, valid MAC)
    is rejected with a message naming the old version; repair_file does not
    report it as clean/no action."""
    p = tmp_path / "d.nhydb"
    lp, _, db_id, _, _, key = _make(p)
    _as_v0271(p, lp, db_id, key)
    before = p.read_bytes(), lp.read_bytes()
    for ro in (False, True):
        with pytest.raises(nhydb.CorruptionError, match=r"NHyDB 0\.27\.1 or earlier.*export"):
            nhydb.NHyDB(p, readonly=ro)
    rep = nhydb.NHyDB.repair_file(p)
    assert rep["clean"] is True  # block structure intact ...
    assert rep["action"] == "abort" and "0.27.1" in rep["plan_error"]  # ... but it never opens
    with pytest.raises(nhydb.CorruptionError, match=r"0\.27\.1"):
        nhydb.NHyDB.repair_file(p, apply=True)
    assert (p.read_bytes(), lp.read_bytes()) == before, "nothing changed"
