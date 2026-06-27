//! Synchronous native bridge — custom URI scheme transport.
//!
//! The website at goopie.xyz calls native functions synchronously via `window.Foo()`.
//! Because Tauri's `invoke()` is async and the page is served over HTTPS (which would
//! block synchronous XHR to plain `http://127.0.0.1` as mixed content under WebKitGTK),
//! we instead register a **Tauri custom URI scheme** (`goopiebridge`).
//!
//! wry marks custom schemes as secure contexts, so requests from the HTTPS page are
//! never treated as mixed content on any webview backend.
//!
//! Architecture:
//!   1. Generate a random per-launch secret token.
//!   2. Inject a JS init-script that defines every `window.*` global as a synchronous
//!      XMLHttpRequest to `goopiebridge://localhost/bridge/<fn>?token=<secret>&args=<json>`.
//!   3. `handle_bridge_request` dispatches each function name to Rust and returns JSON.

use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use crate::{config, extract, games, launcher, offline_site, paths, platform, saves, vehicles};

// ── Shared application state ─────────────────────────────────────────────────

/// Result of a native Google OAuth sign-in attempt.
pub enum GoogleSignInResult {
    /// No sign-in has been started this session.
    Idle,
    /// Sign-in is in progress (system browser is open).
    Pending,
    /// Sign-in succeeded; holds the Google access_token.
    Ok(String),
    /// Sign-in failed or was cancelled; holds an error message.
    Err(String),
}

/// A game process currently being monitored by [`monitor_running_game`].
///
/// `session_id` disambiguates between successive launches: when the user closes
/// the running game to start another, the old monitor thread notices its
/// `session_id` no longer matches `AppState::running_game` and exits quietly
/// instead of clobbering the new session's state.
pub struct RunningGame {
    pub session_id: u64,
    pub game: String,
    pub build: String,
    pub child: std::process::Child,
    pub started_at: Instant,
}

pub struct AppState {
    /// Download progress: -1 = idle, 0-100 = percent.
    pub download_progress: AtomicI32,
    /// Human-readable download progress string ("X MB / Y MB").
    pub download_string: Mutex<String>,
    /// Whether an ISO extraction is currently in progress.
    pub is_extracting: AtomicBool,
    /// Cached vehicle list (for Nuts & Bolts save browser).
    pub vehicles: Mutex<Vec<serde_json::Value>>,
    /// State of the native Google OAuth loopback flow.
    pub google_signin: Mutex<GoogleSignInResult>,
    /// The currently-running game process, if any (polled by the frontend to
    /// drive the Play/Close button and the "close running game?" prompt).
    pub running_game: Mutex<Option<RunningGame>>,
    /// Monotonically increasing counter handed out as each game is launched.
    pub next_session_id: AtomicU64,
    /// Handle to the main window, set once during `.setup()`. Lets bridge
    /// commands (e.g. `setOfflineMode`) `.navigate()` it to switch between the
    /// live site and the embedded offline bundle at runtime.
    pub window: Mutex<Option<tauri::WebviewWindow>>,
    /// Cached result of the last `goopie.xyz` connectivity probe, refreshed
    /// periodically by a background thread (see `offline_site::spawn_connectivity_monitor`).
    /// Bridge calls are synchronous, so the actual (multi-second-timeout) probe
    /// can't run on demand without freezing the UI — the website polls this
    /// instead to grey out "switch to online mode" while the site is unreachable.
    pub goopie_reachable: AtomicBool,
    /// Whether the last launcher-update check found a newer release than ours.
    /// Refreshed by `launcher::spawn_update_monitor` (startup, then every 2h).
    /// Bridge calls are synchronous, so `CheckForLauncherUpdate` reads this
    /// cache instead of hitting the GitHub API on demand.
    pub update_available: AtomicBool,
    /// Latest release tag seen by the update monitor (raw, e.g. "v1.2.0").
    pub latest_version: Mutex<String>,
    /// Whether the update monitor has completed at least one check, so the
    /// website can tell "checked, no update" apart from "hasn't checked yet".
    pub update_checked: AtomicBool,
    /// Human-readable error from the most recent `Play` attempt, if it failed
    /// to launch (e.g. executable not found — likely an incompatible-platform
    /// build — or a spawn error). `None` once successfully launched or after
    /// the frontend has consumed/cleared it via `clearLaunchError`. Launching
    /// is fire-and-forget on a background thread, so this is the pollable
    /// channel the frontend uses to surface the failure (see `getLaunchError`).
    pub last_launch_error: Mutex<Option<String>>,
    pub last_extract_error: Mutex<Option<String>>,
}

