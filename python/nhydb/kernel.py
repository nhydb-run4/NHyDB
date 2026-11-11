"""Stateless NHyDB kernel.

Address mathematics, binary codecs, set kernels, superblock/block header and
the crypto primitives of the external layout plan. All computations run in
the native module (Rust); this module only provides thin wrappers for
Python-specific types (``array.array``, Base64).
"""
from __future__ import annotations

import base64 as _b64
import os as _os
import struct as _struct
import sys as _sys
from array import array as _array
from typing import Any, Dict, Mapping

from . import _native as _n
from ._native import (  # noqa: F401
    NHyDBError as KernelError,
    CorruptionError,
    SchemaError,
    RealmLimitError,
    AddressError,
    REALM_SPAN,
    TYPE_CODES,
    normalize_lane,
    lane_name,
    validate_local_v,
    realm_v_bounds,
    global_v,
    split_global_v,
    dynadron_address,
    parse_n_reference,
    dynadron_n,
    n_to_dynadron_address,
    cell_address,
    enc_vu,
    dec_vu,
    enc_vi,
    dec_vi,
    enc_len,
    dec_len,
    json_key_encode,
    json_key_decode,
    encode_big_posting,
    decode_big_posting,
    sorted_unique_ints,
    intersect_sorted_ints,
    union_sorted_ints,
    difference_sorted_ints,
    enc_value,
    dec_value,
    encode_dense_row,
    compile_dense_batch_codec,
    compile_selective_decoder,
    pack_super,
    unpack_super,
    pack_block_header,
    unpack_block_header,
    align8,
    fmt_bytes,
    json_bytes,
    layout_mac,
)

DEFAULT_REALM_SPAN = REALM_SPAN
DEFAULT_REALM_AUTO_LIMIT = 4096
DEFAULT_DB_MAX_SIZE_BYTES = 50 * (1024 ** 3)
NL = 0
NR = 1
UINT32_MAX = (1 << 32) - 1
UINT64_MAX = (1 << 64) - 1
CODE_TYPES = {v: k for k, v in TYPE_CODES.items()}
FIXED = {
    "u8": _struct.Struct("<B"), "u16": _struct.Struct("<H"),
    "u32": _struct.Struct("<I"), "u64": _struct.Struct("<Q"),
    "i8": _struct.Struct("<b"), "i16": _struct.Struct("<h"),
    "i32": _struct.Struct("<i"), "i64": _struct.Struct("<q"),
    "f32": _struct.Struct("<f"), "f64": _struct.Struct("<d"),
    "bool": _struct.Struct("<?"),
}

SUPERBLOCK_SIZE = 4096
SUPERBLOCK_COUNT = 2
DATA_START = SUPERBLOCK_SIZE * SUPERBLOCK_COUNT
FORMAT_VERSION = (0, 28, 2)
SUPER_MAGIC = b"NHYDB" + bytes(FORMAT_VERSION)
SUPER_MAGIC_016 = b"NHYDB016"  # format of NHyDB 0.28.0 without block directory (no longer opened, CorruptionError)
FORMAT_MAJOR = 0
FORMAT_MINOR = 28
BLOCK_MAGIC = b"NHBL"
BLOCK_VERSION = 16
BLOCK_HEADER_SIZE = 32
BLOCK_FLAG_LANE_NR = 0x0001
DATA_TAG = b"D28D"  # prefix with CRC32 checksum (0.28.2)
DATA_TAG_016 = b"D10D"
SEC_TAG = b"SI28"  # root of a SECONDARY key tree (0.28.2)
SEC_TAG_016 = b"SI10"
CHECKPOINT_TAG = b"CP10"
SPARSE_TAG = b"SP28"  # prefix with CRC32 checksum (0.28.2)
SPARSE_TAG_016 = b"SP10"
EXT_TAG = 0xEA

