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

use sha2::{Digest, Sha256};

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

// ── Cloud save export/import (for cloud_saves.rs) ─────────────────────────────
//
// Reuses the same trimmed-subtree convention as backup/restore (each title
// ID's `00000001` + `Headers/00000001`, see the module doc above) so a
// cloud-synced save behaves exactly like a local one.

/// Zip the live save subtree for `game` plus a deterministic content hash over
/// its files. Returns `None` if there's no live save data yet (nothing to
/// sync). The hash is stable across machines — byte-identical save data always
/// hashes the same — which is what lets sync skip no-op uploads.
pub fn export_live_save_zip(game: &str) -> Option<(Vec<u8>, String)> {
    let user_root = paths::rex_user_folder()?;
    export_live_save_zip_at(&user_root, game)
}

fn export_live_save_zip_at(user_root: &Path, game: &str) -> Option<(Vec<u8>, String)> {
    let profile_dir = layout::live_dir(user_root, game).join(PROFILE_XUID);
    let entries = collect_save_entries(&profile_dir).ok()?;
    if entries.is_empty() {
        return None;
    }
    let hash = hash_entries(&entries);
    let zip_bytes = zip_entries(&entries).ok()?;
    Some((zip_bytes, hash))
}

/// Deterministic content hash of the live save subtree, or `None` if there's
/// no live save data yet. Cheaper than `export_live_save_zip` when the caller
/// only needs to know whether the save changed (skip re-uploading if the hash
/// still matches what was last synced).
pub fn live_save_hash(game: &str) -> Option<String> {
    let user_root = paths::rex_user_folder()?;
    live_save_hash_at(&user_root, game)
}

fn live_save_hash_at(user_root: &Path, game: &str) -> Option<String> {
    let profile_dir = layout::live_dir(user_root, game).join(PROFILE_XUID);
    let entries = collect_save_entries(&profile_dir).ok()?;
    if entries.is_empty() {
        return None;
    }
    Some(hash_entries(&entries))
}

/// Extract a zip produced by `export_live_save_zip` into `game`'s live save
/// dir, replacing any existing save content for the title IDs present in the
/// archive first — mirrors `restore_save`'s replace-in-place semantics, so no
/// stray files from a previous save linger.
pub fn import_save_zip(game: &str, bytes: &[u8]) -> bool {
    let Some(user_root) = paths::rex_user_folder() else {
        eprintln!("[saves] Could not determine the save user folder");
        return false;
    };
    import_save_zip_at(&user_root, game, bytes)
}

fn import_save_zip_at(user_root: &Path, game: &str, bytes: &[u8]) -> bool {
    let entries = match unzip_entries(bytes) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[saves] import_save_zip: failed to read zip: {}", e);
            return false;
        }
    };
    if entries.is_empty() {
        return false;
    }

    let profile_dir = layout::live_dir(&user_root, game).join(PROFILE_XUID);

    let mut title_ids = std::collections::BTreeSet::new();
    for (rel_path, _) in &entries {
        if let Some(title_id) = rel_path.split('/').next() {
            title_ids.insert(title_id.to_string());
        }
    }
    for title_id in &title_ids {
        let save_dir = profile_dir.join(title_id).join(SAVE_CONTENT_TYPE);
        if save_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&save_dir) {
                eprintln!("[saves] import_save_zip: failed to clear {}: {}", save_dir.display(), e);
                return false;
            }
        }
        let headers_dir = profile_dir.join(title_id).join("Headers").join(SAVE_CONTENT_TYPE);
        if headers_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&headers_dir) {
                eprintln!("[saves] import_save_zip: failed to clear {}: {}", headers_dir.display(), e);
                return false;
            }
        }
    }

    for (rel_path, data) in &entries {
        let dest = profile_dir.join(rel_path);
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("[saves] import_save_zip: failed to create {}: {}", parent.display(), e);
                return false;
            }
        }
        if let Err(e) = std::fs::write(&dest, data) {
            eprintln!("[saves] import_save_zip: failed to write {}: {}", dest.display(), e);
            return false;
        }
    }
    true
}

/// Import a zip produced by `export_live_save_zip` directly into a *named
/// slot* (not the live save) — used by cloud sync to preserve a save that's
/// about to be superseded (a remote upload we haven't seen, or the local save
/// before a pull overwrites it) without disturbing whatever's currently live.
/// The result shows up as an ordinary save slot in the Save Manager.
pub fn import_zip_as_slot(game: &str, bytes: &[u8], slot_name: &str) -> bool {
    import_zip_as_slot_at(&games_root(), game, bytes, slot_name)
}

fn import_zip_as_slot_at(games_root: &Path, game: &str, bytes: &[u8], slot_name: &str) -> bool {
    let entries = match unzip_entries(bytes) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[saves] import_zip_as_slot: failed to read zip: {}", e);
            return false;
        }
    };
    if entries.is_empty() {
        return false;
    }
    let dest = layout::saves_dir(games_root, game).join(slot_name);
    for (rel_path, data) in &entries {
        let path = dest.join(rel_path);
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("[saves] import_zip_as_slot: failed to create {}: {}", parent.display(), e);
                return false;
            }
        }
        if let Err(e) = std::fs::write(&path, data) {
            eprintln!("[saves] import_zip_as_slot: failed to write {}: {}", path.display(), e);
            return false;
        }
    }
    true
}

