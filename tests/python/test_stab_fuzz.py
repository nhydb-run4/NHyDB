"""Fuzz harness for the file decoders (T003).

Base: the checked-in golden database (all block types of the file, see
test_stab_format_golden.py). Mutations:

* ``flip``      byte flips anywhere in the file (superblocks, headers, payloads)
* ``trunc``     truncation at arbitrary lengths
* ``crc``       payload byte mutations with corrected payload and header CRC
                (undetectable for the CRC layer, exercises the decoders)
* ``varint``    LEB128 varuint / fixed-width fields overwritten with 2^32,
                2^48, 2^63, 2^64-1 inside payloads (CRCs corrected)
* ``header``    block header fields (type, flags, block id, payload length)
                with corrected header CRC
* ``super``     superblock fields with corrected superblock CRC

Each mutated file is opened (normally and read-only), read in many ways,
verified and inspected/repaired (dry run). Allowed outcomes: success or an
``NHyDBError`` subclass. Everything else (``PanicException``,
``IndexError``, ``MemoryError``, ``OverflowError`` …, a crash of the
process, a hang, excessive memory) is a finding. Cases run in a worker sub
process so that a crash cannot take down the test session; the parent
restarts the worker after the crashing case.

Scale: ``NHYDB_FUZZ_SCALE`` (default 2 ≈ 4 000 cases, ~20 s in a debug
build; e.g. 50 for a long local run). Seeds are fixed; ``NHYDB_FUZZ_SEED`` changes them.
"""
import json
import os
import random
import shutil
import struct
import subprocess
import sys
import threading
import time
import zlib
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
GOLDEN_DB = HERE / "golden" / "golden.nhydb"
SCALE = float(os.environ.get("NHYDB_FUZZ_SCALE", "2"))
SEED = int(os.environ.get("NHYDB_FUZZ_SEED", "20261020"))
MAX_RSS_MB = int(os.environ.get("NHYDB_FUZZ_MAX_RSS_MB", "1536"))
CASE_TIMEOUT = float(os.environ.get("NHYDB_FUZZ_CASE_TIMEOUT", "60"))
DATA_START = 8192
SB = 4096
BIGS = [2**32, 2**48, 2**63, 2**64 - 1]

