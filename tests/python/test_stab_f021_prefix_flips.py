"""F021/RD-08: routing prefixes of DATA/SPARSE blocks are read on open
without the payload CRC. Since 0.28.2 they carry their own CRC32 (prefix
checksum), so a flipped value of the same varint length is rejected on open;
the payload CRC is checked on first use. Silently missing or stale data must
never result: the open (or the first read) must raise CorruptionError.

Each case writes a small database, flips one prefix field in place and
recomputes the header CRC (and optionally the payload CRC). A consistent
manipulation of all CRCs is out of scope (forged block)."""
import struct
import zlib

import numpy as np
import pytest

import nhydb

DATA_TAG, SPARSE_TAG = b"D28D", b"SP28"
PREFIX_FIELDS = {DATA_TAG: 4, SPARSE_TAG: 5}
HDR = 32



def _crc32c(data):
    """CRC-32C (Castagnoli) – the prefix checksum of DATA/SPARSE blocks."""
    c = 0xFFFFFFFF
    for x in data:
        c ^= x
        for _ in range(8):
            c = (c >> 1) ^ (0x82F63B78 & -(c & 1))
    return c ^ 0xFFFFFFFF

def _hdr_crc(h):
    t = bytearray(h)
    t[28:32] = b"\0\0\0\0"
    return zlib.crc32(bytes(t)) & 0xFFFFFFFF


def _vu(b, p):
    """(value, next position) of the VarUInt at p."""
    v, s = 0, 0
    while True:
        x = b[p]
        v |= (x & 0x7F) << s
        p += 1
        s += 7
        if not x & 0x80:
            return v, p


def _blocks(b, tag):
    off = 8192
    while off + HDR <= len(b):
        plen = struct.unpack_from("<Q", b, off + 16)[0]
        ps = off + HDR
        if plen >= 4 and b[ps:ps + 4] == tag:
            yield off, ps, plen
        off = (ps + plen + 7) & ~7


def _flip(path, tag, field, old, new, nth=0, fix_payload_crc=False, fix_prefix_crc=False):
    """Replaces prefix field `field` (0-based VarUInt index after the tag)
    of the nth block with `tag`: old -> new (both single-byte varints). Only
    the header CRC is recomputed unless `fix_prefix_crc` / `fix_payload_crc`."""
    b = bytearray(path.read_bytes())
    off, ps, plen = list(_blocks(b, tag))[nth]
    p = ps + 4
    for _ in range(field):
        p = _vu(b, p)[1]
    assert _vu(b, p) == (old, p + 1), (_vu(b, p), old)
    assert old < 0x80 and new < 0x80
    b[p] = new
    if fix_prefix_crc:
        q = ps + 4
        for _ in range(PREFIX_FIELDS[tag]):
            q = _vu(b, q)[1]
        struct.pack_into("<I", b, q, _crc32c(bytes(b[ps:q])))
    if fix_payload_crc:
        struct.pack_into("<I", b, off + 24, zlib.crc32(bytes(b[ps:ps + plen])) & 0xFFFFFFFF)
    struct.pack_into("<I", b, off + 28, _hdr_crc(b[off:off + HDR]))
    path.write_bytes(bytes(b))


def _two_realm_db(p):
    db = nhydb.NHyDB.create(str(p))
    for r in (1, 2):
        db.create_realm(r, f"r{r}")
        db.create_shape(r, r, f"s{r}", [{"v": 1, "name": "a", "type": "varuint"}])
        db.create_classic_layout(r, r, f"c{r}", shape_id=r)
    db.put_dense_batch(1, 1, [[u] for u in range(1, 31)], shape_id=1, commit=True)
    db.put_dense_batch(2, 1, [[100 + u] for u in range(1, 31)], shape_id=2, commit=True)
    return db


