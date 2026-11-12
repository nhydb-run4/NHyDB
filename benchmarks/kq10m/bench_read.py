"""KQ-10M read benchmark, started by orchestrator.py in its own processes.

    python bench_read.py build   --data DIR --db FILE --out FILE.json
    python bench_read.py measure --data DIR --db FILE --out FILE.json

build    creates the indexed database (import + 3 indexes, as in the index
         benchmark; not part of the read results) and checks it.
measure  one repetition of all read cases. Every phase opens a fresh database
         handle ("cold" for the NHyDB caches, the operating system cache is
         warm) and repeats the call once on the same handle ("warm").

Phases per query:
  count   posting_count(index_id, value)            number of hits only
  ids     find_u(realm, {index: value})             list of record addresses
  rows    reading all 11 fields of the addresses found before
          (select_us and a get() loop; the lookup is not timed)
  query   lookup and reading in one call (find and QueryEngine.execute_numpy)
  range   find_u + read_numpy over the range, only for contiguous hits
Every result is compared with the test data outside the measured time.
"""
import argparse
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common as c  # noqa: E402
from kq import LAGE_ENUM, LAGE_LABELS, NAMES, load_data  # noqa: E402

# (name, field, condition, expected hits, read complete records?)
CASES = [
    ("stufe = 100", "stufe", 100, 988, True),
    ("tq = 1000", "tq", 1000, 2000, True),
    ("tq BETWEEN 3000 AND 3001", "tq", (3000, 3001), 12002, True),
    ("lage = Gz", "lage", "Gz", 3162, True),
    ("lage = NL", "lage", "NL", 4997541, False),
]
ns = c.ns


def expected(data, field, cond):
    col = data[field]
    if field == "lage":
        mask = col == LAGE_ENUM[cond]
    elif isinstance(cond, tuple):
        mask = (col >= cond[0]) & (col <= cond[1])
    else:
        mask = col == cond
    return np.nonzero(mask)[0].astype(np.int64) + 1


def check_ids(got, exp):
    return len(got) == len(exp) and np.array_equal(np.asarray(got, dtype=np.int64), exp)


def check_rows(kind, rows, us, data):
    idx = us - 1
    if kind == "dicts":
        if len(rows) != len(us) or any(r is None for r in rows):
            return False
        for nm in NAMES:
            vals = [r[nm] for r in rows]
            if nm == "lage":
                if vals != [LAGE_LABELS[x] for x in data[nm][idx].tolist()]:
                    return False
            elif not np.array_equal(np.asarray(vals, dtype=np.int64), data[nm][idx]):
                return False
        return True
    if kind == "numpy":
        if len(rows[NAMES[0]]) != len(us):
            return False
        lab = np.array(LAGE_LABELS, dtype=object)
        for nm in NAMES:
            col = rows[nm]
            if nm == "lage":
                if not np.array_equal(col, lab[data[nm][idx]]):
                    return False
            elif not np.array_equal(np.asarray(col).astype(np.int64), data[nm][idx]):
                return False
        return True
    raise ValueError(kind)


def build(a):
    import nhydb  # noqa: F401
    path = Path(a.db)
    c.remove_db(path)
    data = load_data(a.data)
    t = ns()
    db = c.create_kq_db(path)
    c.import_all(db, data, c.RECORDS if a.records is None else a.records, c.BATCH)
    for f in c.INDEX_FIELDS:
        db.create_index(c.INDEX_ID[f], c.REALM, c.INDEX_NAME[f], f)
    db.commit()
    db.build_indexes([c.INDEX_ID[f] for f in c.INDEX_FIELDS], commit=False)
    db.commit()
    db.close()
    build_s = (ns() - t) / 1e9
    import nhydb
    db = nhydb.NHyDB(path)
    v = db.verify(deep=False)
    db.close(commit=False)
    res = {"build_s": build_s, "size": c.db_files(path), "verify_ok": bool(v["ok"]), "ok": bool(v["ok"])}
    c.write_json(a.out, res)
    print(f"read database: built in {build_s:.1f} s, verify {'OK' if res['ok'] else 'FAILED'}", flush=True)
    return 0 if res["ok"] else 2


def measure(a):
    import nhydb
    from nhydb.query import Q, QueryEngine
    data = load_data(a.data)
    path = Path(a.db)
    entries = []

    def timed(fn):
        t = ns()
        res = fn()
        return res, (ns() - t) / 1e6

    def phase(case, ph, api, run, ok_fn, prepare=None):
        t = ns()
        db = nhydb.NHyDB(path)
        open_ms = (ns() - t) / 1e6
        ctx = prepare(db) if prepare else None
        cold, cold_ms = timed(lambda: run(db, ctx))
        warm, warm_ms = timed(lambda: run(db, ctx))
        ok = ok_fn(cold) and ok_fn(warm)
        db.close(commit=False)
        entries.append({"case": case, "phase": ph, "api": api, "open_ms": open_ms, "cold_ms": cold_ms,
                        "warm_ms": warm_ms, "correct": bool(ok)})
        print(f"  {case:<26} {ph:<6} {api:<34} cold {cold_ms:>10.3f} ms  warm {warm_ms:>10.3f} ms  "
              f"{'OK' if ok else 'FAILED'}", flush=True)

    for case, field, cond, n_exp, full in CASES:
        exp = expected(data, field, cond)
        if len(exp) != n_exp and a.records is None:
            raise SystemExit(f"unexpected test data: {case} has {len(exp)} hits, expected {n_exp}")
        idx_name, idx_id = c.INDEX_NAME[field], c.INDEX_ID[field]
        phase(case, "count", "posting_count", lambda db, _: db.posting_count(idx_id, cond), lambda x: x == len(exp))
        phase(case, "ids", "find_u", lambda db, _: db.find_u(c.REALM, {idx_name: cond}), lambda x: check_ids(x, exp))
        if not full:
            continue
        lookup = lambda db: db.find_u(c.REALM, {idx_name: cond})  # noqa: E731
        phase(case, "rows", "select_us", lambda db, us: list(db.select_us(c.REALM, us, NAMES)),
              lambda x: check_rows("dicts", x, exp, data), prepare=lookup)
        phase(case, "rows", "get loop", lambda db, us: [db.get(c.REALM, u) for u in us],
              lambda x: check_rows("dicts", x, exp, data), prepare=lookup)
        phase(case, "query", "find", lambda db, _: list(db.find(c.REALM, {idx_name: cond})),
              lambda x: check_rows("dicts", x, exp, data))
        ref = Q.field(field)
        expr = ref.between(*cond) if isinstance(cond, tuple) else (ref == cond)
        phase(case, "query", "execute_numpy",
              lambda db, _: QueryEngine(db).execute_numpy(expr, realm=c.REALM, select=NAMES),
              lambda x: check_rows("numpy", x, exp, data))
        if len(exp) and int(exp[-1] - exp[0] + 1) == len(exp):
            def run_range(db, _):
                us = db.find_u(c.REALM, {idx_name: cond})
                return db.read_numpy(c.REALM, us[0], len(us), NAMES)
            phase(case, "range", "find_u + read_numpy", run_range, lambda x: check_rows("numpy", x, exp, data))

    res = {"entries": entries, "peak_bytes": c.usage_dict()["peak_bytes"], "ok": all(e["correct"] for e in entries)}
    c.write_json(a.out, res)
    return 0 if res["ok"] else 2


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["build", "measure"])
    ap.add_argument("--data", required=True)
    ap.add_argument("--db", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--records", type=int)
    a = ap.parse_args()
    return build(a) if a.mode == "build" else measure(a)


if __name__ == "__main__":
    sys.exit(main())
