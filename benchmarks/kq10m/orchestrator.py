"""NHyDB KQ-10M benchmark – runs everything and writes the report.

    python orchestrator.py [--runs 3] [--keep-dbs] [--out DIR] [--note TEXT]

Steps (each benchmark in its own Python process):
  0. generate the 10 M record test data set once (temporary .npy files)
  1. write benchmark   (--runs times)
  2. index benchmark   (--runs times)
  3. read benchmark    (one indexed database, --runs repetitions)
  4. report: Benchmarkrun_<timestamp>/report_<timestamp>.md (+ results_<timestamp>.json)

Test data and databases are deleted at the end; with --keep-dbs the
databases stay in Benchmarkrun_<timestamp>/databases/. The report contains no
user name, host name or file paths, only hardware and software versions.
"""
import argparse
import json
import math
import os
import shutil
import statistics as st
import subprocess
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import common as c  # noqa: E402

PY = sys.executable
DATA_BYTES_PER_RECORD = 81          # 10 int64 columns + 1 uint8 column
DB_BYTES_PER_RECORD = 32            # upper estimate incl. indexes


# ----------------------------------------------------------------------
# preflight
# ----------------------------------------------------------------------

def preflight():
    """Checks Python, NumPy and NHyDB before anything is started."""
    problems = []
    if sys.version_info < (3, 12):
        problems.append(f"Python {sys.version.split()[0]} found, Python 3.12 or newer is required.")
    try:
        import numpy  # noqa: F401
    except ImportError:
        problems.append('NumPy is not installed:  python -m pip install "nhydb[numpy]>=' + c.MIN_NHYDB + '"')
    try:
        info = c.nhydb_info()
        v = c.version_tuple(info["engine_version"])
        if v is None or v < c.version_tuple(c.MIN_NHYDB):
            problems.append(f"NHyDB {info['engine_version']} found, this benchmark needs NHyDB {c.MIN_NHYDB} or newer:  "
                            f'python -m pip install --upgrade "nhydb[numpy]>={c.MIN_NHYDB}"')
    except ImportError:
        info = None
        problems.append('NHyDB is not installed:  python -m pip install "nhydb[numpy]>=' + c.MIN_NHYDB + '"')
    return info, problems


# ----------------------------------------------------------------------
# formatting
# ----------------------------------------------------------------------

def md_cell(text, limit=200):
    """User text as a single Markdown table cell (no line breaks, no table syntax)."""
    text = " ".join(str(text).split())
    text = "".join(ch for ch in text if ch.isprintable())
    text = text.replace("\\", "\\\\").replace("|", "\\|").replace("<", "&lt;").replace(">", "&gt;")
    return text[:limit] + ("…" if len(text) > limit else "")


def plural(n, word):
    return f"{n} {word}{'' if n == 1 else 's'}"


def mean(xs):
    return st.mean(xs) if xs else float("nan")


def spread(xs, fmt):
    """'mean' or 'mean (min–max)' for several runs."""
    if not xs:
        return "–"
    m, lo, hi = fmt(mean(xs)), fmt(min(xs)), fmt(max(xs))
    if len(xs) == 1 or lo == hi:
        return m
    # common unit suffix (e.g. " ms", " M records/s") only once
    suffix = os.path.commonprefix([m[::-1], lo[::-1], hi[::-1]])[::-1]
    unit = suffix[suffix.find(" "):] if " " in suffix else ""
    if unit:
        n = len(unit)
        return f"{m} ({lo[:-n]}–{hi[:-n]})"
    return f"{m} ({lo}–{hi})"


def f_s(x):
    return f"{x:.3f} s"


def f_ms(x):
    if x < 1:
        return f"{x:.3f} ms"
    if x < 100:
        return f"{x:.2f} ms"
    return f"{x:,.1f} ms"


def f_mb(b):
    return f"{b / 1e6:,.1f} MB"


def f_mib(b):
    return f"{b / c.MIB:,.0f} MiB"


