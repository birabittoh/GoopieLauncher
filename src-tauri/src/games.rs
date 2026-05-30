//! Game lifecycle: install (ISO), update (GitHub releases), play, uninstall,
//! NeedsUpdate, and package management.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{archive, config, download, AppState};

// ── Path helpers ──────────────────────────────────────────────────────────────

fn games_root() -> PathBuf {
    PathBuf::from(config::get_games_folder())
}

fn game_dir(game: &str) -> PathBuf {
    games_root().join(game)
}

// ── Basic state queries ───────────────────────────────────────────────────────

/// Returns `true` if `<games>/<game>/assets/default.xex` exists.
pub fn is_iso_installed(game: &str) -> bool {
    game_dir(game).join("assets").join("default.xex").exists()
}

/// Returns `true` if the game executable exists (canonical name or sidecar-recorded path).
pub fn is_exe_updated(game: &str) -> bool {
    let dir = game_dir(game);

    // Check canonical name first.
    let canonical = canonical_exe_name(game);
    if dir.join(&canonical).exists() {
        return true;
    }

    // Fall back to the path recorded in .installed.json.
    if let Some(exe_path) = sidecar_exe_path(&dir) {
        if dir.join(&exe_path).exists() {
            return true;
        }
    }

    false
}

/// Returns the raw JSON content of `.installed.json`, or an empty string.
pub fn get_installed_version(game: &str) -> String {
    let path = game_dir(game).join(".installed.json");
    std::fs::read_to_string(&path).unwrap_or_default()
}

// ── Uninstall ─────────────────────────────────────────────────────────────────

/// Remove all game files except the `saves/` subdirectory.
pub fn uninstall(game: &str) {
    let dir = game_dir(game);
    if !dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name() != "saves" {
            let _ = std::fs::remove_dir_all(entry.path())
                .or_else(|_| std::fs::remove_file(entry.path()));
        }
    }
    eprintln!("[games] Uninstalled {} (saves preserved)", game);
}

// ── NeedsUpdate ──────────────────────────────────────────────────────────────

/// Returns `true` if the installed version is out of date (or not installed).
///
/// - Archive assets: compare stored `version` tag in `.installed.json` vs `tag_name` from the
///   GitHub API response.
/// - Legacy single-exe assets: compare local SHA256 vs the `digest` field in the GitHub API.
pub fn needs_update(game: &str, github_api_url: &str, asset_name: Option<&str>) -> bool {
    let effective_asset = asset_name.unwrap_or("").to_string();
    let effective_asset = if effective_asset.is_empty() {
        canonical_exe_name(game)
    } else {
        effective_asset
    };

    if archive::is_archive(&effective_asset) {
        // ── Archive path: version-tag comparison ─────────────────────────────
        let sidecar_path = game_dir(game).join(".installed.json");
        if !sidecar_path.exists() {
            return true;
        }
        let Ok(sidecar) = std::fs::read_to_string(&sidecar_path) else { return true };
        let installed_tag = json_extract_str(&sidecar, "version");

        let Ok(api_body) = download::fetch_to_string(github_api_url) else { return true };
        let remote_tag = json_extract_str(&api_body, "tag_name");
        if remote_tag.is_empty() {
            return true;
        }

        return installed_tag.to_lowercase() != remote_tag.to_lowercase();
    }

    // ── Legacy path: SHA256 comparison ───────────────────────────────────────
    let exe_path = game_dir(game).join(canonical_exe_name(game));
    if !exe_path.exists() {
        return true;
    }
    let local_sha = match download::sha256_file(&exe_path.to_string_lossy()) {
        Some(s) => s,
        None => return true,
    };

    let Ok(api_body) = download::fetch_to_string(github_api_url) else { return true };

    // Parse `"name":"<effective_asset>"` then find the nearest `"digest"` field.
    let remote_sha = extract_asset_digest(&api_body, &effective_asset);
    if let Some(sha) = remote_sha {
        return local_sha.to_lowercase() != sha.to_lowercase();
    }

    true // can't determine → assume needs update
}

// ── Update / download ─────────────────────────────────────────────────────────

