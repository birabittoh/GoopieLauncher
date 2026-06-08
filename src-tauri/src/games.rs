//! Game lifecycle: install (ISO), update (GitHub releases), play, uninstall,
//! NeedsUpdate, and package management.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use crate::{archive, config, download, AppState};

// ── Path helpers ──────────────────────────────────────────────────────────────
//
// Each game gets a root directory `<games>/<recompName>/` containing:
//   builds/<tag>/   one self-contained install per release tag (exe, sidecar, …)
//   assets/         shared ISO data (default.xex etc.) — one copy per game
//   saves/          shared save-game data — one copy per game
//
// Builds used to be extracted directly into the game root, so updating to a
// new version overwrote the previous install in place. `migrate_legacy_install`
// moves such flat installs into `builds/<version>/` the first time they're
// touched, so existing players keep their installed build without a re-download.

fn games_root() -> PathBuf {
    PathBuf::from(config::get_games_folder())
}

/// Root directory for a game: `<games>/<recompName>/`. Houses the shared
/// `assets/`, `saves/`, and `builds/` subdirectories.
fn game_root(game: &str) -> PathBuf {
    games_root().join(game)
}

/// Directory for a single installed build: `<games>/<recompName>/builds/<tag>/`.
/// Lazily migrates a pre-existing flat (single-build) install on first access,
/// so this is the one place build-scoped operations should resolve their path
/// through.
fn build_dir(game: &str, build: &str) -> PathBuf {
    migrate_legacy_install(game);
    game_root(game).join("builds").join(sanitize_build_key(build))
}

/// Sanitise a version tag for safe use as a directory name (replace path
/// separators and other filesystem-unfriendly characters).
fn sanitize_build_key(tag: &str) -> String {
    let cleaned: String = tag
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// Move a pre-existing flat install (root-level `.installed.json` and its
/// sibling files) into `builds/<version>/`. Idempotent: a no-op once migrated,
/// and leaves the legacy files alone if the destination already exists (e.g. a
/// build under the same tag was separately installed) to avoid clobbering data.
fn migrate_legacy_install(game: &str) {
    let root = game_root(game);
    let legacy_sidecar = root.join(".installed.json");
    if !legacy_sidecar.is_file() {
        return;
    }

    let version = std::fs::read_to_string(&legacy_sidecar)
        .map(|s| json_extract_str(&s, "version"))
        .unwrap_or_default();
    let build_key = sanitize_build_key(&version);
    let dest = root.join("builds").join(&build_key);
    if dest.exists() {
        // This is a permanent, idempotent state for games left in it (the
        // legacy sidecar is intentionally not removed, so this check — and
        // therefore this branch — runs on every `build_dir()` call, which
        // happens many times per second while the UI polls install status).
        // Log it once per game per process instead of spamming stderr.
        static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
        if warned.lock().unwrap().insert(game.to_string()) {
            eprintln!(
                "[games] migrate_legacy_install: {} already has builds/{}; leaving legacy files at the game root",
                game, build_key
            );
        }
        return;
    }

    let Ok(entries) = std::fs::read_dir(&root) else { return };
    let entries: Vec<_> = entries.flatten().collect();
    if let Err(e) = std::fs::create_dir_all(&dest) {
        eprintln!("[games] migrate_legacy_install: failed to create {}: {}", dest.display(), e);
        return;
    }
    for entry in entries {
        let name = entry.file_name();
        if name == "assets" || name == "saves" || name == "builds" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        if let Err(e) = std::fs::rename(&from, &to) {
            eprintln!("[games] migrate_legacy_install: failed to move {} -> {}: {}", from.display(), to.display(), e);
        }
    }
    eprintln!("[games] Migrated legacy install of {} into builds/{}", game, build_key);
}

// ── Basic state queries ───────────────────────────────────────────────────────

/// Returns `true` if `<games>/<game>/assets/default.xex` exists. ISO data is
/// shared across all builds of a game, so this is not build-scoped.
pub fn is_iso_installed(game: &str) -> bool {
    game_root(game).join("assets").join("default.xex").exists()
}

/// Returns `true` if the build's executable exists (canonical name or
/// sidecar-recorded path).
pub fn is_exe_updated(game: &str, build: &str) -> bool {
    let dir = build_dir(game, build);

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

/// Returns the raw JSON content of a build's `.installed.json`, or an empty string.
pub fn get_installed_version(game: &str, build: &str) -> String {
    let path = build_dir(game, build).join(".installed.json");
    std::fs::read_to_string(&path).unwrap_or_default()
}

/// Enumerate every installed build for a game by scanning `builds/*` and
/// reading each one's `.installed.json` sidecar. `name` is the on-disk
/// (sanitised) build key — pass it back as the `build` argument to
/// Play/Uninstall/NeedsUpdate/etc.
pub fn get_installed_builds(game: &str) -> Vec<serde_json::Value> {
    migrate_legacy_install(game);
    let builds_root = game_root(game).join("builds");
    let Ok(entries) = std::fs::read_dir(&builds_root) else { return Vec::new() };
    let mut builds: Vec<serde_json::Value> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let sidecar = std::fs::read_to_string(e.path().join(".installed.json")).unwrap_or_default();
            serde_json::json!({
                "name": name,
                "version": json_extract_str(&sidecar, "version"),
                "asset": json_extract_str(&sidecar, "asset"),
                "exePath": json_extract_str(&sidecar, "exePath"),
            })
        })
        .collect();
    builds.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    builds
}

