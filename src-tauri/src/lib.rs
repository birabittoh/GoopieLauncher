mod auth;
mod bridge;
mod launcher;
mod config;
mod archive;
mod download;
mod games;
mod iso;
mod offline_site;
mod paths;
mod platform;
mod saves;
mod vehicles;

use std::sync::Arc;
use tauri::{WebviewUrl, WebviewWindowBuilder};

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Generate a random per-launch token and build the JS init-script before
    // Tauri starts. The custom URI scheme is registered on the Builder so wry
    // marks it as a secure context before the first navigation — preventing any
    // mixed-content block from the HTTPS page.
    let state = Arc::new(AppState::new());
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

            Ok(())
        })
        // No tauri::command handlers — all native calls go through the bridge scheme.
        .invoke_handler(tauri::generate_handler![])
        .run(tauri::generate_context!())
        .expect("error while running Goopie Launcher");
}