WORKER = r'''
import json, os, struct, sys, warnings, zlib
from pathlib import Path
if sys.platform.startswith("linux"):
    import resource
    lim = int(os.environ.get("NHYDB_FUZZ_MAX_RSS_MB", "1536")) * 2 * 2**20
    resource.setrlimit(resource.RLIMIT_AS, (lim, lim))
import nhydb
warnings.simplefilter("ignore")
BIG = 10**100
HUGE_U = 2**70 + 3
work = Path(sys.argv[1]); target = work / "golden.nhydb"
orig = (work / "orig.bin").read_bytes()

def hdr_crc(h):
    t = bytearray(h); t[28:32] = b"\0\0\0\0"; return zlib.crc32(bytes(t)) & 0xffffffff

def fix_block(b, boff):
    plen = struct.unpack_from("<Q", b, boff + 16)[0]
    ps = boff + 32
    if ps + plen <= len(b):
        struct.pack_into("<I", b, boff + 24, zlib.crc32(bytes(b[ps:ps + plen])) & 0xffffffff)
    struct.pack_into("<I", b, boff + 28, hdr_crc(b[boff:boff + 32]))

def leb(n):
    out = bytearray()
    while n >= 0x80:
        out.append((n & 0x7F) | 0x80); n >>= 7
    out.append(n); return bytes(out)

def mutate(c):
    b = bytearray(orig); k = c["kind"]
    if k == "none":
        pass
    elif k == "selftest-abort":
        os.abort()
    elif k == "selftest-error":
        raise IndexError("selftest")
    elif k == "flip":
        b[c["pos"]] ^= c["mask"]
    elif k == "trunc":
        del b[c["len"]:]
    elif k == "crc":
        b[c["pos"]] = c["byte"]; fix_block(b, c["block"])
    elif k == "varint":
        enc = leb(c["value"]) if c["enc"] == "leb" else struct.pack(c["enc"], c["value"] & ((1 << (8 * struct.calcsize(c["enc"]))) - 1))
        end = min(c["pos"] + len(enc), c["block"] + 32 + c["plen"])
        b[c["pos"]:end] = enc[:end - c["pos"]]
        fix_block(b, c["block"])
    elif k == "header":
        fmt = {"type": "<B", "flags": "<H", "block_id": "<Q", "plen": "<Q", "version": "<B"}[c["field"]]
        off = {"type": 5, "flags": 6, "block_id": 8, "plen": 16, "version": 4}[c["field"]]
        struct.pack_into(fmt, b, c["block"] + off, c["value"])
        struct.pack_into("<I", b, c["block"] + 28, hdr_crc(b[c["block"]:c["block"] + 32]))
    elif k == "super":
        lo = c["slot"] * 4096
        struct.pack_into("<Q", b, lo + c["off"], c["value"])
        struct.pack_into("<I", b, lo + 4092, zlib.crc32(bytes(b[lo:lo + 4092])) & 0xffffffff)
    target.write_bytes(bytes(b))

STRICT = False
CALLS = [0]

def guarded(fn):
    try:
        r = fn()
        if hasattr(r, "__next__"):
            for _ in r: pass
        CALLS[0] += 1
    except nhydb.NHyDBError:
        if STRICT:
            raise

def reads(db):
    for f in (lambda: db.verify(deep=True), lambda: db.health(deep=True), db.stats, lambda: db.index_defs):
        guarded(f)
    for u in (1, 2, 3, 5, 7, 12, 24, 30, 31, 40):
        guarded(lambda: db.get(1, u))
    guarded(lambda: db.get_cell(1, 3, 50))
    for u, v in ((1, 1), (2, 2), (3, 9), (4, 9), (5, 1), (HUGE_U, 2), (6, BIG), (8, 1)):
        guarded(lambda: db.get_cell(2, u, v))
    guarded(lambda: db.get_cell(2, 7, 2, lane="NR"))
    for lid, i in ((2, 1), (2, 5), (3, 1), (3, 5)):
        guarded(lambda: db.get_record(lid, i))
    for x in (1, 2, 3):
        guarded(lambda: db.get_math(7, (x, 1)))
        guarded(lambda: db.get_math(7, (x, BIG + 1)))
    guarded(lambda: db.find_u(1, {"status": "active"}))
    guarded(lambda: db.find_u(1, {"amount": 2**62}))
    guarded(lambda: db.sql("SELECT status, COUNT(*) AS n FROM records GROUP BY status"))
    guarded(lambda: db.sql("SELECT id FROM records WHERE FULLTEXT(body, :q)", {"q": "gamma"}))
    guarded(lambda: db.sql("SELECT id FROM records WHERE amount BETWEEN -5000 AND 5000 ORDER BY id"))
    guarded(lambda: list(db.range(1, 1, 50)))
    guarded(lambda: list(db.iter_materialized_sparse_points(2)))
    # a CRC-valid mutation may yield a legal varint beyond int64: documented
    # ValueError of the default dtype path, so read with the object fallback
    guarded(lambda: db.read_numpy(1, 1, 40, object_fallback=True))

def exercise():
    guarded(lambda: nhydb.NHyDB.inspect_tail(target))
    guarded(lambda: nhydb.NHyDB.repair_file(target))
    for kw in ({"readonly": True}, {}):
        try:
            db = nhydb.NHyDB(target, **kw)
        except nhydb.NHyDBError:
            continue
        try:
            reads(db)
        finally:
            try:
                db.close(commit=False)
            except nhydb.NHyDBError:
                pass

for line in sys.stdin:
    c = json.loads(line)
    print("CASE", c["id"], flush=True)
    STRICT = c["kind"] == "none"
    CALLS[0] = 0
    try:
        mutate(c)
        exercise(); r = "ok" if not STRICT else f"ok {CALLS[0]}"
    except BaseException as e:
        r = "BAD " + type(e).__module__ + "." + type(e).__name__ + ": " + str(e)[:300].replace("\n", " ")
    print("RES", c["id"], r, flush=True)
'''


def _blocks(raw):
    out, off = [], DATA_START
    while off + 32 <= len(raw):
        magic, _v, bt, _f, bid, plen, _pc, _hc = struct.unpack_from("<4sBBHQQII", raw, off)
        if magic != b"NHBL":
            break
        out.append((off, bt, plen))
        off += (32 + plen + 7) & ~7
    return out


