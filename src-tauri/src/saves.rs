//! Save-slot management: backup, restore, delete, rename, list.
//!
//! Save slots live at `<games>/<game>/saves/<slot_name>/`.
//! The active-slot marker is a plain-text file at `<games>/<game>/saves/.active`.
//! Backup/restore copies to/from the OS Documents directory under `<game>/`.

use std::path::{Path, PathBuf};

use crate::{config, paths, platform};

fn saves_dir(game: &str) -> PathBuf {
    PathBuf::from(config::get_games_folder())
        .join(game)
        .join("saves")
}

fn active_save_file(game: &str) -> PathBuf {
    saves_dir(game).join(".active")
}

// ── List / query ──────────────────────────────────────────────────────────────

pub fn get_save_slots(game: &str) -> Vec<String> {
    let dir = saves_dir(game);
    if !dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut slots: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    slots.sort();
    slots
}

pub fn get_save_slot_count(game: &str) -> i32 {
    get_save_slots(game).len() as i32
}

pub fn get_active_save(game: &str) -> String {
    std::fs::read_to_string(active_save_file(game))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// ── Backup (game data → slot) ─────────────────────────────────────────────────

pub fn backup_save(game: &str, slot_name: &str) -> bool {
    let Some(docs) = paths::documents_dir() else {
        eprintln!("[saves] Could not determine Documents directory");
        return false;
    };

    let source = docs.join(game);
    if !source.exists() {
        eprintln!("[saves] Source save path does not exist: {}", source.display());
        return false;
    }

    let dest = saves_dir(game).join(slot_name);
    if let Err(e) = copy_dir_all(&source, &dest) {
        eprintln!("[saves] backupSave error: {}", e);
        return false;
    }

    set_active_save(game, slot_name);
    true
}

// ── Restore (slot → game data) ────────────────────────────────────────────────

pub fn restore_save(game: &str, slot_name: &str) -> bool {
    let slot = saves_dir(game).join(slot_name);
    if !slot.exists() {
        eprintln!("[saves] Slot does not exist: {}", slot.display());
        return false;
    }

    let Some(docs) = paths::documents_dir() else { return false };
    let dest = docs.join(game);

    // Remove existing game save data and replace with the slot contents.
    if dest.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dest) {
            eprintln!("[saves] restoreSave: failed to remove existing data: {}", e);
            return false;
        }
    }

    if let Err(e) = copy_dir_all(&slot, &dest) {
        eprintln!("[saves] restoreSave error: {}", e);
        return false;
    }

    set_active_save(game, slot_name);
    true
}

// ── Delete ────────────────────────────────────────────────────────────────────

pub fn delete_save(game: &str, slot_name: &str) -> bool {
    let slot = saves_dir(game).join(slot_name);
    if !slot.exists() {
        return false;
    }
    if let Err(e) = std::fs::remove_dir_all(&slot) {
        eprintln!("[saves] deleteSave error: {}", e);
        return false;
    }
    // Clear active marker if this was the active slot.
    if get_active_save(game) == slot_name {
        let _ = std::fs::remove_file(active_save_file(game));
    }
    true
}

/// Delete the live game save data from Documents (not a slot).
pub fn delete_current_save(game: &str) -> bool {
    let Some(docs) = paths::documents_dir() else { return false };
    let save_path = docs.join(game);
    if !save_path.exists() {
        return true; // Goal achieved — no data present.
    }
    if let Err(e) = std::fs::remove_dir_all(&save_path) {
        eprintln!("[saves] deleteCurrentSave error: {}", e);
        return false;
    }
    let _ = std::fs::remove_file(active_save_file(game));
    true
}

// ── Rename ────────────────────────────────────────────────────────────────────

pub fn rename_save(game: &str, old_name: &str, new_name: &str) -> bool {
    let old = saves_dir(game).join(old_name);
    let new = saves_dir(game).join(new_name);
    if !old.exists() || new.exists() {
        return false;
    }
    if let Err(e) = std::fs::rename(&old, &new) {
        eprintln!("[saves] renameSave error: {}", e);
        return false;
    }
    if get_active_save(game) == old_name {
        set_active_save(game, new_name);
    }
    true
}

// ── Open save folder ──────────────────────────────────────────────────────────

pub fn open_save_folder(game: &str) {
    let Some(docs) = paths::documents_dir() else { return };
    let save_path = docs.join(game);
    let _ = std::fs::create_dir_all(&save_path);
    platform::open_folder(&save_path.to_string_lossy());
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn set_active_save(game: &str, slot_name: &str) {
    let active_file = active_save_file(game);
    if let Some(parent) = active_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&active_file, slot_name);
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let dst_entry = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir_all(&entry.path(), &dst_entry)?;
        } else {
            std::fs::copy(entry.path(), &dst_entry)?;
        }
    }
    Ok(())
}