impl AppState {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            download_progress: AtomicI32::new(-1),
            download_string: Mutex::new(String::new()),
            is_extracting: AtomicBool::new(false),
            vehicles: Mutex::new(Vec::new()),
            google_signin: Mutex::new(GoogleSignInResult::Idle),
            running_game: Mutex::new(None),
            next_session_id: AtomicU64::new(1),
            window: Mutex::new(None),
            goopie_reachable: AtomicBool::new(true),
            update_available: AtomicBool::new(false),
            latest_version: Mutex::new(String::new()),
            update_checked: AtomicBool::new(false),
            last_launch_error: Mutex::new(None),
            last_extract_error: Mutex::new(None),
        }
    }

    /// Set download progress and update the human-readable string.
    pub fn set_download_progress(&self, downloaded: u64, total: u64) {
        let pct = ((downloaded * 100).checked_div(total).unwrap_or(0)) as i32;
        self.download_progress.store(pct, Ordering::Relaxed);
        let mb_down = downloaded / (1024 * 1024);
        let mb_total = total / (1024 * 1024);
        *self.download_string.lock().unwrap() = format!("{} MB / {} MB", mb_down, mb_total);
    }

    pub fn finish_download(&self) {
        self.download_progress.store(-1, Ordering::Relaxed);
        *self.download_string.lock().unwrap() = String::new();
    }
}

// ── Game process monitoring ──────────────────────────────────────────────────

/// Spawn `game`/`build` and start tracking it as the running game, replacing
/// (and killing) any previously-running game first — mirrors the "closing the
/// running game loses unsaved progress" behaviour the frontend warns about.
fn launch_and_track(state: &Arc<AppState>, game: String, build: String, cvar_args: String, custom_exe: String, set_data_root: bool, mount_update: bool) {
    kill_running_game(state);
    *state.last_launch_error.lock().unwrap() = None;

    let child = match games::play(&game, &build, &cvar_args, &custom_exe, set_data_root, mount_update) {
        Ok(child) => child,
        Err(msg) => {
            *state.last_launch_error.lock().unwrap() = Some(msg);
            return;
        }
    };

    let session_id = state.next_session_id.fetch_add(1, Ordering::Relaxed);
    *state.running_game.lock().unwrap() = Some(RunningGame {
        session_id,
        game,
        build,
        child,
        started_at: Instant::now(),
    });

    let state_clone = Arc::clone(state);
    std::thread::spawn(move || monitor_running_game(state_clone, session_id));
}

/// Poll the tracked process until it exits, then clear `running_game` — but
/// only if it's still *our* session (the user may have closed it to launch a
/// different game in the meantime, in which case that swap already cleared/
/// replaced the entry and this thread has nothing left to do).
fn monitor_running_game(state: Arc<AppState>, session_id: u64) {
    loop {
        std::thread::sleep(Duration::from_millis(750));

        let mut lock = state.running_game.lock().unwrap();
        let Some(running) = lock.as_mut() else { return };
        if running.session_id != session_id {
            return;
        }
        match running.child.try_wait() {
            Ok(Some(_status)) => {
                *lock = None;
                drop(lock);
                // The player just closed the game. If a self-update was deferred
                // while it was running (hidden `AutoApplyUpdate` flag on + a newer
                // release already detected), apply it now — see the game-running
                // guard in `launcher::maybe_auto_apply`. No-op otherwise.
                launcher::auto_apply_after_game_exit(&state);
                return;
            }
            Ok(None) => { /* still running */ }
            Err(e) => {
                eprintln!("[bridge] failed to poll running game: {}", e);
                *lock = None;
                return;
            }
        }
    }
}

