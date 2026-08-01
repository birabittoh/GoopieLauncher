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

use std::collections::HashMap;
use std::path::PathBuf;

use image::GenericImageView;

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
        std::fs::remove_file(&path).map_err(|e| format!("Failed to remove applications shortcut: {}", e))?;
        eprintln!("[shortcuts] Removed applications: {}", path.display());
    }
    Ok(())
}

// ── Steam shortcut (shortcuts.vdf) ─────────────────────────────────────────
//
// Binary VDF parsing/writing, the shortcut-appid algorithm, and the grid
// artwork layout below are a Rust port of Graine25's VDFLib
// (https://github.com/Graine25/VDFLib, MIT), written independently for this
// project's own I/O and data model rather than binding to it directly.


/// Binary VDF type markers used in Steam's shortcuts.vdf.
const VDF_OBJECT: u8 = 0x00;
const VDF_STRING: u8 = 0x01;
const VDF_INT32: u8 = 0x02;
const VDF_END: u8 = 0x08;

/// Fields that Steam expects to be stored as 32-bit integers rather than
/// strings. Writing them as strings makes Steam treat the whole shortcut
/// entry as corrupt and silently prune it on the next launch.
const VDF_INT_FIELDS: &[&str] = &[
    "appid",
    "IsHidden",
    "AllowDesktopConfig",
    "AllowOverlay",
    "OpenVR",
    "Devkit",
    "DevkitOverrideAppID",
    "LastPlayTime",
];

/// A single shortcut entry: field name → value.
type ShortcutFields = HashMap<String, String>;
/// Map from shortcut index ("0", "1", …) → fields.
type ShortcutsMap = HashMap<String, ShortcutFields>;

/// Find the path to `userdata/<uid>/config/shortcuts.vdf` for the first
/// Steam user directory found. Prefers a user that already has a
/// `shortcuts.vdf`, but falls back to the first user directory otherwise
/// (the file may not exist yet, e.g. if no non-Steam shortcut has ever been
/// added). Returns `None` only when Steam itself isn't installed.
fn find_shortcuts_vdf() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        let mut v = Vec::new();
        if let Ok(pf) = std::env::var("ProgramFiles(x86)") {
            v.push(PathBuf::from(pf).join("Steam").join("userdata"));
        }
        #[cfg(windows)]
        if let Ok(reg) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey("SOFTWARE\\Valve\\Steam")
        {
            if let Ok(path) = reg.get_value::<String, _>("SteamPath") {
                v.push(PathBuf::from(path.replace('/', "\\")).join("userdata"));
            }
        }
        v
    } else {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        vec![
            home.join(".steam").join("steam").join("userdata"),
            home.join(".local").join("share").join("Steam").join("userdata"),
            home.join(".steam").join("root").join("userdata"),
        ]
    };

    let mut fallback: Option<PathBuf> = None;
    for base in &candidates {
        if !base.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let vdf = entry.path().join("config").join("shortcuts.vdf");
                if vdf.exists() {
                    return Some(vdf);
                }
                if fallback.is_none() {
                    fallback = Some(vdf);
                }
            }
        }
    }
    fallback
}

// ── Binary VDF reader ──────────────────────────────────────────────────────

/// Steam's binary VDF strings are NUL-terminated C-strings, not
/// length-prefixed — an earlier version of this reader assumed a u16
/// length prefix, which produced files Steam's own parser couldn't read
/// (and silently discarded).
fn vdf_read_string(data: &[u8], pos: &mut usize) -> Option<String> {
    let start = *pos;
    while *pos < data.len() && data[*pos] != 0 {
        *pos += 1;
    }
    if *pos >= data.len() {
        return None;
    }
    let s = String::from_utf8_lossy(&data[start..*pos]).into_owned();
    *pos += 1; // skip the NUL
    Some(s)
}

