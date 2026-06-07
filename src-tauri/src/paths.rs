//! Cross-platform path helpers, keeping parity with the C++ launcher's defaults.

use std::path::PathBuf;

/// Return the path to the config file / directory.
///
/// - Windows: uses registry (see `config.rs`) — this returns a placeholder.
/// - Linux/macOS: `~/.config/GoopieLauncher/config.ini`.
#[cfg(not(windows))]
pub fn config_file() -> PathBuf {
    let base = directories::BaseDirs::new()
        .map(|d| d.config_dir().join("GoopieLauncher"))
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

/// Documents directory (for save backups / restore).
///
/// Mirrors the C++ launcher's `GetDocumentsPath_()`: prefer the user-configured
/// directory (honouring `XDG_DOCUMENTS_DIR` via `directories::UserDirs`), but
/// fall back to `$HOME/Documents` when none is configured — e.g. on minimal
/// Linux setups without `~/.config/user-dirs.dirs`. Without this fallback,
/// `document_dir()` returns `None` and every save operation silently fails.
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