/// Kill and reap the currently-tracked game process (if any), clearing the
/// shared state. Used both for the explicit "Close" action and when swapping
/// to a different game.
fn kill_running_game(state: &Arc<AppState>) -> bool {
    let mut lock = state.running_game.lock().unwrap();
    let Some(mut running) = lock.take() else { return false };
    if let Err(e) = running.child.kill() {
        eprintln!("[bridge] failed to kill running game: {}", e);
    }
    let _ = running.child.wait();
    true
}

// ── Token + init-script generation ───────────────────────────────────────────

/// Generate a random per-launch secret token.
pub fn make_token() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        .hash(&mut h);
    std::process::id().hash(&mut h);
    format!("{:016x}{:016x}", h.finish(), h.finish().wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

/// Build the initialization script by substituting the bridge base URL and token
/// into the shim template.
///
/// Custom-scheme URL form differs by webview backend:
/// - WebView2 (Windows): `http://goopiebridge.localhost/bridge/` (secure loopback proxy)
/// - WebKitGTK / WKWebView (Linux/macOS): `goopiebridge://localhost/bridge/`
pub fn make_init_script(token: &str) -> String {
    let base = if cfg!(windows) {
        "http://goopiebridge.localhost/bridge/"
    } else {
        "goopiebridge://localhost/bridge/"
    };
    let shim = include_str!("shim.js");
    shim.replace("__BRIDGE_BASE__", base)
        .replace("__BRIDGE_TOKEN__", token)
}

// ── Custom URI scheme request handler ────────────────────────────────────────

/// Handle one bridge request arriving via the `goopiebridge` custom URI scheme.
///
/// Expected request URI:
///   `goopiebridge://localhost/bridge/<Fn>?token=<secret>&args=<json>`  (Linux/macOS)
///   `http://goopiebridge.localhost/bridge/<Fn>?token=<secret>&args=<json>` (Windows)
pub fn handle_bridge_request(
    state: &Arc<AppState>,
    request: tauri::http::Request<Vec<u8>>,
    token: &str,
) -> tauri::http::Response<Vec<u8>> {
    let uri = request.uri();

    // Only handle /bridge/<fn>
    let fn_name = match uri.path().strip_prefix("/bridge/") {
        Some(rest) => percent_decode(rest),
        None => return resp(404, "null"),
    };

    // Validate token
    let params = parse_query(uri.query().unwrap_or(""));
    if params.get("token").map(|s| s.as_str()).unwrap_or("") != token {
        return resp(403, "null");
    }

    let args_str = params.get("args").cloned().unwrap_or_else(|| "[]".into());
    let args: Vec<serde_json::Value> = serde_json::from_str(&args_str).unwrap_or_default();

    let result = dispatch(&fn_name, args, state);
    let body = serde_json::to_string(&result).unwrap_or_else(|_| "null".into());
    resp(200, &body)
}

fn resp(code: u16, body: &str) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(code)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .body(body.as_bytes().to_vec())
        .unwrap()
}

// ── Dispatch table ────────────────────────────────────────────────────────────

