//! Create and check for game-specific desktop shortcuts.
//!
//! - **Windows**: `.lnk` files on the user's Desktop via the `mslnk` crate.
//! - **Linux**: `.desktop` files in `~/.local/share/applications/`.
//!
//! The shortcut launches the launcher itself with `--play <recompName>`.
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

/// Sanitize a title for use as a filename by replacing characters that are
/// illegal on Windows (`\ / : * ? " < > |`) with dashes.
#[cfg(windows)]
fn sanitize_filename(title: &str) -> String {
    title.chars().map(|c| match c {
        '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
        _ => c,
    }).collect()
}

// ── Windows implementation ──────────────────────────────────────────────────

#[cfg(windows)]
fn desktop_dir() -> Option<PathBuf> {
    directories::UserDirs::new().and_then(|d| d.desktop_dir().map(|p| p.to_path_buf()))
}

#[cfg(windows)]
fn shortcut_path(title: &str) -> Option<PathBuf> {
    desktop_dir().map(|d| d.join(format!("{}.lnk", sanitize_filename(title))))
}

#[cfg(windows)]
pub fn exists(_game: &str, title: &str) -> bool {
    shortcut_path(title).map(|p| p.exists()).unwrap_or(false)
}

#[cfg(windows)]
pub fn create(game: &str, title: &str, icon_url: &str) -> Result<(), String> {
    let exe = launcher_exe()?;

    let lnk_path = shortcut_path(title)
        .ok_or_else(|| "Could not determine Desktop directory".to_string())?;

    let icon_path = if let Some(png) = resolve_icon_png(game, icon_url) {
        // Keep the raw PNG on disk too
        let png_path = games::game_root(game).join("assets").join(".shortcut-icon.png");
        if let Err(e) = std::fs::write(&png_path, &png) {
            eprintln!("[shortcuts] Failed to write .png: {}", e);
        }

        let ico_path = games::game_root(game).join("assets").join(".shortcut-icon.ico");
        let ico_data = png_to_ico(&png);
        if let Err(e) = std::fs::write(&ico_path, &ico_data) {
            eprintln!("[shortcuts] Failed to write .ico: {}", e);
            None
        } else {
            Some(ico_path)
        }
    } else {
        None
    };

    let mut sl = mslnk::ShellLink::new(&exe)
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
    if let Some(ref icon) = icon_path {
        sl.set_icon_location(Some(icon.to_string_lossy().into_owned()));
    }
    sl.create_lnk(&lnk_path)
        .map_err(|e| format!("Failed to write .lnk: {}", e))?;

    eprintln!("[shortcuts] Created: {}", lnk_path.display());
    Ok(())
}

// ── Linux implementation ────────────────────────────────────────────────────

#[cfg(not(windows))]
fn applications_dir() -> PathBuf {
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
fn desktop_file_path(game: &str) -> PathBuf {
    applications_dir().join(format!("goopie-{}.desktop", game))
}

#[cfg(not(windows))]
pub fn exists(game: &str, _title: &str) -> bool {
    desktop_file_path(game).exists()
}

#[cfg(not(windows))]
pub fn create(game: &str, title: &str, icon_url: &str) -> Result<(), String> {
    let exe = launcher_exe()?;

    let icon_path = if let Some(png) = resolve_icon_png(game, icon_url) {
        let png_path = games::game_root(game).join("assets").join(".shortcut-icon.png");
        if let Err(e) = std::fs::write(&png_path, &png) {
            eprintln!("[shortcuts] Failed to write icon PNG: {}", e);
            None
        } else {
            Some(png_path)
        }
    } else {
        None
    };

    let exe_str = exe.to_string_lossy();
    let mut desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={}\n\
         Exec=\"{}\" --play {}{}\n\
         Categories=Game;\n\
         Terminal=false\n",
        title, exe_str, game,
        if is_local_mode() { " --local" } else { "" },
    );

    if let Some(ref icon) = icon_path {
        desktop.push_str(&format!("Icon={}\n", icon.to_string_lossy()));
    }

    let apps_dir = applications_dir();
    if let Err(e) = std::fs::create_dir_all(&apps_dir) {
        return Err(format!("Failed to create applications dir: {}", e));
    }

    let desktop_path = desktop_file_path(game);
    std::fs::write(&desktop_path, &desktop)
        .map_err(|e| format!("Failed to write .desktop file: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&desktop_path, std::fs::Permissions::from_mode(0o755));
    }

    eprintln!("[shortcuts] Created: {}", desktop_path.display());
    Ok(())
}
