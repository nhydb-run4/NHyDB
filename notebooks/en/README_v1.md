# NHyDB 0.28.2 – Notebook Series v1

An interactive introduction to **NHyDB**, the database in which every value has a mathematical
address, and to the **Dynadron** mathematics behind it. Every notebook combines short
explanations, runnable code against the real NHyDB 0.28.2 API, figures drawn from real engine data,
and small experiments you can change yourself.

> Written for **NHyDB 0.28.2**. With other versions, outputs may differ.
>
> 🇩🇪 **Deutsch:** A German edition with identical code is available in [`../de/`](../de/README_v1.md).

## Contents

### Main series (`main/`)

| No. | Notebook | Key question |
|---:|---|---|
| 00 | [Welcome to NHyDB](main/00_start_here_v1.ipynb) | What is this, and where do I start? |
| 01 | [The Address Is the Identity](main/01_the_address_is_the_identity_v1.ipynb) | Why is every cell exactly one number? |
| 02 | [From Mathematics to Stored Values](main/02_from_address_to_data_v1.ipynb) | How do I store something there? |
| 03 | [Records, Fields and Lanes](main/03_records_fields_and_lanes_v1.ipynb) | What does the data model look like? |
| 04 | [One Dataset, Different Layouts](main/04_layouts_side_by_side_v1.ipynb) | How is data placed on cells? |
| 05 | [MATH_MAP: A Text, a Formula, a Database](main/05_math_map_text_and_formula_lab_v1.ipynb) | How do I choose the place by a formula? |
| 06 | [Scientific Results in Cells (MATH_LONG)](main/06_scientific_results_in_cells_v1.ipynb) | How do I store results of computations? |
| 07 | [Huge Addresses, Exact Integers](main/07_realms_and_huge_numbers_v1.ipynb) | How large is the address space really? |
| 08 | [Addresses Are Not Search Indexes](main/08_addresses_are_not_indexes_v1.ipynb) | How do I find data by content? |
| 09 | [Asking Questions of the Data](main/09_queries_and_sql_v1.ipynb) | How do I query data? |
| 10 | [From Single Cells to NumPy Batches](main/10_numpy_batches_v1.ipynb) | How do I handle bulk data? |
| 11 | [What Actually Lives on Disk?](main/11_what_lives_on_disk_v1.ipynb) | Why is the data stored safely? |
| 12 | [One Layout Plan, Two Views (ENC and KEY)](main/12_encrypted_layout_and_key_v1.ipynb) | What does the protected layout plan protect, and how? |
| 13 | [Measure, Don't Assume (Benchmark Lab)](main/13_benchmark_lab_v1.ipynb) | How fast is it on my machine? |
| 14 | [Build a Small Real Application](main/14_complete_application_v1.ipynb) | What does a complete application look like? |

**Reading paths**

* **Core path** (about one hour): 00 → 01 → 02 → 05 → 12
* **Mathematics path:** 01 → 05 → 06 → 07
* **Developer path:** 02 → 03 → 04 → 08 → 09 → 10 → 11 → 13 → 14

Every notebook can also be read on its own. Beginners read from top to bottom; experienced readers
skip the sections marked 📘 *Background* and go to the experiments marked ▶; optional deep dives
are marked ⏩ *Advanced*.

### Cookbook Reference Lab (`reference/`)

Based on the official
[NHyDB Python Cookbook](https://github.com/nhydb-run4/NHyDB/blob/main/docs/cookbook/PYTHON_COOKBOOK.md),
which remains the authoritative reference: all 61 runnable cookbook examples, each as an
independent code cell with the expected output below it.

| Notebook | Cookbook sections |
|---|---|
| [R01 Basics](reference/R01_basics_v1.ipynb) | 1–8: installation, first database, options, commit, records, cells, extensions |
| [R02 Addresses, types, ranges](reference/R02_addresses_types_ranges_v1.ipynb) | 9–12: addresses and N, reading by N, data types, ranges and iterators |
| [R03 Indexes, queries, SQL](reference/R03_indexes_queries_sql_v1.ipynb) | 13–15: secondary indexes, QueryEngine, NHySQL |
| [R04 NumPy and layouts](reference/R04_numpy_and_layouts_v1.ipynb) | 16–17: NumPy batch API, COLUMN/PACKED/CUSTOM, routing, evolution |
| [R05 MATH_MAP](reference/R05_math_map_v1.ipynb) | 18: MATH_MAP |
| [R06 Operations](reference/R06_operations_v1.ipynb) | 19–24: configuration, maintenance, files and backup, errors and locks, system space, CLI |
| [R07 Limitations and API reference](reference/R07_limitations_and_api_reference_v1.ipynb) | 25–27: application examples, known limitations, API reference |

## Getting started

**1. Get the notebooks.** Clone the repository (or download it as a ZIP file from GitHub):

```bash
git clone https://github.com/nhydb-run4/NHyDB.git
cd NHyDB
```

**2. Install Python packages.** You need Python 3.12–3.15. A virtual environment keeps the
installation separate from your system Python (recommended):

```bash
python -m venv .venv
source .venv/bin/activate          # Windows: .venv\Scripts\activate
python -m pip install "nhydb==0.28.2" numpy pandas matplotlib jupyterlab ipykernel
```

**3. Start JupyterLab** in the notebook folder:

```bash
cd notebooks/en
python -m jupyterlab
```

JupyterLab opens the notebook interface in your web browser. In the file browser on the left,
open `main/00_start_here_v1.ipynb` and work through the notebooks one after another:

* **Read** the explanations – the notebooks are stored with their outputs and figures, so you
  can read them before running anything.
* **Run** a cell with *Shift + Enter*, or all cells with *Run → Run All Cells*.
* **Change** values in a cell and run it again – most notebooks contain experiments and small
  tasks made for exactly that.

To stop JupyterLab, close the browser tab and press *Ctrl + C* in the terminal.

All notebooks create their databases in a temporary folder and remove it at the end; no file
outside that folder is touched. Timings in notebooks 08, 10 and 13 were measured on a MacBook Pro
(Apple M1 Max) and will differ on other machines.

**Automated execution (optional).** To run notebooks without the browser interface, for example
as a test, use `jupyter execute` inside `notebooks/en/`:

```bash
jupyter execute main/*.ipynb reference/*.ipynb
```

## License

The notebooks use NHyDB, which is licensed under the *NHyDB / dNHyDB Experimental Software
License 1.0* (source-available, non-production use). Website: https://nhydb.de/nhydb/
