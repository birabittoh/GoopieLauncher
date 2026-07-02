//! Create and check for game-specific shortcuts.
//!
//! Two shortcut types are supported:
//! - **Desktop**: placed on the user's Desktop.
//! - **Applications**: placed in the system application launcher
//!   (`~/.local/share/applications/` on Linux, Start Menu on Windows).
//!
//! Each shortcut launches the launcher itself with `--play <recompName>`.
//! The launcher + website then resolve the current build, cvars, and all other
//! options at runtime — nothing is frozen at shortcut-creation time.

use std::path::PathBuf;

use crate::games;

// ── Icon helpers ────────────────────────────────────────────────────────────

/// Wrap raw PNG bytes into a minimal `.ico` container (single-image,
/// PNG-compressed — supported on Vista+).
#[cfg(windows)]
fn png_to_ico(png: &[u8]) -> Vec<u8> {
    let mut ico = Vec::with_capacity(6 + 16 + png.len());
    // ICONDIR
    ico.extend_from_slice(&[0, 0]); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type = ICO
    ico.extend_from_slice(&1u16.to_le_bytes()); // count = 1
    // ICONDIRENTRY — width/height 0 = 256+, planes=1, bpp=32
    ico.push(0);
    ico.push(0);
    ico.push(0);
    ico.push(0);
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes()); // offset = 6 + 16
    ico.extend_from_slice(png);
    ico
}

/// Try to obtain icon bytes (PNG) for a game.
///
/// 1. If `icon_url` is set, download it.
/// 2. Otherwise, extract the title image from `<game_root>/assets/default.xex`.
/// 3. Returns `None` on any failure (caller proceeds without icon).
fn resolve_icon_png(game: &str, icon_url: &str) -> Option<Vec<u8>> {
    if !icon_url.is_empty() {
        let client = reqwest::blocking::Client::builder()
            .user_agent("Goopie-Launcher/2")
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .ok()?;
        let resp = client.get(icon_url).send().ok()?;
        if resp.status().is_success() {
            return resp.bytes().ok().map(|b| b.to_vec());
        }
        eprintln!("[shortcuts] Failed to download icon URL: {}", icon_url);
        return None;
    }

    let xex_path = games::game_root(game).join("assets").join("default.xex");
    match crate::extract::xex::extract_title_image(&xex_path) {
        Ok(png) => Some(png),
        Err(e) => {
            eprintln!("[shortcuts] XEX icon extraction failed: {}", e);
            None
        }
    }
}

// ── Common helpers ──────────────────────────────────────────────────────────

/// Path to the running launcher executable.
///
/// When running inside an AppImage, `current_exe()` resolves to the binary
/// extracted into the temporary squashfs mount, not the `.AppImage` file the
/// user actually has on disk. The AppImage runtime always sets `$APPIMAGE` to
/// the real file path, so prefer that when available.
fn launcher_exe() -> Result<PathBuf, String> {
    if let Ok(appimage) = std::env::var("APPIMAGE") {
        return Ok(PathBuf::from(appimage));
    }
    std::env::current_exe().map_err(|e| format!("Could not determine launcher path: {}", e))
}

/// Whether the launcher was started with `--local`.
fn is_local_mode() -> bool {
    std::env::args().any(|a| a == "--local")
}

// ── Windows implementation ──────────────────────────────────────────────────

/// Sanitize a title for use as a filename by replacing characters that are
/// illegal on Windows (`\ / : * ? " < > |`) with dashes.
#[cfg(windows)]
fn sanitize_filename(title: &str) -> String {
    title.chars().map(|c| match c {
        '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
        _ => c,
    }).collect()
}

#[cfg(windows)]
fn windows_desktop_dir() -> Option<PathBuf> {
    directories::UserDirs::new().and_then(|d| d.desktop_dir().map(|p| p.to_path_buf()))
}

