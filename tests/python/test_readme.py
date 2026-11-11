"""README checks: the Python example runs and prints exactly the documented
output, and every link to a file of this repository (absolute GitHub URL or
relative path) points to an existing file and, if given, an existing heading
anchor. External links are not checked (no network access in tests)."""

import os
import re
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
README = ROOT / "README.md"
REPO_URL = "https://github.com/nhydb-run4/NHyDB/"
FENCE = re.compile(r"^```([A-Za-z0-9_-]*)[ \t]*\n(.*?)^```[ \t]*$", re.S | re.M)
LINK = re.compile(r"\]\(([^)\s]+)\)")

pytestmark = pytest.mark.skipif(not README.exists(), reason="README not part of this distribution")


def _slug(heading):
    """GitHub heading anchor."""
    s = re.sub(r"[`*_]", "", heading.strip().lower())
    s = re.sub(r"[^\w\- ]", "", s)
    return s.replace(" ", "-")


def _anchors(path):
    text = path.read_text(encoding="utf-8")
    text = re.sub(r"^```.*?^```", "", text, flags=re.S | re.M)
    return {_slug(h) for h in re.findall(r"^#{1,6} (.+)$", text, re.M)}


def _examples():
    text = README.read_text(encoding="utf-8") if README.exists() else ""
    fences = [(m.group(1), m.group(2)) for m in FENCE.finditer(text)]
    out = []
    for i, (lang, code) in enumerate(fences):
        if lang == "python":
            expected = fences[i + 1][1] if i + 1 < len(fences) and fences[i + 1][0] == "text" else None
            out.append(pytest.param(code, expected, id=f"example-{len(out) + 1}"))
    return out


@pytest.mark.parametrize("code,expected", _examples())
def test_readme_example(code, expected, tmp_path):
    env = dict(os.environ, TMPDIR=str(tmp_path), TEMP=str(tmp_path), TMP=str(tmp_path), PYTHONIOENCODING="utf-8")
    r = subprocess.run([sys.executable, "-c", code], cwd=tmp_path, env=env, capture_output=True, text=True,
                       encoding="utf-8", timeout=120)
    assert r.returncode == 0, r.stderr
    if expected is not None:
        assert r.stdout.rstrip("\n").splitlines() == expected.rstrip("\n").splitlines(), r.stdout


def test_readme_has_example():
    assert _examples()


def test_readme_links():
    text = README.read_text(encoding="utf-8")
    bad = []
    for link in LINK.findall(text):
        if link.startswith(REPO_URL):
            rest = link[len(REPO_URL):]
            parts = rest.split("/", 2)  # blob|tree, branch, path
            if len(parts) < 3 or parts[0] not in ("blob", "tree") or parts[1] != "main":
                bad.append(f"{link}: unexpected URL form")
                continue
            target, _, anchor = parts[2].partition("#")
        elif link.startswith("#"):
            target, anchor = "README.md", link[1:]
        elif "://" in link or link.startswith("mailto:"):
            continue
        else:
            target, _, anchor = link.partition("#")
        path = ROOT / target
        if not path.exists():
            bad.append(f"{link}: {target} does not exist")
        elif anchor and path.suffix == ".md" and anchor not in _anchors(path):
            bad.append(f"{link}: anchor #{anchor} not found in {target}")
    assert not bad, "\n".join(bad)
