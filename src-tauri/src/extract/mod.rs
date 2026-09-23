//! Xbox 360 game extraction: ISO (XDVDFS) and XBLA (STFS/LIVE) formats.

pub mod dlc;
pub mod drop;
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

/// Open a native folder dialog, let the user pick an *already extracted* game
/// folder, and link `<games>/<game_name>/assets/` at it — no copying, so the
/// user keeps their files wherever they already live.
pub fn link_assets_folder(game_name: &str, expected_xex_sha: &str, state: Arc<AppState>) {
    state
        .is_extracting
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let result = link_assets_folder_inner(game_name, expected_xex_sha);

    state
        .is_extracting
        .store(false, std::sync::atomic::Ordering::Relaxed);

    match result {
        Some(Ok(())) => eprintln!("[extract] Linked assets folder for {}", game_name),
        Some(Err(e)) => {
            eprintln!("[extract] Linking assets folder failed: {}", e);
            *state.last_extract_error.lock().unwrap() = Some(e.to_string());
        }
        None => {}
    }
}

/// Returns `Ok(())` on success, `Err` on failure, or `None` if the user
/// cancelled the folder picker.
fn link_assets_folder_inner(game_name: &str, expected_xex_sha: &str) -> Option<std::io::Result<()>> {
    let picked = platform::pick_assets_folder()?;
    Some(link_assets_dir(game_name, Path::new(&picked), expected_xex_sha))
}

/// Point `<games>/<game_name>/assets/` at `src`, after checking `src` really
/// holds an extracted game.
///
/// Accepts either the assets directory itself or a game root containing an
/// `assets/` subdirectory, since both are plausible things to pick.
pub fn link_assets_dir(game_name: &str, src: &Path, expected_xex_sha: &str) -> std::io::Result<()> {
    let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg);

    if !src.is_dir() {
        return Err(invalid(format!("not a folder: {}", src.display())));
    }

    // Tolerate the user picking the game root rather than its assets folder.
    let src = match crate::paths::find_case_insensitive(src, "default.xex") {
        Some(_) => src.to_path_buf(),
        None => {
            let nested = crate::paths::find_case_insensitive(src, "assets")
                .filter(|p| crate::paths::find_case_insensitive(p, "default.xex").is_some());
            nested.ok_or_else(|| {
                invalid(
                    "That folder doesn't contain a default.xex. Pick the folder holding the extracted game files."
                        .to_string(),
                )
            })?
        }
    };

    // Canonicalize so the self-link check below compares real paths, and so the
    // symlink survives the user's shell-relative or junction-laden input.
    let src = src.canonicalize().unwrap_or(src);

    if !expected_xex_hashes(expected_xex_sha).is_empty() {
        let xex = crate::paths::find_case_insensitive(&src, "default.xex")
            .ok_or_else(|| invalid("default.xex disappeared while linking".to_string()))?;
        verify_xex(&xex, expected_xex_sha)?;
    }

    let games_folder = config::get_games_folder();
    let game_root = Path::new(&games_folder).join(game_name);
    std::fs::create_dir_all(&game_root)?;
    let dest = game_root.join("assets");

    // Already pointing at (or literally being) the picked folder — nothing to do.
    // Without this, the remove below would delete the very files we're linking to.
    if dest.canonicalize().map(|d| d == src).unwrap_or(false) {
        return Ok(());
    }

    crate::paths::remove_dir_or_link(&dest)?;

    match crate::paths::symlink_dir(&src, &dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Windows only grants symlink creation under Developer Mode or with
            // SeCreateSymbolicLinkPrivilege. Rather than fail the install, fall
            // back to copying the files in.
            eprintln!("[extract] Symlink failed ({}), copying folder instead", e);
            crate::paths::copy_dir_all(&src, &dest).map_err(|copy_err| {
                let _ = crate::paths::remove_dir_or_link(&dest);
                copy_err
            })
        }
    }
}

/// Extract a base-game file (ISO or STFS with default.xex) into `<games>/<game_name>/assets/`.
/// Wipes the existing assets dir, creates it fresh, and cleans up on error.
///
/// When `expected_xex_sha` is non-empty, verifies the extracted `default.xex`'s
/// SHA-256 against it (see [`verify_xex`]) and rolls back (removes the assets
/// dir) on mismatch.
pub fn extract_base_game(game_name: &str, file_path: &str, expected_xex_sha: &str) -> std::io::Result<usize> {
    let games_folder = config::get_games_folder();
    let dest = Path::new(&games_folder)
        .join(game_name)
        .join("assets");

    // May be a symlink to a user-picked folder (see `link_assets_dir`) — unlink
    // it rather than deleting the files it points at.
    if let Err(e) = crate::paths::remove_dir_or_link(&dest) {
        eprintln!("[extract] Warning: could not remove existing assets dir: {}", e);
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
        verify_xex(&dest.join("default.xex"), expected_xex_sha)?;
        Ok(count)
    });

    if result.is_err() {
        let _ = crate::paths::remove_dir_or_link(&dest);
    }

    result
}

