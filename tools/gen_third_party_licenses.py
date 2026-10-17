"""Generate THIRD-PARTY-LICENSES.txt for the Rust crates compiled into the wheel.

Collects every crate that is linked into the Python extension module (normal
dependencies, no build scripts or procedural macros) for the platforms built
by the release workflow, and writes their license notices into one file.

Usage (from the repository root, after `cargo fetch`):

    python tools/gen_third_party_licenses.py

Requirements: a Rust toolchain (`cargo`) and the crate sources in the cargo
registry (`cargo fetch` downloads them). Run it again whenever dependencies
change (`Cargo.lock`) and commit the regenerated file.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "THIRD-PARTY-LICENSES.txt"

# Platforms for which wheels are built (see .github/workflows/release.yml).
TARGETS = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
]

# Preferred choice for dual/multi-licensed crates.
PREFERENCE = ["MIT", "Apache-2.0", "BSD-3-Clause", "BSD-2-Clause", "Zlib", "0BSD", "Unicode-3.0"]

LICENSE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE)", re.I)

MIT_TEMPLATE = """Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""


def cargo(*args: str) -> str:
    return subprocess.run(["cargo", *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout


def extension_crate() -> str:
    text = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
    manifest = re.search(r'manifest-path\s*=\s*"([^"]+)"', text).group(1)
    name = re.search(r'^name\s*=\s*"([^"]+)"', (ROOT / manifest).read_text(encoding="utf-8"), re.M).group(1)
    return name


def linked_crates(package: str) -> set[tuple[str, str]]:
    found = set()
    for target in TARGETS:
        out = cargo("tree", "-q", "-p", package, "-e", "normal,no-proc-macro", "--target", target,
                    "--prefix", "none", "--format", "{p}")
        for line in out.splitlines():
            m = re.match(r"^(\S+) v(\S+)( \(.*\))?", line.strip())
            if m and not (m.group(3) and "/" in m.group(3)):  # skip local workspace crates
                found.add((m.group(1), m.group(2)))
    return found


def choose(expr: str) -> str:
    options = [o.strip("() ") for o in re.split(r"\s+OR\s+", expr or "")]
    for pref in PREFERENCE:
        for o in options:
            if o == pref or o.startswith(pref + " "):
                return o
    return options[0] if options else "UNKNOWN"


def license_texts(pkg_dir: Path, chosen: str) -> list[tuple[str, str]]:
    files = sorted(p for p in pkg_dir.iterdir() if p.is_file() and LICENSE_FILE.match(p.name))
    key = chosen.split()[0].upper().replace("-2.0", "").replace("APACHE", "APACHE")
    wanted = []
    for p in files:
        n = p.name.upper()
        if n.startswith("NOTICE") or n.startswith("COPYRIGHT"):
            wanted.append(p)
        elif key == "MIT" and "MIT" in n:
            wanted.append(p)
        elif key == "APACHE" and "APACHE" in n:
            wanted.append(p)
        elif key == "ZLIB" and "ZLIB" in n:
            wanted.append(p)
        elif key == "UNICODE-3.0" and "UNICODE" in n:
            wanted.append(p)
    if not [p for p in wanted if not p.name.upper().startswith(("NOTICE", "COPYRIGHT"))]:
        generic = [p for p in files if p.name.upper() in ("LICENSE", "LICENSE.MD", "LICENSE.TXT", "COPYING")]
        wanted = generic + wanted
    return [(p.name, p.read_text(encoding="utf-8", errors="replace").strip()) for p in wanted]


def main() -> int:
    meta = json.loads(cargo("metadata", "--format-version", "1", "--locked"))
    pkgs = {(p["name"], p["version"]): p for p in meta["packages"]}
    crates = sorted(linked_crates(extension_crate()))
    sections, summary, apache_text = [], [], None
    for name, version in crates:
        p = pkgs[(name, version)]
        expr = p.get("license") or ""
        chosen = choose(expr)
        texts = license_texts(Path(p["manifest_path"]).parent, chosen)
        extra = []
        if "Unicode-3.0" in expr and not any("UNICODE" in n.upper() for n, _ in texts):
            extra = [(n, t) for n, t in license_texts(Path(p["manifest_path"]).parent, "Unicode-3.0")]
        texts += extra
        body = []
        for fname, text in texts:
            if chosen.startswith("Apache-2.0") and "APACHE" in fname.upper() and "TERMS AND CONDITIONS" in text.upper():
                apache_text = apache_text or text
                body.append(f"[{fname}: Apache License 2.0, full text in the appendix]")
            else:
                body.append(f"--- {fname} ---\n{text}")
        if not texts:
            if chosen == "MIT":
                body.append(MIT_TEMPLATE.format(holders=", ".join(p.get("authors") or [f"the {name} developers"])))
            else:
                body.append(f"[no license file in the package; license: {chosen}]")
        summary.append(f"  {name + ' ' + version:<36} {expr}  (used under: {chosen})")
        sections.append("\n".join([
            "=" * 79,
            f"{name} {version}",
            f"License: {expr}  (used under: {chosen})",
            f"Source: {p.get('repository') or 'https://crates.io/crates/' + name}",
            "=" * 79,
            "",
            "\n\n".join(body),
            "",
        ]))
    rust_std = "\n".join([
        "=" * 79,
        "Rust standard library (core, alloc, std)",
        "License: MIT OR Apache-2.0  (used under: MIT)",
        "Source: https://github.com/rust-lang/rust",
        "=" * 79,
        "",
        MIT_TEMPLATE.format(holders="The Rust Project Developers"),
    ])
    head = [
        "THIRD-PARTY SOFTWARE NOTICES AND LICENSES",
        "",
        f"The compiled extension module of this package contains the following {len(crates) + 1}",
        "third-party components. They are licensed under their own terms, reproduced",
        "below; they are not covered by the project's own license (LICENSE.md).",
        "For components offered under a choice of licenses, the license used is stated.",
        "",
        "Generated by tools/gen_third_party_licenses.py from Cargo.lock.",
        "",
        "Components:",
        f"  {'Rust standard library':<36} MIT OR Apache-2.0  (used under: MIT)",
        *summary,
        "",
    ]
    tail = []
    if apache_text:
        tail = ["=" * 79, "APPENDIX: Apache License, Version 2.0", "=" * 79, "", apache_text, ""]
    OUT.write_text("\n".join(head + [rust_std] + sections + tail), encoding="utf-8")
    print(f"wrote {OUT.relative_to(ROOT)}: {len(crates)} crates + Rust standard library")
    return 0


if __name__ == "__main__":
    sys.exit(main())
