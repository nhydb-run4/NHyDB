"""KQ data set for the NHyDB KQ-10M benchmark.

Every record N = 1 ... 10,000,000 has 11 integer fields that are computed
arithmetically from N:

    N, tp, tq, gp, sp, lage, dyn_u, dyn_v, stufe, station, stufenstation

``lage`` is an enum with four values (N0, NL, Gz, NR). ``kq_columns`` computes
the fields vectorised with NumPy (exact integer square root), ``kq_row`` is the
scalar reference implementation used for spot checks.

Run as a script, it writes the test data to a directory before the benchmark
starts (one .npy file per field), so that generating data never runs inside a
measured section:

    python kq.py --write-data DIR [--records N]
"""
import argparse
import json
import math
import sys
import time
from pathlib import Path

import numpy as np

NAMES = ["N", "tp", "tq", "gp", "sp", "lage", "dyn_u", "dyn_v", "stufe", "station", "stufenstation"]
LAGE_LABELS = ["N0", "NL", "Gz", "NR"]
LAGE_ENUM = {"N0": 0, "NL": 1, "Gz": 2, "NR": 3}

# NHyDB field types of the KQ shape
TYPES = {nm: ("u8" if nm == "lage" else ("varint" if nm == "station" else "varuint")) for nm in NAMES}

CHUNK = 1_000_000
SPOT_CHECKS = 2000
SPOT_SEED = 20261112


def kq_columns(n0: int, count: int) -> dict:
    """KQ columns for N = n0 ... n0+count-1 (dict field name -> ndarray)."""
    n = np.arange(n0, n0 + count, dtype=np.int64)
    x = 4 * n + 1
    r = np.floor(np.sqrt(x.astype(np.float64))).astype(np.int64)
    r -= (r * r > x)
    r += ((r + 1) * (r + 1) <= x)
    tq = (r + 1) // 2
    tp = tq - 1
    gp = n - tq * tp
    sp = np.abs(gp - tq)
    lage = np.where(gp == 0, 0, np.where(gp < tq, 1, np.where(gp == tq, 2, 3))).astype(np.uint8)
    g2 = tq * tq
    dyn_v = np.where(n <= g2, n - tq * (tq - 1), n - g2)
    dyn_u = tq - dyn_v
    station = dyn_v - 1
    width = dyn_u + 2
    return {"N": n, "tp": tp, "tq": tq, "gp": gp, "sp": sp, "lage": lage, "dyn_u": dyn_u, "dyn_v": dyn_v,
            "stufe": station // width + 1, "station": station, "stufenstation": station % width}


def kq_row(n: int) -> tuple:
    """Scalar reference implementation (lage as label)."""
    tq = (math.isqrt(4 * n + 1) + 1) // 2
    tp = tq - 1
    gp = n - tq * tp
    sp = abs(gp - tq)
    if gp == 0:
        lage = "N0"
    elif gp < tq:
        lage = "NL"
    elif gp == tq:
        lage = "Gz"
    else:
        lage = "NR"
    g2 = tq * tq
    dyn_v = n - tq * (tq - 1) if n <= g2 else n - g2
    dyn_u = tq - dyn_v
    station = dyn_v - 1
    width = dyn_u + 2
    return (n, tp, tq, gp, sp, lage, dyn_u, dyn_v, station // width + 1, station, station % width)


def write_data(out: Path, records: int) -> dict:
    """Writes all columns as .npy files (generated in chunks, low peak memory)."""
    out.mkdir(parents=True, exist_ok=True)
    t0 = time.perf_counter()
    arrays = {}
    first = kq_columns(1, 1)
    for nm in NAMES:
        arrays[nm] = np.lib.format.open_memmap(out / f"{nm}.npy", mode="w+", dtype=first[nm].dtype, shape=(records,))
    for u0 in range(1, records + 1, CHUNK):
        count = min(CHUNK, records - u0 + 1)
        cols = kq_columns(u0, count)
        for nm in NAMES:
            arrays[nm][u0 - 1:u0 - 1 + count] = cols[nm]
    gen_s = time.perf_counter() - t0
    # spot check against the scalar reference
    rng = np.random.default_rng(SPOT_SEED)
    us = sorted(set(rng.integers(1, records + 1, SPOT_CHECKS).tolist()) | {1, records})
    bad = 0
    for u in us:
        row = tuple(LAGE_LABELS[int(arrays[nm][u - 1])] if nm == "lage" else int(arrays[nm][u - 1]) for nm in NAMES)
        bad += row != kq_row(u)
    for a in arrays.values():
        a.flush()
    del arrays
    meta = {"records": records, "fields": NAMES, "generate_s": round(gen_s, 3), "spot_checks": len(us),
            "spot_check_mismatches": bad, "bytes": sum((out / f"{nm}.npy").stat().st_size for nm in NAMES)}
    (out / "data.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
    return meta


def load_data(directory, fields=None) -> dict:
    """Loads the pre-generated columns completely into memory."""
    directory = Path(directory)
    return {nm: np.load(directory / f"{nm}.npy") for nm in (fields or NAMES)}


def main() -> int:
    ap = argparse.ArgumentParser(description="Write the KQ test data set.")
    ap.add_argument("--write-data", required=True, metavar="DIR")
    ap.add_argument("--records", type=int, default=10_000_000)
    a = ap.parse_args()
    meta = write_data(Path(a.write_data), a.records)
    print(f"test data: {meta['records']:,} records, {meta['bytes'] / 1e6:.1f} MB, {meta['generate_s']:.1f} s, "
          f"spot check mismatches: {meta['spot_check_mismatches']}", flush=True)
    return 0 if meta["spot_check_mismatches"] == 0 else 2


if __name__ == "__main__":
    sys.exit(main())
