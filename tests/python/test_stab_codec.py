"""Stabilisation: value codec and MATH input conversion (F023, F031, F039, F041)."""
import subprocess
import sys

import pytest

import nhydb
import nhydb.kernel as K
import nhydb.math as rm
from types import SimpleNamespace


def _db(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "c.nhydb"))
    db.create_realm(1, "main")
    return db


def test_bytes_from_huge_count_raises(tmp_path):
    # F023: must raise in-process, never abort (checked in a subprocess)
    code = (
        "import nhydb, tempfile, os\n"
        "d = tempfile.mkdtemp(dir=os.environ['T'])\n"
        "db = nhydb.NHyDB.create(os.path.join(d, 'x.nhydb')); db.create_realm(1, 'r')\n"
        "for n in (2**62, 2**40, 2**64, 2**31):\n"
        "    try:\n"
        "        db.put_cell(1, 1, 1, n, type='bytes'); print('stored', n)\n"
        "    except ValueError as e:\n"
        "        print('ValueError')\n"
        "db.put_cell(1, 1, 1, 4, type='bytes'); print(db.get_value(1, 1, 1))\n"
    )
    import os
    r = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=120,
                       env={**os.environ, "T": str(tmp_path)})
    assert r.returncode == 0, r.stderr[-2000:]
    assert r.stdout.split() == ["ValueError"] * 4 + ["b'\\x00\\x00\\x00\\x00'"]


def test_float_fields_reject_non_finite_ints(tmp_path):
    db = _db(tmp_path)
    for t in ("f64", "f32"):
        with pytest.raises(ValueError):
            db.put_cell(1, 1, 1, 2**2000, type=t)
        with pytest.raises(ValueError):
            db.put_cell(1, 1, 1, 10**400, type=t)
    with pytest.raises(ValueError):
        db.put_cell(1, 1, 1, 2**200, type="f32")
    db.put_cell(1, 1, 2, float("inf"), type="f64")  # explicit float inf stays allowed
    db.put_cell(1, 1, 3, 2**1000, type="f64")
    assert db.get_value(1, 1, 3) == float(2**1000)
    db.close()


def test_utf8_rejects_bytes(tmp_path):
    db = _db(tmp_path)
    with pytest.raises(TypeError):
        db.put_cell(1, 1, 1, b"x", type="utf8")
    db.put_cell(1, 1, 1, 5, type="utf8")
    assert db.get_value(1, 1, 1) == "5"
    db.close()


def test_unknown_dict_keys_rejected(tmp_path):
    db = _db(tmp_path)
    db.create_shape(1, 1, "row", [{"v": 1, "name": "a", "type": "u8"}])
    db.create_classic_layout(1, 1, "rec", shape_id=1)
    with pytest.raises(nhydb.SchemaError, match="unknown field 'b'"):
        db.put(1, 1, {"a": 1, "b": 2})
    db.put(1, 1, {"a": 1})
    rec = db.get(1, 1)
    db.put(1, 2, rec)  # output metadata keys (_realm, _u, ...) are ignored
    assert db.get(1, 2)["a"] == 1
    db.close()


def test_kernel_dec_value_large_pos():
    f = SimpleNamespace(v=1, name="x", type="u8", enum=None)
    for pos in (2**64 - 1, 2**63, 4):
        with pytest.raises(nhydb.CorruptionError):
            K.dec_value(f, b"abc", pos)
    assert K.dec_value(f, b"abc", 1) == (98, 2)


def test_kernel_small_cases_f056():
    assert K.align8(2**64 - 1) == 2**64
    assert K.align8(9) == 16 and K.align8(0) == 0
    h1 = K.pack_block_header(4, 1, b"x", flags=0x1_0001)
    h2 = K.pack_block_header(4, 1, b"x", flags=1)
    assert h1 == h2  # reference: int(flags) & 0xFFFF
    with pytest.raises(ValueError):  # was silently v=0
        K.enc_value(SimpleNamespace(v="abc", name="x", type="u8", enum=None), 1)
    assert nhydb.parse_n_reference("N \u0665\u0668\u0669") == 589
    assert nhydb.parse_n_reference(" N42 ") == 42
    with pytest.raises(nhydb.AddressError):
        nhydb.parse_n_reference("N4x")
    assert nhydb.normalize_lane("\x1fnr\x1f") == 1
    old = rm.get_math_max_intermediate_bits()
    for bad in (0, -1, 2**64):
        with pytest.raises(ValueError):
            rm.set_math_max_intermediate_bits(bad)
    assert rm.get_math_max_intermediate_bits() == old


