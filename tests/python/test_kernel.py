"""Conformance of the stateless kernel building blocks against CPython.

* Unicode properties and mappings: all 0x110000 code points.
* AES-256-GCM against the real `cryptography` library, HMAC/SHA-256
  against `hmac`/`hashlib`, `layout_mac` against its defining formula.
* JSON (`ensure_ascii` True/False) against `json.dumps`.
"""
import hashlib
import hmac
import json
import os
import re
import sys
import unicodedata

import pytest

from nhydb import _native as n

MAXCP = 0x110000
WORD = re.compile(r"\w")


def _chars():
    for cp in range(MAXCP):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        yield cp, chr(cp)


def test_unicode_tables_match_running_python():
    py_ver, uni_ver = n._unicode_versions()
    assert uni_ver == unicodedata.unidata_version, "Unicode version mismatch: regenerate crates/nhydb-core/src/unicode_tables.rs from the running CPython"
    table = n._unicode_prop_table()
    assert len(table) == MAXCP
    bad = []
    for cp, c in _chars():
        exp = (
            (WORD.match(c) is not None) << 0
            | c.isalpha() << 1
            | c.isalnum() << 2
            | c.isdigit() << 3
            | c.isspace() << 4
            | c.isprintable() << 5
            | c.isidentifier() << 6
            | ("a" + c).isidentifier() << 7
        )
        if table[cp] != exp:
            bad.append((hex(cp), table[cp], exp))
            if len(bad) > 20:
                break
    assert not bad, bad


def test_unicode_decimal_and_maps():
    dec = n._unicode_decimal_table()
    for cp, c in _chars():
        d = unicodedata.decimal(c, None)
        assert dec[cp] == (255 if d is None else d), hex(cp)
    chunk = "".join(c for _cp, c in _chars())
    for kind in ("casefold", "lower", "upper"):
        # per-character mapping (lower: without final-sigma context)
        exp = "".join(getattr(c, kind)() for c in chunk)
        assert n._unicode_map(kind, chunk) == exp, kind
    assert n._unicode_map("strip", "　 a\x1f ") == "　 a\x1f ".strip()


def test_aes_gcm_matches_cryptography():
    pytest.importorskip("cryptography")
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    from cryptography.exceptions import InvalidTag
    for size in (0, 1, 15, 16, 17, 1000, 70000):
        key = os.urandom(32); nonce = os.urandom(12); pt = os.urandom(size); aad = os.urandom(size % 50)
        ref = AESGCM(key).encrypt(nonce, pt, aad)
        assert n._aes_gcm_seal(key, nonce, pt, aad) == ref
        assert n._aes_gcm_open(key, nonce, ref, aad) == pt
        assert AESGCM(key).decrypt(nonce, n._aes_gcm_seal(key, nonce, pt, aad), aad) == pt
        bad = bytearray(ref); bad[-1] ^= 1
        with pytest.raises(n.CorruptionError):
            n._aes_gcm_open(key, nonce, bytes(bad), aad)
        with pytest.raises(InvalidTag):
            AESGCM(key).decrypt(nonce, bytes(bad), aad)


def test_layout_mac_matches_reference_formula():
    for _ in range(20):
        key = os.urandom(32); payload = os.urandom(200)
        ref = hmac.new(hashlib.sha256(b"NHYDB027-LAYOUT-MAC:" + key).digest(), payload, hashlib.sha256).hexdigest()
        assert n.layout_mac(key, payload) == ref


@pytest.mark.parametrize("obj", [
    {"b": "äé€😀\x7f\x00\n\"\\", "a": [1, -2, 10**100, 1.5, 1e-7, 1e22, True, None]},
    {"contract": "NHYDB_MATH_MAP_V015", "name": "ä_map", "parameters": ["ﬁx", "y"], "realm_span": 10**100},
    {"\x00NHYDB_FULLTEXT_DOCUMENT_LENGTHS": 1, "Ωmega": {"z": [], "a": {}}},
])
def test_json_dumps_matches_python(obj):
    for ascii_ in (False, True):
        ref = json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=ascii_)
        assert n._json_dumps(obj, ensure_ascii=ascii_) == ref
