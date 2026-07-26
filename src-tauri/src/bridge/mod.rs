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

use crate::{achievements, cloud_saves, config, discord, extract, games, launcher, leaderboards, offline_site, paths, platform, playtime, saves, shortcuts, vehicles};

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

/// Result of the separate, narrower-scoped Drive-consent OAuth attempt run
/// the first time a user enables cloud saves for any game (see
/// `auth::google_sign_in_drive`). Unlike `GoogleSignInResult`, no token is
/// exposed to the frontend: the spawned thread stores the refresh token
/// directly via `cloud_saves::store_refresh_token` before setting this to `Ok`.
pub enum DriveSignInResult {
    /// No Drive consent flow has been started this session.
    Idle,
    /// Consent is in progress (system browser is open).
    Pending,
    /// Consent succeeded; the refresh token has already been persisted.
    Ok,
    /// Consent failed, was cancelled, or Google didn't grant offline access.
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

/// Path to a per-game marker file that exists for exactly as long as `game`
/// has a tracked running process. Cross-process (unlike `AppState`), so a
/// separate short-lived process — the one Steam launches for a `--play`
/// shortcut while the launcher is already running, see `steam_play_relay` in
/// `lib.rs` — can poll it to know when to exit, instead of exiting instantly
/// like tauri-plugin-single-instance's own detection does (which would make
/// Steam think the game already stopped seconds into every session, since it
/// tracks "running" by whether the process it spawned is still alive).
pub fn running_marker_path(game: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("goopie-playing-{}.lock", game))
}

fn mark_game_running(game: &str) {
    let _ = std::fs::write(running_marker_path(game), b"");
}

fn unmark_game_running(game: &str) {
    let _ = std::fs::remove_file(running_marker_path(game));
}

pub struct AppState {
    /// Game download progress: -1 = idle, 0-100 = percent.
    pub download_progress: AtomicI32,
    /// Human-readable download progress string ("X MB / Y MB").
    pub download_string: Mutex<String>,
    /// Launcher self-update download progress: -1 = idle, 0-100 = percent.
    /// Kept separate from `download_progress` — they used to share one counter,
    /// which made the game page's own "updating" progress bar light up (in
    /// sync with the download) whenever the user updated the launcher itself.
    pub launcher_update_progress: AtomicI32,
    /// Human-readable launcher self-update progress string ("X MB / Y MB").
    pub launcher_update_string: Mutex<String>,
    /// Whether an ISO extraction is currently in progress.
    pub is_extracting: AtomicBool,
    /// Cached vehicle list (for Nuts & Bolts save browser).
    pub vehicles: Mutex<Vec<serde_json::Value>>,
    /// State of the native Google OAuth loopback flow.
    pub google_signin: Mutex<GoogleSignInResult>,
    /// State of the separate Drive-consent OAuth loopback flow, run the first
    /// time the user enables cloud saves for any game (see `cloud_saves.rs`).
    pub cloud_drive_signin: Mutex<DriveSignInResult>,
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
    /// Whether an on-demand launcher-update check (triggered by the
    /// `RecheckLauncherUpdate` bridge command) is currently in flight. Lets the
    /// website show a spinner and wait for the fresh verdict, since the check
    /// itself blocks on a GitHub API request and so runs on a background thread.
    pub update_checking: AtomicBool,
    /// Human-readable error from the most recent `Play` attempt, if it failed
    /// to launch (e.g. executable not found — likely an incompatible-platform
    /// build — or a spawn error). `None` once successfully launched or after
    /// the frontend has consumed/cleared it via `clearLaunchError`. Launching
    /// is fire-and-forget on a background thread, so this is the pollable
    /// channel the frontend uses to surface the failure (see `getLaunchError`).
    pub last_launch_error: Mutex<Option<String>>,
    pub last_extract_error: Mutex<Option<String>>,
    /// Result of the most recent `ProcessDrops` batch (global drag-and-drop),
    /// polled by the frontend via `getDropReport` and cleared on read.
    pub drop_report: Mutex<Option<extract::drop::DropReport>>,
    /// Human-readable "Processing N of M" status for the current drop batch.
    pub drop_status: Mutex<String>,
    /// Game to auto-play on startup (set by `--play <recompName>` CLI arg).
    /// The website polls this once on mount, triggers Play, then clears it.
    pub auto_play_game: Mutex<Option<String>>,
    /// When true, the launcher was invoked via a shortcut (`--play`) and should
    /// exit once the launched game closes.
    pub exit_after_game: AtomicBool,
    /// Discord Rich Presence connection + desired/last-applied state. Updated
    /// from `launch_and_track`/`monitor_running_game`/`kill_running_game`
    /// (browsing vs. playing) and from the `setDiscordPresenceEnabled` bridge
    /// command (the Settings toggle). See `discord.rs`.
    pub discord: Mutex<discord::DiscordManager>,
    /// Whether a mod-archive install (drop or Browse) is running in the
    /// background — see `mods::install_archives_async`. Kept separate from
    /// `is_extracting` so a mixed drop (mod zips + a base/update/DLC file in
    /// the same drop) can't race: each activity toggles its own flag, and the
    /// frontend waits for both to clear before reading either report.
    pub mod_installing: AtomicBool,
    /// Result of the most recent mod-archive install, polled by the frontend
    /// via `getModInstallReport` and cleared on read.
    pub mod_install_report: Mutex<Option<crate::mods::InstallReport>>,
}

