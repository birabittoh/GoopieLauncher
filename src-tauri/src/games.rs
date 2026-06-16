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
                "platform": json_extract_str(&sidecar, "platform"),
                "arch": json_extract_str(&sidecar, "arch"),
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
    let exe_path = installed_exe_path(&dir, game);
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
    // Wipe it first so re-installing a different platform's asset for the same
    // tag (e.g. switching the Linux build for the Windows one) starts clean
    // instead of co-mingling both platforms' files in one directory.
    let version = version_tag.unwrap_or("").to_string();
    let dir = build_dir(game, &version);
    let _ = std::fs::remove_dir_all(&dir);
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

    // Local destination for the downloaded file.  Single-exe assets are saved
    // under their real asset name (e.g. `retip-windows-x64.exe`) rather than the
    // host's canonical name, so a Windows build downloaded on Linux keeps its
    // `.exe` and is routed through Proton at launch instead of being mistaken for
    // a native binary.  `exePath` is recorded in the sidecar below.
    let local_path = dir.join(&effective_asset);

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

        let exe_info = if exe_name.is_empty() {
            crate::binfmt::ExeInfo::default()
        } else {
            crate::binfmt::detect_executable(&dir.join(&exe_name))
        };

        write_sidecar(&sidecar_path, &version, &effective_asset, Some(&exe_name), exe_info);
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

        let exe_info = crate::binfmt::detect_executable(&local_path);
        write_sidecar(&sidecar_path, &version, &effective_asset, Some(&effective_asset), exe_info);
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

pub fn play(game: &str, build: &str, cvar_args: &str, custom_exe: &str, set_data_root: bool) -> Result<std::process::Child, String> {
    let dir = build_dir(game, build);

    let exe_path: PathBuf = if !custom_exe.is_empty() {
        dir.join(custom_exe)
    } else {
        installed_exe_path(&dir, game)
    };

    if !exe_path.exists() {
        eprintln!("[games] Play: executable not found: {}", exe_path.display());
        return Err(format!(
            "Executable not found: {}. This build may be for a different platform, or the install may be incomplete/corrupted.",
            exe_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| exe_path.to_string_lossy().into_owned())
        ));
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
    } else {
        // Older builds that don't support --game_data_root look for assets
        // relative to their own directory.  Ensure a junction/symlink exists so
        // they can still find the shared ISO data without copying it.
        ensure_assets_link(game, &dir);
    }

    if !cvar_args.is_empty() {
        // Split on whitespace, respecting quoted strings would be ideal but this
        // matches the C++ behaviour of appending the raw string.
        for token in cvar_args.split_whitespace() {
            args.push(token.to_string());
        }
    }

    // On Linux, transparently route Windows PE executables through Proton so the
    // user doesn't need to configure Wine themselves.  Detect the actual binary
    // format from its header rather than trusting the file name/extension — older
    // installs saved Windows builds under a `-linux-x64` name, and routing on the
    // real platform fixes those too.
    #[cfg(target_os = "linux")]
    {
        let is_windows = crate::binfmt::detect_executable(&exe_path).platform == Some("Windows");
        if is_windows {
            if crate::config::get_use_proton() {
                return play_with_proton(game, &dir, &exe_path, &args);
            }
            return Err(
                "This build is for Windows. Enable Proton support in Settings to run it on Linux."
                    .to_string(),
            );
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
    // Prevent the game from inheriting the launcher's stdio handles.  In debug
    // builds the launcher is a console app whose handles are connected to the
    // terminal; a game that writes to stderr before its window is ready would
    // block once the pipe buffer fills.  In release builds the handles are NULL
    // and any write attempt would crash the child.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    match cmd.spawn() {
        Ok(child) => {
            eprintln!("[games] Process spawned successfully");
            Ok(child)
        }
        Err(e) => {
            eprintln!("[games] Failed to spawn process: {}", e);
            Err(format!("Failed to launch: {}", e))
        }
    }
}

