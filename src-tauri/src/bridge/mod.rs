//! Synchronous loopback HTTP bridge server.
//!
//! The website at goopie.xyz calls native functions synchronously via `window.Foo()`.
//! Because Tauri's `invoke()` is async (Promise-based), and we cannot modify the website,
//! we instead:
//!   1. Bind a tiny HTTP server to `127.0.0.1:<ephemeral port>` at startup.
//!   2. Inject a JS init-script that defines every `window.*` global as a synchronous
//!      XMLHttpRequest to `http://127.0.0.1:<port>/bridge/<fn>?token=<secret>&args=<json>`.
//!   3. The server dispatches each function name to Rust and returns a JSON body.
//!
//! A random per-launch token prevents other local processes from calling the bridge.

use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Mutex,
};

use tiny_http::{Header, Response, Server};

use crate::{config, games, iso, platform, saves, vehicles};

// ── Shared application state ─────────────────────────────────────────────────

pub struct AppState {
    /// Download progress: -1 = idle, 0-100 = percent.
    pub download_progress: AtomicI32,
    /// Human-readable download progress string ("X MB / Y MB").
    pub download_string: Mutex<String>,
    /// Whether an ISO extraction is currently in progress.
    pub is_extracting: AtomicBool,
    /// Cached vehicle list (for Nuts & Bolts save browser).
    pub vehicles: Mutex<Vec<serde_json::Value>>,
}

impl AppState {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            download_progress: AtomicI32::new(-1),
            download_string: Mutex::new(String::new()),
            is_extracting: AtomicBool::new(false),
            vehicles: Mutex::new(Vec::new()),
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

// ── Server startup ────────────────────────────────────────────────────────────

/// Start the bridge server on a random loopback port.
/// Returns `(port, token)` so the init-script can be generated.
pub fn start_server(state: Arc<AppState>) -> (u16, String) {
    let server = Server::http("127.0.0.1:0").expect("failed to bind bridge server");
    let port = server
        .server_addr()
        .to_ip()
        .expect("not a TCP addr")
        .port();

    // Random 32-char hex token
    let token: String = {
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
    };

    let token_clone = token.clone();
    std::thread::Builder::new()
        .name("bridge-server".into())
        .spawn(move || run_server(server, token_clone, state))
        .expect("failed to spawn bridge server thread");

    (port, token)
}

/// Build the initialization script by substituting port/token into the shim template.
pub fn make_init_script(port: u16, token: &str) -> String {
    let shim = include_str!("shim.js");
    shim.replace("__BRIDGE_PORT__", &port.to_string())
        .replace("__BRIDGE_TOKEN__", token)
}

// ── Server loop ───────────────────────────────────────────────────────────────

fn run_server(server: Server, token: String, state: Arc<AppState>) {
    for request in server.incoming_requests() {
        let url = request.url().to_string();

        // Parse path and query from the URL.
        let (path, query) = match url.find('?') {
            Some(pos) => (&url[..pos], &url[pos + 1..]),
            None => (url.as_str(), ""),
        };

        // Only handle /bridge/<fn>
        let fn_name = if let Some(rest) = path.strip_prefix("/bridge/") {
            percent_decode(rest)
        } else {
            let _ = request.respond(Response::from_string("not found").with_status_code(404));
            continue;
        };

        // Parse query params
        let params = parse_query(query);
        let req_token = params.get("token").map(|s| s.as_str()).unwrap_or("");
        if req_token != token {
            let _ = request.respond(Response::from_string("forbidden").with_status_code(403));
            continue;
        }

        let args_str = params.get("args").cloned().unwrap_or_else(|| "[]".into());
        let args: Vec<serde_json::Value> = serde_json::from_str(&args_str).unwrap_or_default();

        let result = dispatch(&fn_name, args, &state);
        let body = serde_json::to_string(&result).unwrap_or_else(|_| "null".into());

        let cors = Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap();
        let ct = Header::from_bytes("Content-Type", "application/json").unwrap();
        let resp = Response::from_string(body)
            .with_header(cors)
            .with_header(ct);
        let _ = request.respond(resp);
    }
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
        "getVersion"   => json!(10_i32),

        // ── Config / paths ────────────────────────────────────────────────────
        "GetGamesPath" => json!(config::get_games_folder()),
        "SetGamesPath" => {
            // Opens a native folder dialog synchronously (blocking call is fine in the
            // bridge thread).
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
        "isIsoInstalled"      => json!(games::is_iso_installed(&str_arg(&args, 0))),
        "isExeUpdated"        => json!(games::is_exe_updated(&str_arg(&args, 0))),
        "getInstalledVersion" => json!(games::get_installed_version(&str_arg(&args, 0))),

        // ── Long-running ops ──────────────────────────────────────────────────
        "Install" => {
            let game_name = str_arg(&args, 0);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || iso::install_iso(&game_name, state_clone));
            Value::Null
        }
        "Uninstall" => {
            games::uninstall(&str_arg(&args, 0));
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
            let api_url = str_arg(&args, 1);
            let asset = if args.get(2).and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                None
            } else {
                Some(str_arg(&args, 2))
            };
            json!(games::needs_update(&game, &api_url, asset.as_deref()))
        }
        "Play" => {
            let game     = str_arg(&args, 0);
            let cvar_args = str_arg(&args, 1);
            let custom_exe = str_arg(&args, 2);
            let set_data_root = args.get(3)
                .map(|v| v.as_bool().unwrap_or(
                    v.as_i64().map(|n| n != 0).unwrap_or(
                        v.as_str().map(|s| matches!(s, "1" | "true" | "yes")).unwrap_or(false)
                    )
                ))
                .unwrap_or(false);
            std::thread::spawn(move || {
                games::play(&game, &cvar_args, &custom_exe, set_data_root);
            });
            Value::Null
        }
        "InstallPackage" => {
            let game       = str_arg(&args, 0);
            let prefix     = str_arg(&args, 1);
            let zip_asset  = str_arg(&args, 2);
            let state_clone = Arc::clone(state);
            std::thread::spawn(move || {
                games::install_package(&game, &prefix, &zip_asset, state_clone);
            });
            Value::Null
        }
        "IsPackageInstalled" => {
            let game = str_arg(&args, 0);
            let zip  = str_arg(&args, 1);
            json!(games::is_package_installed(&game, &zip))
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
