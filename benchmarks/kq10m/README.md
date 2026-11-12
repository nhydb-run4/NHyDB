# NHyDB KQ-10M benchmark

A self-contained benchmark that measures **NHyDB** on your own machine with the
same data set and the same workloads as the benchmarks on
[nhydb.de/benchmarks](https://nhydb.de/benchmarks/):

1. **Writing** – importing 10,000,000 records with 11 integer fields each
2. **Building indexes** – three secondary indexes (`lage`, `stufe`, `tq`)
3. **Reading** – five queries: finding, counting and reading records

At the end it writes a short Markdown report with tables and charts,
`report_<timestamp>.md`, that you can keep or share. Only NHyDB is measured,
no other database is involved.

## Requirements

| | |
|---|---|
| Python | 3.12 or newer (64-bit) |
| NHyDB | 0.28.2 or newer, with NumPy |
| Operating system | macOS, Linux or Windows |
| Free disk space | about 2 GB (about 4 GB with `--keep-dbs`) |
| Free memory | about 2 GB |
| Run time | about 1 minute on a recent Mac, a few minutes on older machines |

## Step 1 – check Python

```bash
python3 --version        # macOS / Linux
py --version             # Windows
```

If the version is older than 3.12 or Python is missing, install a current
version from [python.org](https://www.python.org/downloads/).

## Step 2 – install NHyDB and NumPy

A virtual environment keeps the benchmark separate from your other Python
packages (recommended, but optional).

**macOS / Linux**

```bash
python3 -m venv nhydb-bench
source nhydb-bench/bin/activate
python -m pip install --upgrade pip
python -m pip install "nhydb[numpy]>=0.28.2"
```

**Windows (PowerShell or cmd)**

```bat
py -m venv nhydb-bench
nhydb-bench\Scripts\activate
python -m pip install --upgrade pip
python -m pip install "nhydb[numpy]>=0.28.2"
```

Already installed? Check the version and upgrade if needed:

```bash
python -c "import nhydb, numpy; print(nhydb.ENGINE_VERSION, numpy.__version__)"
python -m pip install --upgrade "nhydb[numpy]>=0.28.2"
```

The benchmark checks this itself before it starts and tells you exactly what
is missing.

## Step 3 – run the benchmark

Get the files of this folder (`benchmarks/kq10m/` in the
[NHyDB repository](https://github.com/nhydb-run4/NHyDB): clone the repository
or download it as ZIP). Keep all files in one directory, open a terminal
there and run:

```bash
python orchestrator.py
```

That is all. The orchestrator runs every part in its own Python process, one
after the other, and prints the progress:

```
[13:04:03] 0_testdata ...
[13:04:04] 0_testdata: OK in 0.7 s – test data: 10,000,000 records, 810.0 MB ...
[13:04:04] 1_write_run1 ...
...
report:  .../Benchmarkrun_20261112_130403/report_20261112_130403.md
result:  VALID
```

### Options

| Option | Meaning |
|---|---|
| `--runs N` | runs per workload (default `3`; the tables show mean and min–max) |
| `--keep-dbs` | keep the databases instead of deleting them (see below) |
| `--out DIR` | folder for the results (default: the folder of the scripts) |
| `--note "TEXT"` | free text for the report, for example the storage device |
| `--help` | show all options |

Examples:

```bash
python orchestrator.py --note "external USB SSD"
python orchestrator.py --runs 5
python orchestrator.py --keep-dbs
```

## Results

Every run creates a new folder, so you can run the benchmark as often as you
like:

```
Benchmarkrun_<timestamp>/
  report_<timestamp>.md     the report (tables + charts)
  results_<timestamp>.json  all values, machine-readable
  raw/                      results of every single run
  logs/                     console output of every step
  databases/                only with --keep-dbs
```

The charts are [Mermaid](https://mermaid.js.org/) diagrams inside the
Markdown file. GitHub and many editors show them as bar charts; elsewhere
they stay readable text.

**Databases are deleted by default**, each one directly after its part. With
`--keep-dbs` every database gets its own name and stays in `databases/`:

```
kq10m_write_run1_<timestamp>.nhydb    import only
kq10m_index_run1_<timestamp>.nhydb    import + 3 indexes
kq10m_read_<timestamp>.nhydb          the indexed database of the read part
```

Next to each database NHyDB keeps its usual side files (`.layout.enc`,
`.layout.key`, `.lock`) and `limiter.txt`, the operating statistics of its
memory limiter. These files stay on your machine and are not part of the
report.

The temporary test data (about 0.8 GB) is always deleted at the end. If you
stop the benchmark with Ctrl+C, the test data and (without `--keep-dbs`) the
databases are removed as well, and the report of the incomplete run is marked
NOT VALID. With `--keep-dbs` the databases of an interrupted run are kept,
including an incomplete one of the step that was running.

## What is measured

- **Test data.** The 10 M records are generated once before the
  measurements and loaded into memory before each measurement starts.
  Generating data is never part of a measured time, so only NHyDB is
  measured.
- **Writing.** `put_numpy_batch` in batches of 10,000 records, **one data
  commit** after the last batch, `durability='fsync'`.
- **Indexes.** `create_index` × 3 and one `build_indexes` call, both with
  commit.
- **Reading.** Every phase opens a fresh database handle. **Cold** is the
  first measured call on that handle, **warm** the same call repeated on it.
  Cold does not mean a completely cold system: the operating system's file
  cache is warm in both cases. For reading complete records the IDs are looked
  up on the same handle before the measured call (not timed). One indexed
  database is built for the read part and measured in `--runs` separate
  processes.
- **Memory.** Peak process memory is reported twice: in total (it includes
  the 0.8 GB of test data in memory), and as additional process memory above
  the test-data baseline. The latter is mostly NHyDB, but can also contain
  memory used by Python and NumPy during the run.
- **Validation.** Every run checks its results against the test data: deep
  verify, all 10 M records compared field by field, exact hit lists and all
  field values. The report says **VALID** only if every check passed.

## Privacy

The report contains **only hardware and software information**: machine model
(e.g. `Mac16,5`), CPU type, core count, memory size, operating system and the
versions of Python, NumPy and NHyDB. It contains **no** user name, host name,
file paths, serial numbers or network data. Please have a look at the report
before you share it; it is a plain text file.

Text you pass with `--note` is copied into the report and the JSON file as it
is. Do not put personal information into it.

## Tips for reliable numbers

- Close other programs; do not use the machine during the run.
- Laptops: connect the power supply.
- Windows: a virus scanner checks every new file, which slows down writing.
  If you can, exclude the benchmark folder.
- Run the benchmark on the drive you want to measure. Folders on network
  drives or USB sticks measure that drive. Use `--out` to choose the drive.

## Reference results

Results of our own reference runs (Apple M1 Max, and a Lenovo ThinkPad P52
with Windows 11 and an Ubuntu Hyper-V VM) are in
[`results/README.md`](results/README.md), each with its full report and JSON
file.

## Sharing your results

We are curious about results from other machines. Sharing is voluntary;
choose whichever way you prefer:

- **GitHub:** open an issue with the template
  [Benchmark result](https://github.com/nhydb-run4/NHyDB/issues/new?template=benchmark_result.yml)
  and paste the content of `report_<timestamp>.md`.
- **E-mail:** send `report_<timestamp>.md` (and, if you like,
  `results_<timestamp>.json`) to achim.meissner@nhydb.de.

Results shared on GitHub are public. Please do not edit the numbers in the
report; add remarks in the note field or in your message instead.

Running the benchmark inside a Git working copy? The `.gitignore` in this
folder keeps result folders, databases and `__pycache__` out of commits.

## Files

| File | Purpose |
|---|---|
| `orchestrator.py` | checks the environment, runs all parts, writes the report |
| `bench_write.py` | write benchmark (one run) |
| `bench_index.py` | index benchmark (one run) |
| `bench_read.py` | read benchmark (build the indexed database, one repetition) |
| `kq.py` | the KQ data set (generator and test data files) |
| `common.py` | shared helpers (memory, machine description, schema) |

## Versions

The benchmark needs NHyDB 0.28.2 or newer. Newer NHyDB versions are expected
to work; if a future version changes the API, the minimum version of the
benchmark will be raised. The report always states the NHyDB version and the
benchmark version it was created with.
