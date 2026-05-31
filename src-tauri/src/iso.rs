//! Xbox 360 ISO extraction using the xdvdfs crate (synchronous mode).
//!
//! xdvdfs is compiled with the `sync` feature, which makes `maybe_async` emit
//! blocking (non-async) code. The `BlockDeviceRead` trait is implemented for any
//! type that implements `std::io::Read + std::io::Seek + Send + Sync`, so a plain
//! `std::fs::File` works directly.

use std::{
    io::Write,
    path::Path,
    sync::Arc,
};

use crate::{config, platform, AppState};

/// Open a native file dialog, let the user pick an ISO, then extract it to
/// `<games>/<game_name>/assets/` on the calling thread.
///
/// Must be called from a worker thread (not the main/UI thread) since `rfd`'s
/// blocking file dialog may show a native dialog.
pub fn install_iso(game_name: &str, state: Arc<AppState>) {
    let Some(iso_path) = platform::pick_iso_file() else {
        eprintln!("[iso] No ISO file selected");
        return;
    };

    if !Path::new(&iso_path).exists() {
        eprintln!("[iso] ISO file does not exist: {}", iso_path);
        return;
    }

    let games_folder = config::get_games_folder();
    let dest = Path::new(&games_folder)
        .join(game_name)
        .join("assets");

    // Remove existing assets directory before extracting.
    if dest.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dest) {
            eprintln!("[iso] Warning: could not remove existing assets dir: {}", e);
        }
    }

    if let Err(e) = std::fs::create_dir_all(&dest) {
        eprintln!("[iso] Failed to create destination directory: {}", e);
        return;
    }

    eprintln!(
        "[iso] Starting extraction: {} → {}",
        iso_path,
        dest.display()
    );

    state
        .is_extracting
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let result = extract_xdvd(&iso_path, &dest);

    state
        .is_extracting
        .store(false, std::sync::atomic::Ordering::Relaxed);

    match result {
        Ok(count) => eprintln!("[iso] Extraction complete: {} files extracted", count),
        Err(e) => eprintln!("[iso] Extraction failed: {}", e),
    }
}

// ── xdvdfs extraction (synchronous) ──────────────────────────────────────────

fn extract_xdvd(iso_path: &str, dest: &Path) -> std::io::Result<usize> {
    let img = std::fs::File::open(iso_path)?;

    let mut dev = xdvdfs::blockdev::OffsetWrapper::new(img)
        .map_err(|e: xdvdfs::util::Error<std::io::Error>| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e))
        })?;

    let volume = xdvdfs::read::read_volume(&mut dev)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e)))?;

    // `file_tree` returns a flat list of (parent_path, DirectoryEntryNode) pairs.
    let tree = volume
        .root_table
        .file_tree(&mut dev)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e)))?;

    let mut count = 0usize;

    for (parent, node) in &tree {
        let name = node
            .name_str()
            .map_err(|e: xdvdfs::util::Error<std::io::Error>| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e))
            })?;

        let rel_path = if parent.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", parent.trim_start_matches('/'), name)
        };

        let out_path = dest.join(&rel_path);

        if node.node.dirent.is_directory() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent_dir) = out_path.parent() {
                std::fs::create_dir_all(parent_dir)?;
            }

            let data = node
                .node
                .dirent
                .read_data_all(&mut dev)
                .map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("read_data_all for {}: {:?}", rel_path, e),
                    )
                })?;

            let mut out_file = std::fs::File::create(&out_path)?;
            out_file.write_all(&data)?;
            count += 1;
        }
    }

    Ok(count)
}