impl AppState {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            download_progress: AtomicI32::new(-1),
            download_string: Mutex::new(String::new()),
            launcher_update_progress: AtomicI32::new(-1),
            launcher_update_string: Mutex::new(String::new()),
            is_extracting: AtomicBool::new(false),
            vehicles: Mutex::new(Vec::new()),
            google_signin: Mutex::new(GoogleSignInResult::Idle),
            cloud_drive_signin: Mutex::new(DriveSignInResult::Idle),
            running_game: Mutex::new(None),
            next_session_id: AtomicU64::new(1),
            window: Mutex::new(None),
            goopie_reachable: AtomicBool::new(true),
            update_available: AtomicBool::new(false),
            latest_version: Mutex::new(String::new()),
            update_checked: AtomicBool::new(false),
            update_checking: AtomicBool::new(false),
            last_launch_error: Mutex::new(None),
            last_extract_error: Mutex::new(None),
            drop_report: Mutex::new(None),
            drop_status: Mutex::new(String::new()),
            auto_play_game: Mutex::new(None),
            exit_after_game: AtomicBool::new(false),
            discord: Mutex::new(discord::DiscordManager::new(config::get_discord_presence_enabled())),
            mod_installing: AtomicBool::new(false),
            mod_install_report: Mutex::new(None),
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

    /// Set launcher self-update progress and update its human-readable string.
    /// See `launcher_update_progress` for why this is separate from `set_download_progress`.
    pub fn set_launcher_update_progress(&self, downloaded: u64, total: u64) {
        let pct = ((downloaded * 100).checked_div(total).unwrap_or(0)) as i32;
        self.launcher_update_progress.store(pct, Ordering::Relaxed);
        let mb_down = downloaded / (1024 * 1024);
        let mb_total = total / (1024 * 1024);
        *self.launcher_update_string.lock().unwrap() = format!("{} MB / {} MB", mb_down, mb_total);
    }

    pub fn finish_launcher_update(&self) {
        self.launcher_update_progress.store(-1, Ordering::Relaxed);
        *self.launcher_update_string.lock().unwrap() = String::new();
    }
}

// ── Game process monitoring ──────────────────────────────────────────────────

