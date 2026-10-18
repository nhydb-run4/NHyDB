"""Maintenance and edge cases: repair, compaction, large u, plugin callbacks."""

import sys

import pytest

import nhydb


def _classic(path):
    db = nhydb.NHyDB.create(path)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=10)
    return db


def test_strict_open_rejects_corrupt_header(tmpdb):
    db = _classic(tmpdb)
    db.put_dense_batch(1, 1, [[1, "a"]], commit=True)
    off = db.stats()["storage"]["by_block_type"]
    db.close()
    raw = bytearray(tmpdb.read_bytes())
    raw[8192 + 9] ^= 0xFF  # corrupt the block ID of the first block → header CRC
    tmpdb.write_bytes(bytes(raw))
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(tmpdb)
    # Fail closed: the committed region (including R0,u1) must be
    # intact, regardless of strict.
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(tmpdb, strict=False)


def test_superblock_fallback(tmpdb):
    db = _classic(tmpdb)
    db.put_dense_batch(1, 1, [[1, "a"]], commit=True)
    slot = db.stats()["generation"] % 2  # active slot = generation mod 2 (starting at 0)
    db.close()
    raw = bytearray(tmpdb.read_bytes())
    raw[slot * 4096 + 20] ^= 0xFF  # destroy the current superblock
    tmpdb.write_bytes(bytes(raw))
    db = nhydb.NHyDB(tmpdb)  # falls back to the previous generation
    assert db.verify()["ok"]


def test_compact_replace_original_with_backup(tmpdb):
    db = _classic(tmpdb)
    for i in range(1, 6):
        db.put(1, i, [i, "v"], commit=True)  # many small blocks
    db.close()
    res = nhydb.NHyDB.compact_file(tmpdb)
    assert res["replaced_original"] and res["backup"] and res["saved_bytes"] > 0
    assert nhydb.NHyDB(tmpdb).get(1, 5)["a"] == 5


def test_close_releases_file_handle(tmpdb):
    """close() releases the OS file handle while the object is still
    referenced: the file can be compacted in place and renamed (on Windows
    an open handle blocks the replacement with "Access is denied")."""
    db = _classic(tmpdb)
    db.put(1, 1, [1, "v"], commit=True)
    db.close()
    with pytest.raises(nhydb.NHyDBError, match="closed"):
        db.get(1, 1)
    assert nhydb.NHyDB.compact_file(tmpdb, make_backup=False)["replaced_original"]
    moved = tmpdb.with_name("moved.nhydb")
    tmpdb.rename(moved)
    moved.rename(tmpdb)
    with nhydb.open(tmpdb) as db2:
        assert db2.get(1, 1)["a"] == 1