def test_math_inputs_exact_python_int(tmp_path):
    db = _db(tmp_path)
    info = db.create_math_map_layout("m", ("x",), u="x", V="1", payload_type="utf8")
    lid = info.layout_id
    db.put_math(lid, (1e39,), "a")
    db.put_math(lid, (2e39,), "b")  # distinct cell (was saturated to i128::MAX)
    assert db.get_math(lid, (int(1e39),)) == "a"
    assert db.get_math(lid, (int(2e39),)) == "b"
    db.put_math(lid, (" +1_0 ",), "s")
    assert db.get_math(lid, (10,)) == "s"
    for bad in ("1__0", "_5", "5_", "1.5"):
        with pytest.raises(ValueError):
            db.put_math(lid, (bad,), "x")
    with pytest.raises(ValueError):
        db.put_math(lid, (float("nan"),), "x")
    with pytest.raises(ValueError):
        db.put_math(lid, (float("inf"),), "x")
    db.close()


def _vu(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def _rd_vu(b, p):
    n = s = 0
    while True:
        x = b[p]
        p += 1
        n |= (x & 0x7F) << s
        s += 7
        if not x & 0x80:
            return n, p


@pytest.mark.parametrize("groups", [2**32, 2**48, 2**61])
def test_sparse_group_count_manipulated_f022(tmp_path, groups):
    """CRC-consistent SPARSE block with a huge group_count → CorruptionError, no panic."""
    import os
    import struct
    import zlib

    p = tmp_path / "s.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(2, "sp")
    db.commit()
    db.put_points(2, [(1, 1, "a", "utf8"), (2, 1, "b", "utf8")], commit=True)
    db.close(commit=False)
    raw = bytearray(p.read_bytes())
    off, last = 8192, None
    while off < len(raw):
        plen = struct.unpack_from("<Q", raw, off + 16)[0]
        last = (off, raw[off + 5], plen)
        off = (off + 32 + plen + 7) & ~7
    boff, bt, plen = last
    assert bt == 10
    pl = bytes(raw[boff + 32:boff + 32 + plen])
    q = 4
    realm, q = _rd_vu(pl, q)
    u0, q = _rd_vu(pl, q)
    u1, q = _rd_vu(pl, q)
    _g, q = _rd_vu(pl, q)
    c, q2 = _rd_vu(pl, q)
    newpl = pl[:4] + _vu(realm) + _vu(u0) + _vu(u1) + _vu(groups) + _vu(c) + pl[q2:]
    hdr = bytearray(raw[boff:boff + 32])
    struct.pack_into("<QI", hdr, 16, len(newpl), zlib.crc32(newpl) & 0xFFFFFFFF)
    hdr[28:32] = b"\0" * 4
    struct.pack_into("<I", hdr, 28, zlib.crc32(bytes(hdr)) & 0xFFFFFFFF)
    end = (boff + 32 + len(newpl) + 7) & ~7
    new = bytearray(raw[:boff]) + hdr + newpl
    new += b"\0" * (end - len(new))
    for s in range(2):
        sb = bytearray(new[s * 4096:(s + 1) * 4096])
        struct.pack_into("<Q", sb, 32, end)
        struct.pack_into("<I", sb, 4092, zlib.crc32(bytes(sb[:4092])) & 0xFFFFFFFF)
        new[s * 4096:(s + 1) * 4096] = sb
    p.write_bytes(new)
    code = (
        "import nhydb, sys\n"
        "try:\n"
        "    db = nhydb.NHyDB(sys.argv[1]); db.get_cell(2, 1, 1); print('no error')\n"
        "except nhydb.NHyDBError as e:\n"
        "    print(type(e).__name__)\n"
    )
    r = subprocess.run([sys.executable, "-c", code, str(p)], capture_output=True, text=True, timeout=120,
                       env=dict(os.environ))
    assert r.returncode == 0, r.stderr[-1500:]
    assert r.stdout.strip() == "CorruptionError", r.stdout


def test_negative_coordinates_single_vs_numpy_f056(tmp_path):
    """F056 (deliberate, v0.14 behaviour): single-value reads treat a negative
    realm/u/v like a missing address and return None, as the reference does;
    writes and N computation reject it. The NumPy batch paths validate the
    whole input array and raise ValueError for negative coordinates."""
    import numpy as np
    db = nhydb.NHyDB.create(str(tmp_path / "neg.nhydb"))
    db.create_realm(1, "r")
    db.create_shape(1, 1, "s", [{"v": 1, "name": "a", "type": "varuint"}])
    db.create_classic_layout(1, 1, "c", shape_id=1)
    db.put_dense_batch(1, 1, [[7]], shape_id=1, commit=True)
    assert db.get(-1, 1) is None and db.get(1, -1) is None
    assert db.get_value(1, 1, -1) is None and db.get_cell(-1, 1, 1) is None
    with pytest.raises(nhydb.SchemaError):
        db.put_cell(-1, 1, 2, 5)
    with pytest.raises(nhydb.RealmLimitError):
        db.put_cell(1, 1, -2, 5)
    with pytest.raises(ValueError):
        nhydb.dynadron_n(-1, 1, 1, 0)
    with pytest.raises(ValueError):
        db.read_numpy(1, -1, 1)
    with pytest.raises(ValueError, match="negative"):
        db.put_cells_numpy(1, np.array([-1]), 1, np.array([5], dtype=np.uint64))
    assert db.get(1, 1)["a"] == 7 and db.verify(deep=True)["ok"]
    db.close()
