#!/usr/bin/env python3
"""Bump the project version across package.json, Cargo.toml, and tauri.conf.json."""

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).parent

FILES = {
    "package.json": ROOT / "package.json",
    "Cargo.toml": ROOT / "src-tauri" / "Cargo.toml",
    "tauri.conf.json": ROOT / "src-tauri" / "tauri.conf.json",
}


def read_versions() -> dict[str, str]:
    versions = {}

    raw = (FILES["package.json"]).read_text()
    versions["package.json"] = json.loads(raw)["version"]

    raw = (FILES["Cargo.toml"]).read_text()
    m = re.search(r'^version\s*=\s*"([^"]+)"', raw, re.MULTILINE)
    if not m:
        sys.exit("Could not find version in Cargo.toml")
    versions["Cargo.toml"] = m.group(1)

    raw = (FILES["tauri.conf.json"]).read_text()
    versions["tauri.conf.json"] = json.loads(raw)["version"]

    return versions


def bump(version: str, part: str) -> str:
    try:
        major, minor, patch = (int(x) for x in version.split("."))
    except ValueError:
        sys.exit(f"Cannot parse version '{version}' as MAJOR.MINOR.PATCH")
    if part == "major":
        return f"{major + 1}.0.0"
    if part == "minor":
        return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"


def write_version(new: str) -> None:
    # package.json
    path = FILES["package.json"]
    data = json.loads(path.read_text())
    data["version"] = new
    path.write_text(json.dumps(data, indent=2) + "\n")

    # Cargo.toml — replace only the top-level package version line
    path = FILES["Cargo.toml"]
    content = re.sub(
        r'^(version\s*=\s*)"[^"]+"',
        rf'\g<1>"{new}"',
        path.read_text(),
        count=1,
        flags=re.MULTILINE,
    )
    path.write_text(content)

    # tauri.conf.json
    path = FILES["tauri.conf.json"]
    data = json.loads(path.read_text())
    data["version"] = new
    path.write_text(json.dumps(data, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "part",
        choices=["major", "minor", "patch"],
        help="Which part of the version to increment",
    )
    args = parser.parse_args()

    versions = read_versions()
    unique = set(versions.values())
    if len(unique) > 1:
        print("Warning: version mismatch across files:")
        for name, v in versions.items():
            print(f"  {name}: {v}")
        current = versions["Cargo.toml"]
        print(f"Using Cargo.toml version ({current}) as base\n")
    else:
        current = unique.pop()

    new = bump(current, args.part)
    write_version(new)
    print(f"{current} → {new}  (bumped {args.part})")


if __name__ == "__main__":
    main()
