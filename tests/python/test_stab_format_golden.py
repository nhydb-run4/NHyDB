"""Format golden test (T004).

A deterministic scenario touching every block type that lives in the
database file (DATA_DENSE incl. extensions and overlays, SPARSE_CELLS,
DELETE, CHECKPOINT, SECONDARY_INDEX, DB_CONFIG, SYSTEM), the layout kinds
CLASSIC/COLUMN/PACKED/MATH, secondary and text indexes and big
coordinates. The SHA-256 of every block payload (SYSTEM blocks only by
type/id/flags: they hold the random db id, creation time and the plan
digest) is compared with ``golden/format_blocks.json``.

SECONDARY_INDEX blocks hold zlib-compressed posting lists. The engine
compresses with the system libz on Unix and with miniz_oxide on Windows;
both produce valid zlib streams, but the compressed bytes may differ. These
blocks are therefore compared structurally (header, directory keys, counts
and codecs) and by their fully decompressed postings against the matching
block of the checked-in ``golden/golden.nhydb``.

In addition a small database created with this scenario is checked in
(``golden/golden.nhydb`` + layout plan + *test-only* key). Every future
version must open and read it.

An intentional format change: regenerate both with

    python tests/python/test_stab_format_golden.py --update

and document the change (CHANGELOG, docs/FORMAT.md).
"""
import hashlib
import json
import os
import shutil
import struct
import sys
import zlib
from pathlib import Path

import pytest

import nhydb
import nhydb.math as rm

HERE = Path(__file__).resolve().parent
GOLDEN_DIR = HERE / "golden"
GOLDEN_JSON = GOLDEN_DIR / "format_blocks.json"
GOLDEN_DB = GOLDEN_DIR / "golden.nhydb"
SIDECARS = ("", ".layout.enc", ".layout.key")
DATA_START = 8192
BT_SYSTEM = 13
BT_SECONDARY = 7
SEC_TAG = b"SI10"
BIG = 10**100
HUGE_U = 2**70 + 3

BLOCK_NAMES = {4: "DATA_DENSE", 5: "DELETE", 6: "CHECKPOINT", 7: "SECONDARY_INDEX",
               10: "SPARSE_CELLS", 12: "DB_CONFIG", 13: "SYSTEM"}