/// Recursively read a binary VDF value. For nested objects (like `tags`),
/// flatten child strings into `"key=value,..."` form.
fn vdf_read_value(data: &[u8], pos: &mut usize, ty: u8) -> Option<String> {
    match ty {
        VDF_STRING => vdf_read_string(data, pos),
        VDF_INT32 => {
            if *pos + 4 > data.len() {
                return None;
            }
            let v = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
            *pos += 4;
            Some(v.to_string())
        }
        VDF_OBJECT => {
            let mut parts = Vec::new();
            loop {
                if *pos >= data.len() {
                    return None;
                }
                let child_ty = data[*pos];
                *pos += 1;
                if child_ty == VDF_END {
                    break;
                }
                let key = vdf_read_string(data, pos)?;
                if let Some(val) = vdf_read_value(data, pos, child_ty) {
                    if val.is_empty() {
                        parts.push(key);
                    } else {
                        parts.push(format!("{}={}", key, val));
                    }
                }
            }
            Some(parts.join(","))
        }
        _ => None,
    }
}

/// Parse the children of a `VDF_OBJECT` starting right after its type+key
/// header (i.e. `pos` points at the first child's type byte) into a map of
/// index → fields. Stops at the object's `VDF_END` terminator.
fn parse_shortcuts_object(data: &[u8], pos: &mut usize) -> ShortcutsMap {
    let mut result = ShortcutsMap::new();
    loop {
        if *pos >= data.len() {
            break;
        }
        let ty = data[*pos];
        *pos += 1;
        if ty == VDF_END {
            break;
        }
        let key = match vdf_read_string(data, pos) {
            Some(k) => k,
            None => break,
        };
        if ty != VDF_OBJECT {
            // skip non-object entries
            let _ = vdf_read_value(data, pos, ty);
            continue;
        }
        // Read shortcut object fields.
        let mut fields = ShortcutFields::new();
        loop {
            if *pos >= data.len() {
                break;
            }
            let field_ty = data[*pos];
            *pos += 1;
            if field_ty == VDF_END {
                break;
            }
            let field_key = match vdf_read_string(data, pos) {
                Some(k) => k,
                None => break,
            };
            let field_val = vdf_read_value(data, pos, field_ty)
                .unwrap_or_default();
            fields.insert(field_key, field_val);
        }
        result.insert(key, fields);
    }
    result
}

/// Parse `shortcuts.vdf` into a map of index → fields. The file's real
/// top-level structure is `{ "shortcuts" { "0" {...}, "1" {...}, ... } }` —
/// the entries live one level deeper than the file root. The root itself
/// has no leading type/key header — it's a bare sequence of entries
/// terminated by a single `VDF_END`.
fn read_shortcuts_vdf(path: &PathBuf) -> ShortcutsMap {
    let Ok(data) = std::fs::read(path) else {
        return ShortcutsMap::new();
    };
    let mut pos = 0;

    loop {
        if pos >= data.len() {
            break;
        }
        let ty = data[pos];
        pos += 1;
        if ty == VDF_END {
            break;
        }
        let key = match vdf_read_string(&data, &mut pos) {
            Some(k) => k,
            None => break,
        };
        if ty != VDF_OBJECT {
            let _ = vdf_read_value(&data, &mut pos, ty);
            continue;
        }
        if key.eq_ignore_ascii_case("shortcuts") {
            return parse_shortcuts_object(&data, &mut pos);
        }
        // Not the section we want — skip past it.
        let _ = parse_shortcuts_object(&data, &mut pos);
    }
    ShortcutsMap::new()
}

// ── Binary VDF writer ──────────────────────────────────────────────────────