/// Download a game release from a GitHub releases download prefix.
///
/// Arguments mirror the C++ `Update` JS function:
/// - `release_url`: download prefix, e.g. `https://github.com/owner/repo/releases/download/v1.0/`
/// - `asset_name`:  specific asset filename; defaults to the platform canonical name.
/// - `version_tag`: version string written into `.installed.json`.
/// - `packages_json`: optional JSON array of extra zip packages to download.
pub fn update(
    game: &str,
    release_url: &str,
    asset_name: Option<&str>,
    version_tag: Option<&str>,
    packages_json: Option<serde_json::Value>,
    state: Arc<AppState>,
) {
    let dir = game_dir(game);
    let _ = std::fs::create_dir_all(&dir);

    // Normalise release URL: ensure trailing slash.
    let base_url = if release_url.ends_with('/') {
        release_url.to_string()
    } else {
        format!("{}/", release_url)
    };

    let effective_asset = asset_name.unwrap_or("").to_string();
    let effective_asset = if effective_asset.is_empty() {
        canonical_exe_name(game)
    } else {
        effective_asset
    };

    let version = version_tag.unwrap_or("").to_string();
    let is_archive = archive::is_archive(&effective_asset);

    // Local destination for the downloaded file.
    let local_path = if is_archive {
        dir.join(&effective_asset)
    } else {
        dir.join(canonical_exe_name(game))
    };

    let download_url = format!("{}{}", base_url, effective_asset);
    eprintln!("[games] Downloading {} → {}", download_url, local_path.display());

    state.download_progress.store(0, std::sync::atomic::Ordering::Relaxed);
    let state_cb = Arc::clone(&state);
    let progress_cb: download::ProgressCallback = Box::new(move |dl, tot| {
        state_cb.set_download_progress(dl, tot);
    });

    if let Err(e) = download::download_file(
        &download_url,
        &local_path.to_string_lossy(),
        Some(&progress_cb),
    ) {
        eprintln!("[games] Update download failed: {}", e);
        state.finish_download();
        return;
    }

    let sidecar_path = dir.join(".installed.json");

    if is_archive {
        // ── New archive format ────────────────────────────────────────────────
        eprintln!("[games] Extracting archive {}", local_path.display());
        let extracted = archive::extract_archive(
            &local_path.to_string_lossy(),
            &dir.to_string_lossy(),
        )
        .is_ok();
        let _ = std::fs::remove_file(&local_path);

        let exe_name = if extracted {
            find_main_executable(&dir, game)
        } else {
            String::new()
        };

        write_sidecar(&sidecar_path, &version, &effective_asset, Some(&exe_name));
    } else {
        // ── Legacy single-exe format ──────────────────────────────────────────
        // Try to download optional .toml config.
        let toml_url = format!("{}{}.toml", base_url, game);
        let toml_path = dir.join(format!("{}.toml", game));
        let _ = download::download_file(&toml_url, &toml_path.to_string_lossy(), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &local_path,
                std::fs::Permissions::from_mode(0o755),
            );
        }

        // Handle tar.gz magic-byte detection for legacy assets.
        #[cfg(unix)]
        if archive::is_gzip_magic(&local_path.to_string_lossy()) {
            eprintln!("[games] Detected tar.gz magic — extracting legacy asset");
            let archive_path = local_path.with_extension("tar.gz");
            if std::fs::rename(&local_path, &archive_path).is_ok() {
                let _ = archive::extract_tar_gz(
                    &archive_path.to_string_lossy(),
                    &dir.to_string_lossy(),
                );
                let _ = std::fs::remove_file(&archive_path);
                // Move the extracted binary to the canonical name if needed.
                relocate_extracted_exe(&dir, game, &local_path);
            }
        }

        write_sidecar(&sidecar_path, &version, &effective_asset, None);
    }

    // ── Optional zip packages ─────────────────────────────────────────────────
    if let Some(pkgs) = packages_json {
        let packages_sidecar = dir.join(".installed_packages.json");
        if let Some(arr) = pkgs.as_array() {
            for pkg in arr {
                let asset = pkg["assetName"].as_str().unwrap_or("").to_string();
                if asset.is_empty() {
                    continue;
                }
                let pkg_url = format!("{}{}", base_url, asset);
                let pkg_local = dir.join(&asset);
                state.download_progress.store(0, std::sync::atomic::Ordering::Relaxed);
                let state_cb2 = Arc::clone(&state);
                let progress_cb2: download::ProgressCallback = Box::new(move |dl, tot| {
                    state_cb2.set_download_progress(dl, tot);
                });
                if download::download_file(&pkg_url, &pkg_local.to_string_lossy(), Some(&progress_cb2)).is_ok()
                    && archive::extract_zip(&pkg_local.to_string_lossy(), &dir.to_string_lossy()).is_ok()
                {
                    let _ = std::fs::remove_file(&pkg_local);
                    update_packages_sidecar(&packages_sidecar, &asset);
                    eprintln!("[games] Package installed: {}", asset);
                }
            }
        }
    }

    state.finish_download();
    eprintln!("[games] Update complete for {}", game);
}

