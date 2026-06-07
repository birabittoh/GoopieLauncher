//! Save-slot management: backup, restore, delete, rename, list.
//!
//! Save slots live at `<games>/<game>/saves/<slot_name>/`.
//! The active-slot marker is a plain-text file at `<games>/<game>/saves/.active`.
//! Backup/restore copies to/from `<rex_user_folder>/<game>/` — the directory the
//! Rex runtime actually writes the live save to (see [`paths::rex_user_folder`]).
//! This is the OS Documents folder on Windows, but `~/.local/share/<game>` (or
//! `$XDG_DATA_HOME/<game>`) on Linux/macOS — *not* Documents.

use std::path::{Path, PathBuf};

use crate::{config, paths, platform};

/// Pure path helpers, parameterized over the two base directories so the core
/// logic can be exercised in tests against temp directories instead of the
/// real games folder / user folder.
mod layout {
    use std::path::{Path, PathBuf};

    pub fn saves_dir(games_root: &Path, game: &str) -> PathBuf {
        games_root.join(game).join("saves")
    }

    pub fn active_save_file(games_root: &Path, game: &str) -> PathBuf {
        saves_dir(games_root, game).join(".active")
    }

    pub fn live_dir(user_root: &Path, game: &str) -> PathBuf {
        user_root.join(game)
    }
}

// ── List / query ──────────────────────────────────────────────────────────────

pub fn get_save_slots(game: &str) -> Vec<String> {
    get_save_slots_at(&games_root(), game)
}

pub fn get_save_slot_count(game: &str) -> i32 {
    get_save_slots(game).len() as i32
}

pub fn get_active_save(game: &str) -> String {
    get_active_save_at(&games_root(), game)
}

// ── Backup (game data → slot) ─────────────────────────────────────────────────

pub fn backup_save(game: &str, slot_name: &str) -> bool {
    let Some(user_root) = paths::rex_user_folder() else {
        eprintln!("[saves] Could not determine the save user folder");
        return false;
    };
    backup_save_at(&games_root(), &user_root, game, slot_name)
}

// ── Restore (slot → game data) ────────────────────────────────────────────────

pub fn restore_save(game: &str, slot_name: &str) -> bool {
    let Some(user_root) = paths::rex_user_folder() else {
        eprintln!("[saves] Could not determine the save user folder");
        return false;
    };
    restore_save_at(&games_root(), &user_root, game, slot_name)
}

// ── Delete ────────────────────────────────────────────────────────────────────

pub fn delete_save(game: &str, slot_name: &str) -> bool {
    delete_save_at(&games_root(), game, slot_name)
}

/// Delete the live game save data (not a slot).
pub fn delete_current_save(game: &str) -> bool {
    let Some(user_root) = paths::rex_user_folder() else {
        eprintln!("[saves] Could not determine the save user folder");
        return false;
    };
    delete_current_save_at(&games_root(), &user_root, game)
}

// ── Rename ────────────────────────────────────────────────────────────────────

pub fn rename_save(game: &str, old_name: &str, new_name: &str) -> bool {
    rename_save_at(&games_root(), game, old_name, new_name)
}

// ── Open save folder ──────────────────────────────────────────────────────────

pub fn open_save_folder(game: &str) {
    let Some(user_root) = paths::rex_user_folder() else { return };
    let save_path = layout::live_dir(&user_root, game);
    let _ = std::fs::create_dir_all(&save_path);
    platform::open_folder(&save_path.to_string_lossy());
}

fn games_root() -> PathBuf {
    PathBuf::from(config::get_games_folder())
}

// ── Core logic, parameterized over base directories (unit-testable) ──────────