/// Extract a base-game file (ISO or STFS) into an arbitrary destination directory,
/// with no wipe/verify/rollback semantics — just format-detect and extract.
///
/// Used by the drag-and-drop flow ([`drop`]), which extracts into a scratch temp
/// directory first, hashes the result against the whole game catalogue, and only
/// then commits it into a specific game's `assets/` dir via [`commit_assets`].
pub fn extract_to_dir(file_path: &str, dest: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dest)?;
    match detect_format(file_path) {
        Ok(Format::Xdvdfs) => xdvdfs::extract(file_path, dest),
        Ok(Format::Stfs) => stfs::extract(file_path, dest),
        Err(e) => Err(e),
    }
}

/// Move an already-extracted assets directory (e.g. a temp dir from
/// [`extract_to_dir`]) into `<games>/<game_name>/assets/`, replacing any
/// existing assets for that game.
///
/// `src` must be on the same filesystem as the games folder (the drop flow
/// stages its temp dir inside the games folder for exactly this reason), so
/// the move is an instant rename rather than a slow copy.
pub fn commit_assets(src: &Path, game_name: &str) -> std::io::Result<()> {
    let games_folder = config::get_games_folder();
    let game_root = Path::new(&games_folder).join(game_name);
    std::fs::create_dir_all(&game_root)?;
    let dest = game_root.join("assets");

    crate::paths::remove_dir_or_link(&dest)?;
    std::fs::rename(src, &dest)
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

/// Splits a game's comma-separated `xex_sha256` field into its hashes, trimmed,
/// with empty entries dropped.
pub fn expected_xex_hashes(field: &str) -> Vec<&str> {
    field.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// Checks `xex`'s SHA-256 against any hash in a game's `xex_sha256` field. A
/// field with no hashes accepts any file.
fn verify_xex(xex: &Path, expected_xex_sha: &str) -> std::io::Result<()> {
    let expected = expected_xex_hashes(expected_xex_sha);
    if expected.is_empty() {
        return Ok(());
    }
    let actual = download::sha256_file(&xex.to_string_lossy()).unwrap_or_default();
    if expected.iter().any(|h| h.eq_ignore_ascii_case(&actual)) {
        return Ok(());
    }
    let wanted = match expected.as_slice() {
        [only] => format!("{}…", &only[..only.len().min(12)]),
        many => format!("one of {} known revisions", many.len()),
    };
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "This game's files don't match what's expected, likely because it's a different region or version than the one supported. (default.xex checksum mismatch: expected {wanted}, got {}…)",
            &actual[..actual.len().min(12)],
        ),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    // A well-formed hash that no test file has.
    const XEX_SHA: &str = "4f4b1c21dcc3a6b4ac7e2c1a4b3c7c4e1f0f7f2ad5b7d8c0a8c1b1ce61b2b2a0";

    fn xex_with_known_hash() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("default.xex");
        std::fs::write(&path, b"xex").unwrap();
        let sha = download::sha256_file(&path.to_string_lossy()).unwrap();
        (tmp, path, sha)
    }

    #[test]
    fn expected_xex_hashes_splits_trims_and_drops_empties() {
        assert_eq!(expected_xex_hashes("aa"), vec!["aa"]);
        assert_eq!(expected_xex_hashes(" aa , bb,,cc "), vec!["aa", "bb", "cc"]);
        assert!(expected_xex_hashes("").is_empty());
        assert!(expected_xex_hashes(" , ,").is_empty());
    }

    #[test]
    fn verify_xex_accepts_any_listed_hash_case_insensitively() {
        let (_tmp, path, sha) = xex_with_known_hash();
        assert!(verify_xex(&path, &sha).is_ok());
        assert!(verify_xex(&path, &format!("{XEX_SHA}, {}", sha.to_uppercase())).is_ok());
    }

    #[test]
    fn verify_xex_accepts_anything_when_no_hash_is_set() {
        let (_tmp, path, _) = xex_with_known_hash();
        assert!(verify_xex(&path, "").is_ok());
        assert!(verify_xex(&path, " , ").is_ok());
    }

    #[test]
    fn verify_xex_rejects_a_hash_outside_the_list() {
        let (_tmp, path, _) = xex_with_known_hash();
        let single = verify_xex(&path, XEX_SHA).unwrap_err();
        assert_eq!(single.kind(), std::io::ErrorKind::InvalidData);
        assert!(single.to_string().contains("expected 4f4b1c21dcc3…"));

        let many = verify_xex(&path, &format!("{XEX_SHA},{XEX_SHA}")).unwrap_err();
        assert!(many.to_string().contains("expected one of 2 known revisions"));
    }
}