BT_REALM = 1
BT_SHAPE = 2
BT_INDEX_DEF = 3
BT_DATA_DENSE = 4
BT_DELETE = 5
BT_CHECKPOINT = 6
BT_SECONDARY = 7
BT_EXTENSION_DEF = 8
BT_LAYOUT_DEF = 9
BT_SPARSE_CELLS = 10
BT_MATH_LAYOUT_STATE = 11
BT_DB_CONFIG = 12
BT_DIRECTORY = 14
BT_SECONDARY_PAGE = 15  # pages and posting chunks of the SECONDARY key trees (0.28.2)
BT_SYSTEM = 13
BLOCK_NAMES = {
    BT_REALM: "REALM", BT_SHAPE: "SHAPE", BT_INDEX_DEF: "INDEX_DEF",
    BT_DATA_DENSE: "DATA_DENSE", BT_DELETE: "DELETE", BT_CHECKPOINT: "CHECKPOINT",
    BT_SECONDARY: "SECONDARY_INDEX", BT_EXTENSION_DEF: "EXTENSION_DEF",
    BT_LAYOUT_DEF: "LAYOUT_DEF", BT_SPARSE_CELLS: "SPARSE_CELLS",
    BT_MATH_LAYOUT_STATE: "MATH_LAYOUT_STATE", BT_DB_CONFIG: "DB_CONFIG", BT_SYSTEM: "SYSTEM_R0_U1",
}


def require_positive(name: str, value: int) -> int:
    value = int(value)
    if value <= 0:
        raise ValueError(f"{name} must be > 0")
    return value


def lane_flags(lane=0) -> int:
    return BLOCK_FLAG_LANE_NR if normalize_lane(lane) else 0


def lane_from_flags(flags: int) -> int:
    return 1 if (int(flags) & BLOCK_FLAG_LANE_NR) else 0


def flatten_dyn_address(addr: Mapping[str, int]) -> Dict[str, int]:
    return {"_dyn_realm": addr["realm"], "_dyn_u": addr["u"],
            "_dyn_v_local": addr["v_local"], "_dyn_v": addr["v"],
            "_dyn_G": addr["G"], "_dyn_station": addr["station"],
            "_dyn_stufenbreite": addr["stufenbreite"], "_dyn_stufe": addr["stufe"],
            "_dyn_stufenstation": addr["stufenstation"], "_dyn_NL": addr["NL"],
            "_dyn_NR": addr["NR"]}


def array_le_bytes(a: _array) -> bytes:
    if _sys.byteorder == "little":
        return a.tobytes()
    b = _array(a.typecode, a)
    b.byteswap()
    return b.tobytes()


def array_from_le(raw: bytes, width: int) -> _array:
    width = int(width)
    if width not in (4, 8):
        raise ValueError("only 4/8-byte arrays are supported")
    a = _array("I" if width == 4 else "Q")
    a.frombytes(raw)
    if _sys.byteorder != "little":
        a.byteswap()
    return a


def enum_to_raw(field: Any, value: Any) -> Any:
    enum = getattr(field, "enum", None)
    if enum is None:
        return value
    if isinstance(value, str):
        try:
            return enum[value]
        except KeyError as e:
            raise ValueError(f"unknown enum value {value!r}") from e
    return int(value)


def enum_from_raw(field: Any, value: Any) -> Any:
    enum = getattr(field, "enum", None)
    if enum is None:
        return value
    rev = {v: k for k, v in enum.items()}
    return rev.get(value, f"<UNKNOWN_ENUM:{value}>")


def layout_seal(key: bytes, plaintext: bytes, aad: bytes) -> dict:
    """AES-256-GCM with a random 12-byte nonce (native)."""
    if len(key) != 32:
        raise ValueError("layout key must be 32 bytes")
    nonce = _os.urandom(12)
    ct = _n._aes_gcm_seal(key, nonce, plaintext, aad)
    return {"nonce": _b64.b64encode(nonce).decode("ascii"), "ciphertext": _b64.b64encode(ct).decode("ascii")}


def layout_open(key: bytes, sealed: dict, aad: bytes) -> bytes:
    if len(key) != 32:
        raise ValueError("layout key must be 32 bytes")
    return _n._aes_gcm_open(key, _b64.b64decode(sealed["nonce"], validate=True),
                            _b64.b64decode(sealed["ciphertext"], validate=True), aad)


__all__ = [n for n in dir() if not n.startswith("_") and n not in {"Any", "Dict", "Mapping", "annotations"}]
