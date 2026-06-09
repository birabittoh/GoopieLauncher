#!/usr/bin/env python3
"""Run the project's unit tests and end-to-end tests.

Phases:
  1. Rust unit tests  (cargo test)
  2. Build the debug launcher binary the e2e tests drive (cargo build)
  3. End-to-end tests  (tests/e2e/*)

Exits non-zero if any phase fails. ``set-version.py`` runs this before cutting a
release, so a red test blocks the publish.

Usage:
  python run_tests.py            # unit + build + e2e
  python run_tests.py --unit-only
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SRC_TAURI = ROOT / "src-tauri"
IS_WINDOWS = sys.platform.startswith("win")


def run(cmd: list[str], **kwargs) -> None:
    print(f"\n$ {' '.join(cmd)}", flush=True)
    subprocess.run(cmd, check=True, **kwargs)


def cargo_env() -> dict[str, str]:
    # The releases API resolves at runtime now, so unit tests and the debug
    # build compile fine without the CI secret. Drop it if present so the build
    # doesn't bake a real endpoint into the test binary.
    env = dict(os.environ)
    env.pop("GOOPIE_RELEASES_API", None)
    return env


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--unit-only", action="store_true", help="Run only the Rust unit tests"
    )
    args = parser.parse_args()

    env = cargo_env()
    manifest = str(SRC_TAURI / "Cargo.toml")

    # 1. Unit tests.
    run(["cargo", "test", "--manifest-path", manifest], env=env)

    if args.unit_only:
        print("\nUnit tests passed (--unit-only).")
        return 0

    # 2. Build the debug binary the e2e tests run.
    run(["cargo", "build", "--manifest-path", manifest], env=env)

    bin_name = "goopie-launcher.exe" if IS_WINDOWS else "goopie-launcher"
    binary = SRC_TAURI / "target" / "debug" / bin_name
    if not binary.exists():
        print(f"error: expected launcher binary at {binary}", file=sys.stderr)
        return 1

    # 3. End-to-end tests.
    e2e_env = dict(os.environ)
    e2e_env["GOOPIE_LAUNCHER_BIN"] = str(binary)
    run([sys.executable, str(ROOT / "tests" / "e2e" / "test_self_update.py")], env=e2e_env)

    print("\nAll tests passed.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except subprocess.CalledProcessError as exc:
        print(f"\nTest phase failed: {' '.join(exc.cmd)} (exit {exc.returncode})", file=sys.stderr)
        sys.exit(1)