/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs`
#[cfg(windows)]
fn windows_startmenu_dir() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|appdata| {
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
    })
}

#[cfg(windows)]
fn windows_lnk_path(dir: Option<PathBuf>, title: &str) -> Option<PathBuf> {
    dir.map(|d| d.join(format!("{}.lnk", sanitize_filename(title))))
}

/// Resolve and cache the icon for a game, returning the `.ico` path.
#[cfg(windows)]
fn windows_icon_path(game: &str, icon_url: &str) -> Option<PathBuf> {
    let png = resolve_icon_png(game, icon_url)?;
    let png_path = games::game_root(game).join("assets").join(".shortcut-icon.png");
    if let Err(e) = std::fs::write(&png_path, &png) {
        eprintln!("[shortcuts] Failed to write .png: {}", e);
    }
    let ico_path = games::game_root(game).join("assets").join(".shortcut-icon.ico");
    let ico_data = png_to_ico(&png);
    if let Err(e) = std::fs::write(&ico_path, &ico_data) {
        eprintln!("[shortcuts] Failed to write .ico: {}", e);
        return None;
    }
    Some(ico_path)
}

#[cfg(windows)]
fn write_lnk(lnk_path: &PathBuf, exe: &PathBuf, game: &str, icon_path: Option<&PathBuf>) -> Result<(), String> {
    let mut sl = mslnk::ShellLink::new(exe)
        .map_err(|e| format!("Failed to create ShellLink: {}", e))?;
    let args = if is_local_mode() {
        format!("--play {} --local", game)
    } else {
        format!("--play {}", game)
    };
    sl.set_arguments(Some(args));
    if let Some(parent) = exe.parent() {
        sl.set_working_dir(Some(parent.to_string_lossy().into_owned()));
    }
    if let Some(icon) = icon_path {
        sl.set_icon_location(Some(icon.to_string_lossy().into_owned()));
    }
    if let Some(parent) = lnk_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create shortcut directory: {}", e))?;
    }
    sl.create_lnk(lnk_path)
        .map_err(|e| format!("Failed to write .lnk: {}", e))?;
    Ok(())
}

#[cfg(windows)]
pub fn exists_desktop(_game: &str, title: &str) -> bool {
    windows_lnk_path(windows_desktop_dir(), title).map(|p| p.exists()).unwrap_or(false)
}

#[cfg(windows)]
pub fn exists_applications(_game: &str, title: &str) -> bool {
    windows_lnk_path(windows_startmenu_dir(), title).map(|p| p.exists()).unwrap_or(false)
}

#[cfg(windows)]
pub fn create_desktop(game: &str, title: &str, icon_url: &str) -> Result<(), String> {
    let exe = launcher_exe()?;
    let path = windows_lnk_path(windows_desktop_dir(), title)
        .ok_or_else(|| "Could not determine Desktop directory".to_string())?;
    let icon = windows_icon_path(game, icon_url);
    write_lnk(&path, &exe, game, icon.as_ref())?;
    eprintln!("[shortcuts] Created desktop: {}", path.display());
    Ok(())
}

#[cfg(windows)]
pub fn create_applications(game: &str, title: &str, icon_url: &str) -> Result<(), String> {
    let exe = launcher_exe()?;
    let path = windows_lnk_path(windows_startmenu_dir(), title)
        .ok_or_else(|| "Could not determine Start Menu directory".to_string())?;
    let icon = windows_icon_path(game, icon_url);
    write_lnk(&path, &exe, game, icon.as_ref())?;
    eprintln!("[shortcuts] Created start menu: {}", path.display());
    Ok(())
}

#[cfg(windows)]
pub fn remove_desktop(_game: &str, title: &str) -> Result<(), String> {
    if let Some(path) = windows_lnk_path(windows_desktop_dir(), title) {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| format!("Failed to remove shortcut: {}", e))?;
            eprintln!("[shortcuts] Removed desktop: {}", path.display());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn remove_applications(_game: &str, title: &str) -> Result<(), String> {
    if let Some(path) = windows_lnk_path(windows_startmenu_dir(), title) {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| format!("Failed to remove shortcut: {}", e))?;
            eprintln!("[shortcuts] Removed start menu: {}", path.display());
        }
    }
    Ok(())
}

// ── Linux implementation ────────────────────────────────────────────────────

#[cfg(not(windows))]
fn xdg_desktop_dir() -> PathBuf {
    // directories::UserDirs reads ~/.config/user-dirs.dirs, which is where
    // the localized desktop folder name (e.g. "Scrivania") is defined.
    // $XDG_DESKTOP_DIR is rarely exported as an env var, so don't rely on it.
    directories::UserDirs::new()
        .and_then(|u| u.desktop_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
                .join("Desktop")
        })
}

#[cfg(not(windows))]
fn xdg_applications_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
                .join(".local")
                .join("share")
        });
    base.join("applications")
}

#[cfg(not(windows))]
fn desktop_filename(game: &str) -> String {
    format!("goopie-{}.desktop", game)
}

/// Write a `.desktop` file to `path`, creating parent directories as needed.
#[cfg(not(windows))]
fn write_desktop_file(path: &PathBuf, game: &str, title: &str, exe: &PathBuf, icon_path: Option<&PathBuf>) -> Result<(), String> {
    let exe_str = exe.to_string_lossy();
    let mut contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={}\n\
         Exec=\"{}\" --play {}{}\n\
         Categories=Game;\n\
         Terminal=false\n",
        title, exe_str, game,
        if is_local_mode() { " --local" } else { "" },
    );
    if let Some(icon) = icon_path {
        contents.push_str(&format!("Icon={}\n", icon.to_string_lossy()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create shortcut directory: {}", e))?;
    }
    std::fs::write(path, &contents)
        .map_err(|e| format!("Failed to write .desktop file: {}", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Resolve and cache the icon PNG for a game, returning its on-disk path.
#[cfg(not(windows))]
fn linux_icon_path(game: &str, icon_url: &str) -> Option<PathBuf> {
    let png = resolve_icon_png(game, icon_url)?;
    let png_path = games::game_root(game).join("assets").join(".shortcut-icon.png");
    if let Err(e) = std::fs::write(&png_path, &png) {
        eprintln!("[shortcuts] Failed to write icon PNG: {}", e);
        return None;
    }
    Some(png_path)
}

#[cfg(not(windows))]
pub fn exists_desktop(game: &str, _title: &str) -> bool {
    xdg_desktop_dir().join(desktop_filename(game)).exists()
}

#[cfg(not(windows))]
pub fn exists_applications(game: &str, _title: &str) -> bool {
    xdg_applications_dir().join(desktop_filename(game)).exists()
}

#[cfg(not(windows))]
pub fn create_desktop(game: &str, title: &str, icon_url: &str) -> Result<(), String> {
    let exe = launcher_exe()?;
    let icon = linux_icon_path(game, icon_url);
    let path = xdg_desktop_dir().join(desktop_filename(game));
    write_desktop_file(&path, game, title, &exe, icon.as_ref())?;
    eprintln!("[shortcuts] Created desktop: {}", path.display());
    Ok(())
}

#[cfg(not(windows))]
pub fn create_applications(game: &str, title: &str, icon_url: &str) -> Result<(), String> {
    let exe = launcher_exe()?;
    let icon = linux_icon_path(game, icon_url);
    let path = xdg_applications_dir().join(desktop_filename(game));
    write_desktop_file(&path, game, title, &exe, icon.as_ref())?;
    eprintln!("[shortcuts] Created applications: {}", path.display());
    Ok(())
}

#[cfg(not(windows))]
pub fn remove_desktop(game: &str, _title: &str) -> Result<(), String> {
    let path = xdg_desktop_dir().join(desktop_filename(game));
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("Failed to remove shortcut: {}", e))?;
        eprintln!("[shortcuts] Removed desktop: {}", path.display());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn remove_applications(game: &str, _title: &str) -> Result<(), String> {
    let path = xdg_applications_dir().join(desktop_filename(game));
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("Failed to remove shortcut: {}", e))?;
        eprintln!("[shortcuts] Removed applications: {}", path.display());
    }
    Ok(())
}