def _overlay_db(p):
    """Realm 1/2 dense u 1..30; one SPARSE block in realm 1 with overlays on
    u = 5 and u = 20 (a = 999)."""
    db = _two_realm_db(p)
    db.put_cells_numpy(1, np.array([5, 20], dtype=np.uint64), 1,
                       np.array([999, 999], dtype=np.uint64), commit=True)
    assert len(list(_blocks(p.read_bytes(), SPARSE_TAG))) == 1
    assert db.get(1, 5)["a"] == 999 and db.get(1, 20)["a"] == 999
    db.close()


def _reads_raise_or_correct(p, checks):
    """Every handle mode: either the open raises CorruptionError, or each
    read raises CorruptionError or returns the correct value."""
    for kw in ({"readonly": True}, {}):
        try:
            db = nhydb.NHyDB(p, **kw)
        except nhydb.CorruptionError:
            continue
        try:
            for read, want in checks:
                try:
                    got = read(db)
                except nhydb.CorruptionError:
                    continue
                assert got == want, (kw, got, want)
        finally:
            db.close()


# SPARSE prefix: realm, u_start, u_end, group_count, cell_count
@pytest.mark.parametrize(
    "field,old,new",
    [(1, 5, 9), (2, 20, 12), (0, 1, 2)],
    ids=["sparse_u_start_up", "sparse_u_end_down", "sparse_realm"],
)
def test_sparse_prefix_flip_never_returns_stale_base(tmp_path, field, old, new):
    p = tmp_path / "sp.nhydb"
    _overlay_db(p)
    _flip(p, SPARSE_TAG, field, old, new)
    _reads_raise_or_correct(p, [
        (lambda db: db.get(1, 5)["a"], 999),
        (lambda db: db.get(1, 20)["a"], 999),
        (lambda db: db.get_value(1, 5, 1), 999),
        (lambda db: db.get_cell(1, 20, 1)["value"], 999),
        (lambda db: db.read_numpy(1, 1, 30, fields=["a"])["a"][4], 999),
        (lambda db: db.get(2, 5)["a"], 105),
    ])


def test_data_realm_flip_is_corruption_on_open(tmp_path):
    p = tmp_path / "dr.nhydb"
    _two_realm_db(p).close()
    _flip(p, DATA_TAG, 0, 1, 2)  # realm 1 -> 2 (shape 1 belongs to realm 1)
    for kw in ({"readonly": True}, {}):
        with pytest.raises(nhydb.CorruptionError, match="DATA block .* prefix checksum"):
            nhydb.NHyDB(p, **kw)


def test_data_realm_flip_with_fixed_prefix_checksum_is_corruption_on_open(tmp_path):
    """Prefix checksum recomputed, payload CRC stale: the shape check of
    0.28.0 still rejects the open (shape 1 belongs to realm 1)."""
    p = tmp_path / "dp.nhydb"
    _two_realm_db(p).close()
    _flip(p, DATA_TAG, 0, 1, 2, fix_prefix_crc=True)
    for kw in ({"readonly": True}, {}):
        with pytest.raises(nhydb.CorruptionError, match="realm"):
            nhydb.NHyDB(p, **kw)


def test_data_realm_flip_to_unknown_realm_is_corruption_on_open(tmp_path):
    p = tmp_path / "du.nhydb"
    _two_realm_db(p).close()
    _flip(p, DATA_TAG, 0, 1, 7)
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB(p, readonly=True)


def test_sparse_prefix_flip_with_fixed_payload_crc_is_caught_by_prefix_checksum(tmp_path):
    p = tmp_path / "fp.nhydb"
    _overlay_db(p)
    _flip(p, SPARSE_TAG, 0, 1, 2, fix_payload_crc=True)
    for kw in ({"readonly": True}, {}):
        with pytest.raises(nhydb.CorruptionError, match="SPARSE block .* prefix checksum"):
            nhydb.NHyDB(p, **kw)


