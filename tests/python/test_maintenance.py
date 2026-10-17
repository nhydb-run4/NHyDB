"""Wartungs-/Randfälle: Repair, Kompaktierung, große u, Plugin-Rückrufe."""
import shutil

import pytest

import nhydb


def _classic(path):
    db = nhydb.NHyDB.create(path)
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varint"}, {"v": 2, "name": "b", "type": "utf8"}])
    db.create_classic_layout(1, 1, "c", shape_id=1, base_reserve_to=10)
    return db


def test_repair_truncated_tail(tmpdb, ref):
    db = _classic(tmpdb)
    db.put_dense_batch(1, 1, [[i, f"x{i}"] for i in range(100)], commit=True)
    db.close()
    good = tmpdb.stat().st_size
    with open(tmpdb, "ab") as f:
        f.write(b"NHBL" + b"\x0e\x04" + b"\x00" * 50)  # kaputter Blockkopf
    rep = nhydb.NHyDB.inspect_tail(tmpdb)
    assert rep == ref.NHyDB.inspect_tail(tmpdb)
    assert not rep["clean"] and rep["last_good_end"] == good
    out = nhydb.NHyDB.repair_file(tmpdb, apply=True, make_backup=False)
    assert out["applied"] and out["after"]["clean"]
    db = nhydb.NHyDB(tmpdb)
    assert db.get(1, 50)["b"] == "x49"
    assert db.verify(deep=True)["ok"]


def test_strict_open_rejects_corrupt_header(tmpdb):
    db = _classic(tmpdb)
    db.put_dense_batch(1, 1, [[1, "a"]], commit=True)
    off = db.stats()["storage"]["by_block_type"]
    db.close()
    raw = bytearray(tmpdb.read_bytes())
    raw[8192 + 9] ^= 0xFF  # Block-ID des ersten Blocks verfälschen → Header-CRC
    tmpdb.write_bytes(bytes(raw))
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(tmpdb)
    # v0.27: fail closed – der committete Bereich (inkl. R0,u1) muss
    # unversehrt sein, unabhängig von strict (wie die Python-Referenz).
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(tmpdb, strict=False)


def test_superblock_fallback(tmpdb):
    db = _classic(tmpdb)
    db.put_dense_batch(1, 1, [[1, "a"]], commit=True)
    slot = db.stats()["generation"] % 2  # aktiver Slot = generation mod 2 bei Start 0
    db.close()
    raw = bytearray(tmpdb.read_bytes())
    raw[slot * 4096 + 20] ^= 0xFF  # aktuellen Superblock zerstören
    tmpdb.write_bytes(bytes(raw))
    db = nhydb.NHyDB(tmpdb)  # fällt auf die vorige Generation zurück
    assert db.verify()["ok"]


def test_compaction_with_tombstoned_base_field(tmp_path, ref):
    p = tmp_path / "a.nhydb"
    db = _classic(p)
    db.put_dense_batch(1, 1, [[i, f"x{i}"] for i in range(1, 11)])
    db.put_dense_batch(1, 3, [[99, "neu"]])          # Overlay (neueres Segment)
    db.delete_point(1, 4, 2)                          # BASE-Feld per Tombstone gelöscht
    db.delete(1, 6)
    db.put_points(1, [(8, 1, 777)])                   # BASE-Feld per Sparse überschrieben
    db.put_points(1, [(20, 5, "frei", "utf8")], lane="NR")
    before = {u: db.get(1, u) for u in range(1, 12)}
    before_nr = db.get_cell(1, 20, 5, lane="NR")["value"]
    db.close()
    # v0.14-Befund: gemergte BASE-Werte werden neu kodiert; ein per Tombstone
    # gelöschtes utf8-Feld (None) wird dabei still zu 'None' (str(None)).
    ref.NHyDB.compact_file(p, output=tmp_path / "py.nhydb", make_backup=False)
    assert before[4]["b"] is None
    assert ref.NHyDB(tmp_path / "py.nhydb").get(1, 4)["b"] == "None"
    res = nhydb.NHyDB.compact_file(p, output=tmp_path / "c.nhydb", make_backup=False)
    assert res["ok"]
    db = nhydb.NHyDB(tmp_path / "c.nhydb")
    after = {u: db.get(1, u) for u in range(1, 12)}
    for u in range(1, 12):
        if before[u] is None:
            assert after[u] is None
        else:
            assert {k: v for k, v in after[u].items() if not k.startswith("_")} == \
                   {k: v for k, v in before[u].items() if not k.startswith("_")}, u
    assert db.get_cell(1, 20, 5, lane="NR")["value"] == before_nr
    assert db.stats()["storage"]["physical_dense_records"] == 9


def test_compact_replace_original_with_backup(tmpdb):
    db = _classic(tmpdb)
    for i in range(1, 6):
        db.put(1, i, [i, "v"], commit=True)  # viele kleine Blöcke
    db.close()
    res = nhydb.NHyDB.compact_file(tmpdb)
    assert res["replaced_original"] and res["backup"] and res["saved_bytes"] > 0
    assert nhydb.NHyDB(tmpdb).get(1, 5)["a"] == 5


def test_big_u_index_and_dense(tmpdb, ref):
    big = 2**70
    db = _classic(tmpdb)
    db.create_index(1, 1, "a", "a")
    db.put_dense_batch(1, big, [[i % 3, "x"] for i in range(10)])
    db.put_dense_batch(1, 1, [[2, "y"]])
    db.build_indexes(commit=True)
    assert db.find_u(1, {"a": 2}) == [1, big + 2, big + 5, big + 8]
    db.close()
    r = ref.NHyDB(tmpdb)  # Referenz liest Big-u-Postings (width 0)
    assert r.find_u(1, {"a": 2}) == [1, big + 2, big + 5, big + 8]
    assert r.get(1, big + 9)["a"] == 0


class Occ:
    """CUSTOM-Plugin mit occupied_addresses, das in die DB zurückruft."""

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
            raise KeyError("plugin-fehler")

    db = _classic(tmpdb)
    db.create_custom_layout(2, 1, "bad", plugin="bad", u_start=100, u_end=100)
    db.register_layout_plugin("bad", Bad())
    with pytest.raises(KeyError, match="plugin-fehler"):
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


def test_failed_append_is_rolled_back(tmp_path):
    """Schreibfehler (hier: Dateigrößenlimit) darf keinen halben Block
    hinterlassen, den ein späterer Commit dauerhaft macht."""
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
    raise SystemExit("kein Fehler")
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


def test_non_strict_open_fails_closed_on_committed_damage(tmpdb, ref):
    """v0.14 erlaubte nicht-striktes Lesen bis zum Schaden; v0.27 verweigert
    jeden beschädigten committeten Block (Rust wie Python-Referenz)."""
    db = _classic(tmpdb)
    db.put(1, 1, [1, "a"], commit=True)
    db.put(1, 2, [2, "b"], commit=True)
    db.close()
    raw = bytearray(tmpdb.read_bytes())
    idx = raw.rfind(b"NHBL\x10\x04")  # letzter DATA-Block (Version 16)
    assert idx > 0
    raw[idx + 9] ^= 0xFF
    tmpdb.write_bytes(bytes(raw))
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(tmpdb, strict=False)
    with pytest.raises(ref.CorruptionError):
        ref.NHyDB(tmpdb, strict=False)