def f_gib(b):
    return f"{b / 2**30:.1f} GiB" if b else "–"


def nice_max(x):
    if x <= 0:
        return 1
    mag = 10 ** math.floor(math.log10(x * 1.15))
    for m in (1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10):
        if m * mag >= x * 1.15:
            return m * mag
    return 10 * mag


def chart(title, labels, values, unit):
    vals = [round(v, 3) for v in values]
    return ("```mermaid\nxychart-beta\n"
            f'    title "{title}"\n'
            f"    x-axis [{', '.join(json.dumps(x) for x in labels)}]\n"
            f'    y-axis "{unit}" 0 --> {nice_max(max(vals))}\n'
            f"    bar [{', '.join(str(v) for v in vals)}]\n```\n")


# ----------------------------------------------------------------------
# report
# ----------------------------------------------------------------------

def report(res):
    env, sysi, nh = res["environment"], res["environment"]["system"], res["environment"]["nhydb"]
    w, ix, rd = res["write"], res["index"], res["read"]
    records = res["parameters"]["records"]
    L = []
    add = L.append
    status = "VALID" if res["valid"] else "NOT VALID – see section 4"
    if records != c.RECORDS:
        status += f" (non-standard: {records:,} records instead of {c.RECORDS:,})"

    add(f"# NHyDB KQ-10M benchmark – NHyDB {nh['engine_version']}\n")
    add(f"**Result: {status}**  \nDate: {res['started']} · benchmark version {c.BENCHMARK_VERSION}\n")
    add("10,000,000 records with 11 integer fields each (the KQ data set: N, tp, tq, gp, sp, lage, dyn_u, dyn_v, "
        "stufe, station, stufenstation; lage is a 4-value enum). Three workloads: writing, building indexes and "
        "reading. Only NHyDB is measured.\n")

    add("## Environment\n")
    add("| | |\n|---|---|")
    cores = sysi.get("physical_cores")
    rows = [
        ("Machine", sysi.get("machine_model") or "–"),
        ("CPU", sysi.get("cpu") or "–"),
        ("CPU cores", f"{cores} physical, {sysi.get('logical_cpus')} logical" if cores else f"{sysi.get('logical_cpus')} logical"),
        ("Memory", f_gib(sysi.get("memory_bytes"))),
        ("Operating system", f"{sysi.get('os')} ({sysi.get('architecture')})"),
        ("Python / NumPy", f"{sysi.get('python')} / {sysi.get('numpy')}"),
        ("NHyDB", f"engine {nh['engine_version']}, file format {nh['file_format']}"
                  + (f", package {nh['package_version']}" if nh.get("package_version") not in (None, nh["engine_version"]) else "")),
        ("Durability", f"durability='{c.DURABILITY}'"),
        ("Protocol", f"batches of {res['parameters']['batch']:,} records via put_numpy_batch, one data commit after "
                     f"the last batch; writing and indexing: {plural(res['parameters']['runs'], 'run')} each, every run in "
                     f"a fresh process with a new database; reading: one indexed database, "
                     f"{plural(res['parameters']['runs'], 'repetition')} in separate processes; tables show means (min–max)"),
        ("Test data", "generated before the measurements and held in memory, so data generation is not part of any "
                      "measured time"),
    ]
    if env.get("note"):
        rows.append(("Note", md_cell(env["note"])))
    for k, v in rows:
        add(f"| {k} | {v} |")
    add("")

    # ---------------------------------------------------------------- writing
    add("## 1. Writing: importing 10 million records\n")
    if w:
        t = [x["times_s"] for x in w]
        imp, wc = [x["import_total"] for x in t], [x["write_calls"] for x in t]
        size = w[0]["size"]["main_bytes"]
        peak = [x["memory"]["peak_after_import_bytes"] for x in w]
        extra = [x["memory"]["peak_after_import_bytes"] - x["memory"]["rss_test_data_bytes"] for x in w]
        add("| | NHyDB " + nh["engine_version"] + " |\n|---|---:|")
        add(f"| Import time (preparation + write calls + commit) | {spread(imp, f_s)} |")
        add(f"| Throughput, complete import | {spread([records / x / 1e6 for x in imp], lambda v: f'{v:.2f} M records/s')} |")
        add(f"| Throughput, write calls only | {spread([records / x / 1e6 for x in wc], lambda v: f'{v:.2f} M records/s')} |")
        add(f"| Write calls | {spread(wc, f_s)} |")
        add(f"| Data commit (once, after 10 M records) | {spread([x['commit'] for x in t], f_s)} |")
        add(f"| Database file after closing | {f_mb(size)} ({size / records:.2f} bytes/record) |")
        add(f"| Peak process memory after import | {spread(peak, f_mib)} |")
        add(f"| Additional process memory above the test-data baseline | {spread(extra, f_mib)} |")
        add("")
        add(chart("Import time (s)", ["preparation", "write calls", "commit"],
                  [mean([x["prep"] for x in t]), mean(wc), mean([x["commit"] for x in t])], "seconds"))
    else:
        add("_not run_\n")

    # ---------------------------------------------------------------- index
    add("## 2. Building indexes\n")
    add("Three secondary indexes on `lage` (4 distinct values), `stufe` and `tq`, built over the same 10 M records "
        "with one `build_indexes` call.\n")
    if ix:
        t = [x["times_s"] for x in ix]
        base = ix[0]["size"]["after_import"]["total_bytes"]
        added = ix[0]["size"]["added_bytes"]
        per = ix[0]["per_index"]
        bpr = [v["payload_bytes"] / v["u_references"] for v in per.values() if v["u_references"]]
        peak = [x["memory"]["peak_after_index_bytes"] for x in ix]
        extra = [x["memory"]["peak_after_index_bytes"] - x["memory"]["rss_test_data_bytes"] for x in ix]
        add("| | NHyDB " + nh["engine_version"] + " |\n|---|---:|")
        add(f"| Build time incl. commits | {spread([x['total_incl_commits'] for x in t], f_s)} |")
        add(f"| of which build_indexes call | {spread([x['build'] for x in t], f_s)} |")
        add(f"| Space added by the 3 indexes | {f_mb(added)} (+{added / base * 100:.1f} %) |")
        add(f"| Bytes per indexed reference | {min(bpr):.2f}–{max(bpr):.2f} |")
        add(f"| All files after indexing | {f_mb(ix[0]['size']['after_index']['total_bytes'])} |")
        add(f"| Peak process memory after indexing | {spread(peak, f_mib)} |")
        add(f"| Additional process memory above the test-data baseline | {spread(extra, f_mib)} |")
        add("")
        add(chart("Disk size (MB)", ["data", "3 indexes", "total"],
                  [base / 1e6, added / 1e6, ix[0]["size"]["after_index"]["total_bytes"] / 1e6], "MB"))
    else:
        add("_not run_\n")

    # ---------------------------------------------------------------- read
    add("## 3. Reading\n")
    add("Five queries against the indexed database. **Cold** = first measured call on a freshly opened database "
        "handle, **warm** = the same call repeated on that handle. Cold is not a completely cold system: the operating "
        "system file cache is warm in both cases. `lage = Gz` hits are scattered (one record per ring), the two `tq` "
        "cases are contiguous ranges.\n")
    if rd["reps"]:
        entries = [e for r in rd["reps"] for e in r["entries"]]

        def cell(case, ph, api, key="cold_ms"):
            es = [e[key] for e in entries if e["case"] == case and e["phase"] == ph and e["api"] == api]
            return spread(es, f_ms) if es else "–"

        def val(case, ph, api, key="cold_ms"):
            es = [e[key] for e in entries if e["case"] == case and e["phase"] == ph and e["api"] == api]
            return mean(es) if es else 0.0

        from bench_read import CASES
        label = {cs[0]: f"`{cs[0]}` ({cs[3]:,}{', scattered' if cs[0] == 'lage = Gz' else ''})" for cs in CASES}
        full = [cs[0] for cs in CASES if cs[4]]
        contiguous = sorted({e["case"] for e in entries if e["phase"] == "range"}, key=[cs[0] for cs in CASES].index)

        add("### Finding matching records (list of IDs, `find_u`)\n")
        add("| Query (hits) | cold | warm |\n|---|---:|---:|")
        for cs in CASES:
            add(f"| {label[cs[0]]} | {cell(cs[0], 'ids', 'find_u')} | {cell(cs[0], 'ids', 'find_u', 'warm_ms')} |")
        add("\n### Counting matches (`posting_count`)\n")
        add("| Query (hits) | cold | warm |\n|---|---:|---:|")
        for cs in CASES:
            add(f"| {label[cs[0]]} | {cell(cs[0], 'count', 'posting_count')} | "
                f"{cell(cs[0], 'count', 'posting_count', 'warm_ms')} |")
        add("\n### Reading complete records (all 11 fields)\n")
        add("Times for reading the found records. The IDs were looked up beforehand on the same handle and are not "
            "included, so the index part of the handle is already loaded in the cold column.\n")
        add("| Query (hits) | `select_us` cold | `select_us` warm | `get` loop cold | `get` loop warm |\n|---|---:|---:|---:|---:|")
        for q in full:
            add(f"| {label[q]} | {cell(q, 'rows', 'select_us')} | {cell(q, 'rows', 'select_us', 'warm_ms')} | "
                f"{cell(q, 'rows', 'get loop')} | {cell(q, 'rows', 'get loop', 'warm_ms')} |")
        add("\n### Search and read in a single call\n")
        add("| Query (hits) | `find` cold | `find` warm | `execute_numpy` cold | `execute_numpy` warm |\n|---|---:|---:|---:|---:|")
        for q in full:
            add(f"| {label[q]} | {cell(q, 'query', 'find')} | {cell(q, 'query', 'find', 'warm_ms')} | "
                f"{cell(q, 'query', 'execute_numpy')} | {cell(q, 'query', 'execute_numpy', 'warm_ms')} |")
        if contiguous:
            add("\n### Contiguous ranges into NumPy (`find_u` + `read_numpy`)\n")
            add("| Query (hits) | cold | warm |\n|---|---:|---:|")
            for q in contiguous:
                add(f"| {label[q]} | {cell(q, 'range', 'find_u + read_numpy')} | "
                    f"{cell(q, 'range', 'find_u + read_numpy', 'warm_ms')} |")
        add("")
        opens = [e["open_ms"] for e in entries]
        add(f"Opening the database: {f_ms(mean(opens))} on average ({len(opens)} handles).\n")
        short = {"stufe = 100": "stufe", "tq = 1000": "tq", "tq BETWEEN 3000 AND 3001": "tq range", "lage = Gz": "lage Gz"}
        add(chart("Reading complete records, select_us cold (ms)", [short.get(q, q) for q in full],
                  [val(q, "rows", "select_us") for q in full], "ms"))
    else:
        add("_not run_\n")

    # ---------------------------------------------------------------- validation
    add("## 4. Validation\n")
    for item in res["validation"]:
        add(f"- {'OK' if item['ok'] else 'FAILED'}: {item['text']}")
    add("")
    add("## How to read these numbers\n")
    add("- One machine, one synthetic data set with integer fields. Other hardware, file systems or access patterns "
        "can lead to different results.\n"
        "- All times are measured from Python with the regular NHyDB Python API.\n"
        "- Background load, power saving, virus scanners and the storage device have a visible influence; "
        "repeat the run if a value looks unusual.\n")
    add(f"_Generated by the NHyDB KQ-10M benchmark {c.BENCHMARK_VERSION}._\n")
    return "\n".join(L)


