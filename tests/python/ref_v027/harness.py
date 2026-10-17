"""Harness für die Python-v0.27-Referenztests.

Die Referenzskripte schreiben ihre Datenbanken neben sich und erwarten die
Module im selben Ordner. Deshalb wird für jeden Lauf eine **flache Kopie** in
einem temporären Verzeichnis angelegt:

* ``engine="python"``: Referenzmodule aus ``reference_v0_27/`` (echte
  AES-256-GCM über ``cryptography``),
* ``engine="rust"``: statt der Referenzmodule die Kompatibilitätsmodule aus
  ``tests/python/ref_v027/shims/`` (gleiche Modulnamen, Rust-Engine).

Die Testskripte selbst werden unverändert kopiert. Der Referenzordner wird
nie beschrieben.
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

# Skript -> Phase, ab der die Rust-Engine es bestehen muss.
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

# Aktuell abgeschlossene Implementierungsphase der Rust-v0.27.
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
