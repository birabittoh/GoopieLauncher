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
//! - `lastSyncedHash` is the common ancestor both sides last agreed on, which
//!   is what lets `decide_pull` tell "the remote is newer" apart from merely
//!   "the remote is different". A pull only ever fast-forwards; reconciling
//!   genuine divergence is the push path's job.
//! - Syncs for one game are serialized against each other by `game_lock`, and
//!   all store writes by `store_lock` — both are reachable concurrently, since
//!   every sync entry point is spawned onto its own thread.
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
//!       "lastCheckedAt": 0,
//!       "driveFileId": "..."
//!     }
//!   }
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
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
    /// When save data last actually changed hands (upload or download).
    #[serde(default, rename = "lastSyncedAt")]
    last_synced_at: u64,
    /// When a sync last completed *successfully*, including the overwhelmingly
    /// common case where the hashes already matched and nothing was
    /// transferred. Distinct from `last_synced_at` because showing only the
    /// latter made a perfectly healthy setup read as "Last synced 3 days ago".
    #[serde(default, rename = "lastCheckedAt")]
    last_checked_at: u64,
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

/// Serializes every read-modify-write of the store file.
///
/// Syncs run on detached background threads, so a push and a pull routinely
/// overlap. Both used to load the whole store, mutate their own copy, and
/// write it back wholesale — so the loser's changes vanished. Losing a game's
/// sync metadata only costs a redundant transfer, but losing `refreshToken`
/// looks to the user like cloud saves spontaneously signed themselves out.
fn store_lock() -> &'static Mutex<()> {
    static STORE: OnceLock<Mutex<()>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(()))
}

/// Read a consistent snapshot of the store.
fn read_store() -> Store {
    let _guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
    load_locked(&paths::cloud_saves_file())
}

/// Load the store, apply `f`, and write the result back — all under
/// [`store_lock`], so concurrent mutators queue instead of clobbering each
/// other. Every mutation must go through this rather than a bare load/save
/// pair.
fn with_store<T>(f: impl FnOnce(&mut Store) -> T) -> T {
    let _guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
    let path = paths::cloud_saves_file();
    let mut store = load_locked(&path);
    let out = f(&mut store);
    save_locked(&path, &store);
    out
}

/// Caller must hold [`store_lock`].
fn load_locked(path: &Path) -> Store {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Store::default(); // not created yet — first run
    };
    match serde_json::from_str(&text) {
        Ok(store) => store,
        Err(e) => {
            // Never silently fall back to defaults over a file that exists:
            // that would discard the refresh token and every game's sync state
            // on the very next write. Move it aside so it stays recoverable.
            eprintln!("[cloud_saves] store is unreadable ({}); preserving it and starting fresh", e);
            let backup = path.with_extension(format!("corrupt-{}.json", now_epoch()));
            if let Err(e) = std::fs::rename(path, &backup) {
                eprintln!("[cloud_saves] could not preserve unreadable store: {}", e);
            }
            Store::default()
        }
    }
}

/// Caller must hold [`store_lock`].
fn save_locked(path: &Path, store: &Store) {
    let json = match serde_json::to_string(store) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[cloud_saves] could not serialize store: {}", e);
            return;
        }
    };
    // Write-then-rename: a crash or a full disk midway through an in-place
    // write leaves a truncated file, which `load_locked` can only treat as
    // unreadable — i.e. cloud saves look signed out through no fault of the
    // user. The rename is atomic, so readers see either the old or new file.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &json) {
        eprintln!("[cloud_saves] could not write store: {}", e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        eprintln!("[cloud_saves] could not replace store: {}", e);
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Serializes syncs for a single game against each other.
///
/// Push and pull both rewrite the same Drive file and the same live save
/// directory, and nothing stopped them running at once: closing a game spawns
/// a push, and navigating to that game's page immediately spawns a pull. Worse,
/// two pushes that both saw "no remote file yet" would each *create* one —
/// Drive permits duplicate names, and `find_file` then picks between them
/// arbitrarily forever after, which is precisely the intermittent
/// "sometimes my save syncs, sometimes it doesn't" symptom.
///
/// Held for the whole sync, so a queued pull runs after the push it raced and
/// then correctly observes matching hashes.
fn game_lock(game: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(locks.entry(game.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))))
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

/// The last sync that actually *moved data* for a game, so the UI can announce
/// it. Only recorded when bytes changed hands (an upload or a download that
/// replaced the live save) — never for the common "hashes already match,
/// nothing to do" outcome, which is what the poll sees the vast majority of
/// the time and which nobody wants a notification about.
#[derive(Debug, Clone)]
struct SyncEvent {
    /// Process-wide monotonic id. The frontend polls `status()` and toasts
    /// whenever this changes, which is race-free without needing push events:
    /// a missed poll tick just means one combined toast instead of two.
    seq: u64,
    /// `"pushed"` (local → Drive) or `"pulled"` (Drive → local).
    kind: &'static str,
    at: u64,
}

