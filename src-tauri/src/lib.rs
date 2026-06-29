mod auth;
mod binfmt;
mod bridge;
mod launcher;
mod config;
mod archive;
mod download;
mod games;
mod extract;
mod offline_site;
mod paths;
mod platform;
mod proton;
mod saves;
mod shortcuts;
mod vehicles;

use std::sync::Arc;
use tauri::{DragDropEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

pub use bridge::AppState;

/// Production URL of the live website.
pub const REMOTE_URL: &str = "https://goopie.xyz";

/// An explicit `--url`/`--local`/`GOOPIE_URL` override, if any. These are dev
/// escape hatches and bypass offline-mode resolution entirely.
///
/// `pub(crate)` so the `setOfflineMode` bridge handler can mirror this
/// priority when navigating live — otherwise toggling to "online" while
/// running with `--local`/`--url`/`GOOPIE_URL` would ignore the override and
/// jump to the production site instead of the dev URL.
pub(crate) fn url_override() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--url" {
            if let Some(url) = args.get(i + 1) {
                return Some(url.clone());
            }
        }
        if args[i] == "--local" {
            return Some("http://localhost:5173/".to_string());
        }
        i += 1;
    }
    if let Ok(url) = std::env::var("GOOPIE_URL") {
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

/// Resolve the URL to load in the main window at startup.
///
/// Priority:
///   1. `--url <URL>` / `--local` / `GOOPIE_URL` — dev overrides, bypass offline logic.
///   2. The user's persisted offline-mode preference (`config::get_offline_mode_preference`):
///      if they've explicitly enabled offline mode, honor it unconditionally — no probe,
///      no requests to `goopie.xyz` at all, until they flip it back themselves.
///   3. Otherwise (the user prefers online): probe `https://goopie.xyz` on every launch
///      and fall back to the embedded offline bundle if it's unreachable. This fallback
///      is purely transient — it's never written back to the persisted preference, so
///      the launcher returns to online mode automatically once connectivity is restored.
fn resolve_url() -> String {
    if let Some(url) = url_override() {
        return url;
    }
    let offline = config::get_offline_mode_preference() || !offline_site::probe_connectivity();
    if offline {
        offline_site::offline_site_url().to_string()
    } else {
        REMOTE_URL.to_string()
    }
}

/// Whether the launcher was invoked with `--self-update-check`: a hidden,
/// headless mode (no window) that runs a single update check, applies it when
/// `AutoApplyUpdate` is set, and exits. Used by the end-to-end test harness.
fn self_update_check_requested() -> bool {
    std::env::args().skip(1).any(|a| a == "--self-update-check")
}

/// If the launcher was invoked with `--play <recompName>`, return the game name.
/// Used by shortcuts to auto-play a game on launch.
fn auto_play_game() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--play" {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Headless self-update check (no GUI): runs the update flow and exits. Must
    // happen before any Tauri setup so it works without a display.
    if self_update_check_requested() {
        launcher::run_self_update_check();
    }

    // Generate a random per-launch token and build the JS init-script before
    // Tauri starts. The custom URI scheme is registered on the Builder so wry
    // marks it as a secure context before the first navigation — preventing any
    // mixed-content block from the HTTPS page.
    let state = Arc::new(AppState::new());
    if let Some(game) = auto_play_game() {
        *state.auto_play_game.lock().unwrap() = Some(game);
        state.exit_after_game.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let token = bridge::make_token();
    let init_script = bridge::make_init_script(&token);
    let url_str = resolve_url();

    // Clone what the scheme handler closures and .setup() need to own.
    let state_for_scheme = Arc::clone(&state);
    let token_for_scheme = token.clone();
    let state_for_setup = Arc::clone(&state);

    tauri::Builder::default()
        .manage(state)
        // Register custom schemes before .setup() so wry marks them as secure
        // contexts — preventing mixed-content blocks from the HTTPS page.
        .register_uri_scheme_protocol("goopiebridge", move |_ctx, request| {
            bridge::handle_bridge_request(&state_for_scheme, request, &token_for_scheme)
        })
        // Serves the embedded offline copy of GoopieWebsite (see offline_site.rs).
        .register_uri_scheme_protocol("goopieoffline", |_ctx, request| {
            offline_site::handle_offline_request(request)
        })
        .setup(move |app| {
            let url = WebviewUrl::External(url_str.parse().expect("invalid launcher URL"));

            let window = WebviewWindowBuilder::new(app, "main", url)
                .title("Goopie Launcher")
                .inner_size(1280.0, 800.0)
                .min_inner_size(800.0, 600.0)
                .resizable(true)
                .initialization_script(&init_script)
                .build()?;

            // Stash the window handle so bridge commands (setOfflineMode) can
            // .navigate() it to switch between the live site and the offline
            // bundle at runtime, without relaunching the app.
            *state_for_setup.window.lock().unwrap() = Some(window);

            // Keeps AppState::goopie_reachable fresh so the website can grey
            // out "switch to online mode" while goopie.xyz is unreachable,
            // without blocking the (synchronous) bridge on a multi-second probe.
            offline_site::spawn_connectivity_monitor(Arc::clone(&state_for_setup));

            // Keeps AppState::update_available fresh (checked roughly every
            // hour — throttled across restarts via config::*_last_update_check
            // so re-opening the launcher repeatedly doesn't burst requests) so
            // the website can show an "update available" prompt without ever
            // blocking the bridge thread on a GitHub API call.
            // This only refreshes the cache — applying an update is an explicit
            // user action (`SelfUpdateLauncher`), unless the hidden
            // `AutoApplyUpdate` setting is enabled (then checks auto-apply).
            launcher::spawn_update_monitor(Arc::clone(&state_for_setup));

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) = event {
                let paths_json: Vec<String> = paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                if let Ok(json) = serde_json::to_string(&paths_json) {
                    let js = format!(
                        "window.dispatchEvent(new CustomEvent('goopie:filedrop', {{ detail: {{ paths: {} }} }}))",
                        json
                    );
                    for webview in window.webviews() {
                        let _ = webview.eval(&js);
                    }
                }
            }
        })
        // No tauri::command handlers — all native calls go through the bridge scheme.
        .invoke_handler(tauri::generate_handler![])
        .run(tauri::generate_context!())
        .expect("error while running Goopie Launcher");
}