def _n(x):
    return max(1, int(round(x * SCALE)))


def make_cases(raw):
    rnd = random.Random(SEED)
    blocks = _blocks(raw)
    cases = []
    used = set(range(0, 68)) | set(range(SB, SB + 68)) | set(range(4092, 4096)) | set(range(SB + 4092, SB + 4096))
    # flips: superblock structures completely, the data area sampled
    for pos in sorted(used):
        cases.append({"kind": "flip", "pos": pos, "mask": 0xFF})
    for pos in rnd.sample(range(DATA_START, len(raw)), min(len(raw) - DATA_START, _n(350))):
        cases.append({"kind": "flip", "pos": pos, "mask": 1 << rnd.randrange(8)})
    # truncation: every block boundary and inside blocks
    lens = {off for off, _bt, _p in blocks} | {off + 1 for off, _bt, _p in blocks} | {off + 40 for off, _bt, _p in blocks}
    lens |= set(rnd.sample(range(0, len(raw)), _n(60)))
    for ln in sorted(lens):
        cases.append({"kind": "trunc", "len": ln})
    # CRC-corrected payload mutations, spread over all blocks
    per_block = max(1, _n(450) // len(blocks))
    for off, _bt, plen in blocks:
        if plen == 0:
            continue
        for pos in rnd.sample(range(plen), min(plen, per_block)):
            cases.append({"kind": "crc", "block": off, "pos": off + 32 + pos,
                          "byte": rnd.choice([0x00, 0xFF, 0x80, 0x7F, rnd.randrange(256)])})
    # varint / fixed-width fields with huge values (CRC corrected)
    per_block = max(1, _n(60) // len(blocks))
    for off, _bt, plen in blocks:
        if plen == 0:
            continue
        positions = sorted({0, 4, 5, 8} & set(range(plen)) | set(rnd.sample(range(plen), min(plen, per_block))))
        for pos in positions:
            for value in BIGS:
                enc = rnd.choice(["leb", "leb", "<I", "<Q"])
                cases.append({"kind": "varint", "block": off, "plen": plen, "pos": off + 32 + pos,
                              "value": value, "enc": enc})
    # header fields (header CRC corrected)
    for off, bt, plen in blocks:
        for value in (0, 1, plen + 8, 2**32, 2**48, 2**63, 2**64 - 1):
            cases.append({"kind": "header", "block": off, "field": "plen", "value": value})
        for value in rnd.sample(range(256), _n(4)) + [0, 4, 5, 6, 7, 10, 12, 13]:
            cases.append({"kind": "header", "block": off, "field": "type", "value": value})
        cases.append({"kind": "header", "block": off, "field": "flags", "value": 0xFFFF})
        cases.append({"kind": "header", "block": off, "field": "block_id", "value": rnd.choice(BIGS[:-1] + [0])})
        cases.append({"kind": "header", "block": off, "field": "version", "value": 15})
    # superblock fields (superblock CRC corrected), both slots
    for slot in (0, 1):
        for fo in range(16, 64, 8):
            for value in [0, 8191, len(raw) + 8, *BIGS]:
                cases.append({"kind": "super", "slot": slot, "off": fo, "value": value})
    for i, c in enumerate(cases):
        c["id"] = i
    return cases


def _rss_mb(pid):
    try:
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True, timeout=5).stdout
        return int(out.strip() or 0) / 1024
    except (OSError, ValueError, subprocess.SubprocessError):
        return 0


def run_cases(work, cases):
    """Runs the cases in worker processes; returns {case id: finding}."""
    findings, results = {}, {}
    pending = list(cases)
    while pending:
        proc = subprocess.Popen([sys.executable, "-c", WORKER, str(work)], stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                env={**os.environ, "NHYDB_FUZZ_MAX_RSS_MB": str(MAX_RSS_MB)})
        killed = {}

        def feed():
            try:
                for c in pending:
                    proc.stdin.write(json.dumps(c) + "\n")
                proc.stdin.close()
            except (BrokenPipeError, OSError, ValueError):
                pass

        def watchdog():
            while proc.poll() is None:
                if os.name == "posix" and _rss_mb(proc.pid) > MAX_RSS_MB:
                    killed["why"] = f"memory > {MAX_RSS_MB} MB"
                    proc.kill()
                    return
                if time.monotonic() - state["since"] > CASE_TIMEOUT:
                    killed["why"] = f"hang > {CASE_TIMEOUT} s"
                    proc.kill()
                    return
                time.sleep(0.2)

        state = {"since": time.monotonic(), "current": None}
        threading.Thread(target=feed, daemon=True).start()
        threading.Thread(target=watchdog, daemon=True).start()
        done = set()
        for line in proc.stdout:
            parts = line.rstrip("\n").split(" ", 2)
            if parts[0] == "CASE":
                state["current"] = int(parts[1])
                state["since"] = time.monotonic()
            elif parts[0] == "RES":
                cid = int(parts[1])
                done.add(cid)
                results[cid] = parts[2]
                if parts[2] != "ok" and not parts[2].startswith("ok "):
                    findings[cid] = parts[2]
        proc.wait()
        err = proc.stderr.read()
        cur = state["current"]
        if cur is not None and cur not in done:
            findings[cur] = f"CRASH rc={proc.returncode} {killed.get('why', '')} {err[-300:]}".strip()
            done.add(cur)
        elif proc.returncode != 0 and not done:
            raise RuntimeError(f"fuzz worker failed to start: {err[-2000:]}")
        pending = [c for c in pending if c["id"] not in done]
    return findings, results


@pytest.fixture(scope="module")
def fuzz_base(tmp_path_factory):
    work = tmp_path_factory.mktemp("fuzz")
    for s in ("", ".layout.enc", ".layout.key"):
        shutil.copyfile(str(GOLDEN_DB) + s, str(work / GOLDEN_DB.name) + s)
    if os.name == "posix":
        os.chmod(str(work / GOLDEN_DB.name) + ".layout.key", 0o600)
    raw = GOLDEN_DB.read_bytes()
    (work / "orig.bin").write_bytes(raw)
    return work, raw


N_PROBES = 86  # number of successful probes on the intact file (both opens)
KINDS = ["flip", "trunc", "crc", "varint", "header", "super"]

# Known product defects found by this harness (reported to the coordinator);
# kind -> reason. Remove an entry once the defect is fixed (strict xfail).
KNOWN = {}


@pytest.mark.parametrize("kind", KINDS)
def test_fuzz_only_nhydb_errors(fuzz_base, kind):
    work, raw = fuzz_base
    cases = [c for c in make_cases(raw) if c["kind"] == kind]
    assert cases
    findings, _results = run_cases(work, cases)
    if kind in KNOWN:
        pytest.xfail(KNOWN[kind]) if findings else None
    by_case = {cid: (next(c for c in cases if c["id"] == cid), msg) for cid, msg in sorted(findings.items())}
    report = "\n".join(f"{json.dumps(c)} -> {msg}" for c, msg in list(by_case.values())[:25])
    assert not findings, f"{len(findings)} of {len(cases)} {kind} cases raised non-NHyDBError / crashed:\n{report}"


def test_fuzz_control_unmodified_file_reads_everything(fuzz_base):
    """Sanity check of the harness: on the intact file every probe succeeds."""
    work, _raw = fuzz_base
    findings, results = run_cases(work, [{"kind": "none", "id": 0}])
    assert not findings and results == {0: f"ok {N_PROBES}"}, results


def test_harness_detects_crash_and_foreign_exceptions(fuzz_base):
    work, _raw = fuzz_base
    cases = [{"kind": "none", "id": 0}, {"kind": "selftest-abort", "id": 1}, {"kind": "selftest-error", "id": 2},
             {"kind": "none", "id": 3}]
    findings, results = run_cases(work, cases)
    assert findings[1].startswith("CRASH"), findings
    assert findings[2].startswith("BAD builtins.IndexError"), findings
    assert results[0] == results[3] == f"ok {N_PROBES}", results  # worker restarted after the crash


def test_case_generation_is_deterministic():
    raw = GOLDEN_DB.read_bytes()
    assert make_cases(raw) == make_cases(raw)
    kinds = {c["kind"] for c in make_cases(raw)}
    assert kinds == set(KINDS)