# ----------------------------------------------------------------------
# main
# ----------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description="NHyDB KQ-10M benchmark (writing, indexing, reading).")
    ap.add_argument("--runs", type=int, default=3, help="runs per workload (default 3)")
    ap.add_argument("--keep-dbs", action="store_true", help="keep the databases in <run folder>/databases/")
    ap.add_argument("--out", default=str(HERE), help="folder for Benchmarkrun_<timestamp> (default: next to the scripts)")
    ap.add_argument("--note", default="", help="free text for the report, e.g. the storage device")
    ap.add_argument("--records", type=int, default=c.RECORDS, help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.runs < 1:
        ap.error("--runs must be at least 1")

    nh, problems = preflight()
    if problems:
        print("The benchmark cannot start:\n" + "\n".join(f"  - {p}" for p in problems), file=sys.stderr)
        return 3

    ts = time.strftime("%Y%m%d_%H%M%S")
    out = Path(a.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    run_dir, n = out / f"Benchmarkrun_{ts}", 1
    while True:
        try:
            run_dir.mkdir()        # never reuse an existing folder
            break
        except FileExistsError:
            n += 1
            run_dir = out / f"Benchmarkrun_{ts}_{n}"
    ts = run_dir.name[len("Benchmarkrun_"):]
    data_dir, db_dir, raw_dir, log_dir = run_dir / "_testdata", run_dir / "databases", run_dir / "raw", run_dir / "logs"
    for d in (data_dir, db_dir, raw_dir, log_dir):
        d.mkdir()

    need = a.records * DATA_BYTES_PER_RECORD + a.records * DB_BYTES_PER_RECORD * (2 * a.runs + 1 if a.keep_dbs else 1)
    free = shutil.disk_usage(run_dir).free
    if free < need * 1.2:
        print(f"Not enough free disk space in {out}: {free / 1e9:.1f} GB free, about {need * 1.2 / 1e9:.1f} GB needed.",
              file=sys.stderr)
        shutil.rmtree(run_dir, ignore_errors=True)
        return 3

    sysi = c.system_info()
    started = time.strftime("%Y-%m-%d %H:%M:%S %z")
    print(f"NHyDB KQ-10M benchmark {c.BENCHMARK_VERSION}: NHyDB {nh['engine_version']}, Python {sysi['python']}, "
          f"{sysi.get('cpu') or sysi['architecture']}")
    print(f"run folder: {run_dir}")
    if sysi.get("memory_bytes") and sysi["memory_bytes"] < 4 * 2**30:
        print("warning: less than 4 GiB memory; the benchmark needs about 2 GiB at its peak.")

    steps = []

    def step(name, args):
        t0 = time.time()
        print(f"[{time.strftime('%H:%M:%S')}] {name} ...", flush=True)
        with open(log_dir / f"{name}.log", "w", encoding="utf-8") as lf:
            rc = subprocess.run([PY, "-B", *map(str, args)], cwd=HERE, stdout=lf, stderr=subprocess.STDOUT).returncode
        last = (log_dir / f"{name}.log").read_text(encoding="utf-8", errors="replace").strip().splitlines()
        dt = time.time() - t0
        steps.append({"step": name, "rc": rc, "seconds": round(dt, 1)})
        print(f"[{time.strftime('%H:%M:%S')}] {name}: {'OK' if rc == 0 else f'FAILED (exit code {rc})'} "
              f"in {dt:.1f} s{(' – ' + last[-1]) if last else ''}", flush=True)
        return rc

    keep = ["--keep-db"] if a.keep_dbs else []
    common_args = ["--data", data_dir, "--records", a.records]
    write, index, reads, read_build = [], [], [], None
    try:
        if step("0_testdata", [HERE / "kq.py", "--write-data", data_dir, "--records", a.records]) == 0:
            for r in range(1, a.runs + 1):
                j = raw_dir / f"write_run{r}.json"
                step(f"1_write_run{r}", [HERE / "bench_write.py", *common_args, "--db",
                                         db_dir / f"kq10m_write_run{r}_{ts}.nhydb", "--out", j, *keep])
                if j.exists():
                    write.append(json.loads(j.read_text(encoding="utf-8")))
            for r in range(1, a.runs + 1):
                j = raw_dir / f"index_run{r}.json"
                step(f"2_index_run{r}", [HERE / "bench_index.py", *common_args, "--db",
                                         db_dir / f"kq10m_index_run{r}_{ts}.nhydb", "--out", j, *keep])
                if j.exists():
                    index.append(json.loads(j.read_text(encoding="utf-8")))
            rdb = db_dir / f"kq10m_read_{ts}.nhydb"
            rec = ["--records", a.records] if a.records != c.RECORDS else []
            j = raw_dir / "read_build.json"
            if step("3_read_build", [HERE / "bench_read.py", "build", "--data", data_dir, "--db", rdb, "--out", j, *rec]) == 0:
                read_build = json.loads(j.read_text(encoding="utf-8"))
                for r in range(1, a.runs + 1):
                    j = raw_dir / f"read_rep{r}.json"
                    step(f"3_read_rep{r}", [HERE / "bench_read.py", "measure", "--data", data_dir, "--db", rdb,
                                            "--out", j, *rec])
                    if j.exists():
                        reads.append(json.loads(j.read_text(encoding="utf-8")))
            if not a.keep_dbs:
                c.remove_db(rdb)
    except KeyboardInterrupt:
        print("interrupted – cleaning up", file=sys.stderr)
        steps.append({"step": "interrupted", "rc": 130, "seconds": 0})
    finally:
        shutil.rmtree(data_dir, ignore_errors=True)
        if not a.keep_dbs:
            shutil.rmtree(db_dir, ignore_errors=True)

    validation = [{"ok": all(s["rc"] == 0 for s in steps),
                   "text": f"all {len(steps)} steps finished without error"
                   if all(s["rc"] == 0 for s in steps) else
                   "failed steps: " + ", ".join(s["step"] for s in steps if s["rc"])}]
    if write:
        validation.append({"ok": all(x["validation"]["ok"] for x in write),
                           "text": f"write ({plural(len(write), 'run')}): deep verify, record count, all records compared with "
                                   f"the test data, {write[0]['validation']['spot_checks']} spot checks via get()"})
    if index:
        validation.append({"ok": all(x["validation"]["ok"] for x in index),
                           "text": f"index ({plural(len(index), 'run')}): verify, counts per lage value, exact hit lists, "
                                   "distinct values and references per index"})
    if reads:
        validation.append({"ok": all(r["ok"] for r in reads) and bool(read_build and read_build["ok"]),
                           "text": f"read ({plural(len(reads), 'repetition')}): every result (IDs, counts, all field values) "
                                   "compared with the test data"})
    expected = {"write": a.runs, "index": a.runs, "read": a.runs}
    got = {"write": len(write), "index": len(index), "read": len(reads)}
    if got != expected:
        validation.append({"ok": False, "text": f"missing results: expected {expected}, got {got}"})

    res = {"benchmark_version": c.BENCHMARK_VERSION, "started": started, "finished": time.strftime("%Y-%m-%d %H:%M:%S %z"),
           "parameters": {"records": a.records, "batch": c.BATCH, "runs": a.runs, "data_commits": 1,
                          "durability": c.DURABILITY, "keep_dbs": a.keep_dbs},
           "environment": {"system": sysi, "nhydb": nh, "note": a.note},
           "steps": steps, "write": write, "index": index, "read": {"build": read_build, "reps": reads},
           "validation": validation}
    res["valid"] = all(v["ok"] for v in validation) and a.records == c.RECORDS
    c.write_json(run_dir / f"results_{ts}.json", res)
    (run_dir / f"report_{ts}.md").write_text(report(res), encoding="utf-8")
    print(f"\nreport:  {run_dir / f'report_{ts}.md'}\nresult:  {'VALID' if res['valid'] else 'NOT VALID'}")
    if a.keep_dbs:
        print(f"databases kept in {db_dir}")
    return 0 if res["valid"] else 1


if __name__ == "__main__":
    sys.exit(main())
