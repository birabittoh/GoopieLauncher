//! Cloud save sync: per-game opt-in state, the Google Drive refresh token, and
//! the push/pull orchestration. Talks to Drive via `drive.rs` and to the save
//! subtree via `saves::{export_live_save_zip, import_save_zip, live_save_hash,
//! import_zip_as_slot}`.
//!
//! Design (see the plan for the full rationale):
//! - One Drive file per game, named `save-<recompName>.zip`, living in the
//!   app's hidden `appDataFolder` (see `auth::DRIVE_APPDATA_SCOPE`) — never
//!   the user's real Drive files.
//! - Sync is hash-based: we never re-upload/re-download unless the content
//!   actually differs from what we last knew about, so a normal close/open
//!   with nothing changed touches the network not at all.
//! - Whenever a pull or push would silently discard save data the other side
//!   hasn't seen yet (a genuine conflict), the data about to be overwritten is
//!   preserved first as an ordinary save slot (`saves::import_zip_as_slot`),
//!   never just dropped.
//!
//! On-disk layout (`paths::cloud_saves_file()`):
//! ```json
//! {
//!   "refreshToken": "...",
//!   "games": {
//!     "<recompName>": {
//!       "enabled": true,
//!       "lastSyncedHash": "...",
//!       "lastSyncedAt": 0,
//!       "driveFileId": "..."
//!     }
//!   }
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{auth, drive, paths, saves};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GameSyncState {
    #[serde(default)]
    enabled: bool,
    #[serde(default, rename = "lastSyncedHash", skip_serializing_if = "Option::is_none")]
    last_synced_hash: Option<String>,
    #[serde(default, rename = "lastSyncedAt")]
    last_synced_at: u64,
    #[serde(default, rename = "driveFileId", skip_serializing_if = "Option::is_none")]
    drive_file_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default, rename = "refreshToken", skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default)]
    games: HashMap<String, GameSyncState>,
}

fn load(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(path: &Path, store: &Store) {
    if let Ok(s) = serde_json::to_string(store) {
        let _ = std::fs::write(path, s);
    }
}

// ── Transient (in-memory only) sync status, for the Save Manager UI ─────────
//
// Not persisted: a "syncing" flag or error message only matters for the life
// of the running launcher, so these live in process-wide statics rather than
// in the JSON store (mirrors how `AppState` tracks other pollable progress).

fn syncing_set() -> &'static Mutex<HashSet<String>> {
    static SYNCING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SYNCING.get_or_init(|| Mutex::new(HashSet::new()))
}

fn error_map() -> &'static Mutex<HashMap<String, String>> {
    static LAST_ERROR: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    LAST_ERROR.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mark_syncing(game: &str, syncing: bool) {
    let mut set = syncing_set().lock().unwrap();
    if syncing {
        set.insert(game.to_string());
    } else {
        set.remove(game);
    }
}

fn set_error(game: &str, msg: Option<String>) {
    let mut map = error_map().lock().unwrap();
    match msg {
        Some(m) => {
            map.insert(game.to_string(), m);
        }
        None => {
            map.remove(game);
        }
    }
}

pub fn is_syncing(game: &str) -> bool {
    syncing_set().lock().unwrap().contains(game)
}

pub fn last_error(game: &str) -> Option<String> {
    error_map().lock().unwrap().get(game).cloned()
}

// ── Local opt-in state ────────────────────────────────────────────────────────

/// Whether cloud saves are enabled for `game`.
pub fn is_enabled(game: &str) -> bool {
    load(&paths::cloud_saves_file())
        .games
        .get(game)
        .map(|g| g.enabled)
        .unwrap_or(false)
}

/// Whether the user has completed the Drive consent flow (i.e. we hold a
/// refresh token). Enabling cloud saves for the first time (for *any* game)
/// triggers this; every game after that reuses the same token.
pub fn has_drive_access() -> bool {
    load(&paths::cloud_saves_file()).refresh_token.is_some()
}

/// Store the refresh token obtained from `auth::google_sign_in_drive`.
pub fn store_refresh_token(refresh_token: &str) {
    let path = paths::cloud_saves_file();
    let mut store = load(&path);
    store.refresh_token = Some(refresh_token.to_string());
    save(&path, &store);
}

/// Enable or disable cloud saves for `game`. Disabling only stops future
/// syncs — it does not delete the Drive-side copy, so re-enabling later
/// resumes from where it left off instead of re-uploading from scratch.
pub fn set_enabled(game: &str, enabled: bool) {
    let path = paths::cloud_saves_file();
    let mut store = load(&path);
    store.games.entry(game.to_string()).or_default().enabled = enabled;
    save(&path, &store);
    if !enabled {
        set_error(game, None);
    }
}

/// Status for the Save Manager UI to poll.
pub fn status(game: &str) -> Value {
    let store = load(&paths::cloud_saves_file());
    let g = store.games.get(game).cloned().unwrap_or_default();
    json!({
        "enabled": g.enabled,
        "signedIn": store.refresh_token.is_some(),
        "lastSyncedAt": g.last_synced_at,
        "syncing": is_syncing(game),
        "error": last_error(game),
    })
}

// ── Sync orchestration ────────────────────────────────────────────────────────

