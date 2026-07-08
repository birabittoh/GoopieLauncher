"""End-to-end test for the launcher's self-update.

It drives the *real* launcher binary headlessly (``--self-update-check``) against
a local mock release server, with the hidden ``AutoApplyUpdate`` setting turned
on, and asserts the binary on disk is replaced by the downloaded payload. The
flag's previous value is saved and restored. On Windows it also asserts the
"Programs and Features" DisplayVersion was refreshed to the new version.

Run directly (``python tests/e2e/test_self_update.py``) or via ``run_tests.py``,
which builds the debug binary first.

A third, Windows-only case covers installing to a protected path (like
``Program Files``), which requires the launcher's UAC-elevation fallback to
kick in. That case pops a real UAC consent prompt a human must approve, and
runs only when ``GOOPIE_E2E_MANUAL=1`` is set in the environment. It's on by
default when driven through ``run_tests.py`` on Windows (which is what
``set-version.py`` uses) — pass ``run_tests.py --skip-manual-e2e`` to omit
it, as CI does, so nothing hangs waiting for a prompt no one can click.
Running this file directly does *not* set the env var, so Case 3 is skipped
unless you export ``GOOPIE_E2E_MANUAL=1`` yourself first.
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
MANUAL_ENV = "GOOPIE_E2E_MANUAL"


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


def run_protected_path_case(
    binary: Path, server: MockReleaseServer, config_dir: Path, marker: str
) -> bool:
    """Install into a directory ACL'd like Program Files (no write access to
    the current non-elevated token) and confirm self-update still lands, via
    the UAC-elevation fallback in ``apply_update``. Requires a human to
    approve the UAC prompt that will appear."""
    print("\n=== Case 3 (manual): protected install path requires UAC elevation ===")
    protected_marker = uuid.uuid4().hex
    protected_dir = binary.parent.parent / f"goopie-e2e-protected-{protected_marker}"
    protected_dir.mkdir(parents=True, exist_ok=True)
    target = protected_dir / binary.name
    shutil.copy2(binary, target)

    test_uninstall_key = f"GoopieLauncherE2E-{protected_marker}-protected"
    cfg.seed_uninstall_entry(test_uninstall_key, protected_dir, "0.0.0")
    cfg.lock_directory_for_elevation(protected_dir)

    ok = True
    try:
        print("A UAC prompt should appear shortly — APPROVE it to let the test proceed.")
        print("(Waiting up to 60s for the elevated copy to complete.)")
        proc = run_check(target, server.releases_url, config_dir)

        def replaced() -> bool:
            try:
                return marker.encode() in target.read_bytes()
            except OSError:
                return False

        if not wait_until(replaced, timeout=60.0):
            ok = False
            print(
                "FAIL: binary in protected path was not replaced — did you "
                "approve the UAC prompt?",
                file=sys.stderr,
            )
            print(f"  stdout={proc.stdout!r} stderr={proc.stderr!r}", file=sys.stderr)
        else:
            print("PASS: self-update elevated past the protected ACL and replaced the binary")

        updated = wait_until(
            lambda: cfg.read_uninstall_display_version(test_uninstall_key) == NEW_VERSION,
            timeout=15.0,
        )
        if updated:
            print(f"PASS: Control Panel DisplayVersion updated to {NEW_VERSION} (protected path)")
        else:
            ok = False
            got = cfg.read_uninstall_display_version(test_uninstall_key)
            print(f"FAIL: DisplayVersion not updated for protected path (got {got!r})", file=sys.stderr)
    finally:
        cfg.unlock_directory(protected_dir)
        cfg.delete_uninstall_entry(test_uninstall_key)
        shutil.rmtree(protected_dir, ignore_errors=True)

    return ok


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
    update_cache = cfg.UpdateCheckSettings(config_dir=None if IS_WINDOWS else config_dir)
    update_cache.save()

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

        # ── Case 3 (manual): protected install path requires UAC elevation ────
        if IS_WINDOWS and os.environ.get(MANUAL_ENV) == "1":
            auto.set(True)  # Case 2 above left it off.
            ok = run_protected_path_case(binary, server, config_dir, marker) and ok
        elif IS_WINDOWS:
            print(
                f"\nSKIP: protected-path elevation case requires manual UAC "
                f"approval; set {MANUAL_ENV}=1 to run it locally."
            )
    finally:
        auto.restore()
        update_cache.restore()
        if IS_WINDOWS:
            cfg.delete_uninstall_entry(test_uninstall_key)
        server.stop()
        shutil.rmtree(workdir, ignore_errors=True)

    return ok


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
