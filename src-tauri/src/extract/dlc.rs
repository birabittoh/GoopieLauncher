//! Title-update and DLC installation logic.
//!
//! Ports the semantics of `ReKameo/scripts/extract-dlc.py` and `extract-tu.py`
//! into Rust: STFS header reading, content extraction, `.header` file generation
//! for the SDK's ContentManager, and install/remove/list operations.

use std::path::{Path, PathBuf};

use crate::{config, download, paths};

use super::stfs;

const XUID: &str = "0000000000000000";
const CONTENT_TYPE_DLC: &str = "00000002";

// ── Update ──────────────────────────────────────────────────────────────────

/// Install a title update from `src_path` into `<game_root>/update/`.
/// When `expected_sha` is non-empty, verifies the SHA-256 of the source file.
pub fn install_update(game: &str, src_path: &str, expected_sha: &str) -> std::io::Result<usize> {
    if !expected_sha.is_empty() {
        let actual = download::sha256_file(src_path).unwrap_or_default();
        if !actual.eq_ignore_ascii_case(expected_sha) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Update checksum mismatch: expected {}…, got {}…",
                    &expected_sha[..12], &actual[..12]
                ),
            ));
        }
    }

    let dest = game_root(game).join("update");
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }
    std::fs::create_dir_all(&dest)?;

    let count = stfs::extract_tree(src_path, &dest)?;
    eprintln!("[dlc] Installed update for {}: {} files", game, count);
    Ok(count)
}

// ── DLC ─────────────────────────────────────────────────────────────────────

/// Installed DLC entry returned by `list_installed_dlc`.
#[derive(serde::Serialize)]
pub struct InstalledDlc {
    pub hash: String,
    pub title_id: String,
    pub name: String,
}

/// Install a DLC STFS package. Extracts into the content dir and writes the
/// `.header` file the SDK's ContentManager needs.
/// Returns the matched DLC name from `dlc_names` (if any).
pub fn install_dlc(game: &str, src_path: &str, dlc_names: &[String]) -> std::io::Result<Option<String>> {
    let meta = stfs::read_header_meta(src_path)?;
    let title_id = format!("{:08X}", meta.title_id);
    let hash = Path::new(src_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let base = content_base(game)?;

    // Remove any existing DLC with the same display name (even under a different hash)
    // to prevent duplicate entries when the same DLC is installed from different files.
    let existing = list_installed_dlc(game);
    for dlc in &existing {
        if dlc.name.trim().eq_ignore_ascii_case(meta.display_name.trim()) && !meta.display_name.trim().is_empty() {
            eprintln!("[dlc] Replacing existing DLC \"{}\" (old hash={}, tid={})", dlc.name, dlc.hash, dlc.title_id);
            remove_dlc(game, &dlc.title_id, &dlc.hash);
        }
    }

    // Content dir: <base>/<xuid>/<title_id>/00000002/<hash>/
    let content_dir = base
        .join(XUID)
        .join(&title_id)
        .join(CONTENT_TYPE_DLC)
        .join(&hash);
    if content_dir.exists() {
        let _ = std::fs::remove_dir_all(&content_dir);
    }
    std::fs::create_dir_all(&content_dir)?;

    let count = stfs::extract_tree(src_path, &content_dir)?;
    eprintln!("[dlc] Extracted DLC {} for {}: {} files", hash, game, count);

    // Write .header
    write_content_header(
        &base,
        XUID,
        &title_id,
        CONTENT_TYPE_DLC,
        &hash,
        meta.content_type,
        &meta.display_name_raw,
        meta.title_id,
    )?;

    // Match against known DLC names
    let matched = dlc_names.iter().find(|n| {
        n.trim().eq_ignore_ascii_case(meta.display_name.trim())
    }).cloned();

    Ok(matched)
}

/// List installed DLC by scanning the Headers directory.
pub fn list_installed_dlc(game: &str) -> Vec<InstalledDlc> {
    let mut result = Vec::new();
    let Some(base) = content_base(game).ok() else { return result };
    let xuid_dir = base.join(XUID);

    let Ok(title_ids) = std::fs::read_dir(&xuid_dir) else { return result };
    for tid_entry in title_ids.flatten() {
        if !tid_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let title_id = tid_entry.file_name().to_string_lossy().into_owned();
        let headers_dir = tid_entry.path().join("Headers").join(CONTENT_TYPE_DLC);
        let Ok(headers) = std::fs::read_dir(&headers_dir) else { continue };
        for header_entry in headers.flatten() {
            let fname = header_entry.file_name().to_string_lossy().into_owned();
            if !fname.ends_with(".header") {
                continue;
            }
            let hash = fname.trim_end_matches(".header").to_string();
            let name = read_display_name_from_header(&header_entry.path());
            result.push(InstalledDlc {
                hash,
                title_id: title_id.clone(),
                name,
            });
        }
    }
    result
}

/// Remove a specific DLC by title_id and hash.
pub fn remove_dlc(game: &str, title_id: &str, hash: &str) {
    let Ok(base) = content_base(game) else { return };

    let content_dir = base
        .join(XUID)
        .join(title_id)
        .join(CONTENT_TYPE_DLC)
        .join(hash);
    if content_dir.exists() {
        let _ = std::fs::remove_dir_all(&content_dir);
    }

    let header = base
        .join(XUID)
        .join(title_id)
        .join("Headers")
        .join(CONTENT_TYPE_DLC)
        .join(format!("{}.header", hash));
    if header.exists() {
        let _ = std::fs::remove_file(&header);
    }

    eprintln!("[dlc] Removed DLC {} (tid={}) for {}", hash, title_id, game);
}

/// Open the DLC content folder in the system file manager.
pub fn open_dlc_folder(game: &str, title_id: &str, hash: &str) {
    let Ok(base) = content_base(game) else { return };
    let dir = base
        .join(XUID)
        .join(title_id)
        .join(CONTENT_TYPE_DLC)
        .join(hash);
    if dir.exists() {
        crate::platform::open_folder(&dir.to_string_lossy());
    }
}

// ── Private helpers ─────────────────────────────────────────────────────────

fn game_root(game: &str) -> PathBuf {
    PathBuf::from(config::get_games_folder()).join(game)
}

fn content_base(game: &str) -> std::io::Result<PathBuf> {
    paths::rex_user_folder()
        .map(|p| p.join(game))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine user data folder",
            )
        })
}