def build_scenario(path):
    """Builds the deterministic golden scenario at ``path`` (closed afterwards)."""
    db = nhydb.NHyDB.create(str(path))
    # CLASSIC realm with all scalar types
    db.create_realm(1, "main")
    db.create_shape(1, 1, "row", [
        {"v": 1, "name": "id", "type": "varuint"},
        {"v": 2, "name": "status", "type": "utf8"},
        {"v": 3, "name": "body", "type": "utf8"},
        {"v": 4, "name": "amount", "type": "varint"},
        {"v": 5, "name": "ratio", "type": "f64"},
        {"v": 6, "name": "small", "type": "u16"},
        {"v": 7, "name": "blob", "type": "bytes"},
        {"v": 8, "name": "f", "type": "f32"},
    ])
    db.set_realm_default_shape(1, 1)
    db.create_classic_layout(1, 1, "records", shape_id=1, base_reserve_to=20)
    db.define_extension(1, 50, "utf8", name="note")
    db.create_index(10, 1, "status", 2)
    db.create_index(11, 1, "amount", 4)
    db.create_fulltext_index(12, 1, "ft_body", 3)
    db.create_token_index(13, 1, "tok_body", 3)
    db.create_trigram_index(14, 1, "tri_body", 3)
    words = ["alpha beta", "gamma delta", "beta gamma epsilon", "zeta"]
    rows = [[u, ("active", "idle", "gone")[u % 3], words[u % 4] + f" {u}", (-1) ** u * u * 1000,
             u / 8, u * 7, bytes([u, 255 - u]), u * 0.5] for u in range(1, 25)]
    db.put_dense_batch(1, 1, rows, shape_id=1, commit=True)
    db.put_extensions(1, 3, {"note": "third"})
    db.put(1, 7, [7, "active", "overlay row 7", -7_000_000, 0.75, 49, b"\x00\x07", 3.5])  # overlay
    db.put(1, 30, [30, "idle", "late", 2**62, -0.0, 65535, b"", -1.25])
    db.commit()
    # sparse realm: mixed types, big u, NL/NR lanes
    db.create_realm(2, "sparse")
    db.put_points(2, [(u, v, f"val{u * v}", "utf8") for u in range(1, 5) for v in range(1, 4)])
    db.put_points(2, [(3, 9, b"\x01\x02bytes", "bytes"), (4, 9, -2**80, "varint"), (5, 1, 2**90, "varuint"),
                      (HUGE_U, 2, "far away", "utf8"), (6, BIG, 1.5, "f64")])
    db.put_cell(2, 7, 2, "NR cell", type="utf8", lane="NR")
    db.commit()
    # COLUMN and PACKED layouts in realm 3
    db.create_realm(3, "cols")
    db.create_shape(3, 1, "two", [{"v": 1, "name": "N", "type": "varuint"}, {"v": 2, "name": "tp", "type": "varint"}])
    db.create_column_layout(2, 3, "col", fields=[{"u": 1000, "name": "N", "type": "varuint"},
                                                  {"u": 1001, "name": "tp", "type": "varint"}])
    db.create_packed_layout(3, 3, "pk", u=2000, span=16, fields=[{"offset": 0, "name": "N", "type": "varuint"},
                                                                 {"offset": 1, "name": "tp", "type": "varint"}])
    for i in range(1, 6):
        db.put_record(2, i, {"N": i, "tp": -i})
        db.put_record(3, i, {"N": i * 10, "tp": i})
    db.commit()
    # MATH layout across two logical realms
    m = rm.MathMap.compile("grid", ("x", "y"), u="x+1", V="y", side="x % 2")
    db.create_math_layout(rm.MathMapLayoutSpec("grid", m, 4096), layout_id=7, payload_type="utf8")
    db.put_math_batch(7, [((x, y), f"{x},{y}") for x in range(1, 4) for y in (1, BIG + 1)], commit=True)
    # deletes of all kinds
    db.delete(1, 5)
    db.delete_cell(2, 2, 2)
    db.delete_record(2, 4)
    db.commit()
    db.put(1, 31, [31, "active", "after checkpoint", 1, 1.0, 1, b"x", 1.0], commit=True)
    db.build_indexes()
    db.checkpoint()
    db.put_points(2, [(8, 1, "after checkpoint", "utf8")], commit=True)
    db.close()


def _block_list(raw):
    out, off = [], DATA_START
    while off + 32 <= len(raw):
        magic, ver, bt, flags, bid, plen, _pc, _hc = struct.unpack_from("<4sBBHQQII", raw, off)
        if magic != b"NHBL":
            break
        payload = raw[off + 32: off + 32 + plen]
        out.append({
            "type": BLOCK_NAMES.get(bt, str(bt)), "bt": bt, "version": ver, "block_id": bid, "flags": flags,
            "len": plen, "sha256": None if bt == BT_SYSTEM else hashlib.sha256(payload).hexdigest(),
        })
        off += (32 + plen + 7) & ~7
    return out, off


def describe_file(path):
    """Comparable description of the file format (header fields + per-block hashes)."""
    raw = Path(path).read_bytes()
    blocks, end = _block_list(raw)
    sb = raw[:4096]
    return {
        "superblock_magic": sb[:8].decode("ascii"),
        "file_size": len(raw),
        "blocks_end": end,
        "blocks": blocks,
    }