/// NUL-terminated C-string, matching Steam's actual binary VDF encoding.
fn vdf_write_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// Serialize a `ShortcutsMap` back to binary VDF and write it to disk.
///
/// Real `shortcuts.vdf` files wrap all entries in a top-level "shortcuts"
/// object: `{ "shortcuts" { "0" {...}, "1" {...} } }`. Omitting that
/// wrapper produces a file Steam can't find any shortcuts in, so it
/// silently discards them on the next launch. The file root itself has
/// no leading type/key header — it's a bare sequence of entries (here,
/// just the one "shortcuts" entry) terminated by a single `VDF_END`.
fn write_shortcuts_vdf(path: &PathBuf, map: &ShortcutsMap) -> Result<(), String> {
    let mut data = Vec::new();
    data.push(VDF_OBJECT);
    vdf_write_string(&mut data, "shortcuts");

    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_by_key(|k| k.parse::<u32>().unwrap_or(0));

    for key in keys {
        let obj = &map[key];
        data.push(VDF_OBJECT);
        vdf_write_string(&mut data, key);

        for (field, value) in obj {
            if field == "tags" {
                // tags is always a nested object ("0" = "favorite", …),
                // even when it has no entries — Steam expects the type
                // marker to be VDF_OBJECT regardless.
                data.push(VDF_OBJECT);
                vdf_write_string(&mut data, field);
                for (i, tag) in value.split(',').enumerate() {
                    let tag = tag.trim();
                    if !tag.is_empty() {
                        data.push(VDF_STRING);
                        vdf_write_string(&mut data, &i.to_string());
                        vdf_write_string(&mut data, tag);
                    }
                }
                data.push(VDF_END);
            } else if VDF_INT_FIELDS.contains(&field.as_str()) {
                data.push(VDF_INT32);
                vdf_write_string(&mut data, field);
                let n: u32 = value.parse().unwrap_or(0);
                data.extend_from_slice(&n.to_le_bytes());
            } else {
                data.push(VDF_STRING);
                vdf_write_string(&mut data, field);
                vdf_write_string(&mut data, value);
            }
        }
        data.push(VDF_END);
    }

    data.push(VDF_END); // closes "shortcuts" object
    data.push(VDF_END); // closes root section

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create Steam config directory: {}", e))?;
    }
    std::fs::write(path, &data)
        .map_err(|e| format!("Failed to write shortcuts.vdf: {}", e))?;
    Ok(())
}

// ── Cross-platform Steam shortcut API ──────────────────────────────────────

/// Steam's "legacy" shortcut appid: `CRC32(Exe + AppName) | 0x80000000`.
/// The high bit flags it as a non-Steam-game entry — Steam derives/
/// validates this from the stored Exe/AppName, so getting the formula
/// wrong (wrong byte order, stray null separators, missing flag bit)
/// makes Steam treat the whole shortcut as invalid and drop it silently.
fn steam_shortcut_appid(name: &str, exe: &str) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(exe.as_bytes());
    h.update(name.as_bytes());
    h.finalize() | 0x8000_0000
}

/// Returns `true` when `fields` looks like a Goopie launcher shortcut for `game`.
fn is_goopie_shortcut(fields: &ShortcutFields, exe: &str, game: &str) -> bool {
    let vdf_exe = fields.get("Exe").map(|s| s.as_str()).unwrap_or("").trim_matches('"');
    let opts = fields.get("LaunchOptions").map(|s| s.as_str()).unwrap_or("");
    vdf_exe == exe && (opts.contains(&format!("--play {}", game))
        || opts.contains(&format!("--play {} --local", game)))
}

/// Download an image from `url` (if non-empty) and save it to
/// `<grid_dir>/<stem>.png`. Silently skips on failure.
fn download_and_save_grid_image(url: &str, grid_dir: &std::path::Path, stem: &str) {
    if url.is_empty() {
        return;
    }
    let client = match reqwest::blocking::Client::builder()
        .user_agent("Goopie-Launcher/2")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[shortcuts] Failed to build HTTP client: {}", e);
            return;
        }
    };
    let resp = match client.get(url).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[shortcuts] Failed to download grid image {}: {}", url, e);
            return;
        }
    };
    if !resp.status().is_success() {
        eprintln!("[shortcuts] Grid image download returned {}", resp.status());
        return;
    }
    let bytes = match resp.bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[shortcuts] Failed to read grid image bytes: {}", e);
            return;
        }
    };
    let _ = std::fs::create_dir_all(grid_dir);
    let path = grid_dir.join(format!("{}.png", stem));
    if let Err(e) = std::fs::write(&path, &bytes) {
        eprintln!("[shortcuts] Failed to write grid image {}: {}", path.display(), e);
    }
}