// ── Package management ────────────────────────────────────────────────────────

pub fn install_package(game: &str, prefix: &str, zip_asset: &str, state: Arc<AppState>) {
    let dir = game_dir(game);
    let _ = std::fs::create_dir_all(&dir);

    let base = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{}/", prefix)
    };

    let url = format!("{}{}", base, zip_asset);
    let local = dir.join(zip_asset);
    let sidecar = dir.join(".installed_packages.json");

    state.download_progress.store(0, std::sync::atomic::Ordering::Relaxed);
    let state_cb = Arc::clone(&state);
    let cb: download::ProgressCallback = Box::new(move |dl, tot| {
        state_cb.set_download_progress(dl, tot);
    });

    eprintln!("[games] InstallPackage: downloading {}", url);
    if download::download_file(&url, &local.to_string_lossy(), Some(&cb)).is_ok()
        && archive::extract_zip(&local.to_string_lossy(), &dir.to_string_lossy()).is_ok()
    {
        let _ = std::fs::remove_file(&local);
        update_packages_sidecar(&sidecar, zip_asset);
        eprintln!("[games] InstallPackage: done {}", zip_asset);
    }
    state.finish_download();
}

pub fn is_package_installed(game: &str, zip_asset: &str) -> bool {
    let sidecar = game_dir(game).join(".installed_packages.json");
    let Ok(contents) = std::fs::read_to_string(&sidecar) else { return false };
    contents.contains(&format!("\"{}\"", zip_asset))
}

// ── Play ──────────────────────────────────────────────────────────────────────

