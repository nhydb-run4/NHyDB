"""Shared fixtures and helpers for the Python test suite.

All tests run against the installed ``nhydb`` package.
"""
import struct
from pathlib import Path

import pytest

import nhydb

DATA_START = 8192
BT_SYSTEM = 13

BODY = {
    0: "the quick brown fox {u}",
    1: "lazy dog sleeps",
    2: "quick quick fox jumps",
    3: "nothing here",
}


def sample_rows(n=100):
    """Deterministic sample records ``[id, status, body, amount]`` for u = 1..n."""
    return [[u, "active" if u % 3 == 0 else "idle", BODY[u % 4].format(u=u), u * 10] for u in range(1, n + 1)]


@pytest.fixture
def sample_db(tmp_path):
    """Database with realm 1, CLASSIC layout ``records`` and 100 committed records."""
    db = nhydb.NHyDB.create(str(tmp_path / "sample.nhydb"))
    db.create_realm(1, "main")
    db.create_shape(1, 1, "row", [
        {"v": 1, "name": "id", "type": "varuint"},
        {"v": 2, "name": "status", "type": "utf8"},
        {"v": 3, "name": "body", "type": "utf8"},
        {"v": 4, "name": "amount", "type": "varuint"},
    ])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, sample_rows(), shape_id=1)
    db.commit()
    yield db
    db.close()


@pytest.fixture
def tmpdb(tmp_path):
    return tmp_path / "t.nhydb"


def blocks(path):
    """Block sequence ``(type, block_id, flags, payload)`` from DATA_START.

    SYSTEM blocks (R0,u1) contain random/time-dependent values (UUID,
    snapshot hash, creation time) and are only compared by type/ID.
    """
    raw = Path(path).read_bytes()
    out = []
    off = DATA_START
    while off + 32 <= len(raw):
        magic, ver, bt, flags, bid, plen, _pcrc, _hcrc = struct.unpack_from("<4sBBHQQII", raw, off)
        if magic != b"NHBL":
            break
        payload = raw[off + 32: off + 32 + plen]
        out.append((bt, bid, flags, None if bt == BT_SYSTEM else payload))
        off += (32 + plen + 7) & ~7
    return out


def data_region(path):
    """Compatibility alias: comparable block sequence (without R0 random values)."""
    return blocks(path)
