# Building from source

This guide builds the Python package `nhydb` from source. Supported:
macOS, Linux and Windows.

## Prerequisites

* **Rust**, a current stable toolchain (tested with Rust 1.99), installed via
  [rustup](https://rustup.rs):
  `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
  Windows additionally needs the *Microsoft C++ Build Tools* (installed by rustup's setup when asked).
* **Python 3.9 or newer** with `pip` and `venv`.
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
`nhydb-0.27.0-cp313-cp313-macosx_11_0_arm64.whl`. It is specific to the
operating system, the CPU architecture and the Python version it was built
with.

## Install

```bash
pip install dist/nhydb-0.27.0-*.whl
pip install numpy                    # optional, NumPy batch API
```

Alternatively, build and install in one step: `pip install .`

## Test

```bash
cargo test --workspace --release           # Rust core
pip install numpy pytest
python -m pytest tests/python                 # Python package (installed wheel)
```

The Python tests import the *installed* package, not the sources in
`python/`.

## Disk space

The Rust build directory `target/` grows to a few hundred MB. It is ignored
by git and can be deleted at any time (`cargo clean`).

## Wheels for other platforms

Wheels for further platforms and Python versions are built automatically by
GitHub Actions (`.github/workflows/release.yml`) when a version tag is
pushed; see [RELEASING.md](RELEASING.md).