class Occ:
    """CUSTOM plugin with occupied_addresses that calls back into the DB."""

    def logical_address(self, layout, object_id, field, lane):
        off = {None: 0, "a": 0, "b": 1}[field]
        return {"u": 100, "v": (object_id - 1) * 2 + off + 1, "lane": lane}

    def type_of(self, layout, address):
        return "varint" if (address["v"] - 1) % 2 == 0 else "utf8"

    def describe(self, layout, address):
        v = address["v_local"]
        return {"record": (v - 1) // 2 + 1, "field": "ab"[(v - 1) % 2], "anchor": (v - 1) % 2 == 0}

    def occupied_addresses(self, db, layout, object_id, lane):
        out = []
        for off in (0, 1):
            v = (object_id - 1) * 2 + off + 1
            if db.get_cell(layout.realm_id, 100, v, lane=lane, dyn_address=False) is not None:
                out.append((100, v))
        return out


def test_plugin_reentrant_occupied(tmpdb):
    db = _classic(tmpdb)
    db.create_custom_layout(2, 1, "occ", plugin="occ", u_start=100, u_end=100,
                            params={"fields": [{"name": "a"}, {"name": "b", "type": "utf8"}]})
    db.register_layout_plugin("occ", Occ())
    db.put_object(2, 3, {"a": -5, "b": "drei"})
    a = db.logical_address(2, 3, "b")
    rec = db.get_n(a["N_ref"], mode="record")
    assert rec["data"] == {"a": -5, "b": "drei"}
    assert db.occupied_n(2, 3) == [f"N{nhydb.dynadron_n(1, 100, 5, 0)}", f"N{nhydb.dynadron_n(1, 100, 6, 0)}"]


def test_plugin_exception_propagates(tmpdb):
    class Bad:
        def logical_address(self, *a):
            raise KeyError("plugin-error")

    db = _classic(tmpdb)
    db.create_custom_layout(2, 1, "bad", plugin="bad", u_start=100, u_end=100)
    db.register_layout_plugin("bad", Bad())
    with pytest.raises(KeyError, match="plugin-error"):
        db.logical_address(2, 1)


def test_cli_get_and_stats(tmpdb):
    import json
    import subprocess
    import sys
    db = _classic(tmpdb)
    db.put(1, 7, [3, "sieben"])
    db.close()
    n = nhydb.dynadron_n(1, 7, 2, "NL")
    out = subprocess.check_output([sys.executable, "-m", "nhydb", "get", str(tmpdb), f"N{n}", "--mode", "cell", "--json"], text=True)
    assert json.loads(out)["value"] == "sieben"
    out = subprocess.check_output([sys.executable, "-m", "nhydb", "stats", str(tmpdb)], text=True)
    assert "NHyDB STATS" in out
    out = subprocess.check_output([sys.executable, "-m", "nhydb", "health", str(tmpdb), "--deep"], text=True)
    assert "Result      :" in out


@pytest.mark.skipif(sys.platform == "win32", reason="RLIMIT_FSIZE/SIGXFSZ are POSIX-only; on all platforms the "
                    "same property is covered by the Rust test db::file_handle_tests::failed_append_is_rolled_back")
def test_failed_append_is_rolled_back(tmp_path):
    """A write error (here: file size limit) must not leave a partial block
    behind that a later commit would make durable."""
    import subprocess
    import sys
    p = tmp_path / "efbig.nhydb"
    code = f"""
import resource, signal, nhydb
signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
db = nhydb.NHyDB.create({str(p)!r})
db.create_realm(1, "r")
db.create_shape(1, 1, "s", [{{"v": 1, "name": "b", "type": "bytes"}}])
db.create_classic_layout(1, 1, "c", shape_id=1)
db.put(1, 1, [b"ok"], commit=True)
soft, hard = resource.getrlimit(resource.RLIMIT_FSIZE)
resource.setrlimit(resource.RLIMIT_FSIZE, (1 << 20, hard))
try:
    db.put(1, 2, [b"x" * (9 << 20)])
    raise SystemExit("no error raised")
except OSError:
    pass
resource.setrlimit(resource.RLIMIT_FSIZE, (soft, hard))
db.put(1, 3, [b"drei"], commit=True)
db.close()
"""
    r = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    db = nhydb.NHyDB(p)
    assert db.get(1, 1)["b"] == b"ok" and db.get(1, 3)["b"] == b"drei" and db.get(1, 2) is None
    assert db.verify(deep=True)["ok"]


def test_crafted_huge_payload_len_is_corruption(tmpdb):
    import struct
    import zlib
    db = _classic(tmpdb)
    db.close()
    raw = bytearray(tmpdb.read_bytes())
    hdr = bytearray(raw[8192:8224])
    struct.pack_into("<Q", hdr, 16, 2**64 - 16)
    struct.pack_into("<I", hdr, 28, 0)
    struct.pack_into("<I", hdr, 28, zlib.crc32(bytes(hdr)))
    raw[8192:8224] = hdr
    tmpdb.write_bytes(bytes(raw))
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(tmpdb)
    assert not nhydb.NHyDB.inspect_tail(tmpdb)["clean"]