/// Download a cover image, detect landscape wraps, and crop to the front
/// cover (right ~47.3%) before saving. Falls back to the full image when
/// decoding fails or the image is already portrait.
fn download_and_save_cover_grid_image(
    url: &str,
    grid_dir: &std::path::Path,
    stem: &str,
) {
    if url.is_empty() {
        return;
    }
    let client = match reqwest::blocking::Client::builder()
        .user_agent("Goopie-Launcher/2")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[shortcuts] Failed to build HTTP client for cover: {}", e);
            return;
        }
    };
    let resp = match client.get(url).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[shortcuts] Failed to download cover {}: {}", url, e);
            return;
        }
    };
    if !resp.status().is_success() {
        eprintln!("[shortcuts] Cover download returned {}", resp.status());
        return;
    }
    let bytes = match resp.bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[shortcuts] Failed to read cover bytes: {}", e);
            return;
        }
    };

    // Try to decode and crop landscape wraps to just the front cover.
    let out_bytes = match image::load_from_memory(&bytes) {
        Ok(img) => {
            let (w, h) = img.dimensions();
            let cropped = if w > h {
                // Landscape wrap — crop right portion (front cover).
                // Matches the ratio actually used to render covers in the
                // game grid/list tiles (useCoverStyle.ts's
                // `backgroundSize: '211% 100%'` shows the rightmost 1/2.11
                // of the image width) rather than the looser 700/1480
                // approximation used only by the decorative background strip.
                let front_w = (w as f64 / 2.11) as u32;
                let x = w - front_w;
                img.crop_imm(x, 0, front_w, h)
            } else {
                img
            };
            let mut buf = std::io::Cursor::new(Vec::new());
            if cropped.write_to(&mut buf, image::ImageFormat::Png).is_ok() {
                Some(buf.into_inner())
            } else {
                None
            }
        }
        Err(e) => {
            eprintln!("[shortcuts] Failed to decode cover image: {}", e);
            // Save the raw bytes as a fallback.
            Some(bytes.to_vec())
        }
    };

    if let Some(data) = out_bytes {
        let _ = std::fs::create_dir_all(grid_dir);
        let path = grid_dir.join(format!("{}.png", stem));
        if let Err(e) = std::fs::write(&path, &data) {
            eprintln!("[shortcuts] Failed to write cover grid image {}: {}", path.display(), e);
        }
    }
}

/// Save Steam grid images (portrait, hero + logo) for a shortcut. Grid
/// filenames use the signed (i32) form of the CRC32 appid.
fn save_steam_grid_images(
    grid_dir: &std::path::Path,
    appid: u32,
    cover_url: &str,
    header_url: &str,
    logo_url: &str,
) {
    // Grid artwork filenames use the *unsigned* decimal appid — Steam does
    // not accept the signed/negative form here even though the "appid"
    // field itself is stored as a two's-complement int32 in the vdf.
    // Portrait / box art — the modern library grid view reads this from the
    // "p"-suffixed filename; the bare "<appid>.png" name is the legacy
    // horizontal capsule and is ignored by the current Steam UI.
    download_and_save_cover_grid_image(cover_url, grid_dir, &format!("{}p", appid));
    // Hero / wide banner — shown at top of game details page.
    download_and_save_grid_image(header_url, grid_dir, &format!("{}_hero", appid));
    // Logo — overlaid on the hero image.
    download_and_save_grid_image(logo_url, grid_dir, &format!("{}_logo", appid));
}