def block_payloads(path):
    """``block_id -> payload`` of all blocks in the file."""
    raw = Path(path).read_bytes()
    out, off = {}, DATA_START
    while off + 32 <= len(raw):
        magic, _ver, _bt, _flags, bid, plen, _pc, _hc = struct.unpack_from("<4sBBHQQII", raw, off)
        if magic != b"NHBL":
            break
        out[bid] = raw[off + 32: off + 32 + plen]
        off += (32 + plen + 7) & ~7
    return out


def _get_vu(buf, p):
    n = shift = 0
    while True:
        b = buf[p]
        p += 1
        n |= (b & 0x7F) << shift
        if b < 0x80:
            return n, p
        shift += 7


def decode_secondary(payload):
    """SECONDARY payload without its compressed bytes: header, directory
    (key, count, codec per entry) and the decompressed posting lists."""
    assert payload[:4] == SEC_TAG
    p = 4
    index_id, p = _get_vu(payload, p)
    source_block_id, p = _get_vu(payload, p)
    entry_count, p = _get_vu(payload, p)
    dir_len, p = _get_vu(payload, p)
    directory, postings = payload[p:p + dir_len], payload[p + dir_len:]
    entries, q, next_rel = [], 0, 0
    for _ in range(entry_count):
        klen, q = _get_vu(directory, q)
        key = directory[q:q + klen]
        q += klen
        rel, q = _get_vu(directory, q)
        clen, q = _get_vu(directory, q)
        count, q = _get_vu(directory, q)
        width = directory[q]
        q += 1
        assert rel == next_rel, "posting lists not stored back to back"
        comp = postings[rel:rel + clen]
        assert len(comp) == clen, "posting list points beyond the block"
        entries.append({"key": key, "count": count, "width": width, "postings": zlib.decompress(comp)})
        next_rel += clen
    assert q == len(directory), "SECONDARY directory trailing bytes"
    assert next_rel == len(postings), "SECONDARY postings trailing bytes"
    return {"index_id": index_id, "source_block_id": source_block_id, "entries": entries}


def contents(db):
    """Representative reads of the scenario (compared between golden DB and fresh build)."""
    out = {
        "classic": [db.get(1, u) for u in (1, 3, 5, 7, 24, 30, 31)],
        "ext3": db.get_cell(1, 3, 50),
        "sparse": [db.get_cell(2, u, v) for u, v in ((8, 1), (1, 1), (2, 2), (3, 9), (4, 9), (5, 1), (HUGE_U, 2), (6, BIG))],
        "nr": db.get_cell(2, 7, 2, lane="NR"),
        "col": [db.get_record(2, i) for i in (1, 4, 5)],
        "pk": [db.get_record(3, i) for i in (1, 5)],
        "math": [db.get_math(7, (x, y)) for x in (1, 2, 3) for y in (1, BIG + 1)],
        "find_status": sorted(db.find_u(1, {"status": "active"})),
        "find_amount": sorted(db.find_u(1, {"amount": 2**62})),
        "sql": db.sql("SELECT status, COUNT(*) AS n FROM records GROUP BY status ORDER BY status")["rows"],
        "fulltext": db.sql("SELECT id FROM records WHERE FULLTEXT(body, :q) ORDER BY id", {"q": "gamma"})["rows"],
    }
    return json.loads(json.dumps(out, default=repr, sort_keys=True))


def _copy_triad(src, dst_dir):
    dst_dir.mkdir(parents=True, exist_ok=True)
    for s in SIDECARS:
        shutil.copyfile(str(src) + s, str(dst_dir / src.name) + s)
    key = Path(str(dst_dir / src.name) + ".layout.key")
    if os.name == "posix":
        os.chmod(key, 0o600)
    return dst_dir / src.name


def _load_golden():
    return json.loads(GOLDEN_JSON.read_text(encoding="utf-8"))


