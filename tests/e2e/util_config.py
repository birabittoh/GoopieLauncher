"""Cross-platform helpers to save / set / restore the hidden ``AutoApplyUpdate``
launcher setting, plus the Windows "Programs and Features" (Uninstall) registry
entries the self-update path refreshes.

The launcher stores config in the Windows registry (``HKCU\\Software\\GoopieLauncher``)
and, on Linux/macOS, in ``config.ini``. The end-to-end test must leave the
machine exactly as it found it, so every mutation here is paired with a saved
snapshot and a faithful restore (re-write the old value, or delete it if it
never existed).

On Linux the test points the launcher at a throwaway config dir via the
``GOOPIE_CONFIG_DIR`` env override, so the real user config is never touched; the
save/restore dance is still implemented for fidelity.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

IS_WINDOWS = sys.platform.startswith("win")

REG_SUBKEY = r"Software\GoopieLauncher"
AUTO_APPLY_VALUE = "AutoApplyUpdate"
LAST_KNOWN_RELEASE_TAG = "LastKnownReleaseTag"
LAST_UPDATE_CHECK = "LastUpdateCheck"


# ── Generic registry string save/restore ──────────────────────────────────────

class _RegistryStringSetting:
    """Save/restore a single REG_SZ value under ``REG_SUBKEY``."""

    def __init__(self, value_name: str, config_dir: Path | None = None) -> None:
        self.value_name = value_name
        self.config_dir = config_dir
        self._existed = False
        self._old_value: str | None = None

    def save(self) -> None:
        if IS_WINDOWS:
            import winreg
            try:
                with winreg.OpenKey(winreg.HKEY_CURRENT_USER, REG_SUBKEY) as key:
                    value, _ = winreg.QueryValueEx(key, self.value_name)
                    self._existed = True
                    self._old_value = str(value)
            except FileNotFoundError:
                self._existed = False
        else:
            line = self._read_ini_line()
            if line is not None:
                self._existed = True
                self._old_value = line

    def restore(self) -> None:
        if IS_WINDOWS:
            import winreg
            if self._existed:
                with winreg.CreateKey(winreg.HKEY_CURRENT_USER, REG_SUBKEY) as key:
                    winreg.SetValueEx(key, self.value_name, 0, winreg.REG_SZ, self._old_value or "")
            else:
                try:
                    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, REG_SUBKEY, 0, winreg.KEY_SET_VALUE) as key:
                        winreg.DeleteValue(key, self.value_name)
                except FileNotFoundError:
                    pass
        else:
            if self._existed:
                self._write_ini_line(self._old_value or "")
            else:
                self._delete_ini_line()

    def _ini_path(self) -> Path:
        assert self.config_dir is not None
        return Path(self.config_dir) / "config.ini"

    def _read_ini_line(self) -> str | None:
        path = self._ini_path()
        if not path.exists():
            return None
        for line in path.read_text().splitlines():
            if line.startswith(self.value_name + "="):
                return line.split("=", 1)[1]
        return None

    def _write_ini_line(self, value: str) -> None:
        path = self._ini_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        lines = path.read_text().splitlines() if path.exists() else []
        out, found = [], False
        for line in lines:
            if line.startswith(self.value_name + "="):
                out.append(f"{self.value_name}={value}")
                found = True
            else:
                out.append(line)
        if not found:
            out.append(f"{self.value_name}={value}")
        path.write_text("\n".join(out) + "\n")

    def _delete_ini_line(self) -> None:
        path = self._ini_path()
        if not path.exists():
            return
        out = [
            line for line in path.read_text().splitlines()
            if not line.startswith(self.value_name + "=")
        ]
        path.write_text("\n".join(out) + ("\n" if out else ""))


# ── AutoApplyUpdate ───────────────────────────────────────────────────────────

class AutoApplySetting:
    """Save/set/restore ``AutoApplyUpdate`` on the current platform.

    On Linux a ``config_dir`` (the dir holding ``config.ini``) must be given —
    pass the same path the launcher sees via ``GOOPIE_CONFIG_DIR``.
    """

    def __init__(self, config_dir: Path | None = None) -> None:
        self.config_dir = config_dir
        self._existed = False
        self._old_value: int | None = None

    def save(self) -> None:
        if IS_WINDOWS:
            import winreg

            try:
                with winreg.OpenKey(winreg.HKEY_CURRENT_USER, REG_SUBKEY) as key:
                    value, _ = winreg.QueryValueEx(key, AUTO_APPLY_VALUE)
                    self._existed = True
                    self._old_value = int(value)
            except FileNotFoundError:
                self._existed = False
        else:
            line = self._read_ini_line()
            if line is not None:
                self._existed = True
                self._old_value = 1 if line == "1" else 0

    def set(self, enabled: bool) -> None:
        if IS_WINDOWS:
            import winreg

            with winreg.CreateKey(winreg.HKEY_CURRENT_USER, REG_SUBKEY) as key:
                winreg.SetValueEx(
                    key, AUTO_APPLY_VALUE, 0, winreg.REG_DWORD, 1 if enabled else 0
                )
        else:
            self._write_ini_line("1" if enabled else "0")

    def restore(self) -> None:
        if IS_WINDOWS:
            import winreg

            if self._existed:
                with winreg.CreateKey(winreg.HKEY_CURRENT_USER, REG_SUBKEY) as key:
                    winreg.SetValueEx(
                        key, AUTO_APPLY_VALUE, 0, winreg.REG_DWORD, int(self._old_value or 0)
                    )
            else:
                try:
                    with winreg.OpenKey(
                        winreg.HKEY_CURRENT_USER, REG_SUBKEY, 0, winreg.KEY_SET_VALUE
                    ) as key:
                        winreg.DeleteValue(key, AUTO_APPLY_VALUE)
                except FileNotFoundError:
                    pass
        else:
            if self._existed:
                self._write_ini_line("1" if self._old_value else "0")
            else:
                self._delete_ini_line()

    # ── INI helpers (Linux/macOS) ────────────────────────────────────────────

    def _ini_path(self) -> Path:
        assert self.config_dir is not None, "config_dir required on non-Windows"
        return Path(self.config_dir) / "config.ini"

    def _read_ini_line(self) -> str | None:
        path = self._ini_path()
        if not path.exists():
            return None
        for line in path.read_text().splitlines():
            if line.startswith(AUTO_APPLY_VALUE + "="):
                return line.split("=", 1)[1]
        return None

    def _write_ini_line(self, value: str) -> None:
        path = self._ini_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        lines = path.read_text().splitlines() if path.exists() else []
        out, found = [], False
        for line in lines:
            if line.startswith(AUTO_APPLY_VALUE + "="):
                out.append(f"{AUTO_APPLY_VALUE}={value}")
                found = True
            else:
                out.append(line)
        if not found:
            out.append(f"{AUTO_APPLY_VALUE}={value}")
        path.write_text("\n".join(out) + "\n")

    def _delete_ini_line(self) -> None:
        path = self._ini_path()
        if not path.exists():
            return
        out = [
            line
            for line in path.read_text().splitlines()
            if not line.startswith(AUTO_APPLY_VALUE + "=")
        ]
        path.write_text("\n".join(out) + ("\n" if out else ""))


# ── Update-check cache (polluted by --self-update-check) ─────────────────────

class UpdateCheckSettings:
    """Save/restore ``LastKnownReleaseTag`` and ``LastUpdateCheck``."""

    def __init__(self, config_dir: Path | None = None) -> None:
        self._tag = _RegistryStringSetting(LAST_KNOWN_RELEASE_TAG, config_dir)
        self._ts = _RegistryStringSetting(LAST_UPDATE_CHECK, config_dir)

    def save(self) -> None:
        self._tag.save()
        self._ts.save()

    def restore(self) -> None:
        self._tag.restore()
        self._ts.restore()


# ── Windows Uninstall (Programs and Features) entry — test fixture ────────────

UNINSTALL_SUBKEY = r"Software\Microsoft\Windows\CurrentVersion\Uninstall"


def seed_uninstall_entry(test_key: str, install_location: Path, display_version: str) -> None:
    """Create a fake per-user (HKCU) Uninstall entry pointing at ``install_location``
    so the self-update's DisplayVersion refresh has something to find. HKCU needs
    no admin, so this works on a stock CI runner."""
    import winreg

    path = f"{UNINSTALL_SUBKEY}\\{test_key}"
    with winreg.CreateKey(winreg.HKEY_CURRENT_USER, path) as key:
        winreg.SetValueEx(key, "DisplayName", 0, winreg.REG_SZ, "Goopie Launcher")
        winreg.SetValueEx(
            key, "InstallLocation", 0, winreg.REG_SZ, str(install_location)
        )
        winreg.SetValueEx(key, "DisplayVersion", 0, winreg.REG_SZ, display_version)


def read_uninstall_display_version(test_key: str) -> str | None:
    import winreg

    path = f"{UNINSTALL_SUBKEY}\\{test_key}"
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, path) as key:
            value, _ = winreg.QueryValueEx(key, "DisplayVersion")
            return value
    except FileNotFoundError:
        return None


def delete_uninstall_entry(test_key: str) -> None:
    import winreg

    path = f"{UNINSTALL_SUBKEY}\\{test_key}"
    try:
        winreg.DeleteKey(winreg.HKEY_CURRENT_USER, path)
    except FileNotFoundError:
        pass


# ── Protected-path ACL simulation (Windows only) ───────────────────────────────
#
# Real Program Files denies Write to a standard (non-elevated) token but allows
# it once the process is UAC-elevated, because the Administrators group is
# present in an admin user's token but flagged "use for deny only" until they
# elevate — the user's own account SID stays fully enabled either way. These
# helpers reproduce that exact shape on a throwaway directory so a test can
# exercise the self-update's elevation fallback without touching the real
# Program Files.

_ADMINISTRATORS_SID = "*S-1-5-32-544"
_SYSTEM_SID = "*S-1-5-18"


def lock_directory_for_elevation(path: Path) -> None:
    """Strip write access to `path` for the current user's non-elevated token
    (read/execute only), while granting Administrators/SYSTEM full control —
    which only takes effect once a process is actually UAC-elevated.

    Run as two separate recursive icacls passes (strip inheritance, then
    grant), each with its own trailing ``/T``. Combining ``/inheritance:r``
    and ``/grant:r`` in a single icacls invocation does not reliably cascade
    the grants down to files that already existed inside `path` (e.g. a
    binary copied there before locking) — they end up with an empty DACL
    (nobody, not even SYSTEM, can open them) instead of the intended ACEs."""
    user = f"{os.environ.get('USERDOMAIN', '.')}\\{os.environ['USERNAME']}"
    subprocess.run(
        ["icacls", str(path), "/inheritance:r", "/T", "/C"],
        check=True,
        capture_output=True,
        text=True,
    )
    subprocess.run(
        [
            "icacls",
            str(path),
            "/grant:r",
            f"{user}:(OI)(CI)RX",
            "/grant:r",
            f"{_ADMINISTRATORS_SID}:(OI)(CI)F",
            "/grant:r",
            f"{_SYSTEM_SID}:(OI)(CI)F",
            "/T",
            "/C",
        ],
        check=True,
        capture_output=True,
        text=True,
    )


def unlock_directory(path: Path) -> None:
    """Undo `lock_directory_for_elevation`, restoring inherited permissions."""
    subprocess.run(
        ["icacls", str(path), "/reset", "/T", "/C"],
        check=False,
        capture_output=True,
        text=True,
    )