def test_sparse_prefix_flip_with_fixed_prefix_checksum_fails_on_first_use(tmp_path):
    """Prefix checksum recomputed, payload CRC stale: the open registers the
    block (no payload CRC on open any more), the first use of the block
    raises; reads never return the stale BASE value."""
    p = tmp_path / "fq.nhydb"
    _overlay_db(p)
    _flip(p, SPARSE_TAG, 1, 5, 9, fix_prefix_crc=True)
    with nhydb.NHyDB(p, readonly=True) as db:
        with pytest.raises(nhydb.CorruptionError):
            db.get(1, 20)
        assert not db.verify()["ok"]
    _reads_raise_or_correct(p, [(lambda db: db.get(1, 20)["a"], 999), (lambda db: db.get(2, 5)["a"], 105)])


def test_sparse_prefix_flip_with_all_crcs_fixed_is_out_of_scope(tmp_path):
    """Documents the boundary: with all CRCs recomputed the block is
    authentic as far as the file can tell; the open must still succeed and
    the database stay readable (no crash, no format error)."""
    p = tmp_path / "fx.nhydb"
    _overlay_db(p)
    _flip(p, SPARSE_TAG, 0, 1, 2, fix_prefix_crc=True, fix_payload_crc=True)
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get(1, 5)["a"] == 5  # overlay now belongs to realm 2
        assert db.get(2, 5)["a"] == 999


def test_intact_sparse_db_opens_and_reads(tmp_path):
    p = tmp_path / "ok.nhydb"
    _overlay_db(p)
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get(1, 5)["a"] == 999 and db.get(1, 6)["a"] == 6
        assert db.verify(deep=True)["ok"]


def _big_sparse_db(p, n=30_000):
    db = _two_realm_db(p)
    db.put_cells_numpy(1, np.arange(1, n + 1, dtype=np.uint64), 7,
                       np.arange(n, dtype=np.uint64), type="u64", commit=True,
                       u_window=n)
    db.close()


def test_large_sparse_block_body_damage_is_caught_on_first_use(tmp_path):
    """RD-08: the open checks only the prefix checksum of a SPARSE block; a
    damaged byte at the end of the payload is found by the payload CRC on
    the first use of the block. The open must not register the block as
    verified, otherwise a partial read of an intact group would pass."""
    p = tmp_path / "bd.nhydb"
    n = 30_000
    _big_sparse_db(p, n)
    b = bytearray(p.read_bytes())
    (off, ps, plen), = _blocks(b, SPARSE_TAG)
    b[ps + plen - 3] ^= 0x01
    p.write_bytes(bytes(b))
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get(2, 5)["a"] == 105  # other realm: untouched
        with pytest.raises(nhydb.CorruptionError, match="CRC"):
            db.get_value(1, 1, 7)  # first group intact, block damaged
        with pytest.raises(nhydb.CorruptionError):
            db.get_value(1, 2, 7)
        assert not db.verify()["ok"]
    before = p.read_bytes()
    with pytest.raises(nhydb.CorruptionError):
        nhydb.NHyDB.compact_file(p, make_backup=False)
    assert p.read_bytes() == before


def test_large_sparse_block_is_crc_checked_on_open(tmp_path):
    """A SPARSE payload above the 64 KiB scan window: intact it opens, a
    flipped realm is rejected on open by the prefix checksum."""
    p = tmp_path / "big.nhydb"
    n = 30_000
    _big_sparse_db(p, n)
    (off, ps, plen), = _blocks(p.read_bytes(), SPARSE_TAG)
    assert plen > 64 << 10
    with nhydb.NHyDB(p, readonly=True) as db:
        assert db.get_value(1, n, 7) == n - 1
    _flip(p, SPARSE_TAG, 0, 1, 2)
    for kw in ({"readonly": True}, {}):
        with pytest.raises(nhydb.CorruptionError, match="SPARSE block .* prefix checksum"):
            nhydb.NHyDB(p, **kw)
