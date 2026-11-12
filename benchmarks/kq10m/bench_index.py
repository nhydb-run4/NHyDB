"""KQ-10M index benchmark: one run, started by orchestrator.py in its own process.

    python bench_index.py --data DIR --db FILE --out FILE.json [--records N] [--batch B] [--keep-db]

1. Import of all records as in the write benchmark (one data commit).
2. Three value indexes on lage (4 distinct values), stufe and tq:
   create_index x 3 + commit, then one build_indexes([1, 2, 3]) call + commit.
3. Close, file sizes, then (outside the measured time) a check of the indexes
   against the test data: counts per lage value, exact hit lists, distinct
   values and references per index.
"""
import argparse
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common as c  # noqa: E402
from kq import LAGE_ENUM, LAGE_LABELS, load_data  # noqa: E402

CHECKS = {"lage = Gz": ("lage", "Gz"), "stufe = 100": ("stufe", 100), "tq = 1000": ("tq", 1000),
          "tq BETWEEN 3000 AND 3001": ("tq", (3000, 3001))}


def hits(data, field, cond):
    col = data[field]
    if field == "lage":
        mask = col == LAGE_ENUM[cond]
    elif isinstance(cond, tuple):
        mask = (col >= cond[0]) & (col <= cond[1])
    else:
        mask = col == cond
    return np.nonzero(mask)[0].astype(np.int64) + 1


def check(path, data, records, per_index):
    import nhydb
    db = nhydb.NHyDB(path)
    v = db.verify(deep=False)
    res = {"verify": bool(v["ok"])}
    for lab in LAGE_LABELS:
        got = len(db.find_u(c.REALM, {c.INDEX_NAME["lage"]: lab}))
        res[f"count lage = {lab}"] = got == int(np.count_nonzero(data["lage"] == LAGE_ENUM[lab]))
    for name, (field, cond) in CHECKS.items():
        got = np.asarray(db.find_u(c.REALM, {c.INDEX_NAME[field]: cond}), dtype=np.int64)
        res[f"hits {name}"] = np.array_equal(got, hits(data, field, cond))
    for f in c.INDEX_FIELDS:
        info = per_index[c.INDEX_NAME[f]]
        res[f"distinct {f}"] = info["distinct_values"] == len(np.unique(data[f]))
        res[f"references {f}"] = info["u_references"] == records
    db.close(commit=False)
    return {"checks": res, "ok": all(res.values())}


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
    db = c.create_kq_db(path)
    t_imp = c.import_all(db, data, a.records, a.batch)
    size_import = c.db_files(path)
    u_import = c.usage_dict()

    t = c.ns()
    for f in c.INDEX_FIELDS:
        db.create_index(c.INDEX_ID[f], c.REALM, c.INDEX_NAME[f], f)
    define_ns = c.ns() - t
    t = c.ns()
    db.commit()
    define_commit_ns = c.ns() - t
    u_b0 = c.usage_dict()
    t = c.ns()
    r = db.build_indexes([c.INDEX_ID[f] for f in c.INDEX_FIELDS], commit=False)
    build_ns = c.ns() - t
    t = c.ns()
    db.commit()
    build_commit_ns = c.ns() - t
    u_b1 = c.usage_dict()
    size_index = c.db_files(path)
    db.close()
    size_closed = c.db_files(path)

    per_index = {x["name"]: {k: x[k] for k in ("distinct_values", "u_references", "payload_bytes")} for x in r["indexes"]}
    val = check(path, data, a.records, per_index)
    total_ns = define_ns + define_commit_ns + build_ns + build_commit_ns
    result = {
        "records": a.records, "batch": a.batch,
        "import_s": (t_imp["prep_ns"] + t_imp["write_ns"] + t_imp["commit_ns"]) / 1e9,
        "times_s": {"define": define_ns / 1e9, "define_commit": define_commit_ns / 1e9, "build": build_ns / 1e9,
                    "build_commit": build_commit_ns / 1e9, "total_incl_commits": total_ns / 1e9},
        "cpu_s_build": (u_b1["user_s"] + u_b1["sys_s"]) - (u_b0["user_s"] + u_b0["sys_s"]),
        "per_index": per_index,
        "size": {"after_import": size_import, "after_index": size_index, "after_close": size_closed,
                 "added_bytes": size_index["total_bytes"] - size_import["total_bytes"]},
        "memory": {"rss_test_data_bytes": rss_data, "peak_after_import_bytes": u_import["peak_bytes"],
                   "peak_after_index_bytes": u_b1["peak_bytes"]},
        "validation": val, "db_kept": a.keep_db,
    }
    c.write_json(a.out, result)
    if not a.keep_db:
        c.remove_db(path)
    print(f"index: build incl. commits {total_ns / 1e9:.3f} s, +{result['size']['added_bytes'] / 1e6:.1f} MB, "
          f"check {'OK' if val['ok'] else 'FAILED'}", flush=True)
    return 0 if val["ok"] else 2


if __name__ == "__main__":
    sys.exit(main())
