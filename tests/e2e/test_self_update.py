"""End-to-end test for the launcher's self-update.

It drives the *real* launcher binary headlessly (``--self-update-check``) against
a local mock release server, with the hidden ``AutoApplyUpdate`` setting turned
on, and asserts the binary on disk is replaced by the downloaded payload. The
flag's previous value is saved and restored. On Windows it also asserts the
"Programs and Features" DisplayVersion was refreshed to the new version.

Run directly (``python tests/e2e/test_self_update.py``) or via ``run_tests.py``,
which builds the debug binary first.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mock_release_server import MockReleaseServer  # noqa: E402
import util_config as cfg  # noqa: E402

IS_WINDOWS = sys.platform.startswith("win")
NEW_TAG = "v9999.0.0"
NEW_VERSION = "9999.0.0"


def launcher_binary() -> Path:
    if env := os.environ.get("GOOPIE_LAUNCHER_BIN"):
        return Path(env)
    name = "goopie-launcher.exe" if IS_WINDOWS else "goopie-launcher"
    return Path(__file__).resolve().parents[2] / "src-tauri" / "target" / "debug" / name


def asset_name() -> str:
    # Must satisfy launcher::is_target_asset for this platform (version-agnostic).
    return (
        "Goopie-Launcher-windows-x86_64.exe"
        if IS_WINDOWS
        else "Goopie-Launcher-linux-x86_64.AppImage"
    )


def make_payload(marker: str) -> bytes:
    if IS_WINDOWS:
        # Content is what we assert on; it need not be a valid PE.
        return b"GOOPIE_E2E_PAYLOAD " + marker.encode()
    # Runnable no-op so the post-apply relaunch spawns cleanly.
    return f"#!/bin/sh\nexit 0\n# GOOPIE_E2E_PAYLOAD {marker}\n".encode()


def run_check(binary: Path, releases_url: str, config_dir: Path | None) -> subprocess.CompletedProcess:
    env = dict(os.environ)
    env["GOOPIE_RELEASES_API"] = releases_url
    if not IS_WINDOWS:
        # APPIMAGE makes replaceable_exe_path() target our temp copy, not the
        # read-only mount path; GOOPIE_CONFIG_DIR redirects config to the temp dir.
        env["APPIMAGE"] = str(binary)
        env["GOOPIE_CONFIG_DIR"] = str(config_dir)
    return subprocess.run(
        [str(binary), "--self-update-check"],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )


def wait_until(predicate, timeout: float = 10.0, interval: float = 0.25) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


def run() -> bool:
    binary = launcher_binary()
    if not binary.exists():
        print(f"FAIL: launcher binary not found at {binary}", file=sys.stderr)
        return False

    marker = uuid.uuid4().hex
    payload = make_payload(marker)
    workdir = Path(tempfile.mkdtemp(prefix="goopie-e2e-"))
    config_dir = workdir / "config"
    config_dir.mkdir(parents=True, exist_ok=True)
    test_uninstall_key = f"GoopieLauncherE2E-{marker}"

    auto = cfg.AutoApplySetting(config_dir=None if IS_WINDOWS else config_dir)
    auto.save()

    server = MockReleaseServer(asset_name(), payload, tag=NEW_TAG).start()
    ok = True
    try:
        # ── Case 1: AutoApplyUpdate ON → the update is applied ────────────────
        under_test = workdir / binary.name
        shutil.copy2(binary, under_test)
        auto.set(True)

        if IS_WINDOWS:
            cfg.seed_uninstall_entry(test_uninstall_key, workdir, "0.0.0")

        proc = run_check(under_test, server.releases_url, config_dir)

        def replaced() -> bool:
            try:
                return marker.encode() in under_test.read_bytes()
            except OSError:
                return False

        if not wait_until(replaced, timeout=15.0):
            ok = False
            print("FAIL: binary was not replaced by the update payload", file=sys.stderr)
            print(f"  stdout={proc.stdout!r} stderr={proc.stderr!r}", file=sys.stderr)
        else:
            print("PASS: self-update replaced the binary with the new payload")

        if IS_WINDOWS:
            updated = wait_until(
                lambda: cfg.read_uninstall_display_version(test_uninstall_key) == NEW_VERSION,
                timeout=15.0,
            )
            if updated:
                print(f"PASS: Control Panel DisplayVersion updated to {NEW_VERSION}")
            else:
                ok = False
                got = cfg.read_uninstall_display_version(test_uninstall_key)
                print(f"FAIL: DisplayVersion not updated (got {got!r})", file=sys.stderr)

        # ── Case 2: AutoApplyUpdate OFF → nothing is applied ──────────────────
        under_test2 = workdir / f"off-{binary.name}"
        shutil.copy2(binary, under_test2)
        before = under_test2.read_bytes()
        auto.set(False)

        proc2 = run_check(under_test2, server.releases_url, config_dir)
        # Give any (erroneous) async apply a moment, then confirm it's untouched.
        time.sleep(1.5)
        if under_test2.read_bytes() != before:
            ok = False
            print("FAIL: binary was modified despite AutoApplyUpdate=off", file=sys.stderr)
        elif "SELFUPDATE: disabled" not in proc2.stdout or proc2.returncode != 11:
            ok = False
            print(
                f"FAIL: expected 'disabled'/exit 11, got rc={proc2.returncode} "
                f"stdout={proc2.stdout!r}",
                file=sys.stderr,
            )
        else:
            print("PASS: update not applied and reported 'disabled' when flag is off")
    finally:
        auto.restore()
        if IS_WINDOWS:
            cfg.delete_uninstall_entry(test_uninstall_key)
        server.stop()
        shutil.rmtree(workdir, ignore_errors=True)

    return ok


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
