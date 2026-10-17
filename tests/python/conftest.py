"""Shared fixtures.

Some tests compare the engine against an external Python reference
implementation (differential/oracle tests). That reference is not part of
this repository; the fixtures ``ref`` and ``ref14`` therefore skip the
dependent tests unless the reference is available next to the repository
root (see ``REFERENCE`` and ``REFERENCE_V027`` below).
"""
import importlib
import importlib.util
import struct
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
REFERENCE = ROOT / "reference"
REFERENCE_V027 = ROOT / "reference_v0_27"
PORTS = Path(__file__).resolve().parent / "ref_ports"

DATA_START = 8192
BT_SYSTEM = 13


def _load_reference_v014():
    if not (REFERENCE / "nhydb_v014.py").exists():
        pytest.skip("reference implementation not included")
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("nhydb_v014", REFERENCE / "nhydb_v014.py")
    mod = importlib.util.module_from_spec(spec)
    sys.modules["nhydb_v014"] = mod
    spec.loader.exec_module(mod)
    return mod


def _load_reference_v027():
    if not (REFERENCE_V027 / "nhydb_v027.py").exists():
        pytest.skip("reference implementation not included")
    pytest.importorskip("cryptography", reason="the external reference implementation needs real AES-256-GCM")
    sys.dont_write_bytecode = True
    if str(REFERENCE_V027) not in sys.path:
        sys.path.insert(0, str(REFERENCE_V027))
    return importlib.import_module("nhydb_v027")


@pytest.fixture(scope="session")
def ref14():
    return _load_reference_v014()


@pytest.fixture(scope="session")
def ref():
    """External reference implementation, not included (core module ``nhydb_v027``)."""
    return _load_reference_v027()


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
