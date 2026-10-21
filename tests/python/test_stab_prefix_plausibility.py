"""F021 rest: a DATA/SPARSE segment prefix with u_start = 0 (CRCs fixed)
is rejected as corruption instead of failing later with ValueError."""
import os
import shutil
import struct
import zlib
from pathlib import Path

import pytest

import nhydb

GOLDEN_DB = Path(__file__).parent / "golden" / "golden.nhydb"
DATA_TAG, SPARSE_TAG = b"D10D", b"SP10"


def _hdr_crc(h):
    t = bytearray(h)
    t[28:32] = b"\0\0\0\0"
    return zlib.crc32(bytes(t)) & 0xFFFFFFFF


def _skip_vu(b, p):
    while b[p] & 0x80:
        p += 1
    return p + 1


def _copy(tmp_path):
    dst = tmp_path / GOLDEN_DB.name
    for s in ("", ".layout.enc", ".layout.key"):
        shutil.copyfile(str(GOLDEN_DB) + s, str(dst) + s)
    if os.name == "posix":
        os.chmod(str(dst) + ".layout.key", 0o600)
    return dst


def _zero_u_start(raw, tag, skip):
    """Sets u_start of the first block with `tag` to 0 and fixes both CRCs."""
    b = bytearray(raw)
    off = 8192
    while off + 32 <= len(b):
        plen = struct.unpack_from("<Q", b, off + 16)[0]
        ps = off + 32
        if b[ps:ps + 4] == tag:
            p = ps + 4
            for _ in range(skip):
                p = _skip_vu(b, p)
            end = _skip_vu(b, p)
            # same varint length: 0 with continuation bytes (0x80 … 0x00)
            for i in range(p, end - 1):
                b[i] = 0x80
            b[end - 1] = 0
            struct.pack_into("<I", b, off + 24, zlib.crc32(bytes(b[ps:ps + plen])) & 0xFFFFFFFF)
            struct.pack_into("<I", b, off + 28, _hdr_crc(b[off:off + 32]))
            return bytes(b)
        off = (ps + plen + 7) & ~7
    pytest.skip(f"no block with tag {tag!r} in the golden file")


@pytest.mark.parametrize("tag,skip", [(DATA_TAG, 2), (SPARSE_TAG, 1)])
def test_u_start_zero_is_corruption(tmp_path, tag, skip):
    db_path = _copy(tmp_path)
    db_path.write_bytes(_zero_u_start(GOLDEN_DB.read_bytes(), tag, skip))
    for kw in ({"readonly": True}, {}):
        with pytest.raises(nhydb.CorruptionError, match="u_start|invalid range"):
            nhydb.NHyDB(db_path, **kw)



def test_shifted_u_start_same_length_is_reported_on_read(tmp_path):
    """F021 rest (performance phase): a flipped u_start (11 -> 41, same
    varint length) passes the open checks (header CRC and offset table are
    intact, the payload CRC is not checked on open). A read of an u that no
    segment covers any more CRC-checks the next block once and raises
    CorruptionError instead of returning None."""
    p = tmp_path / "shift.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 11)], shape_id=1, commit=True)
    db.put_dense_batch(1, 11, [[u] for u in range(11, 24)], shape_id=1, commit=True)
    db.close()
    b = bytearray(p.read_bytes())
    off, seen = 8192, 0
    while True:
        plen = struct.unpack_from("<Q", b, off + 16)[0]
        ps = off + 32
        if b[ps:ps + 4] == DATA_TAG:
            seen += 1
            if seen == 2:
                pos = _skip_vu(b, _skip_vu(b, ps + 4))
                assert b[pos] == 11
                b[pos] = 41
                break
        off = (ps + plen + 7) & ~7
    p.write_bytes(bytes(b))
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get(1, 5)["a"] == 5
        for read in (lambda: db.get(1, 15), lambda: db.get_value(1, 15, 1), lambda: db.get_cell(1, 15, 1)):
            with pytest.raises(nhydb.CorruptionError, match="CRC"):
                read()
        assert db.get(1, 100) is None  # above every segment: nothing to check
        assert not db.verify()["ok"]
