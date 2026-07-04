//! Cross-platform path helpers, keeping parity with the C++ launcher's defaults.

use std::path::PathBuf;

/// Return the path to the config file / directory.
///
/// - Windows: uses registry (see `config.rs`) — this returns a placeholder.
/// - Linux/macOS: `~/.config/GoopieLauncher/config.ini`.
#[cfg(not(windows))]
pub fn config_file() -> PathBuf {
    // `GOOPIE_CONFIG_DIR` lets the end-to-end test harness redirect config reads
    // and writes to a temp directory so they never touch the real user config.
    let base = std::env::var_os("GOOPIE_CONFIG_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            directories::BaseDirs::new().map(|d| d.config_dir().join("GoopieLauncher"))
        })
        .unwrap_or_else(|| PathBuf::from(".config/GoopieLauncher"));
    let _ = std::fs::create_dir_all(&base);
    base.join("config.ini")
}

/// Default games folder when no override is configured.
///
/// - Windows: `%LOCALAPPDATA%\Goopie\Games`
/// - Linux/macOS: `~/.local/share/Goopie/Games`
pub fn default_games_folder() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| {
            #[cfg(windows)]
            { d.data_local_dir().join("Goopie").join("Games") }
            #[cfg(not(windows))]
            { d.data_local_dir().join("Goopie").join("Games") }
        })
        .unwrap_or_else(|| PathBuf::from("Games"))
}

/// Path to the on-disk cache of the games catalogue (`{ lastUpdated, games }`),
/// written by the website (via the bridge) on every successful Firestore fetch
/// and read back when offline. Lives next to the games folder's parent so it
/// survives independently of any single game install.
pub fn games_cache_file() -> PathBuf {
    let base = directories::BaseDirs::new()
        .map(|d| d.data_local_dir().join("Goopie"))
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&base);
    base.join("games-cache.json")
}

/// Path to the on-disk per-game play-time totals (`{ games: { [recompName]:
/// { totalSeconds, lastPlayedAt } } }`), written locally by the launcher when
/// a game session ends — never synced to the cloud (mirrors how achievements
/// are stored). Lives next to `games_cache_file()`.
pub fn playtime_file() -> PathBuf {
    let base = directories::BaseDirs::new()
        .map(|d| d.data_local_dir().join("Goopie"))
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&base);
    base.join("playtime.json")
}

/// Documents directory.
///
/// Mirrors the C++ launcher's `GetDocumentsPath_()`: prefer the user-configured
/// directory (honouring `XDG_DOCUMENTS_DIR` via `directories::UserDirs`), but
/// fall back to `$HOME/Documents` when none is configured — e.g. on minimal
/// Linux setups without `~/.config/user-dirs.dirs`. Without this fallback,
/// `document_dir()` returns `None` and every save operation silently fails.
///
/// NOTE: this is *not* where game saves live on non-Windows platforms (see
/// [`rex_user_folder`], which delegates to this on Windows only — hence the
/// `allow(dead_code)` for non-Windows builds where nothing else calls it).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn documents_dir() -> Option<PathBuf> {
    if let Some(dir) = directories::UserDirs::new().and_then(|d| d.document_dir().map(|p| p.to_path_buf())) {
        return Some(dir);
    }
    #[cfg(not(windows))]
    {
        return Some(PathBuf::from(std::env::var("HOME").ok()?).join("Documents"));
    }
    #[cfg(windows)]
    {
        None
    }
}

/// Base directory under which the Rex runtime stores per-game user data
/// (saves, headers, shader cache, …) — i.e. `<rex_user_folder>/<recompName>/...`.
///
/// Mirrors `rex::filesystem::GetUserFolder()` from the recomp runtime
/// (rexglue-sdk `src/core/filesystem_{win,posix}.cpp`):
/// - Windows: `FOLDERID_Documents` (the OS Documents directory).
/// - Linux/macOS: `$XDG_DATA_HOME`, falling back to `$HOME/.local/share`.
///
/// This is deliberately *different* from [`documents_dir`] on non-Windows —
/// using Documents there means save backup/restore silently targets a path the
/// game never writes to.
pub fn rex_user_folder() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        documents_dir()
    }
    #[cfg(not(windows))]
    {
        rex_user_folder_from_env(|key| std::env::var(key).ok())
    }
}

/// Env-driven implementation of [`rex_user_folder`] for non-Windows platforms,
/// parameterized so it can be exercised in tests without touching real env vars.
#[cfg(not(windows))]
pub(crate) fn rex_user_folder_from_env(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(xdg) = get("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    Some(PathBuf::from(get("HOME")?).join(".local").join("share"))
}

/// Vehicle save base path for Nuts & Bolts.
///
/// Stores: `<home>/renut/B13EBABEBABEBABE/4D5307ED`
pub fn vehicle_save_base() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        directories::UserDirs::new()?
            .document_dir()
            .map(|p| p.join("renut").join("B13EBABEBABEBABE").join("4D5307ED"))
    }
    #[cfg(not(windows))]
    {
        Some(
            PathBuf::from(std::env::var("HOME").ok()?)
                .join("renut")
                .join("B13EBABEBABEBABE")
                .join("4D5307ED"),
        )
    }
}
#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
    }

    #[test]
    fn rex_user_folder_prefers_xdg_data_home() {
        let got = rex_user_folder_from_env(env(&[
            ("XDG_DATA_HOME", "/custom/data"),
            ("HOME", "/home/someone"),
        ]));
        assert_eq!(got, Some(PathBuf::from("/custom/data")));
    }

    #[test]
    fn rex_user_folder_falls_back_to_home_local_share() {
        let got = rex_user_folder_from_env(env(&[("HOME", "/home/someone")]));
        assert_eq!(got, Some(PathBuf::from("/home/someone/.local/share")));
    }

    #[test]
    fn rex_user_folder_ignores_empty_xdg_data_home() {
        let got = rex_user_folder_from_env(env(&[
            ("XDG_DATA_HOME", ""),
            ("HOME", "/home/someone"),
        ]));
        assert_eq!(got, Some(PathBuf::from("/home/someone/.local/share")));
    }

    #[test]
    fn rex_user_folder_none_without_home_or_xdg() {
        let got = rex_user_folder_from_env(env(&[]));
        assert_eq!(got, None);
    }
}
