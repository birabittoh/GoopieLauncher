mod bridge;
mod config;
mod archive;
mod download;
mod games;
mod iso;
mod paths;
mod platform;
mod saves;
mod vehicles;

use std::sync::Arc;
use tauri::{WebviewUrl, WebviewWindowBuilder};

pub use bridge::AppState;

/// Resolve the URL to load in the main window.
/// Priority: `--url <URL>` > `--local` > `GOOPIE_URL` env var > production.
fn resolve_url() -> String {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--url" {
            if let Some(url) = args.get(i + 1) {
                return url.clone();
            }
        }
        if args[i] == "--local" {
            return "http://localhost:5173/".to_string();
        }
        i += 1;
    }
    if let Ok(url) = std::env::var("GOOPIE_URL") {
        if !url.is_empty() {
            return url;
        }
    }
    "https://goopie.xyz".to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Start the synchronous bridge server before building Tauri so we can
    // embed the port/token into the initialization script.
    let state = Arc::new(AppState::new());
    let (port, token) = bridge::start_server(Arc::clone(&state));

    let url_str = resolve_url();
    let init_script = bridge::make_init_script(port, &token);

    tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            let url = WebviewUrl::External(url_str.parse().expect("invalid launcher URL"));

            WebviewWindowBuilder::new(app, "main", url)
                .title("Goopie Launcher")
                .inner_size(1280.0, 800.0)
                .min_inner_size(800.0, 600.0)
                .resizable(true)
                .initialization_script(&init_script)
                .build()?;

            Ok(())
        })
        // No tauri::command handlers — all native calls go through the bridge server.
        .invoke_handler(tauri::generate_handler![])
        .run(tauri::generate_context!())
        .expect("error while running Goopie Launcher");
}
