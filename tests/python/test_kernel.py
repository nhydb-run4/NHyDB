"""Conformance of the stateless kernel building blocks against CPython.

* Unicode properties and mappings: the engine has fixed Unicode 16.0.0
  tables (generated from CPython 3.14), independent of the running Python.
  Fixed Unicode 16.0.0 reference values are checked on every Python version;
  the exhaustive comparison over all 0x110000 code points against the
  running CPython's ``unicodedata`` only runs where that is Unicode 16.0.0
  as well (CPython 3.14).
* AES-256-GCM against the real `cryptography` library, HMAC/SHA-256
  against `hmac`/`hashlib`, `layout_mac` against its defining formula.
* JSON (`ensure_ascii` True/False) against `json.dumps`.
"""
import hashlib
import hmac
import json
import os
import re
import unicodedata

import pytest

from nhydb import _native as n

MAXCP = 0x110000
WORD = re.compile(r"\w")
UNICODE_VERSION = "16.0.0"
same_unicode = pytest.mark.skipif(
    unicodedata.unidata_version != UNICODE_VERSION,
    reason=f"running CPython has Unicode {unicodedata.unidata_version}; the engine tables are "
    f"Unicode {UNICODE_VERSION}, compared exhaustively against CPython 3.14",
)

# Unicode 16.0.0 reference values (independent of the running Python):
# code point -> (property bits as in test_unicode_tables_match_running_python, decimal value or 255)
PROPS_16 = {
    0x30: (173, 0),        # DIGIT ZERO
    0x41: (231, 255),      # LATIN CAPITAL LETTER A
    0x5F: (225, 255),      # LOW LINE
    0x20: (48, 255),       # SPACE
    0x1F: (16, 255),       # control (whitespace)
    0xA0: (16, 255),       # NO-BREAK SPACE: space, not printable
    0xB2: (45, 255),       # SUPERSCRIPT TWO: digit, but no decimal value
    0x300: (160, 255),     # COMBINING GRAVE ACCENT: identifier continue only
    0x378: (0, 255),       # unassigned
    0x663: (173, 3),       # ARABIC-INDIC DIGIT THREE
    0x2163: (229, 255),    # ROMAN NUMERAL FOUR: numeric, not decimal
    0x3000: (16, 255),     # IDEOGRAPHIC SPACE
    0xE000: (0, 255),      # private use
    0xFF10: (173, 0),      # FULLWIDTH DIGIT ZERO
    0x1E4F0: (173, 0),     # NAG MUNDARI DIGIT ZERO (Unicode 15.0)
    0x10D40: (173, 0),     # GARAY DIGIT ZERO (new in Unicode 16.0)
    0x10D49: (173, 9),     # GARAY DIGIT NINE (new in Unicode 16.0)
    0x10D50: (231, 255),   # GARAY CAPITAL LETTER A (new in Unicode 16.0)
    0x1F600: (32, 255),    # GRINNING FACE: printable symbol only
    0xFFFF: (0, 255),      # noncharacter
    0x10FFFF: (0, 255),    # last code point
}
# (character, casefold, lower, upper) per character in Unicode 16.0.0
MAPS_16 = [
    ('\xdf', 'ss', '\xdf', 'SS'),
    ('\u1e9e', 'ss', '\xdf', '\u1e9e'),
    ('\ufb01', 'fi', '\ufb01', 'FI'),
    ('\u0130', 'i\u0307', 'i\u0307', '\u0130'),
    ('\u0149', '\u02bcn', '\u0149', '\u02bcN'),
    ('\u03a3', '\u03c3', '\u03c3', '\u03a3'),
    ('\u01c5', '\u01c6', '\u01c6', '\u01c4'),
    ('\u0390', '\u03b9\u0308\u0301', '\u0390', '\u0399\u0308\u0301'),
    ('\U00010d50', '\U00010d70', '\U00010d70', '\U00010d50'),  # Garay, new in Unicode 16.0
    ('\U00010d70', '\U00010d70', '\U00010d70', '\U00010d50'),
]


def _chars():
    for cp in range(MAXCP):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        yield cp, chr(cp)


def test_unicode_16_reference_values():
    """Runs on every Python version: the engine reports and implements Unicode 16.0.0."""
    py_ver, uni_ver = n._unicode_versions()
    assert uni_ver == UNICODE_VERSION and py_ver.startswith("3.14.")
    table = n._unicode_prop_table()
    dec = n._unicode_decimal_table()
    assert len(table) == MAXCP and len(dec) == MAXCP
    for cp, (props, decimal) in PROPS_16.items():
        assert (table[cp], dec[cp]) == (props, decimal), hex(cp)
    for c, fold, low, up in MAPS_16:
        assert n._unicode_map("casefold", c) == fold, ascii(c)
        assert n._unicode_map("lower", c) == low, ascii(c)
        assert n._unicode_map("upper", c) == up, ascii(c)
    # whole strings are mapped character by character, lower without final-sigma context
    assert n._unicode_map("casefold", "Stra\xdfe \u03a3\u0391\u03a3") == "strasse \u03c3\u03b1\u03c3"
    assert n._unicode_map("lower", "\u039f\u03a3") == "\u03bf\u03c3"
    assert n._unicode_map("upper", "") == ""
    assert n._unicode_map("strip", "\u3000 a\x1f ") == "a"
    assert n._unicode_map("strip", "\xa0\u2028x\x85") == "x"


@same_unicode
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


@same_unicode
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
