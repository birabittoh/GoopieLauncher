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