// ── Uninstall ─────────────────────────────────────────────────────────────────

/// Remove a single installed build (its entire `builds/<tag>/` directory).
/// Shared `saves/` and `assets/` (ISO data) are never touched.
pub fn uninstall(game: &str, build: &str) {
    let dir = build_dir(game, build);
    if !dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        eprintln!("[games] Failed to uninstall {} build {}: {}", game, build, e);
        return;
    }
    eprintln!("[games] Uninstalled {} build {} (saves/assets preserved)", game, build);
}

/// Remove every installed build for a game, keeping `saves/` and `assets/`.
pub fn uninstall_all(game: &str) {
    let root = game_root(game);
    if !root.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == "saves" || name == "assets" {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path())
            .or_else(|_| std::fs::remove_file(entry.path()));
    }
    eprintln!("[games] Uninstalled all builds of {} (saves/assets preserved)", game);
}

/// Remove the extracted ISO/asset data for a game (`assets/`), leaving
/// `saves/` and any installed builds untouched. Lets the user reclaim the disk
/// space an extracted ISO takes up without uninstalling the game itself.
pub fn remove_assets(game: &str) {
    let dir = game_root(game).join("assets");
    if !dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        eprintln!("[games] Failed to remove assets for {}: {}", game, e);
        return;
    }
    eprintln!("[games] Removed assets for {} (saves/builds preserved)", game);
}

// ── NeedsUpdate ──────────────────────────────────────────────────────────────

/// Returns `true` if the installed version is out of date (or not installed).
///
/// - Archive assets: compare stored `version` tag in `.installed.json` vs `tag_name` from the
///   GitHub API response.
/// - Legacy single-exe assets: compare local SHA256 vs the `digest` field in the GitHub API.
pub fn needs_update(game: &str, build: &str, github_api_url: &str, asset_name: Option<&str>) -> bool {
    let dir = build_dir(game, build);
    let effective_asset = asset_name.unwrap_or("").to_string();
    let effective_asset = if effective_asset.is_empty() {
        canonical_exe_name(game)
    } else {
        effective_asset
    };

    if archive::is_archive(&effective_asset) {
        // ── Archive path: version-tag comparison ─────────────────────────────
        let sidecar_path = dir.join(".installed.json");
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
    let exe_path = dir.join(canonical_exe_name(game));
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
    // Each release tag gets its own build directory so switching versions
    // never overwrites a sibling install — the version tag IS the build key.
    let version = version_tag.unwrap_or("").to_string();
    let dir = build_dir(game, &version);
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

pub fn install_package(game: &str, build: &str, prefix: &str, zip_asset: &str, state: Arc<AppState>) {
    let dir = build_dir(game, build);
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

pub fn is_package_installed(game: &str, build: &str, zip_asset: &str) -> bool {
    let sidecar = build_dir(game, build).join(".installed_packages.json");
    let Ok(contents) = std::fs::read_to_string(&sidecar) else { return false };
    contents.contains(&format!("\"{}\"", zip_asset))
}

// ── Play ──────────────────────────────────────────────────────────────────────

pub fn play(game: &str, build: &str, cvar_args: &str, custom_exe: &str, set_data_root: bool) -> Option<std::process::Child> {
    let dir = build_dir(game, build);

    let exe_path: PathBuf = if !custom_exe.is_empty() {
        dir.join(custom_exe)
    } else if let Some(sidecar_exe) = sidecar_exe_path(&dir) {
        dir.join(sidecar_exe)
    } else {
        dir.join(canonical_exe_name(game))
    };

    if !exe_path.exists() {
        eprintln!("[games] Play: executable not found: {}", exe_path.display());
        return None;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755));
    }

    let language = crate::config::get_language();
    let mut args: Vec<String> = vec![format!("--user_language={}", language)];

    if set_data_root {
        // ISO data lives at the shared game root, not inside the build dir.
        args.push(format!(
            "--game_data_root={}",
            game_root(game).join("assets").to_string_lossy()
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
        Ok(child) => {
            eprintln!("[games] Process spawned successfully");
            Some(child)
        }
        Err(e) => {
            eprintln!("[games] Failed to spawn process: {}", e);
            None
        }
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