def test_scenario_format_matches_golden(tmp_path):
    """The current engine writes byte-identical block payloads for the scenario
    (SECONDARY_INDEX: identical structure and decompressed postings)."""
    p = tmp_path / "golden.nhydb"
    build_scenario(p)
    got = describe_file(p)
    want = _load_golden()["format"]
    assert got["superblock_magic"] == want["superblock_magic"]
    assert [(b["type"], b["block_id"], b["flags"]) for b in got["blocks"]] == \
           [(b["type"], b["block_id"], b["flags"]) for b in want["blocks"]], "block sequence changed"
    got_payloads, golden_payloads = block_payloads(p), block_payloads(GOLDEN_DB)
    for g, w in zip(got["blocks"], want["blocks"]):
        msg = f"block {w['block_id']} ({w['type']}) differs: format change? see module docstring"
        if w["bt"] != BT_SECONDARY:
            assert g == w, msg
            continue
        ref = golden_payloads[w["block_id"]]
        assert hashlib.sha256(ref).hexdigest() == w["sha256"], "checked-in golden.nhydb and format_blocks.json diverge"
        assert {k: g[k] for k in ("type", "bt", "version", "block_id", "flags")} == \
               {k: w[k] for k in ("type", "bt", "version", "block_id", "flags")}, msg
        assert decode_secondary(got_payloads[g["block_id"]]) == decode_secondary(ref), msg
    assert got["blocks_end"] == got["file_size"]
    if all(g == w for g, w in zip(got["blocks"], want["blocks"])):
        assert got == want


def test_scenario_covers_all_file_block_types():
    types = {b["type"] for b in _load_golden()["format"]["blocks"]}
    assert set(BLOCK_NAMES.values()) <= types


def test_checked_in_golden_db_opens_and_reads(tmp_path):
    """The checked-in database (created by an earlier build) stays readable."""
    p = _copy_triad(GOLDEN_DB, tmp_path / "copy")
    g = _load_golden()
    assert describe_file(p) == g["format"], "checked-in golden.nhydb and format_blocks.json diverge"
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.verify(deep=True)["ok"]
        assert contents(db) == g["contents"]
    with nhydb.NHyDB(p) as db:
        assert db.verify(deep=True)["ok"] and db.health()["ok"]
        assert db.get(1, 7)["body"] == "overlay row 7"
        assert db.get(1, 5) is None
        assert db.get_cell(2, HUGE_U, 2)["value"] == "far away"
        assert db.get_math(7, (2, BIG + 1)) == f"2,{BIG + 1}"
        db.put(1, 40, [40, "idle", "new", 0, 0.0, 0, b"", 0.0], commit=True)
    with nhydb.NHyDB(p) as db:
        assert db.get(1, 40)["body"] == "new"
    rep = nhydb.NHyDB.compact_file(p, make_backup=False)
    assert rep
    with nhydb.NHyDB(p) as db:
        assert db.verify(deep=True)["ok"] and db.get_cell(2, 8, 1)["value"] == "after checkpoint"


def test_fresh_build_reads_like_golden_db(tmp_path):
    p = tmp_path / "golden.nhydb"
    build_scenario(p)
    with nhydb.NHyDB(p, readonly=True) as db:
        assert contents(db) == _load_golden()["contents"]


def _update():
    import tempfile
    GOLDEN_DIR.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "golden.nhydb"
        build_scenario(p)
        with nhydb.NHyDB(p, readonly=True) as db:
            cont = contents(db)
        data = {
            "comment": "T004 format golden; regenerate with: python tests/python/test_stab_format_golden.py --update",
            "engine_version": nhydb.__version__ if hasattr(nhydb, "__version__") else None,
            "format": describe_file(p),
            "contents": cont,
        }
        for s in SIDECARS:
            shutil.copyfile(str(p) + s, str(GOLDEN_DB) + s)
    GOLDEN_JSON.write_text(json.dumps(data, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    print("written", GOLDEN_JSON, GOLDEN_DB)


if __name__ == "__main__":
    if "--update" in sys.argv:
        _update()
    else:
        print(__doc__)
