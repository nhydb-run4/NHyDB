"""Shared fixtures and helpers for the Python test suite.

All tests run against the installed ``nhydb`` package.
"""
import struct
import zlib
from pathlib import Path

import pytest

import nhydb

ROOT = Path(__file__).resolve().parents[2]


def _source_files(root):
    """Same file set and order as ``crates/nhydb-py/build.rs`` (T024)."""
    files = [root / r for r in ("Cargo.toml", "Cargo.lock", "crates/nhydb-core/Cargo.toml", "crates/nhydb-py/Cargo.toml")]
    for d in ("crates/nhydb-core/src", "crates/nhydb-py/src"):
        files += (root / d).rglob("*.rs")
    rel = sorted((p.relative_to(root).as_posix(), p) for p in files if p.is_file())
    return rel


def _hashed_bytes(rel, data):
    """Same as ``hashed_bytes`` in build.rs: manifests without ``license-file``
    lines (``maturin sdist`` rewrites them, P-08)."""
    if not rel.endswith("Cargo.toml"):
        return data
    parts = data.split(b"\n")
    lines = [x + b"\n" for x in parts[:-1]] + ([parts[-1]] if parts[-1] else [])
    return b"".join(x for x in lines if not x.lstrip(b" \t\n\x0c\r").startswith(b"license-file"))


def source_hash(root=ROOT):
    """CRC-32 fingerprint of the Rust sources (None outside a source checkout)."""
    if not (root / "crates/nhydb-core/src").is_dir():
        return None
    crc = 0
    for rel, p in _source_files(root):
        for part in (rel.encode(), b"\0", _hashed_bytes(rel, p.read_bytes()), b"\0"):
            crc = zlib.crc32(part, crc)
    return f"crc32:{crc:08x}"


def pytest_configure(config):
    """Warn (never fail) if the installed native module does not match the checkout."""
    built = getattr(nhydb._native, "BUILD_SOURCE_HASH", None)
    here = source_hash()
    if here is not None and built not in (None, "unknown") and built != here:
        config.issue_config_time_warning(
            pytest.PytestWarning(
                f"nhydb native module was built from other sources ({built}) than this checkout ({here}); "
                "rebuild it (maturin develop) before trusting the test results"
            ),
            stacklevel=2,
        )


def pytest_report_header(config):
    built = getattr(nhydb._native, "BUILD_SOURCE_HASH", None)
    here = source_hash()
    state = "n/a" if here is None or built in (None, "unknown") else ("ok" if built == here else "MISMATCH")
    return f"nhydb native build: {built} (checkout {here}, {state})"

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
