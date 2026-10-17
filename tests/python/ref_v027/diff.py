"""Differential tests: Rust engine vs. external reference implementation (not included).

A scenario is Python source code that works with ``from nhydb_v027 import ...``
and reports its observations via ``emit(label, value)``. The scenario runs
twice in separate processes (reference implementation and Rust shim), each in
its own temp directory; the observations are compared as JSON. Exceptions are
recorded as ``{"exc": class_name, "msg": text}``.

Random values (UUID, keys, nonce, timestamps) must not be reported directly;
``normalize`` removes known random fields.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap
from pathlib import Path

from .harness import prepare

PRELUDE = r'''
import json, sys, traceback
_OBS = []
def _norm(v):
    if isinstance(v, bytes):
        return {"__bytes__": v.hex()}
    if isinstance(v, (list, tuple)):
        return [_norm(x) for x in v]
    if isinstance(v, dict):
        return {str(k): _norm(x) for k, x in v.items()}
    if isinstance(v, float) and v != v:
        return "NaN"
    if isinstance(v, int) and not isinstance(v, bool) and abs(v) > 2**53:
        return {"__int__": str(v)}
    return v
def emit(label, value):
    _OBS.append([label, _norm(value)])
def attempt(label, fn, *a, **k):
    try:
        emit(label, fn(*a, **k))
    except Exception as e:
        emit(label, {"exc": type(e).__name__, "msg": str(e)})
'''

EPILOGUE = r'''
sys.stdout.write("\n@@OBS@@" + json.dumps(_OBS, sort_keys=True, default=repr))
'''


def run(code: str, workdir: Path, engine: str, timeout: int = 600):
    prepare(workdir, engine)
    script = workdir / "_scenario.py"
    script.write_text(PRELUDE + textwrap.dedent(code) + EPILOGUE, encoding="utf-8")
    env = dict(os.environ)
    env.pop("PYTHONPATH", None)
    r = subprocess.run([sys.executable, "-B", script.name], cwd=workdir, env=env,
                       capture_output=True, text=True, timeout=timeout)
    if r.returncode != 0 or "@@OBS@@" not in r.stdout:
        raise AssertionError(f"scenario ({engine}) failed:\n{r.stdout[-3000:]}\n{r.stderr[-5000:]}")
    return json.loads(r.stdout.rsplit("@@OBS@@", 1)[1])


def compare(code: str, tmp_path: Path, *, ignore=()):
    """Run ``code`` against both engines and return (python, rust)."""
    py = run(code, tmp_path / "py", "python")
    rs = run(code, tmp_path / "rs", "rust")
    if ignore:
        py = [x for x in py if x[0] not in ignore]
        rs = [x for x in rs if x[0] not in ignore]
    return py, rs