fn drive_file_name(game: &str) -> String {
    format!("save-{}.zip", game)
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Exchange the stored refresh token for a fresh access token. Simpler than
/// caching a short-lived access token: sync only runs on game close/open, so
/// a full refresh call per sync is cheap and avoids tracking expiry.
fn access_token() -> Result<String, String> {
    let refresh_token = load(&paths::cloud_saves_file())
        .refresh_token
        .ok_or_else(|| "Cloud saves aren't connected to Google Drive yet".to_string())?;
    auth::refresh_access_token(&refresh_token)
}

fn update_synced_metadata(game: &str, hash: Option<String>, updated_at: u64, drive_file_id: String) {
    let path = paths::cloud_saves_file();
    let mut store = load(&path);
    let entry = store.games.entry(game.to_string()).or_default();
    entry.last_synced_hash = hash;
    entry.last_synced_at = updated_at;
    entry.drive_file_id = Some(drive_file_id);
    save(&path, &store);
}

/// Push path: call after a game closes. Uploads the live save to Drive if it
/// changed since the last sync. Entirely a no-op (no network/token use) if
/// cloud saves are disabled for this game or the save hasn't changed.
///
/// Always call from a background thread — this makes blocking HTTP calls.
pub fn sync_after_game_exit(game: &str) {
    if !is_enabled(game) {
        return;
    }
    mark_syncing(game, true);
    let result = push(game);
    mark_syncing(game, false);
    match result {
        Ok(()) => set_error(game, None),
        Err(e) => {
            eprintln!("[cloud_saves] sync_after_game_exit({}): {}", game, e);
            set_error(game, Some(e));
        }
    }
}

fn push(game: &str) -> Result<(), String> {
    let Some((zip_bytes, local_hash)) = saves::export_live_save_zip(game) else {
        return Ok(()); // no live save data yet — nothing to push
    };

    let path = paths::cloud_saves_file();
    let store = load(&path);
    let last_synced_hash = store.games.get(game).and_then(|g| g.last_synced_hash.clone());

    if last_synced_hash.as_deref() == Some(local_hash.as_str()) {
        return Ok(()); // unchanged since last sync
    }

    let token = access_token()?;
    let name = drive_file_name(game);
    let remote = drive::find_file(&token, &name)?;

    if let Some(remote) = &remote {
        if remote.hash.as_deref() == Some(local_hash.as_str()) {
            // Already in sync (e.g. another sync beat us to it) — just catch
            // our local bookkeeping up, no re-upload needed.
            update_synced_metadata(game, Some(local_hash), remote.updated_at.unwrap_or_else(now_epoch), remote.id.clone());
            return Ok(());
        }
        // The remote copy has content we haven't seen before — someone else
        // uploaded since our last sync. Preserve it as a save slot before we
        // overwrite it with this session's save, so it's never silently lost.
        if last_synced_hash.as_deref() != remote.hash.as_deref() {
            let remote_bytes = drive::download(&token, &remote.id)?;
            let slot = format!("cloud-conflict-{}", now_epoch());
            let _ = saves::import_zip_as_slot(game, &remote_bytes, &slot);
        }
    }

    let updated_at = now_epoch();
    let file_id = drive::upload(&token, remote.as_ref().map(|r| r.id.as_str()), &name, &zip_bytes, &local_hash, updated_at)?;
    update_synced_metadata(game, Some(local_hash), updated_at, file_id);
    Ok(())
}

/// Pull path: call when the user opens a game's page. Downloads the Drive
/// copy if it differs from what's on disk. Entirely a no-op (no network/token
/// use) if cloud saves are disabled for this game.
///
/// Always call from a background thread — this makes blocking HTTP calls.
pub fn sync_on_open(game: &str) {
    if !is_enabled(game) {
        return;
    }
    mark_syncing(game, true);
    let result = pull(game);
    mark_syncing(game, false);
    match result {
        Ok(()) => set_error(game, None),
        Err(e) => {
            eprintln!("[cloud_saves] sync_on_open({}): {}", game, e);
            set_error(game, Some(e));
        }
    }
}

fn pull(game: &str) -> Result<(), String> {
    let token = access_token()?;
    let name = drive_file_name(game);
    let Some(remote) = drive::find_file(&token, &name)? else {
        return Ok(()); // nothing uploaded yet for this game
    };

    let local_hash = saves::live_save_hash(game);
    if local_hash == remote.hash {
        // Already in sync — just make sure our bookkeeping agrees.
        update_synced_metadata(game, remote.hash.clone(), remote.updated_at.unwrap_or_else(now_epoch), remote.id.clone());
        return Ok(());
    }

    // The remote differs from what's on disk — pull it down, but back up
    // whatever's currently live first (if any) so a local save is never
    // silently discarded, even if the two had genuinely diverged rather than
    // the remote simply being newer.
    if local_hash.is_some() {
        if let Some((local_bytes, _)) = saves::export_live_save_zip(game) {
            let slot = format!("cloud-backup-{}", now_epoch());
            let _ = saves::import_zip_as_slot(game, &local_bytes, &slot);
        }
    }

    let remote_bytes = drive::download(&token, &remote.id)?;
    if saves::import_save_zip(game, &remote_bytes) {
        update_synced_metadata(game, remote.hash.clone(), remote.updated_at.unwrap_or_else(now_epoch), remote.id.clone());
    }
    Ok(())
}