fn event_map() -> &'static Mutex<HashMap<String, SyncEvent>> {
    static LAST_EVENT: OnceLock<Mutex<HashMap<String, SyncEvent>>> = OnceLock::new();
    LAST_EVENT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn record_event(game: &str, kind: &'static str) {
    let event = SyncEvent { seq: next_seq(), kind, at: now_epoch() };
    event_map().lock().unwrap().insert(game.to_string(), event);
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
    read_store().games.get(game).map(|g| g.enabled).unwrap_or(false)
}

/// Whether the user has completed the Drive consent flow (i.e. we hold a
/// refresh token). Enabling cloud saves for the first time (for *any* game)
/// triggers this; every game after that reuses the same token.
pub fn has_drive_access() -> bool {
    read_store().refresh_token.is_some()
}

/// Store the refresh token obtained from `auth::google_sign_in_drive`.
pub fn store_refresh_token(refresh_token: &str) {
    with_store(|store| store.refresh_token = Some(refresh_token.to_string()));
}

/// Enable or disable cloud saves for `game`. Disabling only stops future
/// syncs — it does not delete the Drive-side copy, so re-enabling later
/// resumes from where it left off instead of re-uploading from scratch.
pub fn set_enabled(game: &str, enabled: bool) {
    with_store(|store| store.games.entry(game.to_string()).or_default().enabled = enabled);
    if !enabled {
        set_error(game, None);
    }
}

