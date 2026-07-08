#!/usr/bin/env python3
"""Bump the project version across package.json, Cargo.toml, and tauri.conf.json."""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).parent

FILES = {
    "package.json": ROOT / "package.json",
    "Cargo.toml": ROOT / "src-tauri" / "Cargo.toml",
    "Cargo.lock": ROOT / "src-tauri" / "Cargo.lock",
    "tauri.conf.json": ROOT / "src-tauri" / "tauri.conf.json",
}

# Matches the goopie-launcher package entry in Cargo.lock:
#   name = "goopie-launcher"
#   version = "X.Y.Z"
CARGO_LOCK_ENTRY_RE = re.compile(
    r'(name = "goopie-launcher"\nversion = )"([^"]+)"'
)


def read_versions() -> dict[str, str]:
    versions = {}

    raw = (FILES["package.json"]).read_text()
    versions["package.json"] = json.loads(raw)["version"]

    raw = (FILES["Cargo.toml"]).read_text()
    m = re.search(r'^version\s*=\s*"([^"]+)"', raw, re.MULTILINE)
    if not m:
        sys.exit("Could not find version in Cargo.toml")
    versions["Cargo.toml"] = m.group(1)

    raw = (FILES["Cargo.lock"]).read_text()
    m = CARGO_LOCK_ENTRY_RE.search(raw)
    if not m:
        sys.exit("Could not find goopie-launcher entry in Cargo.lock")
    versions["Cargo.lock"] = m.group(2)

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

    # Cargo.lock — replace only the goopie-launcher package's own version entry
    # (other crates' "version" lines must stay untouched).
    path = FILES["Cargo.lock"]
    content, n = CARGO_LOCK_ENTRY_RE.subn(rf'\g<1>"{new}"', path.read_text(), count=1)
    if n != 1:
        sys.exit("Could not find goopie-launcher entry in Cargo.lock")
    path.write_text(content)

    # tauri.conf.json
    path = FILES["tauri.conf.json"]
    data = json.loads(path.read_text())
    data["version"] = new
    path.write_text(json.dumps(data, indent=2) + "\n")


def confirm(prompt: str) -> bool:
    reply = input(f"{prompt} [y/N] ").strip().lower()
    return reply in ("y", "yes")


def run(*cmd: str) -> None:
    subprocess.run(cmd, cwd=ROOT, check=True)


def run_tests() -> None:
    """Run the unit + end-to-end test suite, aborting the release on failure.

    On Windows this includes the protected-path (UAC elevation) e2e case,
    which pops a real UAC prompt — approve it for the release to proceed.
    """
    print("Running tests before release...")
    try:
        subprocess.run([sys.executable, str(ROOT / "run_tests.py")], cwd=ROOT, check=True)
    except subprocess.CalledProcessError:
        sys.exit("Tests failed — aborting release (no changes made)")


def check_git_state(require_main: bool = False) -> None:
    if require_main:
        branch = subprocess.run(
            ("git", "rev-parse", "--abbrev-ref", "HEAD"),
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        if branch != "main":
            sys.exit(f"Must be on 'main' branch to release (currently on '{branch}')")

    status = subprocess.run(
        ("git", "status", "--porcelain"),
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    if status.strip():
        sys.exit("Working tree is not clean — commit or stash your changes first")


def git_author() -> str:
    name = subprocess.run(
        ("git", "config", "user.name"),
        cwd=ROOT,
        capture_output=True,
        text=True,
    ).stdout.strip()
    email = subprocess.run(
        ("git", "config", "user.email"),
        cwd=ROOT,
        capture_output=True,
        text=True,
    ).stdout.strip()
    return f"{name} <{email}>"


def publish(new: str, push: bool) -> None:
    tag = f"v{new}"
    run("git", "add", *(str(p) for p in FILES.values()))
    run("git", "commit", "-m", f"Bump version to {new}")
    if push:
        run("git", "tag", tag)
        run("git", "push")
        run("git", "push", "origin", tag)
        print(f"Pushed commit and tag {tag} — release workflow should trigger shortly")
    else:
        print(f"Committed {new} locally (use --push-only to tag and publish)")


def push_only() -> None:
    versions = read_versions()
    unique = set(versions.values())
    if len(unique) > 1:
        sys.exit("Version mismatch across files — resolve before pushing")
    current = unique.pop()
    tag = f"v{current}"
    run("git", "tag", tag)
    run("git", "push")
    run("git", "push", "origin", tag)
    print(f"Pushed commit and tag {tag} — release workflow should trigger shortly")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "part",
        nargs="?",
        choices=["major", "minor", "patch"],
        help="Which part of the version to increment (not used with --push-only)",
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--push",
        action="store_true",
        help="Commit, tag, and push to origin in one step",
    )
    group.add_argument(
        "--push-only",
        action="store_true",
        help="Tag the current HEAD and push without making any file edits",
    )
    args = parser.parse_args()

    if args.push_only:
        if args.part:
            parser.error("--push-only cannot be combined with a version part")
        check_git_state(require_main=True)
        push_only()
        return

    if not args.part:
        parser.error("part is required unless --push-only is given")

    check_git_state(require_main=args.push)

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

    # Gate the release on a green test suite before prompting / touching files.
    run_tests()

    author = git_author()
    if not confirm(
        f"Bump version {current} → {new} and publish release {new} "
        f"as {author}?"
    ):
        print("Aborted — no changes made")
        return

    write_version(new)
    print(f"{current} → {new}  (bumped {args.part})")
    publish(new, args.push)


if __name__ == "__main__":
    main()
