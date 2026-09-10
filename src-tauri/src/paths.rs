//! Cross-platform path helpers, keeping parity with the C++ launcher's defaults.

use std::path::{Path, PathBuf};

/// Resolve `dir/name` tolerating a differently-cased `name` on disk.
///
/// Extraction normalizes casing on write, but games extracted before that and
/// files copied in by hand can carry any casing — and on Linux/Android the
/// filesystem won't paper over it the way Windows and macOS do. Returns the
/// exact-case path when it exists, otherwise the first case-insensitive match,
/// otherwise `None`.
pub fn find_case_insensitive(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if exact.exists() {
        return Some(exact);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .map(|e| e.path())
}

/// Remove a directory, a symlink to a directory, or a plain file at `path`.
///
/// `std::fs::remove_dir_all` is not enough on its own: on Unix it refuses to
/// act on a symlink (`ENOTDIR`), and while on Windows it would happily unlink
/// the reparse point, we never want to risk recursing into the *target* of a
/// user-picked assets folder. So symlinks are always unlinked, never followed.
/// Missing paths are a no-op.
pub fn remove_dir_or_link(path: &Path) -> std::io::Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(()); // nothing there
    };
    if meta.is_symlink() {
        // On Windows a directory symlink must be removed with `remove_dir`;
        // on Unix every symlink is unlinked with `remove_file`.
        #[cfg(windows)]
        {
            return std::fs::remove_dir(path).or_else(|_| std::fs::remove_file(path));
        }
        #[cfg(not(windows))]
        {
            return std::fs::remove_file(path);
        }
    }
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// `true` if something exists at `path`, without following a final symlink.
///
/// `Path::exists` resolves the link and so reports `false` for a dangling one,
/// which would then make `create_dir_all` fail with "file exists".
pub fn path_or_link_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Create a symlink at `link` pointing at the directory `target`.
///
/// Windows needs the directory-specific call (and either Developer Mode or the
/// `SeCreateSymbolicLinkPrivilege`); Unix has a single `symlink`.
pub fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link)
    }
}

/// Recursively copy `src` into `dst`, creating `dst` if needed.
pub fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

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

/// Path to the on-disk cloud-save-sync state: the Google Drive refresh token
/// (obtained via a separate, narrower-scoped consent than the main sign-in —
/// see `auth::google_sign_in`) plus per-game sync bookkeeping (`enabled`,
/// `lastSyncedHash`, `lastSyncedAt`, `driveFileId`). See `cloud_saves.rs`.
/// Lives next to `playtime_file()`.
pub fn cloud_saves_file() -> PathBuf {
    let base = directories::BaseDirs::new()
        .map(|d| d.data_local_dir().join("Goopie"))
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&base);
    base.join("cloud-saves.json")
}

/// Directory holding disk-cached game images (covers, headers, title/logo
/// art) fetched through the `goopieimg` custom URI scheme — see
/// `image_cache.rs`. Lives next to `games_cache_file()` in the same `Goopie`
/// app-data folder, in its own subdirectory since it can hold many files.
pub fn image_cache_dir() -> PathBuf {
    let base = directories::BaseDirs::new()
        .map(|d| d.data_local_dir().join("Goopie").join("image-cache"))
        .unwrap_or_else(|| PathBuf::from("image-cache"));
    let _ = std::fs::create_dir_all(&base);
    base
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