/// Write a `.header` file matching the SDK's XCONTENT_AGGREGATE_DATA layout.
/// Direct port of `extract-dlc.py`'s `write_content_header`.
fn write_content_header(
    data_dir: &Path,
    xuid: &str,
    title_id_str: &str,
    content_type_str: &str,
    hash: &str,
    content_type_u32: u32,
    display_name_raw: &[u8; 256],
    title_id_u32: u32,
) -> std::io::Result<()> {
    let mut header = vec![0u8; 0x148 + 4];

    // device_id = 1 (HDD)
    header[0x000..0x004].copy_from_slice(&1u32.to_be_bytes());

    // content_type
    header[0x004..0x008].copy_from_slice(&content_type_u32.to_be_bytes());

    // display_name (BE UTF-16, already in raw form)
    header[0x008..0x008 + 256].copy_from_slice(display_name_raw);

    // file_name = the package's hex name (ASCII)
    let fn_bytes = hash.as_bytes();
    let copy_len = fn_bytes.len().min(42);
    header[0x108..0x108 + copy_len].copy_from_slice(&fn_bytes[..copy_len]);

    // title_id
    header[0x13C..0x140].copy_from_slice(&title_id_u32.to_be_bytes());

    // license_mask = 0xFFFFFFFF (all license bits granted, LE)
    header[0x148..0x14C].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());

    let header_dir = data_dir
        .join(xuid)
        .join(title_id_str)
        .join("Headers")
        .join(content_type_str);
    std::fs::create_dir_all(&header_dir)?;
    let header_path = header_dir.join(format!("{}.header", hash));
    std::fs::write(&header_path, &header)?;
    eprintln!("[dlc] Wrote header: {}", header_path.display());
    Ok(())
}

/// Read display name from a .header file (offset 0x008, 256 bytes, UTF-16 BE).
fn read_display_name_from_header(path: &Path) -> String {
    let Ok(data) = std::fs::read(path) else { return String::new() };
    if data.len() < 0x008 + 256 {
        return String::new();
    }
    let raw = &data[0x008..0x008 + 256];
    let utf16: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&utf16)
        .trim_end_matches('\0')
        .trim()
        .to_string()
}
