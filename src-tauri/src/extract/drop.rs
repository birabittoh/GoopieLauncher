//! Orchestrates a batch of drag-and-dropped files against the whole game
//! catalogue, rather than a single pre-selected game.
//!
//! Unlike [`super::install_asset_file`] (which always installs for one game
//! the caller already knows), this module identifies *which* game (if any)
//! each dropped file belongs to by matching checksums against every game in
//! the catalogue the frontend hands us, falling back to whatever game page
//! the user currently has focused.

use std::{path::Path, sync::Arc};

use crate::{config, download, platform, AppState};

use super::{dlc, stfs, Format};

/// One entry from the website's game catalogue, trimmed to just the fields
/// needed to match a dropped file against a game. Field names are camelCase
/// on the wire (see `bridge/mod.rs`'s `ProcessDrops` deserialization).
#[derive(serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueEntry {
    pub recomp_name: String,
    pub title: String,
    #[serde(default)]
    pub xex_sha256: String,
    #[serde(default)]
    pub update_checksum: String,
    #[serde(default)]
    pub update_status: String,
    #[serde(default)]
    pub dlc_names: Vec<String>,
}

/// Outcome of processing a single dropped file.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DropItem {
    pub file: String,
    pub kind: &'static str,   // "base" | "update" | "dlc" | "unknown"
    pub status: &'static str, // "installed" | "ignored" | "error"
    pub game: Option<String>,
    pub game_title: Option<String>,
    pub message: String,
}

/// Full result of a [`process_drops`] batch, polled by the frontend via
/// `getDropReport`.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DropReport {
    pub items: Vec<DropItem>,
    /// Game the frontend should navigate to / focus after processing, if any.
    pub focus_game: Option<String>,
}

fn find_by_xex_sha<'a>(catalogue: &'a [CatalogueEntry], sha: &str) -> Option<&'a CatalogueEntry> {
    if sha.is_empty() {
        return None;
    }
    catalogue
        .iter()
        .find(|g| !g.xex_sha256.is_empty() && g.xex_sha256.eq_ignore_ascii_case(sha))
}

fn find_by_update_checksum<'a>(catalogue: &'a [CatalogueEntry], sha: &str) -> Option<&'a CatalogueEntry> {
    if sha.is_empty() {
        return None;
    }
    catalogue
        .iter()
        .find(|g| !g.update_checksum.is_empty() && g.update_checksum.eq_ignore_ascii_case(sha))
}

fn find_focused<'a>(catalogue: &'a [CatalogueEntry], focused: &Option<String>) -> Option<&'a CatalogueEntry> {
    let name = focused.as_deref()?;
    catalogue.iter().find(|g| g.recomp_name == name)
}

/// Process every dropped path against `catalogue`, updating `state.drop_report`
/// (and `state.is_extracting`) as it goes. Runs synchronously on the calling
/// thread — the bridge dispatches this on a spawned thread.
pub fn process_drops(
    paths: &[String],
    focused: Option<String>,
    catalogue: Vec<CatalogueEntry>,
    state: Arc<AppState>,
) {
    state.is_extracting.store(true, std::sync::atomic::Ordering::Relaxed);
    *state.drop_status.lock().unwrap() = format!("Processing 0 of {}", paths.len());

    let mut items = Vec::with_capacity(paths.len());
    let mut focus_game: Option<String> = None;

    for (i, path) in paths.iter().enumerate() {
        *state.drop_status.lock().unwrap() = format!("Processing {} of {}", i + 1, paths.len());
        let item = process_one(path, &focused, &catalogue);
        if item.status == "installed" {
            if let Some(g) = &item.game {
                focus_game.get_or_insert_with(|| g.clone());
            }
        }
        items.push(item);
    }

    *state.drop_report.lock().unwrap() = Some(DropReport { items, focus_game });
    *state.drop_status.lock().unwrap() = String::new();
    state.is_extracting.store(false, std::sync::atomic::Ordering::Relaxed);
}

fn process_one(path: &str, focused: &Option<String>, catalogue: &[CatalogueEntry]) -> DropItem {
    let file_name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());

    if !Path::new(path).exists() {
        return DropItem {
            file: file_name,
            kind: "unknown",
            status: "error",
            game: None,
            game_title: None,
            message: "file not found".to_string(),
        };
    }

    match super::detect_format(path) {
        Ok(Format::Stfs) => {
            let has_xex = stfs::has_default_xex(path).unwrap_or(false);
            if has_xex {
                return process_base_game(path, &file_name, focused, catalogue);
            }
            match stfs::read_header_meta(path) {
                Ok(meta) => match meta.content_type {
                    0xB0000 => process_update(path, &file_name, focused, catalogue),
                    0x2 => process_dlc(path, &file_name, focused, catalogue),
                    _ => process_base_game(path, &file_name, focused, catalogue),
                },
                Err(e) => DropItem {
                    file: file_name,
                    kind: "unknown",
                    status: "error",
                    game: None,
                    game_title: None,
                    message: format!("could not read package header: {}", e),
                },
            }
        }
        Ok(Format::Xdvdfs) | Err(_) => process_base_game(path, &file_name, focused, catalogue),
    }
}

