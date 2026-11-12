"""KQ-10M write benchmark: one run, started by orchestrator.py in its own process.

    python bench_write.py --data DIR --db FILE --out FILE.json [--records N] [--batch B] [--keep-db]

Measured (time.perf_counter_ns):
  setup     create the database, realm, shape and layout, first commit
  prep      building the column dict for each batch (views into the test data)
  write     the put_numpy_batch calls
  commit    the single data commit after the last batch
  close     closing the database
The test data is loaded completely into memory before the clock starts.
After closing, the database is checked outside the measured time: deep
verify, all records compared with the test data, spot checks via get().
"""
import argparse
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common as c  # noqa: E402
from kq import LAGE_LABELS, NAMES, kq_row, load_data  # noqa: E402

CHECK_CHUNK = 100_000
SAMPLE = 1000
SAMPLE_SEED = 20261016


def validate(path, data, records):
    import nhydb
    out = {}
    db = nhydb.NHyDB(path)
    v = db.verify(deep=True)
    out["verify_ok"] = bool(v["ok"])
    out["physical_records"] = int(v["physical_records"])
    out["count_ok"] = out["physical_records"] == records
    lab = np.array(LAGE_LABELS, dtype=object)
    bad_chunks = 0
    for u0 in range(1, records + 1, CHECK_CHUNK):
        n = min(CHECK_CHUNK, records - u0 + 1)
        got = db.read_numpy(c.REALM, u0, n)
        ok = bool(np.all(got["_present"]))
        for nm in NAMES:
            exp = data[nm][u0 - 1:u0 - 1 + n]
            ok = ok and (np.array_equal(got[nm], lab[exp]) if nm == "lage"
                         else np.array_equal(got[nm].astype(np.int64), exp))
        bad_chunks += not ok
    out["full_compare_failed_chunks"] = bad_chunks
    rng = np.random.default_rng(SAMPLE_SEED)
    us = sorted(set(rng.integers(1, records + 1, SAMPLE).tolist()) | {1, records})
    bad = 0
    for u in us:
        rec = db.get(c.REALM, u)
        bad += rec is None or tuple(rec[k] for k in NAMES) != kq_row(u)
    out["spot_checks"] = len(us)
    out["spot_check_mismatches"] = bad
    out["beyond_last_absent"] = db.get(c.REALM, records + 1) is None
    db.close(commit=False)
    out["ok"] = bool(out["verify_ok"] and out["count_ok"] and bad_chunks == 0 and bad == 0 and out["beyond_last_absent"])
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--db", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--records", type=int, default=c.RECORDS)
    ap.add_argument("--batch", type=int, default=c.BATCH)
    ap.add_argument("--keep-db", action="store_true")
    a = ap.parse_args()
    path = Path(a.db)
    c.remove_db(path)

    data = load_data(a.data)
    rss_data = c.rss_bytes()
    u_start = c.usage_dict()

    t = c.ns()
    db = c.create_kq_db(path)
    setup_ns = c.ns() - t
    t_imp = c.import_all(db, data, a.records, a.batch)
    u_import = c.usage_dict()
    t = c.ns()
    db.close()
    close_ns = c.ns() - t
    u_close = c.usage_dict()
    size = c.db_files(path)

    val = validate(path, data, a.records)
    import_ns = t_imp["prep_ns"] + t_imp["write_ns"] + t_imp["commit_ns"]
    result = {
        "records": a.records, "batch": a.batch, "batches": t_imp["batches"], "data_commits": 1,
        "times_s": {"setup": setup_ns / 1e9, "prep": t_imp["prep_ns"] / 1e9, "write_calls": t_imp["write_ns"] / 1e9,
                    "commit": t_imp["commit_ns"] / 1e9, "import_total": import_ns / 1e9, "close": close_ns / 1e9},
        "cpu_s_import": (u_import["user_s"] + u_import["sys_s"]) - (u_start["user_s"] + u_start["sys_s"]),
        "size": size,
        "memory": {"rss_test_data_bytes": rss_data, "peak_after_import_bytes": u_import["peak_bytes"],
                   "peak_after_close_bytes": u_close["peak_bytes"]},
        "validation": val, "db_kept": a.keep_db,
    }
    c.write_json(a.out, result)
    if not a.keep_db:
        c.remove_db(path)
    ti = result["times_s"]
    print(f"write: import {ti['import_total']:.3f} s ({a.records / ti['import_total'] / 1e6:.2f} M records/s), "
          f"file {size['main_bytes'] / 1e6:.1f} MB, validation {'OK' if val['ok'] else 'FAILED'}", flush=True)
    return 0 if val["ok"] else 2


if __name__ == "__main__":
    sys.exit(main())
