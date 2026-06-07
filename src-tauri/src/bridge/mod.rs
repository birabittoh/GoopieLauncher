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
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Mutex,
};

use crate::{config, download, games, iso, launcher, platform, saves, vehicles};

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

    match name {
        // ── Platform ──────────────────────────────────────────────────────────
        "GetPlatform"  => json!(platform::get_platform()),
        "GetArch"      => json!(platform::get_arch()),
        "getVersion"   => json!(env!("CARGO_PKG_VERSION")),

        "CheckForLauncherUpdate" => {
            let api_url = env!("GOOPIE_RELEASES_API");
            match download::fetch_to_string(api_url) {
                Ok(body) => {
                    let remote_tag = games::json_extract_str(&body, "tag_name");
                    let current = env!("CARGO_PKG_VERSION");
                    let remote_clean = remote_tag.trim_start_matches('v');
                    let has_update = !remote_clean.is_empty() && remote_clean != current;
                    json!({
                        "hasUpdate": has_update,
                        "latestVersion": remote_tag,
                        "current": current,
                    })
                }
                Err(_) => Value::Null,
            }
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
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || iso::install_iso(&game_name, state_clone));
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
            let set_data_root = args.get(4)
                .map(|v| v.as_bool().unwrap_or(
                    v.as_i64().map(|n| n != 0).unwrap_or(
                        v.as_str().map(|s| matches!(s, "1" | "true" | "yes")).unwrap_or(false)
                    )
                ))
                .unwrap_or(false);
            std::thread::spawn(move || {
                games::play(&game, &build, &cvar_args, &custom_exe, set_data_root);
            });
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
