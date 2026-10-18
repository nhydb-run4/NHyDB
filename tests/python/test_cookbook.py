"""Executes the Python cookbook and the example programs.

Every fenced block in ``docs/cookbook/PYTHON_COOKBOOK.md`` whose info string
is exactly ``python`` runs as its own script in an empty temporary
directory. Blocks marked ``python-no-run`` (or any other info string) are
not executed. Blocks containing the line ``# requires: numpy`` are skipped
when NumPy is not installed. Every ``examples/python/*.py`` runs the same way.
"""
import re
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
COOKBOOK = ROOT / "docs" / "cookbook" / "PYTHON_COOKBOOK.md"
EXAMPLES = ROOT / "examples" / "python"
FENCE = re.compile(r"^```([^\n`]*)\n(.*?)^```\s*$", re.S | re.M)


def _blocks():
    if not COOKBOOK.exists():
        return []
    text = COOKBOOK.read_text(encoding="utf-8")
    out = []
    for i, m in enumerate(m for m in FENCE.finditer(text) if m.group(1).strip() == "python"):
        heads = re.findall(r"^#{2,4} (.+)$", text[: m.start()], re.M)
        title = re.sub(r"[^A-Za-z0-9]+", "-", heads[-1]).strip("-")[:50] if heads else "start"
        out.append(pytest.param(m.group(2), id=f"{i + 1:03d}-{title}"))
    return out


def _run(code, cwd):
    if "# requires: numpy" in code:
        pytest.importorskip("numpy")
    r = subprocess.run([sys.executable, "-c", code], cwd=cwd, capture_output=True, text=True, timeout=120)
    assert r.returncode == 0, f"block failed (exit {r.returncode}):\n{r.stderr[-4000:]}\n--- code ---\n{code}"


@pytest.mark.parametrize("code", _blocks())
def test_cookbook_block(code, tmp_path):
    _run(code, tmp_path)


@pytest.mark.parametrize("script", sorted(EXAMPLES.glob("*.py")) if EXAMPLES.exists() else [], ids=lambda p: p.name)
def test_example_program(script, tmp_path):
    _run(script.read_text(encoding="utf-8"), tmp_path)


def test_cookbook_found():
    if not COOKBOOK.exists():
        pytest.skip("cookbook not part of this distribution")
    assert len(_blocks()) >= 40
