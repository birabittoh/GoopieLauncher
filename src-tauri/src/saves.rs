//! Save-slot management: backup, restore, delete, rename, list.
//!
//! Save slots live at `<games>/<game>/saves/<slot_name>/`.
//! The active-slot marker is a plain-text file at `<games>/<game>/saves/.active`.
//! Backup/restore copies to/from `<rex_user_folder>/<game>/` — the directory the
//! Rex runtime actually writes the live save to (see [`paths::rex_user_folder`]).
//! This is the OS Documents folder on Windows, but `~/.local/share/<game>` (or
//! `$XDG_DATA_HOME/<game>`) on Linux/macOS — *not* Documents.
//!
//! The Rex runtime mirrors an Xbox 360 storage device layout under
//! `<live_dir>/`:
//! ```text
//! <live_dir>/B13EBABEBABEBABE/<title_id>/00000001/<save>/...   ← actual save data
//! <live_dir>/B13EBABEBABEBABE/<title_id>/Headers/00000001/...  ← save metadata (name/icon)
//! <live_dir>/0000000000000000/<title_id>/00000002/...          ← installed DLC
//! <live_dir>/achievements/...                                  ← achievement progress
//! <live_dir>/cache/...                                         ← shader cache
//! ```
//! `B13EBABEBABEBABE` is the fixed profile XUID the SDK uses for the local
//! profile (see [`paths::vehicle_save_base`] for another consumer of it), and
//! `00000001` is the Xbox content-type ID for a saved game. Backup/restore only
//! touch that save subtree — DLC, achievements, and the shader cache are left
//! alone, since copying them is both wasteful and not what "restore a save"
//! should affect.
//!
//! Older save slots (created before this distinction existed) hold a full copy
//! of `<live_dir>` instead, recognizable by a top-level `B13EBABEBABEBABE`
//! directory. Restoring those still works — only the save subtree within them
//! is applied — but new backups always write the trimmed-down format.

use std::path::{Path, PathBuf};

use crate::{config, paths, platform};

/// Fixed profile XUID the Rex runtime uses for the local user's content.
const PROFILE_XUID: &str = "B13EBABEBABEBABE";
/// Xbox content-type ID for a saved game.
const SAVE_CONTENT_TYPE: &str = "00000001";

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
    let live = layout::live_dir(user_root, game);
    if !live.exists() {
        eprintln!("[saves] Source save path does not exist: {}", live.display());
        return false;
    }

    let profile_dir = live.join(PROFILE_XUID);
    if !profile_dir.is_dir() {
        eprintln!("[saves] No save data found at: {}", profile_dir.display());
        return false;
    }

    let dest = layout::saves_dir(games_root, game).join(slot_name);
    if let Err(e) = copy_save_subtrees(&profile_dir, &dest) {
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

    if let Err(e) = migrate_legacy_slot(&slot) {
        eprintln!("[saves] restoreSave: failed to migrate legacy slot: {}", e);
        return false;
    }

    let profile_dest = layout::live_dir(user_root, game).join(PROFILE_XUID);
    if let Err(e) = copy_save_subtrees(&slot, &profile_dest) {
        eprintln!("[saves] restoreSave error: {}", e);
        return false;
    }

    set_active_save_at(games_root, game, slot_name);
    true
}

/// If `slot` is in the old full-directory-copy format (top level contains a
/// `B13EBABEBABEBABE` profile directory), promote its title-id subdirectories
/// to the slot root and remove everything else (cache, achievements, DLC).
/// After this call the slot is always in the canonical trimmed format.
fn migrate_legacy_slot(slot: &Path) -> std::io::Result<()> {
    let legacy_profile = slot.join(PROFILE_XUID);
    if !legacy_profile.is_dir() {
        return Ok(());
    }

    // Move each <title_id> directory up from slot/B13E.../<title_id> → slot/<title_id>.
    for entry in std::fs::read_dir(&legacy_profile)?.flatten() {
        if entry.file_type()?.is_dir() {
            std::fs::rename(entry.path(), slot.join(entry.file_name()))?;
        }
    }

    // Remove everything that isn't a title-id directory (profile dir, cache,
    // achievements, DLC anonymous XUID, …).
    for entry in std::fs::read_dir(slot)?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Title-id directories are exactly 8 uppercase hex characters.
        let is_title_id = name_str.len() == 8
            && name_str.chars().all(|c| c.is_ascii_hexdigit());
        if !is_title_id {
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
    }

    Ok(())
}

