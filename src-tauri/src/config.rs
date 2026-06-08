//! Persistent configuration: games folder path and UI language preference.
//!
//! - Windows: stored in the registry under `HKCU\Software\GoopieLauncher`,
//!   keeping 1:1 parity with the C++ launcher so existing installs are found.
//! - Linux/macOS: stored in `~/.config/GoopieLauncher/config.ini` (`key=value`).

use crate::paths;

// ── Games folder ──────────────────────────────────────────────────────────────

pub fn get_games_folder() -> String {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\GoopieLauncher")
        {
            if let Ok(s) = key.get_value::<String, _>("GamesPath") {
                if !s.is_empty() {
                    return s;
                }
            }
        }
        paths::default_games_folder().to_string_lossy().into_owned()
    }
    #[cfg(not(windows))]
    {
        let val = ini_read("GamesPath", "");
        if !val.is_empty() {
            return val;
        }
        paths::default_games_folder().to_string_lossy().into_owned()
    }
}

pub fn set_games_path(path: &str) {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey("Software\\GoopieLauncher")
        {
            let _ = key.set_value("GamesPath", &path.to_string());
        }
    }
    #[cfg(not(windows))]
    {
        ini_write("GamesPath", path);
    }
}

// ── Language ──────────────────────────────────────────────────────────────────

/// Returns the stored language integer (default 1 = English).
pub fn get_language() -> i32 {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\GoopieLauncher")
        {
            if let Ok(v) = key.get_value::<u32, _>("UserLanguage") {
                return v as i32;
            }
        }
        1
    }
    #[cfg(not(windows))]
    {
        ini_read("UserLanguage", "1").parse().unwrap_or(1)
    }
}

pub fn set_language(lang: i32) {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey("Software\\GoopieLauncher")
        {
            let _ = key.set_value("UserLanguage", &(lang as u32));
        }
    }
    #[cfg(not(windows))]
    {
        ini_write("UserLanguage", &lang.to_string());
    }
}

// ── Offline mode preference ───────────────────────────────────────────────────

/// Whether the user has explicitly switched the launcher into offline mode
/// (via the toggle button). Defaults to `false` (prefer online).
///
/// This is the user's *wish*, not the effective mode: when it's `false` (the
/// user wants the live site) but `goopie.xyz` can't be reached, the launcher
/// still falls back to the embedded offline bundle for that launch — without
/// touching this persisted preference, so it recovers automatically once
/// connectivity returns. When it's `true`, the launcher stays offline
/// unconditionally until the user flips it back — connectivity is irrelevant.
///
/// Stored via the same mechanism as the games-folder/language settings, so it
/// survives launcher restarts exactly like those do.
pub fn get_offline_mode_preference() -> bool {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\GoopieLauncher")
            .ok()
            .and_then(|key| key.get_value::<u32, _>("OfflineMode").ok())
            .map(|v| v != 0)
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        ini_read("OfflineMode", "0") == "1"
    }
}

pub fn set_offline_mode_preference(offline: bool) {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey("Software\\GoopieLauncher")
        {
            let _ = key.set_value("OfflineMode", &(offline as u32));
        }
    }
    #[cfg(not(windows))]
    {
        ini_write("OfflineMode", if offline { "1" } else { "0" });
    }
}

// ── Launcher-update check throttle ────────────────────────────────────────────

/// Unix timestamp (seconds) of the last time the launcher checked
/// `GOOPIE_RELEASES_API` for a newer release, or `0` if it never has.
///
/// Persisted across restarts so that re-opening the launcher repeatedly in a
/// short span doesn't fire a fresh GitHub API request every time — see
/// `launcher::spawn_update_monitor`, which only checks once the configured
/// interval has actually elapsed since this timestamp.
pub fn get_last_update_check() -> u64 {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\GoopieLauncher")
            .ok()
            .and_then(|key| key.get_value::<String, _>("LastUpdateCheck").ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
    #[cfg(not(windows))]
    {
        ini_read("LastUpdateCheck", "0").parse().unwrap_or(0)
    }
}

pub fn set_last_update_check(timestamp: u64) {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey("Software\\GoopieLauncher")
        {
            let _ = key.set_value("LastUpdateCheck", &timestamp.to_string());
        }
    }
    #[cfg(not(windows))]
    {
        ini_write("LastUpdateCheck", &timestamp.to_string());
    }
}

// ── INI helpers (non-Windows) ─────────────────────────────────────────────────

#[cfg(not(windows))]
fn ini_read(key: &str, default: &str) -> String {
    let path = paths::config_file();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return default.to_string();
    };
    for line in contents.lines() {
        if let Some(pos) = line.find('=') {
            if &line[..pos] == key {
                return line[pos + 1..].to_string();
            }
        }
    }
    default.to_string()
}

#[cfg(not(windows))]
fn ini_write(key: &str, value: &str) {
    let path = paths::config_file();
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    if let Ok(contents) = std::fs::read_to_string(&path) {
        for line in contents.lines() {
            if let Some(pos) = line.find('=') {
                if &line[..pos] == key {
                    lines.push(format!("{}={}", key, value));
                    found = true;
                    continue;
                }
            }
            lines.push(line.to_string());
        }
    }
    if !found {
        lines.push(format!("{}={}", key, value));
    }
    let _ = std::fs::write(&path, lines.join("\n") + "\n");
}
