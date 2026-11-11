# Building from source

This guide builds the Python package `nhydb` from source. Supported:
macOS, Linux and Windows.

## Prerequisites

* **Rust**, a current stable toolchain (tested with Rust 1.99), installed via
  [rustup](https://rustup.rs):
  `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
  Windows additionally needs the *Microsoft C++ Build Tools* (installed by rustup's setup when asked).
* **Python 3.12–3.15** (CPython) with `pip` and `venv`.
* On macOS: Xcode Command Line Tools (`xcode-select --install`).

Rust dependencies are downloaded from crates.io on the first build.

## Build a wheel

```bash
python3 -m venv .venv
source .venv/bin/activate            # Windows: .venv\Scripts\activate
pip install maturin
maturin build --release --out dist
```

The wheel is written to `dist/`, e.g.
`nhydb-<version>-cp313-cp313-macosx_11_0_arm64.whl`. It is specific to the
operating system, the CPU architecture and the Python version it was built
with.

`.cargo/config.toml` in the source tree aligns function starts to 64 bytes
(`-C llvm-args=-align-all-functions=6`); the published performance figures
were measured with it. Build from the repository root and do not set the
`RUSTFLAGS` environment variable, which would replace this setting.

## Install

```bash
pip install dist/nhydb-*.whl
pip install numpy                    # optional, NumPy batch API
```

Alternatively, build and install in one step: `pip install .`

## Test

```bash
cargo test --workspace --release           # Rust core
pip install numpy pytest
python -m pytest tests/python                 # Python package (installed wheel)
python -m pytest tests/smoke                  # short smoke tests (as in GitHub Actions)
```

The Python tests import the *installed* package, not the sources in
`python/`. The test run prints a fingerprint of the Rust sources
(`nhydb._native.BUILD_SOURCE_HASH`) and warns if the installed module was
built from different sources.

Longer runs of the robustness tests are controlled by environment
variables (the defaults keep the suite fast):

| Variable | Effect |
|---|---|
| `NHYDB_FUZZ_SCALE` | scale factor for the number of fuzz cases (byte flips, truncation, mutated payloads) |
| `NHYDB_FUZZ_SEED` | start seed of the fuzz tests (reproduce a run) |
| `NHYDB_CRASH_ROUNDS` | number of rounds of the crash simulator (process killed while committing) |

The format golden file (SHA-256 per block of a deterministic scenario) is
regenerated only after an intended format change:

```bash
python tests/python/test_stab_format_golden.py --update
```

## Disk space

The Rust build directory `target/` grows to a few hundred MB. It is ignored
by git and can be deleted at any time (`cargo clean`).

## Wheels for other platforms

Wheels for further platforms and Python versions are built automatically by
GitHub Actions (`.github/workflows/release.yml`) when a version tag is
pushed; see [RELEASING.md](RELEASING.md).
