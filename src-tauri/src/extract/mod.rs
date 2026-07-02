//! Xbox 360 game extraction: ISO (XDVDFS) and XBLA (STFS/LIVE) formats.

pub mod dlc;
mod stfs;
pub mod xex;
mod xdvdfs;

use std::{
    io::Read,
    path::Path,
    sync::Arc,
};

use crate::{config, download, platform, AppState};

/// Open a native file dialog, let the user pick a game file, then extract it to
/// `<games>/<game_name>/assets/` on the calling thread.
///
/// Detects the format (ISO vs XBLA) automatically from the file header.
pub fn install_game(game_name: &str, iso_only: bool, expected_xex_sha: &str, state: Arc<AppState>) {
    state
        .is_extracting
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let result = install_game_inner(game_name, iso_only, expected_xex_sha);

    state
        .is_extracting
        .store(false, std::sync::atomic::Ordering::Relaxed);

    match result {
        Some(Ok(count)) => eprintln!("[extract] Extraction complete: {} files extracted", count),
        Some(Err(e)) => {
            eprintln!("[extract] Extraction failed: {}", e);
            *state.last_extract_error.lock().unwrap() = Some(e.to_string());
        }
        None => {}
    }
}

/// Extract a base-game file (ISO or STFS with default.xex) into `<games>/<game_name>/assets/`.
/// Wipes the existing assets dir, creates it fresh, and cleans up on error.
///
/// When `expected_xex_sha` is non-empty, verifies the extracted `default.xex`'s
/// SHA-256 against it and rolls back (removes the assets dir) on mismatch.
pub fn extract_base_game(game_name: &str, file_path: &str, expected_xex_sha: &str) -> std::io::Result<usize> {
    let games_folder = config::get_games_folder();
    let dest = Path::new(&games_folder)
        .join(game_name)
        .join("assets");

    if dest.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dest) {
            eprintln!("[extract] Warning: could not remove existing assets dir: {}", e);
        }
    }

    std::fs::create_dir_all(&dest)?;

    eprintln!(
        "[extract] Starting extraction: {} → {}",
        file_path,
        dest.display()
    );

    let result = match detect_format(file_path) {
        Ok(Format::Xdvdfs) => {
            eprintln!("[extract] Detected XDVDFS (ISO) format");
            xdvdfs::extract(file_path, &dest)
        }
        Ok(Format::Stfs) => {
            eprintln!("[extract] Detected STFS (XBLA) format");
            stfs::extract(file_path, &dest)
        }
        Err(e) => Err(e),
    };

    let result = result.and_then(|count| {
        if !expected_xex_sha.is_empty() {
            let xex_path = dest.join("default.xex");
            let actual = download::sha256_file(&xex_path.to_string_lossy()).unwrap_or_default();
            if !actual.eq_ignore_ascii_case(expected_xex_sha) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "default.xex checksum mismatch: expected {}…, got {}…",
                        &expected_xex_sha[..expected_xex_sha.len().min(12)],
                        &actual[..actual.len().min(12)],
                    ),
                ));
            }
        }
        Ok(count)
    });

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dest);
    }

    result
}

/// Returns `Ok(count)` on success, `Err` on failure, or `None` if the user
/// cancelled the file picker or the file doesn't exist.
fn install_game_inner(game_name: &str, iso_only: bool, expected_xex_sha: &str) -> Option<std::io::Result<usize>> {
    let file_path = platform::pick_game_file(iso_only)?;

    if !Path::new(&file_path).exists() {
        eprintln!("[extract] File does not exist: {}", file_path);
        return None;
    }

    Some(extract_base_game(game_name, &file_path, expected_xex_sha))
}

/// Route a dropped/picked file to the right installer (base game, update, or DLC).
///
/// - Non-STFS magic → base game (ISO).
/// - STFS with `default.xex` → base game.
/// - STFS content_type `0xB0000` → title update.
/// - STFS content_type `0x2` → DLC.
/// - Otherwise → base game fallback.
pub fn install_asset_file(
    game_name: &str,
    src_path: &str,
    update_checksum: &str,
    dlc_names: &[String],
    allow_update: bool,
    expected_xex_sha: &str,
    state: Arc<AppState>,
) {
    state
        .is_extracting
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let result = install_asset_file_inner(game_name, src_path, update_checksum, dlc_names, allow_update, expected_xex_sha);

    state
        .is_extracting
        .store(false, std::sync::atomic::Ordering::Relaxed);

    match result {
        Ok(()) => eprintln!("[extract] Asset install complete for {}", game_name),
        Err(e) => {
            eprintln!("[extract] Asset install failed: {}", e);
            *state.last_extract_error.lock().unwrap() = Some(e.to_string());
        }
    }
}

fn install_asset_file_inner(
    game_name: &str,
    src_path: &str,
    update_checksum: &str,
    dlc_names: &[String],
    allow_update: bool,
    expected_xex_sha: &str,
) -> std::io::Result<()> {
    if !Path::new(src_path).exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("file not found: {}", src_path),
        ));
    }

    match detect_format(src_path) {
        Ok(Format::Stfs) => {
            // Check if it has default.xex → base game
            if stfs::has_default_xex(src_path)? {
                extract_base_game(game_name, src_path, expected_xex_sha)?;
                return Ok(());
            }
            // Route by content_type
            let meta = stfs::read_header_meta(src_path)?;
            match meta.content_type {
                0xB0000 => {
                    if !allow_update {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "title update installation is not enabled for this game",
                        ));
                    }
                    dlc::install_update(game_name, src_path, update_checksum)?;
                }
                0x2 => {
                    dlc::install_dlc(game_name, src_path, dlc_names)?;
                }
                _ => {
                    extract_base_game(game_name, src_path, expected_xex_sha)?;
                }
            }
            Ok(())
        }
        Ok(Format::Xdvdfs) | Err(_) => {
            extract_base_game(game_name, src_path, expected_xex_sha)?;
            Ok(())
        }
    }
}

/// Open a multi-file picker then route each selected file through `install_asset_file`.
pub fn install_asset_pick(
    game_name: &str,
    update_checksum: &str,
    dlc_names: &[String],
    iso_only: bool,
    allow_update: bool,
    expected_xex_sha: &str,
    state: Arc<AppState>,
) {
    let paths = platform::pick_game_files(iso_only);
    for path in paths {
        install_asset_file(game_name, &path, update_checksum, dlc_names, allow_update, expected_xex_sha, Arc::clone(&state));
    }
}

pub fn install_asset_files(
    game_name: &str,
    paths: &[String],
    update_checksum: &str,
    dlc_names: &[String],
    allow_update: bool,
    expected_xex_sha: &str,
    state: Arc<AppState>,
) {
    for path in paths {
        install_asset_file(game_name, path, update_checksum, dlc_names, allow_update, expected_xex_sha, Arc::clone(&state));
    }
}

enum Format {
    Xdvdfs,
    Stfs,
}

fn detect_format(path: &str) -> std::io::Result<Format> {
    let mut f = std::fs::File::open(path)?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;

    match &magic {
        b"LIVE" | b"CON " | b"PIRS" => Ok(Format::Stfs),
        _ => Ok(Format::Xdvdfs),
    }
}