/// Launch a Windows `.exe` through Proton on Linux.
///
/// Picks the user-selected Proton installation (falling back to the newest
/// detected one), creates a per-game Wine prefix directory on first run, and
/// spawns `<proton>/proton run <exe> [args]` with the required Proton
/// environment variables.
#[cfg(target_os = "linux")]
fn play_with_proton(
    game: &str,
    build_dir: &std::path::Path,
    exe_path: &std::path::Path,
    args: &[String],
) -> Result<std::process::Child, String> {
    use crate::proton;

    let installations = proton::list_installations();
    if installations.is_empty() {
        return Err(
            "No Proton installation found. \
             Install Proton through Steam (or a custom build like GE-Proton), \
             or turn off \"Use Proton\" in Settings."
                .to_string(),
        );
    }

    // Honour the user's explicit selection when it's still present on disk;
    // otherwise default to the newest detected installation (index 0).
    let selected_path = crate::config::get_selected_proton();
    let proton_install = if !selected_path.is_empty()
        && installations.iter().any(|p| p.path == selected_path)
    {
        installations
            .into_iter()
            .find(|p| p.path == selected_path)
            .unwrap()
    } else {
        installations.into_iter().next().unwrap()
    };

    let proton_script = std::path::PathBuf::from(&proton_install.path).join("proton");

    // Per-game Wine prefix: <games>/<game>/prefix/
    // Proton populates the `pfx` subdirectory inside it on first launch.
    let compat_data = game_root(game).join("prefix");
    if let Err(e) = std::fs::create_dir_all(&compat_data) {
        eprintln!(
            "[games] Warning: could not create Proton prefix dir {}: {}",
            compat_data.display(),
            e
        );
    }

    let steam_compat_client = proton::steam_client_install_path().unwrap_or_default();

    // `user_data_root` and `log_file` are launcher-managed cvars (see below). The
    // game persists modified cvars to `<build>/<game>.toml` on "save config", and
    // re-applies that file at startup *after* command-line flags — so leftover
    // copies would override our values. Worse, Wine paths written into the toml
    // by an earlier launcher build used backslashes, which are invalid TOML
    // escapes and make the whole file fail to parse (so none of the player's
    // settings load). Strip both keys before launch: it un-corrupts the existing
    // file and lets each run get its own values.
    let config_path = build_dir.join(format!("{}.toml", game));
    strip_config_keys(&config_path, &["user_data_root", "log_file"]);

    let mut proton_args = args.to_vec();

    // The Windows build runs as a Windows process under Wine, so the Rex runtime
    // resolves its user/save folder to the prefix's Documents directory instead
    // of `~/.local/share/<game>`. Force `user_data_root` to the real Linux save
    // dir (the same path the save manager reads). The value is used verbatim by
    // the SDK — no game-name suffix — and is a Wine path (`Z:` maps to the host
    // root). Use forward slashes so it stays valid if the game writes it back to
    // its TOML config.
    if let Some(user_dir) = crate::paths::rex_user_folder().map(|p| p.join(game)) {
        let _ = std::fs::create_dir_all(&user_dir);
        proton_args.push(format!("--user_data_root=Z:{}", user_dir.to_string_lossy()));
    } else {
        eprintln!("[games] Warning: could not resolve user data folder; saves may land in the Wine prefix");
    }

    // Logs: natively the runtime writes `<exe_dir>/logs/<game>_NNN.log`, but under
    // Wine its default sequential logger can't open that exe-relative path, so set
    // `log_file` explicitly to the next sequential name in the build dir's logs/
    // folder (Wine path, forward slashes), matching native naming.
    let logs_dir = build_dir.join("logs");
    let _ = std::fs::create_dir_all(&logs_dir);
    let log_path = logs_dir.join(next_log_name(&logs_dir, game));
    proton_args.push(format!("--log_file=Z:{}", log_path.to_string_lossy()));

    eprintln!(
        "[games] Launching via Proton ({}): {} {:?}",
        proton_install.name,
        exe_path.display(),
        proton_args
    );

    let mut cmd = std::process::Command::new(&proton_script);
    cmd.arg("run");
    cmd.arg(exe_path);
    cmd.args(&proton_args);
    cmd.current_dir(build_dir);
    cmd.env("STEAM_COMPAT_DATA_PATH", &compat_data);
    cmd.env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &steam_compat_client);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    match cmd.spawn() {
        Ok(child) => {
            eprintln!("[games] Proton process spawned successfully");
            Ok(child)
        }
        Err(e) => {
            eprintln!("[games] Failed to spawn Proton process: {}", e);
            Err(format!("Failed to launch via Proton: {}", e))
        }
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Ensure `<build_dir>/assets` is a junction (Windows) or symlink (Unix)
/// pointing at `<game_root>/assets`.
///
/// This is needed for older builds that do not support `--game_data_root`: they
/// look for assets relative to their own directory, so linking the shared asset
/// folder in makes them work without copying data or passing extra flags.
///
/// Does nothing if:
///  - the shared assets directory does not exist yet (game not ISO-installed), or
///  - `<build_dir>/assets` already exists and already points at the right target.
fn ensure_assets_link(game: &str, build_dir: &Path) {
    let target = game_root(game).join("assets");
    if !target.exists() {
        return;
    }

    let link = build_dir.join("assets");

    // Already correct — nothing to do.
    #[cfg(windows)]
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT (0x400) covers both junctions and symlinks.
        if meta.file_attributes() & 0x400 != 0 {
            return;
        }
        // It exists but is a real directory — leave it alone.
        return;
    }
    #[cfg(unix)]
    if link.exists() {
        return;
    }

    #[cfg(windows)]
    {
        // Use `junction` crate if available; fall back to mklink /J via cmd.
        if let Err(e) = junction::create(&target, &link) {
            eprintln!(
                "[games] Failed to create junction {} -> {}: {}",
                link.display(), target.display(), e
            );
        } else {
            eprintln!(
                "[games] Created assets junction: {} -> {}",
                link.display(), target.display()
            );
        }
    }
    #[cfg(unix)]
    {
        if let Err(e) = std::os::unix::fs::symlink(&target, &link) {
            eprintln!(
                "[games] Failed to create symlink {} -> {}: {}",
                link.display(), target.display(), e
            );
        } else {
            eprintln!(
                "[games] Created assets symlink: {} -> {}",
                link.display(), target.display()
            );
        }
    }
}

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