/// Copy only the saved-game content type (`00000001`, plus its `Headers`
/// counterpart) for each title ID found directly under `src` into `dst`,
/// replacing any existing save data for that title ID. Leaves DLC,
/// achievements, and the shader cache untouched.
fn copy_save_subtrees(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(src)?.flatten() {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let title_id = entry.file_name();
        let src_title_dir = entry.path();
        let dst_title_dir = dst.join(&title_id);

        let src_save = src_title_dir.join(SAVE_CONTENT_TYPE);
        if src_save.is_dir() {
            let dst_save = dst_title_dir.join(SAVE_CONTENT_TYPE);
            if dst_save.exists() {
                std::fs::remove_dir_all(&dst_save)?;
            }
            copy_dir_all(&src_save, &dst_save)?;
        }

        let src_headers = src_title_dir.join("Headers").join(SAVE_CONTENT_TYPE);
        if src_headers.is_dir() {
            let dst_headers = dst_title_dir.join("Headers").join(SAVE_CONTENT_TYPE);
            if dst_headers.exists() {
                std::fs::remove_dir_all(&dst_headers)?;
            }
            copy_dir_all(&src_headers, &dst_headers)?;
        }
    }
    Ok(())
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
    const TITLE_ID: &str = "4D5307D2";

    /// Sets up `<tmp>/games` and `<tmp>/user` roots plus a live save dir at
    /// `<tmp>/user/<game>/` mirroring the real Xbox-content-style layout the
    /// Rex runtime writes: an actual save (under the profile XUID), DLC
    /// (under the anonymous XUID), achievements, and a shader cache. Returns
    /// `(tempdir, games_root, user_root)`.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let games_root = tmp.path().join("games");
        let user_root = tmp.path().join("user");
        let live = layout::live_dir(&user_root, GAME);

        let save_dir = live.join(PROFILE_XUID).join(TITLE_ID).join(SAVE_CONTENT_TYPE).join("0000000000000001");
        std::fs::create_dir_all(&save_dir).unwrap();
        std::fs::write(save_dir.join("SaveData.bin"), b"save-data-v1").unwrap();

        let headers_dir = live.join(PROFILE_XUID).join(TITLE_ID).join("Headers").join(SAVE_CONTENT_TYPE);
        std::fs::create_dir_all(&headers_dir).unwrap();
        std::fs::write(headers_dir.join("0000000000000001.header"), b"header-v1").unwrap();

        // Data that backup/restore must leave untouched.
        let dlc_dir = live.join("0000000000000000").join(TITLE_ID).join("00000002").join("somehash");
        std::fs::create_dir_all(&dlc_dir).unwrap();
        std::fs::write(dlc_dir.join("dlcfile.bin"), b"dlc-data").unwrap();

        let achievements_dir = live.join("achievements");
        std::fs::create_dir_all(&achievements_dir).unwrap();
        std::fs::write(achievements_dir.join(format!("{TITLE_ID}.toml")), b"achievement-data").unwrap();

        let shader_cache_dir = live.join("cache").join("shaders").join("shareable");
        std::fs::create_dir_all(&shader_cache_dir).unwrap();
        std::fs::write(shader_cache_dir.join("shader.bin"), b"shader-data").unwrap();

        (tmp, games_root, user_root)
    }

    fn save_data_path(user_root: &Path) -> PathBuf {
        layout::live_dir(user_root, GAME)
            .join(PROFILE_XUID)
            .join(TITLE_ID)
            .join(SAVE_CONTENT_TYPE)
            .join("0000000000000001")
            .join("SaveData.bin")
    }

    #[test]
    fn backup_copies_only_the_save_subtree_into_slot_and_marks_active() {
        let (_tmp, games_root, user_root) = fixture();

        assert!(backup_save_at(&games_root, &user_root, GAME, "slot1"));

        let slot = layout::saves_dir(&games_root, GAME).join("slot1");
        let copied_save = slot.join(TITLE_ID).join(SAVE_CONTENT_TYPE).join("0000000000000001").join("SaveData.bin");
        assert_eq!(std::fs::read(copied_save).unwrap(), b"save-data-v1");
        let copied_header = slot.join(TITLE_ID).join("Headers").join(SAVE_CONTENT_TYPE).join("0000000000000001.header");
        assert_eq!(std::fs::read(copied_header).unwrap(), b"header-v1");

        // DLC, achievements, and the shader cache must not have been copied.
        assert!(!slot.join("0000000000000000").exists());
        assert!(!slot.join("achievements").exists());
        assert!(!slot.join("cache").exists());
        assert!(!slot.join(PROFILE_XUID).exists(), "slot should be trimmed to the title-id subtree, not the profile dir");

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
    fn backup_fails_when_live_dir_has_no_save_data() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let games_root = tmp.path().join("games");
        let user_root = tmp.path().join("user");
        // Live dir exists (game has run) but nothing has been saved yet.
        std::fs::create_dir_all(layout::live_dir(&user_root, GAME).join("cache")).unwrap();

        assert!(!backup_save_at(&games_root, &user_root, GAME, "slot1"));
    }

    #[test]
    fn restore_replaces_save_data_without_touching_dlc_achievements_or_cache() {
        let (_tmp, games_root, user_root) = fixture();
        assert!(backup_save_at(&games_root, &user_root, GAME, "slot1"));

        // Mutate the live save so restore has something to overwrite/detect.
        let live = layout::live_dir(&user_root, GAME);
        let save_path = save_data_path(&user_root);
        std::fs::write(&save_path, b"clobbered").unwrap();
        let stale_path = save_path.parent().unwrap().join("stale.tmp");
        std::fs::write(&stale_path, b"should be removed by restore").unwrap();

        assert!(restore_save_at(&games_root, &user_root, GAME, "slot1"));

        assert_eq!(std::fs::read(&save_path).unwrap(), b"save-data-v1");
        assert!(!stale_path.exists(), "restore should wipe the existing save content type before copying");
        assert_eq!(get_active_save_at(&games_root, GAME), "slot1");

        // Unrelated data untouched by restore.
        assert!(live.join("0000000000000000").join(TITLE_ID).join("00000002").join("somehash").join("dlcfile.bin").exists());
        assert!(live.join("achievements").join(format!("{TITLE_ID}.toml")).exists());
        assert!(live.join("cache").join("shaders").join("shareable").join("shader.bin").exists());
    }

    #[test]
    fn restore_migrates_legacy_full_directory_slots_and_restores_save() {
        let (_tmp, games_root, user_root) = fixture();

        // Simulate a slot created by the old backup logic: a full copy of
        // the live dir, including cache/DLC/achievements.
        let live = layout::live_dir(&user_root, GAME);
        let legacy_slot = layout::saves_dir(&games_root, GAME).join("legacy-slot");
        copy_dir_all(&live, &legacy_slot).unwrap();

        // Wipe the live save data, as if restoring onto a fresh install.
        let save_path = save_data_path(&user_root);
        std::fs::write(&save_path, b"clobbered").unwrap();

        assert!(restore_save_at(&games_root, &user_root, GAME, "legacy-slot"));

        assert_eq!(std::fs::read(&save_path).unwrap(), b"save-data-v1");
        assert_eq!(get_active_save_at(&games_root, GAME), "legacy-slot");

        // Slot must have been migrated in-place to the canonical format.
        assert!(!legacy_slot.join(PROFILE_XUID).exists(), "profile XUID dir should be gone after migration");
        assert!(!legacy_slot.join("cache").exists(), "cache dir should be gone after migration");
        assert!(!legacy_slot.join("achievements").exists(), "achievements dir should be gone after migration");
        assert!(!legacy_slot.join("0000000000000000").exists(), "DLC dir should be gone after migration");
        assert!(legacy_slot.join(TITLE_ID).join(SAVE_CONTENT_TYPE).exists(), "title-id save dir should be at slot root after migration");
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
        assert_eq!(std::fs::read(save_data_path(&user_root)).unwrap(), b"save-data-v1");
    }
}