pub fn play(game: &str, cvar_args: &str, custom_exe: &str, set_data_root: bool) {
    let dir = game_dir(game);

    let exe_path: PathBuf = if !custom_exe.is_empty() {
        dir.join(custom_exe)
    } else if let Some(sidecar_exe) = sidecar_exe_path(&dir) {
        dir.join(sidecar_exe)
    } else {
        dir.join(canonical_exe_name(game))
    };

    if !exe_path.exists() {
        eprintln!("[games] Play: executable not found: {}", exe_path.display());
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755));
    }

    let language = crate::config::get_language();
    let mut args: Vec<String> = vec![format!("--user_language={}", language)];

    if set_data_root {
        args.push(format!(
            "--game_data_root={}",
            dir.join("assets").to_string_lossy()
        ));
    }

    if !cvar_args.is_empty() {
        // Split on whitespace, respecting quoted strings would be ideal but this
        // matches the C++ behaviour of appending the raw string.
        for token in cvar_args.split_whitespace() {
            args.push(token.to_string());
        }
    }

    eprintln!(
        "[games] Launching: {} {:?}",
        exe_path.display(),
        args
    );

    let mut cmd = std::process::Command::new(&exe_path);
    cmd.args(&args);
    cmd.current_dir(&dir);

    match cmd.spawn() {
        Ok(_child) => {
            // Let it run independently; we don't wait on it.
            eprintln!("[games] Process spawned successfully");
        }
        Err(e) => eprintln!("[games] Failed to spawn process: {}", e),
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Platform-canonical executable name for a game (no extension on Linux, .exe on Windows).
fn canonical_exe_name(game: &str) -> String {
    #[cfg(windows)]
    { format!("{}-windows-x64.exe", game) }
    #[cfg(not(windows))]
    { format!("{}-linux-x64", game) }
}

/// Read the `exePath` field from `.installed.json`.
fn sidecar_exe_path(dir: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(dir.join(".installed.json")).ok()?;
    let v = json_extract_str(&contents, "exePath");
    if v.is_empty() { None } else { Some(v) }
}

/// Write `.installed.json`.
fn write_sidecar(path: &Path, version: &str, asset: &str, exe_path: Option<&str>) {
    let exe_field = exe_path.map(|e| format!(r#","exePath":"{}""#, e.replace('\\', "\\\\").replace('"', "\\\""))).unwrap_or_default();
    let json = format!(
        r#"{{"version":"{}","asset":"{}"{}}}"#,
        version.replace('"', "\\\""),
        asset.replace('"', "\\\""),
        exe_field,
    );
    if let Err(e) = std::fs::write(path, json) {
        eprintln!("[games] Failed to write sidecar {}: {}", path.display(), e);
    }
}

/// Append an asset name to the packages sidecar JSON.
fn update_packages_sidecar(path: &Path, asset: &str) {
    let existing = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let key = format!("\"{}\"", asset.replace('"', "\\\""));
    if existing.contains(&key) {
        return;
    }
    let trimmed = existing.trim_end_matches('}');
    let separator = if trimmed == "{" { "" } else { "," };
    let new_json = format!("{}{}{}:true}}", trimmed, separator, key);
    let _ = std::fs::write(path, new_json);
}

/// Find the main executable in a game directory after archive extraction.
fn find_main_executable(dir: &Path, game: &str) -> String {
    #[cfg(windows)]
    let preferred = format!("{}.exe", game);
    #[cfg(not(windows))]
    let preferred = game.to_string();

    let mut fallback = String::new();

    let Ok(entries) = std::fs::read_dir(dir) else { return fallback };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let fname = entry.file_name().to_string_lossy().into_owned();
        if fname.to_lowercase() == preferred.to_lowercase() {
            return fname;
        }
        #[cfg(windows)]
        if fallback.is_empty() && fname.to_lowercase().ends_with(".exe") {
            fallback = fname;
        }
        #[cfg(unix)]
        if fallback.is_empty() && !fname.contains('.') {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = entry.metadata() {
                if meta.permissions().mode() & 0o111 != 0 {
                    fallback = fname;
                }
            }
        }
    }

    fallback
}

/// After tar.gz extraction, try to move the executable to the canonical location.
#[cfg(unix)]
fn relocate_extracted_exe(dir: &Path, game: &str, canonical: &Path) {
    if canonical.exists() {
        return; // already in place
    }
    let canonical_name = canonical.file_name().unwrap_or_default().to_string_lossy().into_owned();
    // (a) bare game name at root
    let candidate = dir.join(game);
    if candidate.exists() {
        let _ = std::fs::rename(&candidate, canonical);
        return;
    }
    // (b) one level deep in a subdirectory
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
        for name in [canonical_name.as_str(), game] {
            let p = entry.path().join(name);
            if p.exists() {
                let _ = std::fs::rename(&p, canonical);
                return;
            }
        }
    }
}

// ── JSON extraction helpers (avoid serde_json for simple cases) ────────────────

/// Extract a JSON string value by key using string search.
/// Handles both `"key":"value"` and `"key": "value"` spacing.
pub fn json_extract_str(json: &str, key: &str) -> String {
    for pattern in [
        format!("\"{}\":\"", key),
        format!("\"{}\": \"", key),
    ] {
        if let Some(pos) = json.find(&pattern) {
            let start = pos + pattern.len();
            if let Some(end) = json[start..].find('"') {
                return json[start..start + end].to_string();
            }
        }
    }
    String::new()
}

/// Extract the SHA256 `digest` for a named asset from a GitHub API response.
fn extract_asset_digest(api_body: &str, asset_name: &str) -> Option<String> {
    let name_search = format!("\"name\":\"{}\"", asset_name);
    let name_search2 = format!("\"name\": \"{}\"", asset_name);

    let asset_pos = api_body.find(&name_search)
        .or_else(|| api_body.find(&name_search2))?;

    for pattern in ["\"digest\":", "\"digest\": "] {
        if let Some(pos) = api_body[asset_pos..].find(pattern) {
            let after = asset_pos + pos + pattern.len();
            let after = api_body[after..].trim_start();
            if let Some(content) = after.strip_prefix('"') {
                if let Some(end) = content.find('"') {
                    let digest = &content[..end];
                    // Strip `sha256:` prefix if present.
                    return Some(
                        digest.strip_prefix("sha256:").unwrap_or(digest).to_string()
                    );
                }
            }
        }
    }
    None
}
