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

import sys
from pathlib import Path

IS_WINDOWS = sys.platform.startswith("win")

REG_SUBKEY = r"Software\GoopieLauncher"
AUTO_APPLY_VALUE = "AutoApplyUpdate"


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
