"""Helper of test_regress_0282.py: reads, re-encodes and appends CRC-valid
block directory blocks (pages DP17, runs DR17, root DT17; docs/FORMAT.md).
Origin: reviewer tool of the 0.28.2 review (2026-10-27), used to forge
directory states that only a manipulation can produce.
"""
import json
import struct
import zlib

SB = 4096


def vu_enc(n):
    out = bytearray()
    while n >= 0x80:
        out.append((n & 0x7F) | 0x80)
        n >>= 7
    out.append(n)
    return bytes(out)


def vu_dec(b, p):
    v, s = 0, 0
    while True:
        x = b[p]
        v |= (x & 0x7F) << s
        p += 1
        s += 7
        if not x & 0x80:
            return v, p


def zz(d):
    return ((d << 1) ^ (d >> 63)) & 0xFFFFFFFFFFFFFFFF


def unzz(n):
    return (n >> 1) ^ -(n & 1)


def crc(b):
    return zlib.crc32(bytes(b)) & 0xFFFFFFFF


class DB:
    def __init__(self, path):
        self.path = path
        self.raw = bytearray(open(path, "rb").read())
        best = None
        for slot in (0, 1):
            s = self.raw[slot * SB:(slot + 1) * SB]
            if crc(s[:SB - 4]) != struct.unpack_from("<I", s, SB - 4)[0]:
                continue
            gen = struct.unpack_from("<Q", s, 16)[0]
            if best is None or gen > best[0]:
                best = (gen, slot)
        self.slot = best[1]
        self.sup = self.super_fields()

    # superblock: 16 gen, 24 ckpt, 32 last_valid_end, 40 next_bid, 48 created, 56 updated, 64 flags(u32), 68 dir_root
    def super_fields(self):
        o = self.slot * SB
        q = lambda i: struct.unpack_from("<Q", self.raw, o + 16 + 8 * i)[0]
        return {"gen": q(0), "ckpt": q(1), "end": q(2), "next_bid": q(3),
                "dir_root": struct.unpack_from("<Q", self.raw, o + 68)[0]}

    def write_super(self, **kw):
        o = self.slot * SB
        f = dict(self.sup, **kw)
        struct.pack_into("<Q", self.raw, o + 16, f["gen"])
        struct.pack_into("<Q", self.raw, o + 32, f["end"])
        struct.pack_into("<Q", self.raw, o + 40, f["next_bid"])
        struct.pack_into("<Q", self.raw, o + 68, f["dir_root"])
        struct.pack_into("<I", self.raw, o + SB - 4, crc(self.raw[o:o + SB - 4]))
        # other slot: make it invalid-older by leaving it; ensure ours wins
        self.sup = f

    def header(self, off):
        magic, ver, bt, fl, bid, plen, pc, hc = struct.unpack_from("<4sBBHQQII", self.raw, off)
        return {"bt": bt, "flags": fl, "bid": bid, "plen": plen}

    def payload(self, off):
        h = self.header(off)
        return bytes(self.raw[off + 32:off + 32 + h["plen"]])

    def append(self, payload, bt=14, flags=0, bid=None):
        """Appends a block at the commit end (file cut there), returns offset."""
        end = self.sup["end"]
        del self.raw[end:]
        if bid is None:
            bid = self.sup["next_bid"]
            self.sup["next_bid"] += 1
        h = bytearray(struct.pack("<4sBBHQQII", b"NHBL", 16, bt, flags, bid, len(payload), crc(payload), 0))
        struct.pack_into("<I", h, 28, crc(h))
        blk = bytes(h) + payload
        blk += b"\0" * ((-len(blk)) % 8)
        off = len(self.raw)
        self.raw += blk
        self.sup["end"] = len(self.raw)
        return off

    def commit(self, root_off):
        self.write_super(gen=self.sup["gen"] + 1, dir_root=root_off)

    def save(self, path):
        open(path, "wb").write(bytes(self.raw))

    # ---------- directory decoding ----------
    def root(self, off=None):
        off = off if off is not None else self.sup["dir_root"]
        pl = self.payload(off)
        assert pl[:4] == b"DT17"
        n, p = vu_dec(pl, 4)
        return json.loads(pl[p:p + n].decode())

    def run(self, off):
        b = self.payload(off)
        assert b[:4] == b"DR17", b[:4]
        p = 4
        level, p = vu_dec(b, p)
        nparts, p = vu_dec(b, p)
        parts = []
        for _ in range(nparts):
            kind = b[p]; p += 1
            realm, p = vu_dec(b, p)
            lane = b[p]; p += 1
            npages, p = vu_dec(b, p)
            pages = []
            prev_first = 0
            for _ in range(npages):
                o8, p = vu_dec(b, p)
                n, p = vu_dec(b, p)
                df, p = vu_dec(b, p)
                first = prev_first + df
                span, p = vu_dec(b, p)
                mb, p = vu_dec(b, p)
                pages.append({"off": o8 * 8, "n": n, "first": first, "max_hi": first + span, "max_bid": mb})
                prev_first = first
            parts.append({"kind": kind, "realm": realm, "lane": lane, "pages": pages})
        return {"level": level, "parts": parts}

    def page(self, off):
        b = self.payload(off)
        assert b[:4] == b"DP17"
        kind = b[4]
        p = 5
        realm, p = vu_dec(b, p)
        lane = b[p]; p += 1
        n, p = vu_dec(b, p)
        ents = []
        prev = None
        for _ in range(n):
            du, p = vu_dec(b, p)
            dbid, p = vu_dec(b, p)
            e = {"u": (prev["u"] if prev else 0) + du, "bid": (prev["bid"] if prev else 0) + unzz(dbid)}
            if kind == 1:
                e["count"], p = vu_dec(b, p)
                e["shape"], p = vu_dec(b, p)
                d, p = vu_dec(b, p)
                e["off"] = ((prev["off"] // 8 if prev else 0) + unzz(d)) * 8
                e["plen"], p = vu_dec(b, p)
                e["prefix"], p = vu_dec(b, p)
            elif kind == 2:
                e["span"], p = vu_dec(b, p)
                d, p = vu_dec(b, p)
                e["off"] = ((prev["off"] // 8 if prev else 0) + unzz(d)) * 8
                e["plen"], p = vu_dec(b, p)
                e["prefix"], p = vu_dec(b, p)
                e["groups"], p = vu_dec(b, p)
                e["cells"], p = vu_dec(b, p)
            ents.append(e)
            prev = e
        assert p == len(b)
        return {"kind": kind, "realm": realm, "lane": lane, "entries": ents}


def enc_page(kind, realm, lane, ents, raw_n=None):
    out = bytearray(b"DP17") + bytes([kind]) + vu_enc(realm) + bytes([lane]) + vu_enc(len(ents) if raw_n is None else raw_n)
    prev = None
    for e in ents:
        out += vu_enc(e["u"] - (prev["u"] if prev else 0))
        out += vu_enc(zz(e["bid"] - (prev["bid"] if prev else 0)))
        if kind == 1:
            out += vu_enc(e["count"]) + vu_enc(e["shape"])
            out += vu_enc(zz(e["off"] // 8 - (prev["off"] // 8 if prev else 0)))
            out += vu_enc(e["plen"]) + vu_enc(e["prefix"])
        elif kind == 2:
            out += vu_enc(e["span"])
            out += vu_enc(zz(e["off"] // 8 - (prev["off"] // 8 if prev else 0)))
            out += vu_enc(e["plen"]) + vu_enc(e["prefix"]) + vu_enc(e["groups"]) + vu_enc(e["cells"])
        prev = e
    return bytes(out)


def page_summary(kind, ents):
    hi = [e["u"] + (e["count"] - 1 if kind == 1 and e["count"] else e.get("span", 0)) for e in ents]
    return {"n": len(ents), "first": ents[0]["u"], "max_hi": max(hi), "max_bid": max(e["bid"] for e in ents)}


def enc_run(level, parts, raw_npages=None):
    out = bytearray(b"DR17") + vu_enc(level) + vu_enc(len(parts))
    for part in parts:
        out += bytes([part["kind"]]) + vu_enc(part["realm"]) + bytes([part["lane"]])
        out += vu_enc(len(part["pages"]) if raw_npages is None else raw_npages)
        prev_first = 0
        for pg in part["pages"]:
            out += vu_enc(pg["off"] // 8) + vu_enc(pg["n"]) + vu_enc(pg["first"] - prev_first)
            out += vu_enc(pg["max_hi"] - pg["first"]) + vu_enc(pg["max_bid"])
            prev_first = pg["first"]
    return bytes(out)


def enc_root(obj):
    j = json.dumps(obj, sort_keys=True, separators=(",", ":")).encode()
    return b"DT17" + vu_enc(len(j)) + j