fn get_save_slots_at(games_root: &Path, game: &str) -> Vec<String> {
    let dir = layout::saves_dir(games_root, game);
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

fn get_active_save_at(games_root: &Path, game: &str) -> String {
    std::fs::read_to_string(layout::active_save_file(games_root, game))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn set_active_save_at(games_root: &Path, game: &str, slot_name: &str) {
    let active_file = layout::active_save_file(games_root, game);
    if let Some(parent) = active_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&active_file, slot_name);
}

fn backup_save_at(games_root: &Path, user_root: &Path, game: &str, slot_name: &str) -> bool {
    let source = layout::live_dir(user_root, game);
    if !source.exists() {
        eprintln!("[saves] Source save path does not exist: {}", source.display());
        return false;
    }

    let dest = layout::saves_dir(games_root, game).join(slot_name);
    if let Err(e) = copy_dir_all(&source, &dest) {
        eprintln!("[saves] backupSave error: {}", e);
        return false;
    }

    set_active_save_at(games_root, game, slot_name);
    true
}

fn restore_save_at(games_root: &Path, user_root: &Path, game: &str, slot_name: &str) -> bool {
    let slot = layout::saves_dir(games_root, game).join(slot_name);
    if !slot.exists() {
        eprintln!("[saves] Slot does not exist: {}", slot.display());
        return false;
    }

    let dest = layout::live_dir(user_root, game);

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

    set_active_save_at(games_root, game, slot_name);
    true
}

fn delete_save_at(games_root: &Path, game: &str, slot_name: &str) -> bool {
    let slot = layout::saves_dir(games_root, game).join(slot_name);
    if !slot.exists() {
        return false;
    }
    if let Err(e) = std::fs::remove_dir_all(&slot) {
        eprintln!("[saves] deleteSave error: {}", e);
        return false;
    }
    // Clear active marker if this was the active slot.
    if get_active_save_at(games_root, game) == slot_name {
        let _ = std::fs::remove_file(layout::active_save_file(games_root, game));
    }
    true
}

fn delete_current_save_at(games_root: &Path, user_root: &Path, game: &str) -> bool {
    let save_path = layout::live_dir(user_root, game);
    if !save_path.exists() {
        return true; // Goal achieved — no data present.
    }
    if let Err(e) = std::fs::remove_dir_all(&save_path) {
        eprintln!("[saves] deleteCurrentSave error: {}", e);
        return false;
    }
    let _ = std::fs::remove_file(layout::active_save_file(games_root, game));
    true
}

fn rename_save_at(games_root: &Path, game: &str, old_name: &str, new_name: &str) -> bool {
    let old = layout::saves_dir(games_root, game).join(old_name);
    let new = layout::saves_dir(games_root, game).join(new_name);
    if !old.exists() || new.exists() {
        return false;
    }
    if let Err(e) = std::fs::rename(&old, &new) {
        eprintln!("[saves] renameSave error: {}", e);
        return false;
    }
    if get_active_save_at(games_root, game) == old_name {
        set_active_save_at(games_root, game, new_name);
    }
    true
}

// ── Internal helpers ──────────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    const GAME: &str = "kameorepowered";

    /// Sets up `<tmp>/games` and `<tmp>/user` roots plus a live save dir at
    /// `<tmp>/user/<game>/` containing one file. Returns `(tempdir, games_root, user_root)`.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let games_root = tmp.path().join("games");
        let user_root = tmp.path().join("user");
        let live = layout::live_dir(&user_root, GAME);
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("KameoSave.svg"), b"save-data-v1").unwrap();
        (tmp, games_root, user_root)
    }

    #[test]
    fn backup_copies_live_into_slot_and_marks_active() {
        let (_tmp, games_root, user_root) = fixture();

        assert!(backup_save_at(&games_root, &user_root, GAME, "slot1"));

        let copied = layout::saves_dir(&games_root, GAME).join("slot1").join("KameoSave.svg");
        assert_eq!(std::fs::read(copied).unwrap(), b"save-data-v1");
        assert_eq!(get_active_save_at(&games_root, GAME), "slot1");
        assert_eq!(get_save_slots_at(&games_root, GAME), vec!["slot1".to_string()]);
    }

    #[test]
    fn backup_fails_when_live_dir_is_missing() {
        // Regression test for the reported bug: on Linux the launcher looked
        // for the live save under Documents, where it never existed, so
        // backup silently failed (and deleteCurrentSave silently "succeeded"
        // without touching the real save).
        let tmp = tempfile::tempdir().expect("tempdir");
        let games_root = tmp.path().join("games");
        let user_root = tmp.path().join("user"); // live dir intentionally not created

        assert!(!backup_save_at(&games_root, &user_root, GAME, "slot1"));
        assert!(get_save_slots_at(&games_root, GAME).is_empty());
        assert_eq!(get_active_save_at(&games_root, GAME), "");
    }

    #[test]
    fn restore_replaces_live_with_slot_contents() {
        let (_tmp, games_root, user_root) = fixture();
        assert!(backup_save_at(&games_root, &user_root, GAME, "slot1"));

        // Mutate the live save so restore has something to overwrite/detect.
        let live = layout::live_dir(&user_root, GAME);
        std::fs::write(live.join("KameoSave.svg"), b"clobbered").unwrap();
        std::fs::write(live.join("stale.tmp"), b"should be removed by restore").unwrap();

        assert!(restore_save_at(&games_root, &user_root, GAME, "slot1"));

        assert_eq!(std::fs::read(live.join("KameoSave.svg")).unwrap(), b"save-data-v1");
        assert!(!live.join("stale.tmp").exists(), "restore should wipe the live dir before copying");
        assert_eq!(get_active_save_at(&games_root, GAME), "slot1");
    }

    #[test]
    fn restore_fails_when_slot_is_missing() {
        let (_tmp, games_root, user_root) = fixture();
        assert!(!restore_save_at(&games_root, &user_root, GAME, "does-not-exist"));
    }

    #[test]
    fn delete_save_removes_slot_and_clears_active_marker_only_if_active() {
        let (_tmp, games_root, user_root) = fixture();
        assert!(backup_save_at(&games_root, &user_root, GAME, "slot1"));
        assert!(backup_save_at(&games_root, &user_root, GAME, "slot2"));
        assert_eq!(get_active_save_at(&games_root, GAME), "slot2");

        // Deleting a non-active slot leaves the marker untouched.
        assert!(delete_save_at(&games_root, GAME, "slot1"));
        assert_eq!(get_active_save_at(&games_root, GAME), "slot2");
        assert_eq!(get_save_slots_at(&games_root, GAME), vec!["slot2".to_string()]);

        // Deleting the active slot clears the marker.
        assert!(delete_save_at(&games_root, GAME, "slot2"));
        assert_eq!(get_active_save_at(&games_root, GAME), "");
        assert!(get_save_slots_at(&games_root, GAME).is_empty());
    }

    #[test]
    fn delete_current_save_removes_live_dir_and_active_marker() {
        let (_tmp, games_root, user_root) = fixture();
        assert!(backup_save_at(&games_root, &user_root, GAME, "slot1"));
        let live = layout::live_dir(&user_root, GAME);
        assert!(live.exists());

        assert!(delete_current_save_at(&games_root, &user_root, GAME));

        assert!(!live.exists(), "live save directory should be gone");
        assert_eq!(get_active_save_at(&games_root, GAME), "");
    }

    #[test]
    fn delete_current_save_is_a_noop_when_already_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let games_root = tmp.path().join("games");
        let user_root = tmp.path().join("user");
        // No live dir created — mirrors the buggy scenario where the launcher
        // looked in the wrong base directory and found nothing.
        assert!(delete_current_save_at(&games_root, &user_root, GAME));
    }

    #[test]
    fn rename_save_renames_dir_and_follows_active_marker() {
        let (_tmp, games_root, user_root) = fixture();
        assert!(backup_save_at(&games_root, &user_root, GAME, "old-name"));
        assert_eq!(get_active_save_at(&games_root, GAME), "old-name");

        assert!(rename_save_at(&games_root, GAME, "old-name", "new-name"));

        assert_eq!(get_save_slots_at(&games_root, GAME), vec!["new-name".to_string()]);
        assert_eq!(get_active_save_at(&games_root, GAME), "new-name");
    }

    #[test]
    fn rename_save_fails_if_source_missing_or_dest_exists() {
        let (_tmp, games_root, user_root) = fixture();
        assert!(backup_save_at(&games_root, &user_root, GAME, "a"));
        assert!(backup_save_at(&games_root, &user_root, GAME, "b"));

        assert!(!rename_save_at(&games_root, GAME, "does-not-exist", "c"));
        assert!(!rename_save_at(&games_root, GAME, "a", "b"), "should not overwrite an existing slot");
    }

    /// End-to-end: backup the live save, wipe it (as "Create New Save" does),
    /// then restore — the live data should come back byte-for-byte identical.
    /// This is the exact flow the user reported as broken.
    #[test]
    fn round_trip_backup_then_wipe_then_restore() {
        let (_tmp, games_root, user_root) = fixture();
        let live = layout::live_dir(&user_root, GAME);

        assert!(backup_save_at(&games_root, &user_root, GAME, "before-reset"));
        assert!(delete_current_save_at(&games_root, &user_root, GAME));
        assert!(!live.exists());

        assert!(restore_save_at(&games_root, &user_root, GAME, "before-reset"));
        assert_eq!(std::fs::read(live.join("KameoSave.svg")).unwrap(), b"save-data-v1");
    }
}

