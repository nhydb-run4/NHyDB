"""Executes the Python cookbook and the example programs.

Every fenced block in ``docs/cookbook/PYTHON_COOKBOOK.md`` whose info string
is exactly ``python`` runs as its own script in an empty temporary
directory. Blocks marked ``python-no-run`` (or any other info string) are
not executed. Blocks containing the line ``# requires: numpy`` are skipped
when NumPy is not installed. If a ``text`` block directly follows a
``python`` block (only blank lines in between), it is the documented output
and must equal the script's stdout exactly (T014, as in test_readme.py).
Every ``examples/python/*.py`` runs the same way.
"""
import os
import re
import subprocess
import sys
from pathlib import Path

import pytest

import timing

ROOT = Path(__file__).resolve().parents[2]
COOKBOOK = ROOT / "docs" / "cookbook" / "PYTHON_COOKBOOK.md"
EXAMPLES = ROOT / "examples" / "python"
FENCE = re.compile(r"^```([^\n`]*)\n(.*?)^```\s*$", re.S | re.M)


def _blocks():
    if not COOKBOOK.exists():
        return []
    text = COOKBOOK.read_text(encoding="utf-8")
    fences = list(FENCE.finditer(text))
    out = []
    per_title = {}
    for m in (f for f in fences if f.group(1).strip() == "python"):
        heads = re.findall(r"^#{2,4} (.+)$", text[: m.start()], re.M)
        # T-09: stable test ids – the heading without its section number plus
        # the position of the block under that heading (or an explicit
        # `# id: name` line in the block), so that a block added elsewhere
        # does not rename the tests (the dir3 evaluation refers to them)
        head = re.sub(r"^\d+(\.\d+)*\.?\s*", "", heads[-1]) if heads else "start"
        title = re.sub(r"[^A-Za-z0-9]+", "-", head).strip("-")[:50] or "start"
        per_title[title] = per_title.get(title, 0) + 1
        marker = re.search(r"^# id: ([A-Za-z0-9_-]+)\s*$", m.group(2), re.M)
        test_id = marker.group(1) if marker else f"{title}-{per_title[title]}"
        k = fences.index(m)
        nxt = fences[k + 1] if k + 1 < len(fences) else None
        expected = None
        if nxt is not None and nxt.group(1).strip() == "text" and not text[m.end():nxt.start()].strip():
            expected = nxt.group(2)
        out.append(pytest.param(m.group(2), expected, id=test_id))
    return out


def _run(code, cwd, expected=None):
    if "# requires: numpy" in code:
        pytest.importorskip("numpy")
    env = dict(os.environ, PYTHONIOENCODING="utf-8")
    r = subprocess.run([sys.executable, "-c", code], cwd=cwd, env=env, capture_output=True, text=True,
                       encoding="utf-8", timeout=timing.limit(120))
    assert r.returncode == 0, f"block failed (exit {r.returncode}):\n{r.stderr[-4000:]}\n--- code ---\n{code}"
    if expected is not None:
        assert r.stdout.rstrip("\n").splitlines() == expected.rstrip("\n").splitlines(), \
            f"documented output differs:\n--- stdout ---\n{r.stdout}\n--- documented ---\n{expected}"


@pytest.mark.parametrize("code,expected", _blocks())
def test_cookbook_block(code, expected, tmp_path):
    _run(code, tmp_path, expected)


@pytest.mark.parametrize("script", sorted(EXAMPLES.glob("*.py")) if EXAMPLES.exists() else [], ids=lambda p: p.name)
def test_example_program(script, tmp_path):
    _run(script.read_text(encoding="utf-8"), tmp_path)


def test_cookbook_found():
    if not COOKBOOK.exists():
        pytest.skip("cookbook not part of this distribution")
    assert len(_blocks()) >= 40
    # nearly every example documents its output; keep it that way
    assert sum(p.values[1] is not None for p in _blocks()) >= 50