/// Walk a `<live>/B13EBABEBABEBABE` profile dir and collect only the
/// save-game content type for each title ID — `<title_id>/00000001/**` and
/// `<title_id>/Headers/00000001/**` — as `(forward-slash relative path,
/// bytes)` pairs, sorted by path for determinism (so the same save data
/// always hashes/zips identically regardless of directory-listing order).
fn collect_save_entries(profile_dir: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut entries = Vec::new();
    if !profile_dir.is_dir() {
        return Ok(entries);
    }
    for title_entry in std::fs::read_dir(profile_dir)?.flatten() {
        if !title_entry.file_type()?.is_dir() {
            continue;
        }
        let title_dir = title_entry.path();
        walk_dir_relative(&title_dir.join(SAVE_CONTENT_TYPE), profile_dir, &mut entries)?;
        walk_dir_relative(&title_dir.join("Headers").join(SAVE_CONTENT_TYPE), profile_dir, &mut entries)?;
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// Recursively collect every file under `dir` as `(path relative to `base`
/// with forward slashes, contents)`.
fn walk_dir_relative(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_dir_relative(&path, base, out)?;
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            out.push((rel_str, std::fs::read(&path)?));
        }
    }
    Ok(())
}

/// Deterministic SHA-256 over a sorted `(path, bytes)` list — order and
/// lengths are folded in so this can't collide across different directory
/// shapes with the same concatenated bytes.
fn hash_entries(entries: &[(String, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    for (path, data) in entries {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update((data.len() as u64).to_le_bytes());
        hasher.update(data);
    }
    hex::encode(hasher.finalize())
}

fn zip_entries(entries: &[(String, Vec<u8>)]) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    use zip::write::{SimpleFileOptions, ZipWriter};

    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = ZipWriter::new(cursor);
        let opts = SimpleFileOptions::default();
        for (path, data) in entries {
            zw.start_file(path, opts)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            zw.write_all(data)?;
        }
        zw.finish().map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    }
    Ok(buf)
}

fn unzip_entries(bytes: &[u8]) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut entries = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if entry.name().ends_with('/') {
            continue;
        }
        let name = entry.name().to_string();
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut data)?;
        entries.push((name, data));
    }
    Ok(entries)
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

    // ── Cloud save export/import ──────────────────────────────────────────────

    #[test]
    fn export_then_import_round_trips_the_live_save_byte_for_byte() {
        let (_tmp, _games_root, user_root) = fixture();

        let (zip_bytes, hash) = export_live_save_zip_at(&user_root, GAME)
            .expect("live save data exists, export should succeed");
        assert!(!zip_bytes.is_empty());
        assert_eq!(live_save_hash_at(&user_root, GAME).unwrap(), hash, "hash should match what export reports");

        // Mutate the live save so import has something to overwrite/detect.
        let save_path = save_data_path(&user_root);
        std::fs::write(&save_path, b"clobbered").unwrap();
        let stale_path = save_path.parent().unwrap().join("stale.tmp");
        std::fs::write(&stale_path, b"should be removed by import").unwrap();

        assert!(import_save_zip_at(&user_root, GAME, &zip_bytes));

        assert_eq!(std::fs::read(&save_path).unwrap(), b"save-data-v1");
        assert!(!stale_path.exists(), "import should wipe the existing save content type before extracting");
        assert_eq!(live_save_hash_at(&user_root, GAME).unwrap(), hash, "re-hashing the restored save should match");

        // Unrelated data untouched by import.
        let live = layout::live_dir(&user_root, GAME);
        assert!(live.join("0000000000000000").join(TITLE_ID).join("00000002").join("somehash").join("dlcfile.bin").exists());
        assert!(live.join("achievements").join(format!("{TITLE_ID}.toml")).exists());
        assert!(live.join("cache").join("shaders").join("shareable").join("shader.bin").exists());
    }

    #[test]
    fn export_and_hash_are_none_when_there_is_no_live_save_data() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let user_root = tmp.path().join("user"); // live dir intentionally not created
        assert!(export_live_save_zip_at(&user_root, GAME).is_none());
        assert!(live_save_hash_at(&user_root, GAME).is_none());
    }

    #[test]
    fn hash_is_stable_regardless_of_directory_listing_order() {
        let (_tmp, _games_root, user_root) = fixture();
        let hash_a = live_save_hash_at(&user_root, GAME).unwrap();
        let hash_b = live_save_hash_at(&user_root, GAME).unwrap();
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn hash_changes_when_save_content_changes() {
        let (_tmp, _games_root, user_root) = fixture();
        let before = live_save_hash_at(&user_root, GAME).unwrap();
        std::fs::write(save_data_path(&user_root), b"different-save-data").unwrap();
        let after = live_save_hash_at(&user_root, GAME).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn import_zip_as_slot_writes_an_ordinary_browsable_slot() {
        let (_tmp, games_root, user_root) = fixture();
        let (zip_bytes, _hash) = export_live_save_zip_at(&user_root, GAME).unwrap();

        assert!(import_zip_as_slot_at(&games_root, GAME, &zip_bytes, "cloud-backup-123"));

        let slots = get_save_slots_at(&games_root, GAME);
        assert!(slots.contains(&"cloud-backup-123".to_string()), "cloud backup should show up as an ordinary slot");

        let slot = layout::saves_dir(&games_root, GAME).join("cloud-backup-123");
        let copied_save = slot.join(TITLE_ID).join(SAVE_CONTENT_TYPE).join("0000000000000001").join("SaveData.bin");
        assert_eq!(std::fs::read(copied_save).unwrap(), b"save-data-v1");
    }
}