fn process_base_game(
    path: &str,
    file_name: &str,
    focused: &Option<String>,
    catalogue: &[CatalogueEntry],
) -> DropItem {
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let games_folder = config::get_games_folder();
    let available = platform::available_space(&games_folder);
    // Extracted assets are roughly the size of the source image; require some
    // headroom since STFS/ISO extraction isn't perfectly 1:1.
    if file_size > 0 && available < file_size {
        return DropItem {
            file: file_name.to_string(),
            kind: "base",
            status: "error",
            game: None,
            game_title: None,
            message: "not enough free disk space to extract this file".to_string(),
        };
    }

    let temp = match tempfile::Builder::new()
        .prefix(".goopie-tmp-")
        .tempdir_in(&games_folder)
    {
        Ok(t) => t,
        Err(e) => {
            return DropItem {
                file: file_name.to_string(),
                kind: "base",
                status: "error",
                game: None,
                game_title: None,
                message: format!("could not create temp directory: {}", e),
            };
        }
    };

    if let Err(e) = super::extract_to_dir(path, temp.path()) {
        return DropItem {
            file: file_name.to_string(),
            kind: "base",
            status: "error",
            game: None,
            game_title: None,
            message: format!("extraction failed: {}", e),
        };
    }

    let xex_path = temp.path().join("default.xex");
    let sha = download::sha256_file(&xex_path.to_string_lossy()).unwrap_or_default();

    if let Some(game) = find_by_xex_sha(catalogue, &sha) {
        return commit_base_game(temp, path.into(), file_name, game);
    }

    if let Some(game) = find_focused(catalogue, focused) {
        if game.xex_sha256.is_empty() {
            return commit_base_game(temp, path.into(), file_name, game);
        }
        return DropItem {
            file: file_name.to_string(),
            kind: "base",
            status: "error",
            game: None,
            game_title: None,
            message: format!(
                "This doesn't match {}'s expected files (different region/version?).",
                game.title
            ),
        };
    }

    DropItem {
        file: file_name.to_string(),
        kind: "base",
        status: "error",
        game: None,
        game_title: None,
        message: "These files don't match any known game in the catalogue.".to_string(),
    }
}

fn commit_base_game(
    temp: tempfile::TempDir,
    _src_path: String,
    file_name: &str,
    game: &CatalogueEntry,
) -> DropItem {
    // `keep` disarms the auto-cleanup guard; `commit_assets` takes ownership
    // (renames or removes it) from here.
    let temp_path = temp.keep();
    match super::commit_assets(&temp_path, &game.recomp_name) {
        Ok(()) => DropItem {
            file: file_name.to_string(),
            kind: "base",
            status: "installed",
            game: Some(game.recomp_name.clone()),
            game_title: Some(game.title.clone()),
            message: format!("Installed as {}", game.title),
        },
        Err(e) => {
            let _ = std::fs::remove_dir_all(&temp_path);
            DropItem {
                file: file_name.to_string(),
                kind: "base",
                status: "error",
                game: Some(game.recomp_name.clone()),
                game_title: Some(game.title.clone()),
                message: format!("failed to install: {}", e),
            }
        }
    }
}

fn process_update(
    path: &str,
    file_name: &str,
    focused: &Option<String>,
    catalogue: &[CatalogueEntry],
) -> DropItem {
    let sha = download::sha256_file(path).unwrap_or_default();

    if let Some(game) = find_by_update_checksum(catalogue, &sha) {
        return install_update_for(path, file_name, game);
    }

    if let Some(game) = find_focused(catalogue, focused) {
        if !game.update_checksum.is_empty() {
            return DropItem {
                file: file_name.to_string(),
                kind: "update",
                status: "error",
                game: None,
                game_title: None,
                message: format!("This doesn't match {}'s expected title update.", game.title),
            };
        }
        if game.update_status != "hidden" {
            return install_update_for(path, file_name, game);
        }
    }

    DropItem {
        file: file_name.to_string(),
        kind: "update",
        status: "ignored",
        game: None,
        game_title: None,
        message: "Title update checksum not known; drop it on a game's page to install it there.".to_string(),
    }
}

fn install_update_for(path: &str, file_name: &str, game: &CatalogueEntry) -> DropItem {
    match dlc::install_update(&game.recomp_name, path, "") {
        Ok(_) => DropItem {
            file: file_name.to_string(),
            kind: "update",
            status: "installed",
            game: Some(game.recomp_name.clone()),
            game_title: Some(game.title.clone()),
            message: format!("Installed title update for {}", game.title),
        },
        Err(e) => DropItem {
            file: file_name.to_string(),
            kind: "update",
            status: "error",
            game: Some(game.recomp_name.clone()),
            game_title: Some(game.title.clone()),
            message: format!("failed to install update: {}", e),
        },
    }
}

fn process_dlc(
    path: &str,
    file_name: &str,
    focused: &Option<String>,
    catalogue: &[CatalogueEntry],
) -> DropItem {
    let Some(game) = find_focused(catalogue, focused) else {
        return DropItem {
            file: file_name.to_string(),
            kind: "dlc",
            status: "ignored",
            game: None,
            game_title: None,
            message: "Drop DLC on a game's page to install it.".to_string(),
        };
    };

    match dlc::install_dlc(&game.recomp_name, path, &game.dlc_names) {
        Ok(_) => DropItem {
            file: file_name.to_string(),
            kind: "dlc",
            status: "installed",
            game: Some(game.recomp_name.clone()),
            game_title: Some(game.title.clone()),
            message: format!("Installed DLC for {}", game.title),
        },
        Err(e) => DropItem {
            file: file_name.to_string(),
            kind: "dlc",
            status: "error",
            game: Some(game.recomp_name.clone()),
            game_title: Some(game.title.clone()),
            message: format!("failed to install DLC: {}", e),
        },
    }
}
