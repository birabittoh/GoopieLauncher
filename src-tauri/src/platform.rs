//! Platform information and OS-level operations (open URL, open folder, pick folder).

use std::ffi::OsString;
use std::sync::Mutex;

/// Serializes the env-var save/restore dance in [`with_clean_appimage_env`] —
/// `std::env::set_var`/`remove_var` mutate global process state.
static APPIMAGE_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Variables the AppImage runtime points at the mounted squashfs image.
/// Child processes (xdg-open, the browser/file-manager it launches) inherit
/// them and can crash or refuse to start against the bundled libraries —
/// xdg-open then exits with status 4 ("the action failed"), surfaced as
/// `ExitStatus(unix_wait_status(1024))`.
const APPIMAGE_ENV_VARS: &[&str] = &[
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "GIO_EXTRA_MODULES",
    "GSETTINGS_SCHEMA_DIR",
    "GTK_PATH",
    "GTK_EXE_PREFIX",
    "GTK_IM_MODULE_FILE",
    "GDK_PIXBUF_MODULE_FILE",
    "QT_PLUGIN_PATH",
    "XDG_DATA_DIRS",
];

/// Runs `f` with the AppImage-injected library/module paths removed from the
/// environment, then restores them. A no-op unless `APPIMAGE` is set (i.e. we
/// are actually running from a mounted AppImage), since spawning external
/// helpers like `xdg-open` is otherwise unaffected.
fn with_clean_appimage_env<T>(f: impl FnOnce() -> T) -> T {
    if std::env::var_os("APPIMAGE").is_none() {
        return f();
    }

    let _guard = APPIMAGE_ENV_LOCK.lock().unwrap();
    let saved: Vec<(&str, Option<OsString>)> = APPIMAGE_ENV_VARS
        .iter()
        .map(|&key| (key, std::env::var_os(key)))
        .collect();

    for &key in APPIMAGE_ENV_VARS {
        std::env::remove_var(key);
    }

    let result = f();

    for (key, value) in saved {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    result
}

/// Opens `url` in the system browser, sanitizing the AppImage environment
/// first (see [`with_clean_appimage_env`]). Used where the caller needs to
/// know whether the launch succeeded (e.g. the OAuth flow).
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    with_clean_appimage_env(|| open::that(url))
}

/// Returns "Windows", "macOS", or "Linux".
pub fn get_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos"   => "macOS",
        _         => "Linux",
    }
}

/// Returns the machine architecture (e.g. "x86_64", "aarch64").
pub fn get_arch() -> &'static str {
    std::env::consts::ARCH
}

/// Open a URL in the default browser.
pub fn open_url(url: &str) {
    let _ = open_in_browser(url);
}

/// Open a directory in the system file manager, creating it if necessary.
pub fn open_folder(path: &str) {
    let _ = std::fs::create_dir_all(path);
    let _ = with_clean_appimage_env(|| open::that(path));
}

/// Returns the free space (in bytes) on the filesystem containing `path`.
/// `path` need not exist yet — only its nearest existing ancestor is queried.
/// Returns `0` if the space can't be determined (missing drive, permissions, etc.),
/// which callers should treat as "insufficient" rather than "unlimited".
pub fn available_space(path: &str) -> u64 {
    let mut probe = std::path::PathBuf::from(path);
    while !probe.exists() {
        if !probe.pop() {
            return 0;
        }
    }
    fs4::available_space(&probe).unwrap_or(0)
}

/// Show a native folder-picker dialog and return the selected path, or `None` if cancelled.
pub fn pick_folder(title: &str) -> Option<String> {
    rfd::FileDialog::new()
        .set_title(title)
        .pick_folder()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Show a native file-picker dialog for game images.
/// When `iso_only` is true, the dialog filters for `.iso` files.
pub fn pick_game_file(iso_only: bool) -> Option<String> {
    let mut dialog = rfd::FileDialog::new()
        .set_title("Select game file");
    if iso_only {
        dialog = dialog.add_filter("ISO image", &["iso"]);
    }
    dialog
        .pick_file()
        .map(|p| p.to_string_lossy().into_owned())
}

pub fn pick_game_files(iso_only: bool) -> Vec<String> {
    let mut dialog = rfd::FileDialog::new()
        .set_title("Select game file(s)");
    if iso_only {
        dialog = dialog.add_filter("ISO image", &["iso"]);
    }
    dialog
        .pick_files()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}