/// Next sequential log filename for a build's `logs/` dir, matching the Rex
/// runtime's native `<game>_NNN.log` naming (`logs/<game>_001.log`, `_002`, …).
#[cfg(target_os = "linux")]
fn next_log_name(logs_dir: &Path, game: &str) -> String {
    let prefix = format!("{}_", game);
    let mut max_seq = 0u32;
    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(seq) = name.strip_suffix(".log").and_then(|s| s.strip_prefix(&prefix)) {
                if let Ok(n) = seq.parse::<u32>() {
                    max_seq = max_seq.max(n);
                }
            }
        }
    }
    format!("{}_{:03}.log", game, max_seq + 1)
}

/// Remove the given `key = value` lines from a `<game>.toml` cvar config, leaving
/// other settings untouched. Used to drop launcher-managed cvars the game would
/// otherwise persist and re-apply (and which, as Wine paths, can corrupt the TOML).
#[cfg(target_os = "linux")]
fn strip_config_keys(config_path: &Path, keys: &[&str]) {
    let Ok(contents) = std::fs::read_to_string(config_path) else { return };
    let mut out = String::new();
    for line in contents.lines() {
        let key = line.split('=').next().unwrap_or("").trim();
        if !keys.contains(&key) {
            out.push_str(line);
            out.push('\n');
        }
    }
    let _ = std::fs::write(config_path, out);
}

/// Resolve a build's installed executable: the sidecar-recorded `exePath` if it
/// exists on disk, otherwise the host-canonical name (covers older installs that
/// recorded no `exePath`).
fn installed_exe_path(dir: &Path, game: &str) -> PathBuf {
    if let Some(exe) = sidecar_exe_path(dir) {
        let p = dir.join(&exe);
        if p.exists() {
            return p;
        }
    }
    dir.join(canonical_exe_name(game))
}

/// Write `.installed.json`. `exe_info` carries the platform/arch detected by
/// [`crate::binfmt::detect_executable`] from the installed executable (if any
/// was found/scanned) — `None` fields are omitted, which the frontend treats
/// as "unknown" and never gates on.
fn write_sidecar(path: &Path, version: &str, asset: &str, exe_path: Option<&str>, exe_info: crate::binfmt::ExeInfo) {
    let exe_field = exe_path.map(|e| format!(r#","exePath":"{}""#, e.replace('\\', "\\\\").replace('"', "\\\""))).unwrap_or_default();
    let platform_field = exe_info.platform.map(|p| format!(r#","platform":"{}""#, p)).unwrap_or_default();
    let arch_field = exe_info.arch.map(|a| format!(r#","arch":"{}""#, a)).unwrap_or_default();
    let json = format!(
        r#"{{"version":"{}","asset":"{}"{}{}{}}}"#,
        version.replace('"', "\\\""),
        asset.replace('"', "\\\""),
        exe_field,
        platform_field,
        arch_field,
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
    // Match either platform's packaging regardless of host: a Windows build
    // (`{game}.exe`) is valid on Linux too since it's routed through Proton at
    // launch.  Exact name beats a generic fallback.
    let game_lower = game.to_lowercase();
    let preferred = [format!("{}.exe", game_lower), game_lower];

    let mut exe_fallback = String::new();  // any `.exe` — a Windows build
    let mut elf_fallback = String::new();  // any extensionless executable — a Linux build

    let Ok(entries) = std::fs::read_dir(dir) else { return String::new() };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let fname = entry.file_name().to_string_lossy().into_owned();
        let lower = fname.to_lowercase();
        if preferred.contains(&lower) {
            return fname;
        }
        if exe_fallback.is_empty() && lower.ends_with(".exe") {
            exe_fallback = fname.clone();
        }
        if elf_fallback.is_empty() && !fname.contains('.') {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if entry.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) {
                    elf_fallback = fname.clone();
                }
            }
            #[cfg(not(unix))]
            { elf_fallback = fname.clone(); }
        }
    }

    // Prefer the host's native binary when present, otherwise the other
    // platform's (Linux can still run a Windows build via Proton).
    #[cfg(unix)]
    { if !elf_fallback.is_empty() { elf_fallback } else { exe_fallback } }
    #[cfg(not(unix))]
    { if !exe_fallback.is_empty() { exe_fallback } else { elf_fallback } }
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
