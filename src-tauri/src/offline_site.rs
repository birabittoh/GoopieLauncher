//! Serves a compile-time-embedded static copy of GoopieWebsite via the
//! `goopieoffline` custom URI scheme, and probes connectivity to the live
//! site so the launcher can fall back to it automatically.
//!
//! The embedded bundle lives in `offline-site/` (built by
//! `scripts/build-offline-site.sh` from the GoopieWebsite submodule; a
//! placeholder page ships so a plain `cargo build` always has something to
//! embed). It is a single-page app, so any path that doesn't match an
//! embedded file falls back to `index.html`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use include_dir::{include_dir, Dir};

static OFFLINE_SITE: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/offline-site");

/// Base URL the offline site is served from, matching the platform-specific
/// custom-scheme convention used for `goopiebridge` (see `bridge::make_init_script`).
pub fn offline_site_url() -> &'static str {
    if cfg!(windows) {
        "http://goopieoffline.localhost/"
    } else {
        "goopieoffline://localhost/"
    }
}

/// Handle one request arriving via the `goopieoffline` custom URI scheme.
pub fn handle_offline_request(request: tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    let path = request.uri().path().trim_start_matches('/');

    let file = OFFLINE_SITE
        .get_file(path)
        .or_else(|| OFFLINE_SITE.get_file("index.html"));

    match file {
        Some(file) => tauri::http::Response::builder()
            .status(200)
            .header("Content-Type", content_type_for(file.path().to_str().unwrap_or("")))
            .body(file.contents().to_vec())
            .unwrap(),
        None => tauri::http::Response::builder()
            .status(404)
            .header("Content-Type", "text/plain")
            .body(b"not found".to_vec())
            .unwrap(),
    }
}

fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Probe whether `https://goopie.xyz` is reachable, with a short timeout so
/// startup doesn't hang when there's no connectivity.
pub fn probe_connectivity() -> bool {
    let client = match reqwest::blocking::Client::builder()
        .user_agent("Goopie-Launcher/2")
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    client
        .head("https://goopie.xyz")
        .send()
        .map(|resp| resp.status().is_success() || resp.status().is_redirection())
        .unwrap_or(false)
}

/// Spawns a background thread that keeps `AppState::goopie_reachable` fresh by
/// re-probing every 20 seconds. Bridge calls are synchronous, so the website
/// reads this cached flag (via `isGoopieReachable`) instead of triggering a
/// multi-second probe on every check — e.g. to grey out "switch to online
/// mode" while offline-by-preference and goopie.xyz can't actually be reached.
pub fn spawn_connectivity_monitor(state: std::sync::Arc<crate::AppState>) {
    std::thread::spawn(move || loop {
        let reachable = probe_connectivity();
        state.goopie_reachable.store(reachable, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(20));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_index_at_root() {
        let req = tauri::http::Request::builder()
            .uri("goopieoffline://localhost/")
            .body(Vec::new())
            .unwrap();
        let resp = handle_offline_request(req);
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("Content-Type").unwrap().to_str().unwrap().starts_with("text/html"));
        assert!(String::from_utf8_lossy(resp.body()).contains("<html"));
    }

    #[test]
    fn falls_back_to_index_for_spa_routes() {
        let req = tauri::http::Request::builder()
            .uri("goopieoffline://localhost/library/some-game")
            .body(Vec::new())
            .unwrap();
        let resp = handle_offline_request(req);
        assert_eq!(resp.status(), 200);
        assert!(String::from_utf8_lossy(resp.body()).contains("<html"));
    }

    /// Only meaningful once `scripts/build-offline-site.sh` has embedded a real
    /// bundle (a plain `cargo build`/`cargo test` ships with just the
    /// placeholder `index.html`, which has no `assets/` directory) — skips
    /// rather than failing so CI doesn't need the Node/bun toolchain to run
    /// the unit tests.
    #[test]
    fn serves_embedded_assets_with_correct_content_type() {
        let Some(assets_dir) = OFFLINE_SITE.get_dir("assets") else {
            eprintln!("skipping: no embedded assets/ (placeholder bundle, run scripts/build-offline-site.sh for a real one)");
            return;
        };
        let Some(asset) = assets_dir
            .files()
            .find(|f| f.path().extension().and_then(|e| e.to_str()) == Some("js"))
        else {
            return;
        };
        let path = asset.path().to_str().unwrap();
        let req = tauri::http::Request::builder()
            .uri(format!("goopieoffline://localhost/{}", path))
            .body(Vec::new())
            .unwrap();
        let resp = handle_offline_request(req);
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("Content-Type").unwrap(), "text/javascript; charset=utf-8");
    }
}
