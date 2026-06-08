use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{bridge::AppState, config, download, games};

// ── Periodic update check ─────────────────────────────────────────────────────

/// How often to re-check for a new launcher release.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Spawns a background thread that checks `GOOPIE_RELEASES_API` for a newer
/// launcher release every hour, caching the result in `AppState` so the
/// (synchronous) `CheckForLauncherUpdate` bridge call never blocks on a
/// GitHub API request (mirrors `offline_site::spawn_connectivity_monitor`).
///
/// The timestamp of the last check is persisted (`config::get/set_last_update_check`)
/// so that the interval is enforced *across restarts* too — opening the
/// launcher repeatedly within an hour reuses the existing cached result
/// instead of firing a fresh request each time.
///
/// This never applies an update on its own — it only refreshes the cache that
/// drives the "update available" icon; the user must explicitly opt in via
/// `SelfUpdateLauncher`.
pub fn spawn_update_monitor(state: Arc<AppState>) {
    // `LastUpdateCheck`/`LastKnownReleaseTag` persist across restarts, but
    // `AppState`'s cache doesn't — without this, a restart inside the throttle
    // window below would leave `update_checked` false (and the "update
    // available" prompt hidden) until the next live check actually runs, up to
    // an hour later. Re-derive `has_update` against *this* binary's version
    // rather than trusting a possibly-stale cached verdict (e.g. right after a
    // self-update, the cached tag may now match `current`).
    let cached_tag = config::get_last_known_release_tag();
    if !cached_tag.is_empty() {
        apply_remote_tag(&state, cached_tag);
    }

    std::thread::spawn(move || loop {
        let elapsed = unix_now().saturating_sub(config::get_last_update_check());
        let interval_secs = UPDATE_CHECK_INTERVAL.as_secs();
        if elapsed < interval_secs {
            std::thread::sleep(Duration::from_secs(interval_secs - elapsed));
        }
        check_for_update(&state);
        std::thread::sleep(UPDATE_CHECK_INTERVAL);
    });
}

/// Compare `remote_tag` against this binary's version and store the verdict in
/// `AppState` (and, for `check_for_update`'s callers, persist the tag so a
/// restart within the throttle window can reuse it — see `spawn_update_monitor`).
fn apply_remote_tag(state: &Arc<AppState>, remote_tag: String) {
    let current = env!("CARGO_PKG_VERSION");
    let remote_clean = remote_tag.trim_start_matches('v');
    let has_update = !remote_clean.is_empty() && remote_clean != current;

    state.update_available.store(has_update, Ordering::Relaxed);
    *state.latest_version.lock().unwrap() = remote_tag;
    state.update_checked.store(true, Ordering::Relaxed);
}

/// Fetch the latest release tag and refresh `AppState`'s update cache. Leaves
/// the previous cached values untouched on a fetch error (transient network
/// hiccups shouldn't flip "update available" back off) — and, importantly,
/// leaves `LastUpdateCheck` untouched too, so a failed check doesn't throttle
/// the *next* attempt for a full interval (see `spawn_update_monitor`).
fn check_for_update(state: &Arc<AppState>) {
    let api_url = env!("GOOPIE_RELEASES_API");
    let body = match download::fetch_to_string(api_url) {
        Ok(b) => b,
        Err(e) => { eprintln!("[launcher] update check failed: {e:?}"); return; }
    };

    // Only stamp the throttle timestamp on a *successful* check — see doc comment.
    config::set_last_update_check(unix_now());

    let remote_tag = games::json_extract_str(&body, "tag_name");
    config::set_last_known_release_tag(&remote_tag);
    apply_remote_tag(state, remote_tag);
}

pub fn self_update(state: Arc<AppState>) {
    let api_url = env!("GOOPIE_RELEASES_API");

    let body = match download::fetch_to_string(api_url) {
        Ok(b) => b,
        Err(e) => { eprintln!("[launcher] fetch releases failed: {e:?}"); return; }
    };

    let url = match find_asset_url(&body) {
        Some(u) => u,
        None => { eprintln!("[launcher] no matching asset found in release"); return; }
    };

    let staging = staging_path();

    let state_ref = Arc::clone(&state);
    let progress_cb: download::ProgressCallback = Box::new(move |dl, total| {
        state_ref.set_download_progress(dl, total);
    });

    if let Err(e) = download::download_file(&url, &staging.to_string_lossy(), Some(&progress_cb)) {
        eprintln!("[launcher] download failed: {e:?}");
        state.finish_download();
        return;
    }
    state.finish_download();

    if let Err(e) = apply_update(&staging) {
        eprintln!("[launcher] apply update failed: {e:?}");
    }
}

// ── Asset URL lookup ──────────────────────────────────────────────────────────

fn find_asset_url(api_body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(api_body).ok()?;
    let assets = json.get("assets")?.as_array()?;

    for asset in assets {
        let name = asset.get("name")?.as_str().unwrap_or("");
        let url  = asset.get("browser_download_url")?.as_str().unwrap_or("");
        if is_target_asset(name) {
            return Some(url.to_string());
        }
    }
    None
}

#[cfg(windows)]
fn is_target_asset(name: &str) -> bool {
    // Our portable exe: "Goopie-Launcher-v*-windows-x86_64.exe"
    // Exclude the NSIS installer which ends with "-setup.exe"
    name.contains("-windows-") && name.ends_with(".exe") && !name.ends_with("-setup.exe")
}

#[cfg(not(windows))]
fn is_target_asset(name: &str) -> bool {
    name.ends_with(".AppImage")
}

// ── Staging path ──────────────────────────────────────────────────────────────

#[cfg(windows)]
fn staging_path() -> std::path::PathBuf {
    std::env::temp_dir().join("goopie-launcher-update.exe")
}

#[cfg(not(windows))]
fn staging_path() -> std::path::PathBuf {
    // Same directory as the current exe so rename stays on the same filesystem.
    let current = std::env::current_exe().unwrap_or_default();
    current.parent().unwrap_or(std::path::Path::new("."))
        .join("goopie-launcher-update.AppImage")
}

// ── Platform-specific apply ───────────────────────────────────────────────────

#[cfg(windows)]
fn apply_update(staging: &std::path::Path) -> std::io::Result<()> {
    let current_exe = std::env::current_exe()?;

    let bat = std::env::temp_dir().join("goopie-update.bat");
    let contents = format!(
        "@echo off\r\n\
         ping -n 3 127.0.0.1 > nul\r\n\
         copy /y \"{src}\" \"{dst}\"\r\n\
         start \"\" \"{dst}\"\r\n\
         del \"%~f0\"\r\n",
        src = staging.display(),
        dst = current_exe.display(),
    );
    std::fs::write(&bat, contents)?;

    std::process::Command::new("cmd")
        .args(["/C", "start", "/B", "/MIN", bat.to_str().unwrap_or("")])
        .spawn()?;

    std::process::exit(0);
}

#[cfg(not(windows))]
fn apply_update(staging: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let current_exe = std::env::current_exe()?;
    let tmp = current_exe.with_extension("new");

    std::fs::copy(staging, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &current_exe)?;
    std::fs::remove_file(staging).ok();

    std::process::Command::new(&current_exe).spawn()?;
    std::process::exit(0);
}
