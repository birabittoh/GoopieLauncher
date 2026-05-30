use std::sync::Arc;

use crate::{bridge::AppState, download};

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