/// Spawn `game`/`build` and start tracking it as the running game, replacing
/// (and killing) any previously-running game first — mirrors the "closing the
/// running game loses unsaved progress" behaviour the frontend warns about.
fn launch_and_track(state: &Arc<AppState>, game: String, build: String, cvar_args: String, custom_exe: String, set_data_root: bool, mount_update: bool, cvar_types: String, title_id: String) {
    kill_running_game(state);
    *state.last_launch_error.lock().unwrap() = None;

    // A broken enabled-mods layout (missing/misordered `requires`, an active
    // `conflicts` pair, a code mod with no binary for this OS, ...) must never
    // reach the SDK — it would hard-fail Setup() with a much less actionable
    // message. Reuse the existing pollable launch-error channel so the
    // frontend's Play flow needs no changes to surface this.
    let installed_sidecar = games::get_installed_version(&game, &build);
    let installed_version = games::json_extract_str(&installed_sidecar, "version");
    let validation = crate::mods::validate(&game, &installed_version);
    if !validation.ok {
        let reasons: Vec<&str> = validation.issues.iter()
            .filter(|i| i.kind == "error")
            .map(|i| i.message.as_str())
            .collect();
        *state.last_launch_error.lock().unwrap() = Some(format!(
            "Can't launch: {}. Open Manage → Mods to fix.",
            reasons.join(" ")
        ));
        return;
    }

    // Combine any extra leaderboard store files (e.g. one kept from an older
    // title id) into the one the game actually writes to, so it sees the
    // full leaderboard for this session. Undone in `restore_after_exit` once
    // the game closes.
    let title_id = if title_id.is_empty() { None } else { Some(title_id.as_str()) };
    leaderboards::merge_all_for_launch(&game, title_id);

    let child = match games::play(&game, &build, &cvar_args, &custom_exe, set_data_root, mount_update, &cvar_types) {
        Ok(child) => child,
        Err(msg) => {
            // Launch never actually started — undo the merge immediately
            // instead of leaving it stuck until some future successful launch.
            leaderboards::restore_after_exit(&game);
            *state.last_launch_error.lock().unwrap() = Some(msg);
            return;
        }
    };

    let session_id = state.next_session_id.fetch_add(1, Ordering::Relaxed);
    if discord::presence_enabled_for_game(&game) {
        discord::set_playing(state, &game, discord::now_epoch_secs());
    } else {
        discord::set_hidden(state);
    }
    mark_game_running(&game);
    *state.running_game.lock().unwrap() = Some(RunningGame {
        session_id,
        game,
        build,
        child,
        started_at: Instant::now(),
    });

    // Collapse to the tray while the game runs — only when both settings are
    // on (the second is meaningless, and hidden in the UI, without the
    // first). Hiding (not minimizing) is what actually stops WebView2 from
    // rendering in the background.
    if config::get_collapse_to_tray() && config::get_collapse_after_play() {
        if let Some(window) = state.window.lock().unwrap().as_ref() {
            let _ = window.hide();
        }
        discord::set_window_visible(state, false);
    }

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
                playtime::record_session(&running.game, running.started_at.elapsed().as_secs());
                let game = running.game.clone();
                unmark_game_running(&game);
                *lock = None;
                drop(lock);
                leaderboards::restore_after_exit(&game);
                // Push the save to Drive if cloud saves are enabled for this
                // game and it changed — no-ops instantly otherwise (see
                // `cloud_saves::sync_after_game_exit`). Spawned so a slow/failed
                // upload never blocks the exit-handling thread.
                std::thread::spawn(move || cloud_saves::sync_after_game_exit(&game));
                discord::set_browsing(&state);
                launcher::auto_apply_after_game_exit(&state);
                if state.exit_after_game.load(Ordering::Relaxed) {
                    if let Some(window) = state.window.lock().unwrap().as_ref() {
                        let _ = window.close();
                    }
                } else if config::get_collapse_to_tray() && config::get_collapse_after_play() {
                    // Bring the launcher back once the game the user launched
                    // it for has exited — it was only hidden for the
                    // duration of that session, not indefinitely.
                    if let Some(window) = state.window.lock().unwrap().as_ref() {
                        crate::show_main_window(window);
                    }
                    discord::set_window_visible(&state, true);
                }
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

/// Kills the currently running game only if it's still `game` — used from
/// `lib.rs`'s `wait_for_pid_exit` watcher when the relay process for a Steam
/// (or other already-running-instance) shortcut launch disappears before the
/// game itself exited (e.g. the user hit Steam's own "Close" button, which
/// only terminates the process *Steam* launched). The name check avoids
/// closing a *different* game the user has since started directly, in the
/// unlikely case that race happens.
pub fn kill_running_game_if_matches(state: &Arc<AppState>, game: &str) -> bool {
    {
        let lock = state.running_game.lock().unwrap();
        match lock.as_ref() {
            Some(running) if running.game == game => {}
            _ => return false,
        }
    }
    kill_running_game(state)
}

/// Kill and reap the currently-tracked game process (if any), clearing the
/// shared state. Used both for the explicit "Close" action and when swapping
/// to a different game.
fn kill_running_game(state: &Arc<AppState>) -> bool {
    let mut lock = state.running_game.lock().unwrap();
    let Some(mut running) = lock.take() else { return false };
    playtime::record_session(&running.game, running.started_at.elapsed().as_secs());
    unmark_game_running(&running.game);
    leaderboards::restore_after_exit(&running.game);
    // Same Drive push as the natural-exit path in `monitor_running_game` —
    // this fn also runs for an explicit "Close" and for swapping to a
    // different game, both of which end the session just as much as the
    // game quitting on its own. Spawned so it never delays reaping the
    // process or (on a swap) launching the next game.
    let game = running.game.clone();
    std::thread::spawn(move || cloud_saves::sync_after_game_exit(&game));
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

    /// Deserialize a structured argument that may arrive either as its native
    /// JSON shape (array/object — the expected case) or, if a caller
    /// mistakenly pre-`JSON.stringify()`s it before the shim's own
    /// `JSON.stringify(args)` wraps the whole call, double-encoded as a JSON
    /// string containing JSON. Tolerating both avoids silently discarding the
    /// argument (`serde_json::from_value` on a `Value::String` where a
    /// sequence/object is expected just fails, and `.ok()` swallows it).
    fn json_arg<T: serde::de::DeserializeOwned + Default>(args: &[Value], i: usize) -> T {
        match args.get(i) {
            Some(Value::String(s)) => serde_json::from_str(s).ok(),
            Some(other) => serde_json::from_value(other.clone()).ok(),
            None => None,
        }
        .unwrap_or_default()
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

        "RecheckLauncherUpdate" => {
            // Fire-and-forget: the check blocks on a GitHub API request, and
            // this bridge call is a synchronous XHR, so running it inline would
            // freeze the webview. The frontend polls `isCheckingLauncherUpdate`
            // until it clears, then reads the refreshed `CheckForLauncherUpdate`
            // cache for the verdict (see `launcher::recheck_now`).
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || launcher::recheck_now(state_clone));
            Value::Null
        }
        "isCheckingLauncherUpdate" => json!(state.update_checking.load(Ordering::Relaxed)),

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
            let expected_xex_sha = str_arg(&args, 2);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || extract::install_game(&game_name, !is_xbla, &expected_xex_sha, state_clone));
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
            let expected_xex_sha = str_arg(&args, 5);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                extract::install_asset_file(&game, &path, &checksum, &dlc_names, allow_update, &expected_xex_sha, state_clone);
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
            let expected_xex_sha = str_arg(&args, 5);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                extract::install_asset_pick(&game, &checksum, &dlc_names, !is_xbla, allow_update, &expected_xex_sha, state_clone);
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
            let expected_xex_sha = str_arg(&args, 5);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                extract::install_asset_files(&game, &paths, &checksum, &dlc_names, allow_update, &expected_xex_sha, state_clone);
            });
            Value::Null
        }
        // ── Global drag-and-drop (catalogue-wide matching) ───────────────────
        "ProcessDrops" => {
            let paths: Vec<String> = args.get(0)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let focused_raw = str_arg(&args, 1);
            let focused = if focused_raw.is_empty() { None } else { Some(focused_raw) };
            let catalogue: Vec<extract::drop::CatalogueEntry> = args.get(2)
                .and_then(|v| match v {
                    Value::String(s) => serde_json::from_str(s).ok(),
                    other => serde_json::from_value(other.clone()).ok(),
                })
                .unwrap_or_default();
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                extract::drop::process_drops(&paths, focused, catalogue, state_clone);
            });
            Value::Null
        }
        "getDropReport" => {
            match state.drop_report.lock().unwrap().take() {
                Some(report) => json!(report),
                None => Value::Null,
            }
        }
        "getDropStatus" => json!(state.drop_status.lock().unwrap().clone()),

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
        "openGameConfigFile" => {
            games::open_config_file(&str_arg(&args, 0));
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

        // ── Mods ──────────────────────────────────────────────────────────────
        "getMods" => {
            json!(crate::mods::list_mods(&str_arg(&args, 0)))
        }
        "setModsState" => {
            let game = str_arg(&args, 0);
            let entries: Vec<crate::mods::SidecarEntry> = json_arg(&args, 1);
            crate::mods::set_state(&game, entries);
            Value::Null
        }
        "installModArchives" => {
            // Fire-and-forget: extraction can take several seconds for large
            // mod zips, and this bridge call is a *synchronous* XHR — running
            // it inline would freeze the whole webview. The frontend polls
            // `isInstallingMods`/`getModInstallReport` instead (see
            // `mods::install_archives_async`).
            let game = str_arg(&args, 0);
            let paths: Vec<String> = json_arg(&args, 1);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || crate::mods::install_archives_async(state_clone, game, paths));
            Value::Null
        }
        "pickModArchives" => {
            // The file dialog itself is a blocking modal the user is actively
            // interacting with, so picking synchronously is fine — only the
            // extraction afterward is offloaded. Returns `true` if an install
            // was started (frontend should poll), `false` if the user picked
            // nothing (dialog cancelled).
            let game = str_arg(&args, 0);
            let paths = crate::platform::pick_zip_files();
            let started = !paths.is_empty();
            if started {
                let state_clone = Arc::clone(state);
                std::thread::spawn(move || crate::mods::install_archives_async(state_clone, game, paths));
            }
            json!(started)
        }
        "isInstallingMods" => json!(state.mod_installing.load(Ordering::Relaxed)),
        "getModInstallReport" => {
            match state.mod_install_report.lock().unwrap().take() {
                Some(r) => json!(r),
                None => Value::Null,
            }
        }
        "removeMod" => {
            crate::mods::remove_mod(&str_arg(&args, 0), &str_arg(&args, 1));
            Value::Null
        }
        "openModsFolder" => {
            crate::mods::open_mods_folder(&str_arg(&args, 0));
            Value::Null
        }
        "getModValidation" => {
            let game = str_arg(&args, 0);
            let installed_version = games::installed_game_version(&game);
            json!(crate::mods::validate(&game, &installed_version))
        }
        "autoSortMods" => {
            crate::mods::auto_sort(&str_arg(&args, 0));
            Value::Null
        }
        "installModFromUrl" => {
            // Fire-and-forget, same reasoning as `installModArchives`: the
            // download + extraction can take a while, and the bridge is a
            // synchronous XHR. The frontend polls
            // `isInstallingMods`/`getModInstallReport` and `getDownloadProgress`
            // (see `mods::install_from_url_async`).
            let game = str_arg(&args, 0);
            let url = str_arg(&args, 1);
            let desired_id = str_arg(&args, 2);
            let expected_checksum = args.get(3).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || crate::mods::install_from_url_async(state_clone, game, url, desired_id, expected_checksum));
            Value::Null
        }
        "fetchModMetadata" => {
            // Synchronous: this is a single small metadata-only fetch (used at
            // submission time to auto-fill a catalog entry), not a full mod
            // install, so it doesn't warrant the background-thread + poll
            // pattern above.
            match crate::mods::fetch_metadata(&str_arg(&args, 0)) {
                Ok(meta) => json!(meta),
                Err(e) => json!({ "error": e }),
            }
        }
        "computeModChecksum" => {
            // Synchronous, same reasoning as `fetchModMetadata`: a single
            // small download+hash, called at approve/accept-update time to
            // stamp the catalog entry's `checksum` field.
            match crate::mods::compute_url_checksum(&str_arg(&args, 0)) {
                Ok(sum) => json!({ "checksum": sum }),
                Err(e) => json!({ "error": e }),
            }
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
            // Optional (7th): JSON map of cvar tag -> declared CVarType. Absent
            // when an older website/shim doesn't send it — `str_arg` returns ""
            // for a missing index, and `games::play` falls back to inferring
            // each value's TOML type from its formatted string.
            let cvar_types = str_arg(&args, 6);
            // Optional (8th): the game's title id, configured in Edit Game.
            // Used to pick the real "Live" leaderboard store file out of any
            // same-looking decoys before merging. Empty when unset/absent.
            let title_id = str_arg(&args, 7);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                launch_and_track(&state_clone, game, build, cvar_args, custom_exe, set_data_root, mount_update, cvar_types, title_id);
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
            let was_running = kill_running_game(state);
            if was_running {
                // Only revert to "Browsing games" here, not inside
                // `kill_running_game` itself — that fn is also called at the
                // start of `launch_and_track` to swap games, where the
                // immediately-following `set_playing` would otherwise cause a
                // visible "Browsing" flicker on Discord between games.
                discord::set_browsing(state);
            }
            json!(was_running)
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
        "isLauncherUpdating"             => json!(state.launcher_update_progress.load(Ordering::Relaxed) != -1),
        "getLauncherUpdateProgress"       => json!(state.launcher_update_progress.load(Ordering::Relaxed)),
        "getLauncherUpdateProgressString" => json!(state.launcher_update_string.lock().unwrap().clone()),
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
        "openBackupsFolder" => {
            let game = str_arg(&args, 0);
            saves::open_backups_folder(&game);
            json!(true)
        }
        "OpenExternalLink" => {
            platform::open_url(&str_arg(&args, 0));
            Value::Null
        }

        // ── Shortcuts ────────────────────────────────────────────────────────
        "desktopShortcutExists" => {
            let game  = str_arg(&args, 0);
            let title = str_arg(&args, 1);
            json!(shortcuts::exists_desktop(&game, &title))
        }
        "appShortcutExists" => {
            let game  = str_arg(&args, 0);
            let title = str_arg(&args, 1);
            json!(shortcuts::exists_applications(&game, &title))
        }
        "CreateDesktopShortcut" => {
            let game     = str_arg(&args, 0);
            let title    = str_arg(&args, 1);
            let icon_url = str_arg(&args, 2);
            std::thread::spawn(move || {
                if let Err(e) = shortcuts::create_desktop(&game, &title, &icon_url) {
                    eprintln!("[bridge] CreateDesktopShortcut error: {}", e);
                }
            });
            Value::Null
        }
        "CreateAppShortcut" => {
            let game     = str_arg(&args, 0);
            let title    = str_arg(&args, 1);
            let icon_url = str_arg(&args, 2);
            std::thread::spawn(move || {
                if let Err(e) = shortcuts::create_applications(&game, &title, &icon_url) {
                    eprintln!("[bridge] CreateAppShortcut error: {}", e);
                }
            });
            Value::Null
        }
        "RemoveDesktopShortcut" => {
            let game  = str_arg(&args, 0);
            let title = str_arg(&args, 1);
            if let Err(e) = shortcuts::remove_desktop(&game, &title) {
                eprintln!("[bridge] RemoveDesktopShortcut error: {}", e);
            }
            Value::Null
        }
        "RemoveAppShortcut" => {
            let game  = str_arg(&args, 0);
            let title = str_arg(&args, 1);
            if let Err(e) = shortcuts::remove_applications(&game, &title) {
                eprintln!("[bridge] RemoveAppShortcut error: {}", e);
            }
            Value::Null
        }
        "steamShortcutExists" => {
            let game  = str_arg(&args, 0);
            let title = str_arg(&args, 1);
            json!(shortcuts::exists_steam(&game, &title))
        }
        "steamInstalled" => json!(shortcuts::steam_installed()),
        "CreateSteamShortcut" => {
            let game       = str_arg(&args, 0);
            let title      = str_arg(&args, 1);
            let icon_url   = str_arg(&args, 2);
            let cover_url  = str_arg(&args, 3);
            let header_url = str_arg(&args, 4);
            let logo_url   = str_arg(&args, 5);
            std::thread::spawn(move || {
                if let Err(e) = shortcuts::create_steam(&game, &title, &icon_url, &cover_url, &header_url, &logo_url) {
                    eprintln!("[bridge] CreateSteamShortcut error: {}", e);
                }
            });
            Value::Null
        }
        "RemoveSteamShortcut" => {
            let game  = str_arg(&args, 0);
            let title = str_arg(&args, 1);
            if let Err(e) = shortcuts::remove_steam(&game, &title) {
                eprintln!("[bridge] RemoveSteamShortcut error: {}", e);
            }
            Value::Null
        }
        "getAutoPlayGame" => {
            let lock = state.auto_play_game.lock().unwrap();
            match lock.as_ref() {
                Some(game) => json!(game),
                None => Value::Null,
            }
        }
        "clearAutoPlayGame" => {
            *state.auto_play_game.lock().unwrap() = None;
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

        // ── Cloud save sync (Google Drive appDataFolder) ─────────────────────
        // `setCloudSaveEnabled` only requests the Drive-consent flow the first
        // time *any* game is enabled (see `cloud_saves::has_drive_access`) —
        // every game after that reuses the same stored refresh token, and
        // `getCloudSaveStatus` is what the Save Manager polls to drive its
        // toggle/status line (mirrors the getSaveSlots-style poll pattern).
        "getCloudSaveStatus" => json!(cloud_saves::status(&str_arg(&args, 0))),
        "setCloudSaveEnabled" => {
            let game = str_arg(&args, 0);
            let enabled = bool_arg(&args, 1, true);
            if enabled && !cloud_saves::has_drive_access() {
                json!({ "ok": false, "needsConsent": true })
            } else {
                cloud_saves::set_enabled(&game, enabled);
                if enabled {
                    // Sync immediately on enable rather than waiting for the
                    // next game close, so the toggle feels responsive.
                    std::thread::spawn(move || cloud_saves::sync_after_game_exit(&game));
                }
                json!({ "ok": true, "needsConsent": false })
            }
        }
        // Fire-and-forget: opens the system browser for the Drive-scope
        // consent screen. Poll `getCloudSaveSignInResult` every ~500ms until
        // status is "ok" or "error", mirroring `GoogleSignIn` above.
        "cloudSaveSignIn" => {
            *state.cloud_drive_signin.lock().unwrap() = DriveSignInResult::Pending;
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                let result = crate::auth::google_sign_in_drive();
                let mut lock = state_clone.cloud_drive_signin.lock().unwrap();
                *lock = match result {
                    Ok(tokens) => match tokens.refresh_token {
                        Some(refresh_token) => {
                            cloud_saves::store_refresh_token(&refresh_token);
                            DriveSignInResult::Ok
                        }
                        None => DriveSignInResult::Err(
                            "Google didn't grant offline access — please try again.".to_string(),
                        ),
                    },
                    Err(msg) => DriveSignInResult::Err(msg),
                };
            });
            Value::Null
        }
        "getCloudSaveSignInResult" => {
            let lock = state.cloud_drive_signin.lock().unwrap();
            match &*lock {
                DriveSignInResult::Idle    => json!({ "status": "idle" }),
                DriveSignInResult::Pending => json!({ "status": "pending" }),
                DriveSignInResult::Ok      => json!({ "status": "ok" }),
                DriveSignInResult::Err(m)  => json!({ "status": "error", "message": m }),
            }
        }
        // Manual "sync now" hooks for the Save Manager panel (enable toggle)
        // and the game page (on open) — both otherwise happen automatically
        // on game close/open, see the `cloud_saves::sync_after_game_exit`
        // call sites in `monitor_running_game`/`kill_running_game` above.
        "syncCloudSaveNow" => {
            let game = str_arg(&args, 0);
            std::thread::spawn(move || cloud_saves::sync_after_game_exit(&game));
            Value::Null
        }
        "syncCloudSaveOnOpen" => {
            let game = str_arg(&args, 0);
            std::thread::spawn(move || cloud_saves::sync_on_open(&game));
            Value::Null
        }

        // ── Achievements ──────────────────────────────────────────────────────
        "getAchievements" => {
            let title_ids: Vec<String> = json_arg(&args, 1);
            let title_ids = if title_ids.is_empty() { None } else { Some(title_ids) };
            json!(achievements::get_achievements(&str_arg(&args, 0), title_ids))
        }
        "getAchievementSummary" => json!(achievements::get_achievement_summary(&str_arg(&args, 0))),
        "listAchievementFiles" => json!(achievements::list_achievement_files(&str_arg(&args, 0))),

        // ── Leaderboards ───────────────────────────────────────────────────────
        // If nothing is running, any merge from a previous session should
        // already have been restored on exit — but restore defensively here
        // too (e.g. the launcher was killed mid-session) before the Manage
        // tab reads the files, so it never shows a stuck merged-only view.
        "listLeaderboardFiles" => {
            let game = str_arg(&args, 0);
            if state.running_game.lock().unwrap().is_none() {
                leaderboards::restore_after_exit(&game);
            }
            json!(leaderboards::list_leaderboard_files(&game))
        }
        "getLeaderboards" => {
            let title_ids: Vec<String> = json_arg(&args, 1);
            json!(leaderboards::get_leaderboards(&str_arg(&args, 0), title_ids))
        }

        // ── Play-time (local-only, never synced to the cloud) ────────────────
        "getPlaytime" => playtime::get_playtime(&str_arg(&args, 0)),

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

        // ── Discord Rich Presence ────────────────────────────────────────────
        "getDiscordPresenceEnabled" => json!(config::get_discord_presence_enabled()),
        "setDiscordPresenceEnabled" => {
            let enabled = args.first().and_then(|v| v.as_bool()).unwrap_or(true);
            discord::set_enabled(state, enabled);
            json!(true)
        }

        // ── Tray / taskbar collapse ──────────────────────────────────────────
        "getCollapseToTray" => json!(config::get_collapse_to_tray()),
        "setCollapseToTray" => {
            config::set_collapse_to_tray(bool_arg(&args, 0, true));
            json!(true)
        }
        "getCollapseAfterPlay" => json!(config::get_collapse_after_play()),
        "setCollapseAfterPlay" => {
            config::set_collapse_after_play(bool_arg(&args, 0, true));
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