fn dispatch(name: &str, args: Vec<serde_json::Value>, state: &Arc<AppState>) -> serde_json::Value {
    use serde_json::{json, Value};

    fn str_arg(args: &[Value], i: usize) -> String {
        args.get(i)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn bool_arg(args: &[Value], i: usize, default: bool) -> bool {
        args.get(i)
            .map(|v| v.as_bool().unwrap_or(
                v.as_i64().map(|n| n != 0).unwrap_or(
                    v.as_str().map(|s| matches!(s, "1" | "true" | "yes")).unwrap_or(default)
                )
            ))
            .unwrap_or(default)
    }

    match name {
        // ── Platform ──────────────────────────────────────────────────────────
        "GetPlatform"  => json!(platform::get_platform()),
        "GetArch"      => json!(platform::get_arch()),
        "getVersion"   => json!(env!("CARGO_PKG_VERSION")),

        "CheckForLauncherUpdate" => {
            // Instant read of the cache kept fresh by `launcher::spawn_update_monitor`
            // (checked at startup, then every 2h) — never blocks the bridge thread
            // on a GitHub API call (see commit 3b39268 for why that matters).
            json!({
                "hasUpdate": state.update_available.load(Ordering::Relaxed),
                "latestVersion": state.latest_version.lock().unwrap().clone(),
                "current": env!("CARGO_PKG_VERSION"),
                "checked": state.update_checked.load(Ordering::Relaxed),
            })
        }

        "SelfUpdateLauncher" => {
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || launcher::self_update(state_clone));
            Value::Null
        }

        // ── Config / paths ────────────────────────────────────────────────────
        "GetGamesPath" => json!(config::get_games_folder()),
        "SetGamesPath" => {
            // Opens a native folder dialog synchronously (blocking call is fine in the
            // scheme handler thread).
            if let Some(path) = platform::pick_folder("Select Games Folder") {
                config::set_games_path(&path);
            }
            Value::Null
        }
        "GetLanguage"  => json!(config::get_language()),
        "SetLanguage"  => {
            let lang = args.first().and_then(|v| v.as_i64()).unwrap_or(1) as i32;
            config::set_language(lang);
            json!(true)
        }

        // ── Proton (Linux) ────────────────────────────────────────────────────
        // Available on all platforms for bridge-compilation purposes, but
        // `getProtonInstallations` always returns [] on non-Linux hosts.
        "getProtonInstallations" => json!(crate::proton::list_installations()),
        "getUseProton"           => json!(config::get_use_proton()),
        "setUseProton"           => {
            let enabled = args.first()
                .map(|v| match v {
                    serde_json::Value::Bool(b) => *b,
                    serde_json::Value::Number(n) => n.as_i64().unwrap_or(1) != 0,
                    serde_json::Value::String(s) => s != "0" && s != "false",
                    _ => true,
                })
                .unwrap_or(true);
            config::set_use_proton(enabled);
            json!(true)
        }
        "getSelectedProton"      => json!(config::get_selected_proton()),
        "setSelectedProton"      => {
            config::set_selected_proton(&str_arg(&args, 0));
            json!(true)
        }

        // ── Game state ────────────────────────────────────────────────────────
        // `build` identifies which installed build (release tag directory) an
        // op applies to — see games::get_installed_builds for how the website
        // discovers the available build keys for a game.
        "isIsoInstalled"      => json!(games::is_iso_installed(&str_arg(&args, 0))),
        "isExeUpdated"        => json!(games::is_exe_updated(&str_arg(&args, 0), &str_arg(&args, 1))),
        "getInstalledVersion" => json!(games::get_installed_version(&str_arg(&args, 0), &str_arg(&args, 1))),
        "getInstalledBuilds"  => json!(games::get_installed_builds(&str_arg(&args, 0))),

        // ── Long-running ops ──────────────────────────────────────────────────
        "Install" => {
            let game_name = str_arg(&args, 0);
            let is_xbla = args.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || extract::install_game(&game_name, !is_xbla, state_clone));
            Value::Null
        }
        "Uninstall" => {
            games::uninstall(&str_arg(&args, 0), &str_arg(&args, 1));
            Value::Null
        }
        "UninstallAll" => {
            games::uninstall_all(&str_arg(&args, 0));
            Value::Null
        }
        "RemoveAssets" => {
            games::remove_assets(&str_arg(&args, 0));
            Value::Null
        }

        // ── Update & DLC management ──────────────────────────────────────────
        "InstallAssetFile" => {
            let game = str_arg(&args, 0);
            let path = str_arg(&args, 1);
            let checksum = str_arg(&args, 2);
            let dlc_names: Vec<String> = args.get(3)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let allow_update = bool_arg(&args, 4, true);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                extract::install_asset_file(&game, &path, &checksum, &dlc_names, allow_update, state_clone);
            });
            Value::Null
        }
        "InstallAssetPick" => {
            let game = str_arg(&args, 0);
            let checksum = str_arg(&args, 1);
            let dlc_names: Vec<String> = args.get(2)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let is_xbla = args.get(3).and_then(|v| v.as_bool()).unwrap_or(false);
            let allow_update = bool_arg(&args, 4, true);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                extract::install_asset_pick(&game, &checksum, &dlc_names, !is_xbla, allow_update, state_clone);
            });
            Value::Null
        }
        "InstallAssetFiles" => {
            let game = str_arg(&args, 0);
            let paths: Vec<String> = args.get(1)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let checksum = str_arg(&args, 2);
            let dlc_names: Vec<String> = args.get(3)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let allow_update = bool_arg(&args, 4, true);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                extract::install_asset_files(&game, &paths, &checksum, &dlc_names, allow_update, state_clone);
            });
            Value::Null
        }
        "isUpdateInstalled" => {
            json!(games::is_update_installed(&str_arg(&args, 0)))
        }
        "RemoveUpdate" => {
            games::remove_update(&str_arg(&args, 0));
            Value::Null
        }
        "openUpdateFolder" => {
            games::open_update_folder(&str_arg(&args, 0));
            Value::Null
        }
        "openBuildLogsFolder" => {
            games::open_build_logs_folder(&str_arg(&args, 0), &str_arg(&args, 1));
            Value::Null
        }
        "getInstalledDlc" => {
            json!(extract::dlc::list_installed_dlc(&str_arg(&args, 0)))
        }
        "RemoveDlc" => {
            extract::dlc::remove_dlc(&str_arg(&args, 0), &str_arg(&args, 1), &str_arg(&args, 2));
            Value::Null
        }
        "openDlcFolder" => {
            extract::dlc::open_dlc_folder(&str_arg(&args, 0), &str_arg(&args, 1), &str_arg(&args, 2));
            Value::Null
        }
        "Update" => {
            let game_name  = str_arg(&args, 0);
            let release_url = str_arg(&args, 1);
            let asset_name  = if args.get(2).and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                None
            } else {
                Some(str_arg(&args, 2))
            };
            let version_tag = if args.get(3).and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                None
            } else {
                Some(str_arg(&args, 3))
            };
            let packages_json = args.get(4).cloned();
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                games::update(&game_name, &release_url, asset_name.as_deref(),
                              version_tag.as_deref(), packages_json, state_clone);
            });
            Value::Null
        }
        "NeedsUpdate" => {
            let game = str_arg(&args, 0);
            let build = str_arg(&args, 1);
            let api_url = str_arg(&args, 2);
            let asset = if args.get(3).and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                None
            } else {
                Some(str_arg(&args, 3))
            };
            json!(games::needs_update(&game, &build, &api_url, asset.as_deref()))
        }
        "Play" => {
            let game     = str_arg(&args, 0);
            let build    = str_arg(&args, 1);
            let cvar_args = str_arg(&args, 2);
            let custom_exe = str_arg(&args, 3);
            let set_data_root = bool_arg(&args, 4, false);
            let mount_update = bool_arg(&args, 5, true);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                launch_and_track(&state_clone, game, build, cvar_args, custom_exe, set_data_root, mount_update);
            });
            Value::Null
        }
        // ── Running-game tracking ─────────────────────────────────────────────
        // The frontend polls these to swap the Play button to "Close" and to
        // decide whether to prompt before launching a different game.
        "isGameRunning" => {
            let lock = state.running_game.lock().unwrap();
            json!(lock.is_some())
        }
        "getRunningGame" => {
            let lock = state.running_game.lock().unwrap();
            match lock.as_ref() {
                Some(running) => json!({
                    "game": running.game,
                    "build": running.build,
                    "secondsPlayed": running.started_at.elapsed().as_secs(),
                }),
                None => Value::Null,
            }
        }
        // Kills the running game immediately ("all unsaved progress will be lost",
        // per the confirmation prompt the frontend shows before calling this).
        "closeGame" => {
            json!(kill_running_game(state))
        }
        // ── Launch error reporting ────────────────────────────────────────────
        // `Play` is fire-and-forget on a background thread, so a launch failure
        // (e.g. the installed build's executable is missing — likely the wrong
        // platform — or the process failed to spawn) can't be returned
        // synchronously. The frontend polls `getLaunchError` after calling Play
        // and should call `clearLaunchError` once it has shown/dismissed it (or
        // before retrying) so a stale error isn't re-displayed.
        "getLaunchError" => {
            // Consume on read: clear the stored error once handed to the frontend
            // so the website's 2s poll doesn't keep re-surfacing a dismissed error.
            match state.last_launch_error.lock().unwrap().take() {
                Some(msg) => json!(msg),
                None => Value::Null,
            }
        }
        "clearLaunchError" => {
            *state.last_launch_error.lock().unwrap() = None;
            Value::Null
        }
        "InstallPackage" => {
            let game       = str_arg(&args, 0);
            let build      = str_arg(&args, 1);
            let prefix     = str_arg(&args, 2);
            let zip_asset  = str_arg(&args, 3);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                games::install_package(&game, &build, &prefix, &zip_asset, state_clone);
            });
            Value::Null
        }
        "IsPackageInstalled" => {
            let game  = str_arg(&args, 0);
            let build = str_arg(&args, 1);
            let zip   = str_arg(&args, 2);
            json!(games::is_package_installed(&game, &build, &zip))
        }

        // ── Progress polling ──────────────────────────────────────────────────
        "isExtracting"        => json!(state.is_extracting.load(Ordering::Relaxed)),
        "isUpdating"          => json!(state.download_progress.load(Ordering::Relaxed) != -1),
        "getDownloadProgress" => json!(state.download_progress.load(Ordering::Relaxed)),
        "getDownloadString"   => json!(state.download_string.lock().unwrap().clone()),
        // Extract progress is not tracked per-file (xdvdfs doesn't report it);
        // return a placeholder that keeps the UI from hanging.
        "getExtractProgress"  => json!(50_i32),
        "getExtractError" => {
            match state.last_extract_error.lock().unwrap().take() {
                Some(msg) => json!(msg),
                None => Value::Null,
            }
        }
        "clearExtractError" => {
            *state.last_extract_error.lock().unwrap() = None;
            Value::Null
        }

        // ── Folder operations ─────────────────────────────────────────────────
        "OpenGamesFolder" => {
            platform::open_folder(&config::get_games_folder());
            json!(true)
        }
        "openSaveFolder" => {
            let game = str_arg(&args, 0);
            saves::open_save_folder(&game);
            json!(true)
        }
        "OpenExternalLink" => {
            platform::open_url(&str_arg(&args, 0));
            Value::Null
        }

        // ── Save management ───────────────────────────────────────────────────
        "getSaveSlots"      => json!(saves::get_save_slots(&str_arg(&args, 0))),
        "getSaveSlotCount"  => json!(saves::get_save_slot_count(&str_arg(&args, 0))),
        "getActiveSave"     => json!(saves::get_active_save(&str_arg(&args, 0))),
        "backupSave"        => json!(saves::backup_save(&str_arg(&args, 0), &str_arg(&args, 1))),
        "restoreSave"       => json!(saves::restore_save(&str_arg(&args, 0), &str_arg(&args, 1))),
        "deleteSave"        => json!(saves::delete_save(&str_arg(&args, 0), &str_arg(&args, 1))),
        "renameSave"        => json!(saves::rename_save(&str_arg(&args, 0), &str_arg(&args, 1), &str_arg(&args, 2))),
        "deleteCurrentSave" => json!(saves::delete_current_save(&str_arg(&args, 0))),

        // ── Vehicle browser ───────────────────────────────────────────────────
        "getVehicleCount" => {
            json!(state.vehicles.lock().unwrap().len() as i32)
        }
        "getVehicle" => {
            let idx = args.first().and_then(|v| v.as_i64()).unwrap_or(0) as usize;
            let lock = state.vehicles.lock().unwrap();
            match lock.get(idx) {
                Some(v) => json!(v.to_string()),
                None => Value::Null,
            }
        }
        "reloadVehicles" => {
            let list = vehicles::reload_vehicles();
            *state.vehicles.lock().unwrap() = list;
            json!(true)
        }

        // ── Offline mode ──────────────────────────────────────────────────────
        // `isOfflineMode` reflects the *effective* mode for this launch: the
        // user's explicit choice if they've enabled offline mode (honored
        // unconditionally), otherwise the *cached* connectivity flag (refreshed
        // every 20s by spawn_connectivity_monitor, see AppState::goopie_reachable)
        // — they prefer online, so unreachability is a transient fallback, never
        // persisted. We deliberately do NOT call `probe_connectivity()` here: this
        // bridge call is synchronous (blocking XHR, see bridge/shim.js) and is
        // invoked from React render paths (e.g. Sidebar, on a 1.5s timer and on
        // every navigation) — a multi-second live probe on every call would freeze
        // the page's main thread during scroll/navigation. The one-shot startup
        // mode resolution in `lib.rs::resolve_url` is the only place that still
        // wants (and can afford) a live probe, and calls `probe_connectivity()`
        // directly.
        // `setOfflineMode` persists the user's explicit choice (survives
        // restarts, see config::set_offline_mode_preference) and immediately
        // navigates the window so the toggle takes effect without relaunching.
        "isOfflineMode" => json!(
            config::get_offline_mode_preference()
                || !state.goopie_reachable.load(Ordering::Relaxed)
        ),
        // Cached, instantly-readable connectivity status (see AppState::goopie_reachable) —
        // lets the website grey out "switch to online mode" while goopie.xyz is unreachable,
        // without blocking the UI thread on a multi-second probe for every check.
        "isGoopieReachable" => json!(state.goopie_reachable.load(Ordering::Relaxed)),
        "setOfflineMode" => {
            let offline = args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            config::set_offline_mode_preference(offline);
            if let Some(window) = state.window.lock().unwrap().as_ref() {
                let url = if offline {
                    offline_site::offline_site_url().to_string()
                } else {
                    // Mirror `resolve_url()`'s dev-override priority — otherwise
                    // toggling to "online" while running with `--local`/`--url`/
                    // `GOOPIE_URL` would ignore the override and jump to the
                    // production site instead of the dev URL.
                    crate::url_override().unwrap_or_else(|| crate::REMOTE_URL.to_string())
                };
                if let Ok(parsed) = url.parse() {
                    let _ = window.navigate(parsed);
                }
            }
            json!(true)
        }

        // ── Game-data disk cache (for offline use) ───────────────────────────
        // Shape: `{ lastUpdated: <ISO-8601 string>, games: Game[] }`. Written by
        // the website on every successful Firestore fetch, read back as a
        // fallback when offline (Firestore is unreachable from the embedded site).
        "getCachedGamesData" => match std::fs::read_to_string(paths::games_cache_file()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
            Err(_) => Value::Null,
        },
        "setCachedGamesData" => {
            let data = args.into_iter().next().unwrap_or(Value::Null);
            if let Ok(s) = serde_json::to_string(&data) {
                let _ = std::fs::write(paths::games_cache_file(), s);
            }
            json!(true)
        }

        // ── Misc ──────────────────────────────────────────────────────────────
        "testFunction" => json!("yes"),

        // ── Google OAuth (system-browser loopback + PKCE) ─────────────────────
        // Fire-and-forget: resets state to Pending and spawns the OAuth thread.
        // The website polls `getGoogleSignInResult` until status is "ok"/"error".
        "GoogleSignIn" => {
            *state.google_signin.lock().unwrap() = GoogleSignInResult::Pending;
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                let result = crate::auth::google_sign_in();
                let mut lock = state_clone.google_signin.lock().unwrap();
                *lock = match result {
                    Ok(token) => GoogleSignInResult::Ok(token),
                    Err(msg) => GoogleSignInResult::Err(msg),
                };
            });
            Value::Null
        }
        // Poll endpoint: returns { status, accessToken?, message? }.
        "getGoogleSignInResult" => {
            let lock = state.google_signin.lock().unwrap();
            match &*lock {
                GoogleSignInResult::Idle    => json!({ "status": "idle" }),
                GoogleSignInResult::Pending => json!({ "status": "pending" }),
                GoogleSignInResult::Ok(t)   => json!({ "status": "ok", "accessToken": t }),
                GoogleSignInResult::Err(m)  => json!({ "status": "error", "message": m }),
            }
        }

        _ => {
            eprintln!("[GoopieLauncher] unknown bridge function: {}", name);
            Value::Null
        }
    }
}

// ── URL utilities ─────────────────────────────────────────────────────────────

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.split('&') {
        if let Some(pos) = pair.find('=') {
            let key = percent_decode(&pair[..pos]);
            let val = percent_decode(&pair[pos + 1..]);
            map.insert(key, val);
        }
    }
    map
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4 | lo) as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    out
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
