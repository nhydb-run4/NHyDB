#!/usr/bin/env python3
"""Checks the release files in a directory before they are published.

Expects exactly one source distribution and exactly one wheel per supported
platform and CPython version, all with the version from Cargo.toml, and
nothing else.

Usage: python tools/check_dist.py DIST_DIR [--version X.Y.Z]
"""
import argparse
import re
import sys
import tomllib
from pathlib import Path

PYTHONS = ["312", "313", "314", "315"]
PLATFORMS = {
    "linux-x86_64": r"manylinux_2_\d+_x86_64",
    "linux-aarch64": r"manylinux_2_\d+_aarch64",
    "macos-x86_64": r"macosx_\d+_\d+_x86_64",
    "macos-arm64": r"macosx_\d+_\d+_arm64",
    "windows-x64": r"win_amd64",
}


def cargo_version(root: Path) -> str:
    with open(root / "Cargo.toml", "rb") as f:
        return tomllib.load(f)["workspace"]["package"]["version"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("dist", type=Path)
    ap.add_argument("--version")
    a = ap.parse_args()
    version = a.version or cargo_version(Path(__file__).resolve().parent.parent)
    files = sorted(p.name for p in a.dist.iterdir() if p.is_file())
    errors = []
    expected_sdist = f"nhydb-{version}.tar.gz"
    if expected_sdist not in files:
        errors.append(f"missing source distribution {expected_sdist}")
    found = {}
    for name in files:
        if name == expected_sdist:
            continue
        m = re.fullmatch(rf"nhydb-{re.escape(version)}-cp(\d+)-cp(\d+)-(.+)\.whl", name)
        if not m or m[1] != m[2] or m[1] not in PYTHONS:
            errors.append(f"unexpected file {name}")
            continue
        plat = [k for k, rx in PLATFORMS.items() if re.fullmatch(rx, m[3])]
        if len(plat) != 1:
            errors.append(f"unexpected platform tag in {name}")
            continue
        key = (plat[0], m[1])
        if key in found:
            errors.append(f"duplicate wheel for {key}: {found[key]} and {name}")
        found[key] = name
    for plat in PLATFORMS:
        for py in PYTHONS:
            if (plat, py) not in found:
                errors.append(f"missing wheel for {plat}, CPython {py[0]}.{py[1:]}")
    for name in files:
        print(name)
    if errors:
        for e in errors:
            print(f"ERROR: {e}", file=sys.stderr)
        return 1
    print(f"OK: version {version}, 1 sdist, {len(found)} wheels ({len(PLATFORMS)} platforms x {len(PYTHONS)} Python versions)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