/// Status for the Save Manager UI to poll.
pub fn status(game: &str) -> Value {
    let store = read_store();
    let g = store.games.get(game).cloned().unwrap_or_default();
    let last_event = event_map()
        .lock()
        .unwrap()
        .get(game)
        .map(|e| json!({ "seq": e.seq, "kind": e.kind, "at": e.at }));
    json!({
        "enabled": g.enabled,
        "signedIn": store.refresh_token.is_some(),
        "lastSyncedAt": g.last_synced_at,
        "lastCheckedAt": g.last_checked_at,
        "syncing": is_syncing(game),
        "error": last_error(game),
        // Null until this launcher session has actually transferred a save for
        // this game — see `SyncEvent`.
        "lastEvent": last_event,
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
    let refresh_token = read_store()
        .refresh_token
        .ok_or_else(|| "Cloud saves aren't connected to Google Drive yet".to_string())?;
    auth::refresh_access_token(&refresh_token)
}

fn update_synced_metadata(game: &str, hash: Option<String>, updated_at: u64, drive_file_id: String) {
    with_store(|store| {
        let entry = store.games.entry(game.to_string()).or_default();
        entry.last_synced_hash = hash;
        entry.last_synced_at = updated_at;
        entry.drive_file_id = Some(drive_file_id);
    });
}

/// Record that a sync completed without error, whether or not it moved data.
fn mark_checked(game: &str) {
    let now = now_epoch();
    with_store(|store| store.games.entry(game.to_string()).or_default().last_checked_at = now);
}

/// The hash we last confirmed both sides agreed on — the common ancestor the
/// pull decision is made against.
fn last_synced_hash(game: &str) -> Option<String> {
    read_store().games.get(game).and_then(|g| g.last_synced_hash.clone())
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
    let lock = game_lock(game);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    mark_syncing(game, true);
    let result = push(game);
    mark_syncing(game, false);
    match result {
        Ok(()) => {
            set_error(game, None);
            mark_checked(game);
        }
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

    let last_synced_hash = last_synced_hash(game);

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
    record_event(game, "pushed");
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
    let lock = game_lock(game);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    mark_syncing(game, true);
    let result = pull(game);
    mark_syncing(game, false);
    match result {
        Ok(()) => {
            set_error(game, None);
            mark_checked(game);
        }
        Err(e) => {
            eprintln!("[cloud_saves] sync_on_open({}): {}", game, e);
            set_error(game, Some(e));
        }
    }
}

/// What a pull should do, given the three hashes involved.
#[derive(Debug, PartialEq, Eq)]
enum PullAction {
    /// Both sides hold the same content.
    UpToDate,
    /// Local is unchanged since the last sync and the remote has advanced —
    /// safe to take the remote wholesale.
    FastForward,
    /// The live save holds changes that were never uploaded. Leave it alone.
    KeepLocal,
}

/// Decide a pull using `last_synced` as the common ancestor.
///
/// The point is to distinguish "the remote is newer" from merely "the remote
/// is different" — hashes alone can't order two saves in time, so the only
/// sound signal for *which side moved* is whether each still matches what both
/// sides last agreed on.
fn decide_pull(local: Option<&str>, remote: Option<&str>, last_synced: Option<&str>) -> PullAction {
    if local == remote {
        return PullAction::UpToDate;
    }
    match local {
        // Nothing on disk to lose — the cross-device case a pull exists for.
        None => PullAction::FastForward,
        // Local still matches the last agreed state, so only the remote moved.
        Some(_) if local == last_synced => PullAction::FastForward,
        // Local moved (and possibly the remote too). Either way there is
        // unsynced local work; a pull here would destroy it.
        Some(_) => PullAction::KeepLocal,
    }
}

fn pull(game: &str) -> Result<(), String> {
    let token = access_token()?;
    let name = drive_file_name(game);
    let Some(remote) = drive::find_file(&token, &name)? else {
        return Ok(()); // nothing uploaded yet for this game
    };

    let local_hash = saves::live_save_hash(game);
    let last_synced = last_synced_hash(game);

    match decide_pull(local_hash.as_deref(), remote.hash.as_deref(), last_synced.as_deref()) {
        PullAction::UpToDate => {
            // Already in sync — just make sure our bookkeeping agrees.
            update_synced_metadata(game, remote.hash.clone(), remote.updated_at.unwrap_or_else(now_epoch), remote.id.clone());
            return Ok(());
        }
        PullAction::KeepLocal => {
            // The live save has changes Drive has never seen. Opening a game's
            // page must never cost the user those changes — which is exactly
            // what happened when this only asked "does the remote differ?":
            // any push that didn't run (crash, kill, offline) meant the next
            // page open silently reverted local progress to the older cloud
            // copy. Reconciling is the push path's job, and it preserves the
            // remote copy as a slot before overwriting it.
            return Ok(());
        }
        PullAction::FastForward => {}
    }

    // Local is unchanged since the last sync and the remote has moved on, so
    // taking the remote is a fast-forward, not a merge. Back the live save up
    // anyway — Drive only retains the newest upload, so this is the only copy
    // of the superseded content that survives locally.
    if local_hash.is_some() {
        if let Some((local_bytes, _)) = saves::export_live_save_zip(game) {
            let slot = format!("cloud-backup-{}", now_epoch());
            let _ = saves::import_zip_as_slot(game, &local_bytes, &slot);
        }
    }

    let remote_bytes = drive::download(&token, &remote.id)?;
    if saves::import_save_zip(game, &remote_bytes) {
        update_synced_metadata(game, remote.hash.clone(), remote.updated_at.unwrap_or_else(now_epoch), remote.id.clone());
        record_event(game, "pulled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = "hash-local";
    const REMOTE: &str = "hash-remote";
    const COMMON: &str = "hash-common";

    #[test]
    fn identical_hashes_need_no_transfer() {
        assert_eq!(decide_pull(Some(COMMON), Some(COMMON), Some(COMMON)), PullAction::UpToDate);
        // Agreeing on "neither side has a save" is still agreement.
        assert_eq!(decide_pull(None, None, None), PullAction::UpToDate);
    }

    #[test]
    fn missing_local_save_always_fast_forwards() {
        // The cross-device case: a fresh machine takes the cloud copy.
        assert_eq!(decide_pull(None, Some(REMOTE), None), PullAction::FastForward);
        assert_eq!(decide_pull(None, Some(REMOTE), Some(COMMON)), PullAction::FastForward);
    }

    #[test]
    fn remote_advancing_alone_fast_forwards() {
        // Played on another device; this one hasn't touched its save since.
        assert_eq!(decide_pull(Some(COMMON), Some(REMOTE), Some(COMMON)), PullAction::FastForward);
    }

    #[test]
    fn unsynced_local_changes_are_never_clobbered() {
        // The reported bug: a push that never ran (crash, kill, offline) left
        // the remote behind, and opening the game page reverted local progress.
        assert_eq!(decide_pull(Some(LOCAL), Some(COMMON), Some(COMMON)), PullAction::KeepLocal);
        // Genuine divergence — both sides moved. Push reconciles and preserves
        // the remote as a slot; a pull would throw local work away.
        assert_eq!(decide_pull(Some(LOCAL), Some(REMOTE), Some(COMMON)), PullAction::KeepLocal);
        // No sync has ever been recorded, so nothing establishes the remote as
        // newer. Local work still wins.
        assert_eq!(decide_pull(Some(LOCAL), Some(REMOTE), None), PullAction::KeepLocal);
    }

    #[test]
    fn unknown_remote_hash_does_not_discard_local_work() {
        // A Drive file with no `appProperties.hash` (older upload, or one whose
        // metadata part didn't stick) is unorderable against local.
        assert_eq!(decide_pull(Some(LOCAL), None, Some(LOCAL)), PullAction::FastForward);
        assert_eq!(decide_pull(Some(LOCAL), None, Some(COMMON)), PullAction::KeepLocal);
    }
}
