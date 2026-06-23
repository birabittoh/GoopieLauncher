//! Xbox 360 game extraction: ISO (XDVDFS) and XBLA (STFS/LIVE) formats.

mod stfs;
mod xdvdfs;

use std::{
    io::Read,
    path::Path,
    sync::Arc,
};

use crate::{config, platform, AppState};

/// Open a native file dialog, let the user pick a game file, then extract it to
/// `<games>/<game_name>/assets/` on the calling thread.
///
/// Detects the format (ISO vs XBLA) automatically from the file header.
pub fn install_game(game_name: &str, state: Arc<AppState>) {
    state
        .is_extracting
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let result = install_game_inner(game_name);

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

/// Returns `Ok(count)` on success, `Err` on failure, or `None` if the user
/// cancelled the file picker or the file doesn't exist.
fn install_game_inner(game_name: &str) -> Option<std::io::Result<usize>> {
    let file_path = platform::pick_game_file()?;

    if !Path::new(&file_path).exists() {
        eprintln!("[extract] File does not exist: {}", file_path);
        return None;
    }

    let games_folder = config::get_games_folder();
    let dest = Path::new(&games_folder)
        .join(game_name)
        .join("assets");

    if dest.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dest) {
            eprintln!("[extract] Warning: could not remove existing assets dir: {}", e);
        }
    }

    if let Err(e) = std::fs::create_dir_all(&dest) {
        eprintln!("[extract] Failed to create destination directory: {}", e);
        return Some(Err(e));
    }

    eprintln!(
        "[extract] Starting extraction: {} → {}",
        file_path,
        dest.display()
    );

    let result = match detect_format(&file_path) {
        Ok(Format::Xdvdfs) => {
            eprintln!("[extract] Detected XDVDFS (ISO) format");
            xdvdfs::extract(&file_path, &dest)
        }
        Ok(Format::Stfs) => {
            eprintln!("[extract] Detected STFS (XBLA) format");
            stfs::extract(&file_path, &dest)
        }
        Err(e) => Err(e),
    };

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dest);
    }

    Some(result)
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
