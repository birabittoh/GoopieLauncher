r"""Manual test: self-update from a UAC-protected directory.

Copies the debug binary into C:\Program Files\GoopieLauncherTest\,
starts a mock release server, enables AutoApplyUpdate, and runs
--self-update-check.  The initial unelevated copy should fail and the
launcher should retry via UAC elevation.

Run from a normal (non-admin) terminal:
    python tests/e2e/test_protected_dir.py

What to look for:
  - UAC prompts appear (accept them).
  - NO console window flashes at any point.
  - The script prints PASS when the binary is replaced.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mock_release_server import MockReleaseServer  # noqa: E402
import util_config as cfg  # noqa: E402

PROTECTED_DIR = Path(os.environ.get(
    "GOOPIE_TEST_DIR",
    r"C:\Program Files\GoopieLauncherTest",
))
NEW_TAG = "v9999.0.0"
NEW_VERSION = "9999.0.0"
ASSET_NAME = "Goopie-Launcher-windows-x86_64.exe"


def launcher_binary() -> Path:
    if env := os.environ.get("GOOPIE_LAUNCHER_BIN"):
        return Path(env)
    return Path(__file__).resolve().parents[2] / "src-tauri" / "target" / "debug" / "goopie-launcher.exe"


def _run_elevated(ps_command: str, label: str = "test setup") -> None:
    """Run *ps_command* elevated via Start-Process -Verb RunAs.

    Writes the command to a temp .ps1 file to avoid nested-quoting
    issues. The PowerShell window is deliberately kept VISIBLE with a
    bright title so the tester can distinguish test-harness windows from
    any window the launcher itself might open (which would be a bug).
    """
    title = f"[E2E TEST] {label}"
    script = tempfile.NamedTemporaryFile(
        suffix=".ps1", prefix="goopie-e2e-", delete=False, mode="w", encoding="utf-8",
    )
    script.write(
        f"$Host.UI.RawUI.WindowTitle = '{title}'\n"
        f"try {{\n"
        f"  {ps_command}\n"
        f"}} catch {{\n"
        f"  Write-Host ('ERROR: ' + $_) -ForegroundColor Red\n"
        f"  Write-Host 'Press any key to close...'\n"
        f"  $null = $Host.UI.RawUI.ReadKey('NoEcho,IncludeKeyDown')\n"
        f"  exit 1\n"
        f"}}\n"
        f"Start-Sleep -Milliseconds 600\n"
    )
    script.close()
    try:
        subprocess.call([
            "powershell", "-NoProfile", "-Command",
            f"Start-Process powershell -Verb RunAs -Wait "
            f"-ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File','{script.name}')",
        ])
    finally:
        os.unlink(script.name)


def main() -> int:
    binary = launcher_binary()
    if not binary.exists():
        print(f"FAIL: binary not found at {binary}", file=sys.stderr)
        return 1

    marker = uuid.uuid4().hex
    payload = b"GOOPIE_E2E_PAYLOAD " + marker.encode()
    test_uninstall_key = f"GoopieLauncherE2E-{marker}"

    # ── Create the protected directory + copy binary (elevated) ────────
    # These windows are VISIBLE and titled "[E2E TEST] ..." so you can
    # distinguish them from any window the launcher itself might spawn.
    if not PROTECTED_DIR.exists():
        print(f"Creating {PROTECTED_DIR} (UAC prompt)...")
        _run_elevated(
            f"New-Item -ItemType Directory -Force '{PROTECTED_DIR}'",
            label="mkdir",
        )
        if not PROTECTED_DIR.exists():
            print("FAIL: could not create protected directory", file=sys.stderr)
            return 1

    dest = PROTECTED_DIR / binary.name
    print(f"Copying binary to {dest} (UAC prompt)...")
    _run_elevated(
        f"Copy-Item -Force '{binary}' '{dest}'",
        label="copy binary",
    )
    if not dest.exists():
        print("FAIL: could not copy binary to protected directory", file=sys.stderr)
        return 1

    # ── Set up config and mock server ────────────────────────────────────
    auto = cfg.AutoApplySetting()
    auto.save()
    update_cache = cfg.UpdateCheckSettings()
    update_cache.save()
    auto.set(True)

    cfg.seed_uninstall_entry(test_uninstall_key, PROTECTED_DIR, "0.0.0")

    server = MockReleaseServer(ASSET_NAME, payload, tag=NEW_TAG).start()
    ok = True

    try:
        print(f"\nMock server at {server.releases_url}")
        print(f"Running --self-update-check from {dest}")
        print(">>> A UAC prompt should appear — accept it.")
        print(">>> NO console window should flash.\n")

        env = dict(os.environ)
        env["GOOPIE_RELEASES_API"] = server.releases_url
        proc = subprocess.run(
            [str(dest), "--self-update-check"],
            env=env,
            capture_output=True,
            text=True,
            timeout=120,
        )
        print(f"  exit code: {proc.returncode}")
        if proc.stdout.strip():
            print(f"  stdout: {proc.stdout.strip()}")
        if proc.stderr.strip():
            print(f"  stderr: {proc.stderr.strip()}")

        print("\nWaiting for async apply (up to 30s)...")
        deadline = time.time() + 30.0
        replaced = False
        while time.time() < deadline:
            try:
                if marker.encode() in dest.read_bytes():
                    replaced = True
                    break
            except OSError:
                pass
            time.sleep(1.0)

        if replaced:
            print("PASS: binary was replaced by the update payload")
        else:
            ok = False
            print("FAIL: binary was NOT replaced after 30s", file=sys.stderr)

        # Check DisplayVersion
        dv = cfg.read_uninstall_display_version(test_uninstall_key)
        if dv == NEW_VERSION:
            print(f"PASS: DisplayVersion updated to {NEW_VERSION}")
        else:
            ok = False
            print(f"FAIL: DisplayVersion is {dv!r}, expected {NEW_VERSION!r}", file=sys.stderr)

    finally:
        auto.restore()
        update_cache.restore()
        cfg.delete_uninstall_entry(test_uninstall_key)
        server.stop()
        print(f"\nCleaning up {PROTECTED_DIR}...")
        _run_elevated(
            f"Remove-Item -Recurse -Force '{PROTECTED_DIR}'",
            label="cleanup",
        )

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
