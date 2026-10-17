"""Harness for the test scripts of the external reference implementation (not included).

The test scripts write their databases next to themselves and expect the
modules in the same directory. Therefore every run gets a **flat copy** in a
temporary directory:

* ``engine="python"``: modules of the reference implementation (``REF``,
  real AES-256-GCM via ``cryptography``),
* ``engine="rust"``: instead of those modules, the compatibility shims from
  ``tests/python/ref_v027/shims/`` (same module names, Rust engine).

The test scripts themselves are copied unchanged. The reference directory is
never written to.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
REF = ROOT / "reference_v0_27"
TESTS = REF / "Testscripte"
SHIMS = Path(__file__).resolve().parent / "shims"

MODULES = ("nhydb_v027", "nhydb_math_v027", "nhydb_layout_v027", "nhydb_index_v027", "nhydb_query_v027")

# Script -> minimum implementation level at which the Rust engine must pass it.
SCRIPTS = {
    "nhydb_math_v027_kernel_selftest": 1,
    "nhydb_v027_verify_smoke_selftest": 1,
    "nhydb_v027_regression_selftest": 1,
    "nhydb_v027_security_selftest": 1,
    "nhydb_index_v027_selftest": 4,
    "nhydb_math_v027_selftest": 3,
    "nhydb_layout_v027_selftest": 3,
    "nhydb_math_layout_v027_stress": 3,
    "nhydb_v027_math_integration_selftest": 3,
    "nhydb_query_v027_selftest": 4,
    "nhydb_sql_v027_selftest": 5,
}

# Implementation level of the Rust engine (all scripts above are enabled).
RUST_PHASE = 5


def prepare(workdir: Path, engine: str) -> Path:
    if engine == "python" and not (REF / "nhydb_v027.py").exists():
        import pytest
        pytest.skip("reference implementation not included")
    workdir.mkdir(parents=True, exist_ok=True)
    for f in TESTS.glob("*.py"):
        shutil.copy2(f, workdir / f.name)
    if engine == "python":
        for m in MODULES:
            shutil.copy2(REF / f"{m}.py", workdir / f"{m}.py")
    elif engine == "rust":
        for m in MODULES:
            src = SHIMS / f"{m}.py"
            if src.exists():
                shutil.copy2(src, workdir / f"{m}.py")
    else:
        raise ValueError(engine)
    return workdir


def run_script(name: str, workdir: Path, engine: str, *args: str, timeout: int = 900) -> subprocess.CompletedProcess:
    prepare(workdir, engine)
    env = dict(os.environ)
    env.pop("PYTHONPATH", None)
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return subprocess.run([sys.executable, "-B", f"{name}.py", *args], cwd=workdir, env=env,
                          capture_output=True, text=True, timeout=timeout)