/// Delete Steam grid images for a shortcut by appid.
fn remove_steam_grid_images(grid_dir: &std::path::Path, appid: u32) {
    for suffix in &["p", "_hero", "_logo"] {
        let path = grid_dir.join(format!("{}{}.png", appid, suffix));
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Derive the Steam `config/grid` directory from a `shortcuts.vdf` path.
fn grid_dir_from_vdf(vdf_path: &std::path::Path) -> std::path::PathBuf {
    vdf_path.parent().unwrap_or(vdf_path).join("grid")
}

/// Check whether a Goopie shortcut for `game` exists in the user's Steam
/// `shortcuts.vdf`.
pub fn exists_steam(game: &str, _title: &str) -> bool {
    let Some(vdf_path) = find_shortcuts_vdf() else {
        return false;
    };
    let exe = match launcher_exe() {
        Ok(e) => e.to_string_lossy().into_owned(),
        Err(_) => return false,
    };
    let shortcuts = read_shortcuts_vdf(&vdf_path);
    shortcuts.values().any(|f| is_goopie_shortcut(f, &exe, game))
}

/// Returns `true` when Steam is installed and a `shortcuts.vdf` was found.
pub fn steam_installed() -> bool {
    find_shortcuts_vdf().is_some()
}

/// Add a non-Steam shortcut for `game` to the user's Steam `shortcuts.vdf`.
pub fn create_steam(game: &str, title: &str, icon_url: &str, cover_url: &str, header_url: &str, logo_url: &str) -> Result<(), String> {
    let vdf_path = find_shortcuts_vdf()
        .ok_or_else(|| "Could not find Steam shortcuts.vdf. Is Steam installed?".to_string())?;
    let mut shortcuts = read_shortcuts_vdf(&vdf_path);

    let exe = launcher_exe()?;
    let exe_str = exe.to_string_lossy();

    // Skip if it already exists.
    if shortcuts.values().any(|f| is_goopie_shortcut(f, &exe_str, game)) {
        eprintln!("[shortcuts] Steam shortcut already exists for {}", game);
        return Ok(());
    }

    // Resolve icon — Steam supports PNG directly.
    let icon_path = resolve_icon_png(game, icon_url)
        .and_then(|png| {
            let p = games::game_root(game).join("assets").join(".shortcut-icon.png");
            std::fs::write(&p, &png).ok()?;
            Some(p.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    let start_dir = exe.parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let args = if is_local_mode() {
        format!("--play {} --local", game)
    } else {
        format!("--play {}", game)
    };

    let appid = steam_shortcut_appid(title, &format!("\"{}\"", exe_str));

    let mut fields = ShortcutFields::new();
    fields.insert("appid".into(), appid.to_string());
    fields.insert("AppName".into(), title.into());
    fields.insert("Exe".into(), format!("\"{}\"", exe_str));
    fields.insert("StartDir".into(), format!("\"{}\"", start_dir));
    fields.insert("icon".into(), icon_path);
    fields.insert("ShortcutPath".into(), String::new());
    fields.insert("LaunchOptions".into(), args);
    fields.insert("IsHidden".into(), "0".into());
    fields.insert("AllowDesktopConfig".into(), "1".into());
    fields.insert("AllowOverlay".into(), "1".into());
    fields.insert("OpenVR".into(), "0".into());
    fields.insert("Devkit".into(), "0".into());
    fields.insert("DevkitGameID".into(), String::new());
    fields.insert("DevkitOverrideAppID".into(), String::new());
    fields.insert("LastPlayTime".into(), "0".into());
    fields.insert("FlatpakAppID".into(), String::new());
    fields.insert("tags".into(), String::new());

    let next_idx = shortcuts.keys()
        .filter_map(|k| k.parse::<u32>().ok())
        .max()
        .map_or(0, |m| m + 1);

    shortcuts.insert(next_idx.to_string(), fields);
    write_shortcuts_vdf(&vdf_path, &shortcuts)?;

    // Save grid images (hero + logo) so Steam shows them in the library.
    let grid = grid_dir_from_vdf(&vdf_path);
    save_steam_grid_images(&grid, appid, cover_url, header_url, logo_url);

    eprintln!("[shortcuts] Created Steam shortcut: {} (appid {})", title, appid);
    Ok(())
}

/// Remove the Goopie shortcut for `game` from the user's Steam `shortcuts.vdf`.
pub fn remove_steam(game: &str, _title: &str) -> Result<(), String> {
    let vdf_path = find_shortcuts_vdf()
        .ok_or_else(|| "Could not find Steam shortcuts.vdf. Is Steam installed?".to_string())?;
    let mut shortcuts = read_shortcuts_vdf(&vdf_path);

    let exe = launcher_exe()?;
    let exe_str = exe.to_string_lossy();
    let grid = grid_dir_from_vdf(&vdf_path);

    let mut removed_appids = Vec::new();
    let to_remove: Vec<String> = shortcuts.iter()
        .filter(|(_, f)| {
            if is_goopie_shortcut(f, &exe_str, game) {
                if let Some(id) = f.get("appid").and_then(|s| s.parse::<u32>().ok()) {
                    removed_appids.push(id);
                }
                true
            } else {
                false
            }
        })
        .map(|(k, _)| k.clone())
        .collect();

    if to_remove.is_empty() {
        eprintln!("[shortcuts] No Steam shortcut found for {}", game);
    } else {
        for key in &to_remove {
            shortcuts.remove(key);
        }
        write_shortcuts_vdf(&vdf_path, &shortcuts)?;
        // Clean up grid images for removed shortcuts.
        for id in &removed_appids {
            remove_steam_grid_images(&grid, *id);
        }
        eprintln!("[shortcuts] Removed Steam shortcut for {}", game);
    }
    Ok(())
}
