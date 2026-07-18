use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{bridge::AppState, config, download, games};

// ── Periodic update check ─────────────────────────────────────────────────────

/// How often to re-check for a new launcher release.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The releases API endpoint to query for the latest launcher version.
///
/// Resolved at *runtime* first (the `GOOPIE_RELEASES_API` env var), falling back
/// to the value baked in at compile time. Two reasons:
///   - the compile-time value is now `option_env!` (not `env!`), so a plain
///     `cargo build`/`cargo test` no longer fails when the secret is absent — the
///     unit tests and the e2e debug build compile without it;
///   - the e2e test harness can point the launcher at a local mock release server.
///
/// The env override is honored only in debug builds (`cfg!(debug_assertions)`),
/// so shipped release binaries always use the URL baked in at compile time and
/// can't be redirected by a stray environment variable.
fn releases_api_url() -> Option<String> {
    if cfg!(debug_assertions) {
        if let Ok(url) = std::env::var("GOOPIE_RELEASES_API") {
            if !url.is_empty() {
                return Some(url);
            }
        }
    }
    option_env!("GOOPIE_RELEASES_API").map(|s| s.to_string())
}

/// Spawns a background thread that checks `GOOPIE_RELEASES_API` for a newer
/// launcher release every hour, caching the result in `AppState` so the
/// (synchronous) `CheckForLauncherUpdate` bridge call never blocks on a
/// GitHub API request (mirrors `offline_site::spawn_connectivity_monitor`).
///
/// The timestamp of the last check is persisted (`config::get/set_last_update_check`)
/// so that the interval is enforced *across restarts* too — opening the
/// launcher repeatedly within an hour reuses the existing cached result
/// instead of firing a fresh request each time.
///
/// By default this never applies an update on its own — it only refreshes the
/// cache that drives the "update available" icon, and the user explicitly opts
/// in via `SelfUpdateLauncher`. The one exception is the hidden `AutoApplyUpdate`
/// setting: when enabled, each live check auto-applies via `maybe_auto_apply`.
pub fn spawn_update_monitor(state: Arc<AppState>) {
    // `LastUpdateCheck`/`LastKnownReleaseTag` persist across restarts, but
    // `AppState`'s cache doesn't — without this, a restart inside the throttle
    // window below would leave `update_checked` false (and the "update
    // available" prompt hidden) until the next live check actually runs, up to
    // an hour later. Re-derive `has_update` against *this* binary's version
    // rather than trusting a possibly-stale cached verdict (e.g. right after a
    // self-update, the cached tag may now match `current`).
    let cached_tag = config::get_last_known_release_tag();
    if !cached_tag.is_empty() {
        apply_remote_tag(&state, cached_tag);
    }

    std::thread::spawn(move || loop {
        let elapsed = unix_now().saturating_sub(config::get_last_update_check());
        let interval_secs = UPDATE_CHECK_INTERVAL.as_secs();
        if elapsed < interval_secs {
            std::thread::sleep(Duration::from_secs(interval_secs - elapsed));
        }
        check_for_update(&state);
        std::thread::sleep(UPDATE_CHECK_INTERVAL);
    });
}

/// Compare `remote_tag` against this binary's version and store the verdict in
/// `AppState` (and, for `check_for_update`'s callers, persist the tag so a
/// restart within the throttle window can reuse it — see `spawn_update_monitor`).
fn apply_remote_tag(state: &Arc<AppState>, remote_tag: String) {
    let current = env!("CARGO_PKG_VERSION");
    let remote_clean = remote_tag.trim_start_matches('v');
    let has_update = version_is_newer(current, remote_clean);

    state.update_available.store(has_update, Ordering::Relaxed);
    *state.latest_version.lock().unwrap() = remote_tag;
    state.update_checked.store(true, Ordering::Relaxed);
}

/// Returns `true` when `remote` is strictly newer than `current` (semver).
/// Falls back to a simple string inequality if either side isn't valid semver.
fn version_is_newer(current: &str, remote: &str) -> bool {
    if remote.is_empty() {
        return false;
    }
    let parse = |s: &str| -> Option<(u64, u64, u64)> {
        let mut parts = s.splitn(3, '.');
        Some((
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ))
    };
    match (parse(current), parse(remote)) {
        (Some(c), Some(r)) => r > c,
        _ => remote != current,
    }
}

/// Fetch the latest release tag and refresh `AppState`'s update cache. Leaves
/// the previous cached values untouched on a fetch error (transient network
/// hiccups shouldn't flip "update available" back off) — and, importantly,
/// leaves `LastUpdateCheck` untouched too, so a failed check doesn't throttle
/// the *next* attempt for a full interval (see `spawn_update_monitor`).
fn check_for_update(state: &Arc<AppState>) {
    let Some(api_url) = releases_api_url() else {
        eprintln!("[launcher] no releases API configured; skipping update check");
        return;
    };
    let body = match download::fetch_to_string(&api_url) {
        Ok(b) => b,
        Err(e) => { eprintln!("[launcher] update check failed: {e:?}"); return; }
    };

    // Only stamp the throttle timestamp on a *successful* check — see doc comment.
    config::set_last_update_check(unix_now());

    let remote_tag = games::json_extract_str(&body, "tag_name");
    config::set_last_known_release_tag(&remote_tag);
    apply_remote_tag(state, remote_tag);

    // With the hidden `AutoApplyUpdate` setting on, apply the update we just
    // found without waiting for the user — see `maybe_auto_apply`. Off by
    // default, so the normal flow stays "explicit user action only".
    maybe_auto_apply(state);
}

/// The gating decision for an unattended auto-apply, factored out so it can be
/// unit-tested without touching the registry/INI or the network. An update is
/// applied on its own only when **all three** hold:
///   - the hidden `AutoApplyUpdate` flag is on,
///   - a newer release was actually detected, and
///   - **no game is currently running.**
///
/// The last condition is the important one: applying an update calls
/// `apply_update`, which replaces the binary and `exit(0)`s the launcher. Doing
/// that mid-session would orphan the game the player launched and tear down the
/// launcher↔game bridge out from under them. So when a game is running we defer,
/// even with the flag on — the update is re-attempted the moment the game closes
/// (see `auto_apply_after_game_exit`, called from the bridge's game monitor).
fn should_auto_apply(flag_enabled: bool, update_available: bool, game_running: bool) -> bool {
    flag_enabled && update_available && !game_running
}

/// Whether a game launched by the launcher is currently being tracked as running.
fn game_is_running(state: &AppState) -> bool {
    state.running_game.lock().unwrap().is_some()
}

/// If the hidden `AutoApplyUpdate` setting is enabled *and* a newer release was
/// detected (`AppState::update_available`) *and* no game is running, download and
/// apply it immediately — no UI interaction. A no-op when the setting is off (the
/// default), preserving the explicit-`SelfUpdateLauncher`-only behavior; also a
/// no-op (deferred) while a game is running, so the player's session is never
/// killed mid-game by an unattended update.
fn maybe_auto_apply(state: &Arc<AppState>) {
    let flag = config::get_auto_apply_update();
    let available = state.update_available.load(Ordering::Relaxed);
    let game_running = game_is_running(state);

    if flag && available && game_running {
        eprintln!("[launcher] auto-update deferred until the running game closes");
        return;
    }
    if should_auto_apply(flag, available, game_running) {
        self_update(Arc::clone(state));
    }
}

/// Re-evaluate a deferred auto-apply now that the tracked game has exited (see
/// the game-running guard in `maybe_auto_apply`). Called by the bridge's game
/// monitor as soon as a game closes; a no-op unless `AutoApplyUpdate` is on and
/// a newer release is still pending from an earlier check.
pub(crate) fn auto_apply_after_game_exit(state: &Arc<AppState>) {
    maybe_auto_apply(state);
}

/// Headless update entry point used by the `--self-update-check` CLI flag (see
/// `lib::run`). Runs a single update check — which, with `AutoApplyUpdate` on,
/// downloads and applies a newer release and never returns (`apply_update` exits
/// the process). When nothing is applied it prints a machine-readable
/// `SELFUPDATE: <outcome>` line and exits with a distinct code so the end-to-end
/// test harness can assert on the result without a GUI:
///   - `10` `noupdate`  — checked, already up to date
///   - `11` `disabled`  — newer release available but `AutoApplyUpdate` is off
///   - `12` `error`     — the check failed, or an apply was attempted but failed
///
/// (The `applied` success case exits `0` from inside `apply_update`; the harness
/// confirms it by the replaced binary on disk, not this code path.)
pub fn run_self_update_check() -> ! {
    let state = Arc::new(AppState::new());
    check_for_update(&state);

    // Still here ⇒ no update was applied; classify why for the harness.
    let (label, code) = if !state.update_checked.load(Ordering::Relaxed) {
        ("error", 12)
    } else if !state.update_available.load(Ordering::Relaxed) {
        ("noupdate", 10)
    } else if !config::get_auto_apply_update() {
        ("disabled", 11)
    } else {
        ("error", 12) // available + enabled but we returned ⇒ apply failed
    };
    println!("SELFUPDATE: {label}");
    std::process::exit(code);
}

pub fn self_update(state: Arc<AppState>) {
    let Some(api_url) = releases_api_url() else {
        eprintln!("[launcher] no releases API configured; cannot self-update");
        return;
    };

    let body = match download::fetch_to_string(&api_url) {
        Ok(b) => b,
        Err(e) => { eprintln!("[launcher] fetch releases failed: {e:?}"); return; }
    };

    let url = match find_asset_url(&body) {
        Some(u) => u,
        None => { eprintln!("[launcher] no matching asset found in release"); return; }
    };

    // The version we're updating *to*, so the relaunch step can refresh the
    // Windows "Programs and Features" version (see `apply_update`). Empty if the
    // release JSON lacks a tag — apply_update then leaves the registry alone.
    let new_version = games::json_extract_str(&body, "tag_name")
        .trim_start_matches('v')
        .to_string();

    let staging = staging_path();

    let state_ref = Arc::clone(&state);
    let progress_cb: download::ProgressCallback = Box::new(move |dl, total| {
        state_ref.set_launcher_update_progress(dl, total);
    });

    if let Err(e) = download::download_file(&url, &staging.to_string_lossy(), Some(&progress_cb)) {
        eprintln!("[launcher] download failed: {e:?}");
        state.finish_launcher_update();
        return;
    }
    state.finish_launcher_update();

    if let Err(e) = apply_update(&staging, &new_version) {
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

/// This host's CPU architecture in the release-asset naming convention
/// (`_build.yml`'s `inputs.arch`: `"x86_64"` / `"aarch64"`), derived from
/// `mods::host_arch()` (which reports the mod-manifest convention instead:
/// `"x64"` / `"arm64"`) so there's a single source of truth for arch
/// detection across the launcher.
fn host_release_arch() -> &'static str {
    match crate::mods::host_arch() {
        "x64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    }
}

#[cfg(windows)]
fn is_target_asset(name: &str) -> bool {
    // Our portable exe, e.g. "Goopie-Launcher-windows-x86_64.exe". Match is
    // version-agnostic (release assets are versionless; CI ones embed a short
    // SHA) and arch-specific (both x86_64 and aarch64 builds are published,
    // so a plain "-windows-" substring would ambiguously match either).
    // Exclude the NSIS installer which ends with "-setup.exe".
    let platform_arch = format!("-windows-{}", host_release_arch());
    name.contains(&platform_arch) && name.ends_with(".exe") && !name.ends_with("-setup.exe")
}

#[cfg(not(windows))]
fn is_target_asset(name: &str) -> bool {
    // Both a `.AppImage` and a plain portable binary are published for Linux,
    // for both x86_64 and aarch64 — match whichever kind and architecture
    // we're currently running as, so a portable install doesn't get swapped
    // for an AppImage (or vice versa), and one arch never grabs the other's
    // binary (see the AArch64-on-x86_64 bug this guards against).
    let platform_arch = format!("-linux-{}", host_release_arch());
    if !name.contains(&platform_arch) {
        return false;
    }
    let is_appimage_asset = name.ends_with(".AppImage");
    if running_as_appimage() {
        is_appimage_asset
    } else {
        !is_appimage_asset
    }
}

/// Whether we're currently running from a mounted `.AppImage` rather than a
/// plain portable binary. See `replaceable_exe_path` for why `$APPIMAGE` is
/// the reliable signal here.
#[cfg(not(windows))]
fn running_as_appimage() -> bool {
    std::env::var_os("APPIMAGE").is_some()
}

// ── Replaceable executable path ───────────────────────────────────────────────

/// The on-disk file to replace on self-update.
///
/// `std::env::current_exe()` is *not* what we want when running as an
/// `.AppImage`: the AppImage runtime mounts the bundle (read-only squashfs,
/// typically under `/tmp/.mount_*`) and execs the binary from inside that
/// mount, so `current_exe()` resolves to a read-only path — staging a download
/// next to it fails with `ReadOnlyFilesystem`. AppImage runtimes export
/// `$APPIMAGE` with the real, writable path to the `.AppImage` file itself, so
/// prefer that; fall back to `current_exe()` for a plain portable binary (e.g.
/// during development).
#[cfg(not(windows))]
fn replaceable_exe_path() -> std::io::Result<std::path::PathBuf> {
    if let Ok(appimage) = std::env::var("APPIMAGE") {
        return Ok(std::path::PathBuf::from(appimage));
    }
    std::env::current_exe()
}

// ── Staging path ──────────────────────────────────────────────────────────────

#[cfg(windows)]
fn staging_path() -> std::path::PathBuf {
    std::env::temp_dir().join("goopie-launcher-update.exe")
}

#[cfg(not(windows))]
fn staging_path() -> std::path::PathBuf {
    // Same directory as the replaceable file so the final rename stays on the
    // same filesystem (see `replaceable_exe_path` for why it's not current_exe).
    // The staged file is renamed onto the real path in `apply_update` regardless
    // of extension, so a fixed name works whether we downloaded an AppImage or
    // a plain portable binary.
    let current = replaceable_exe_path().unwrap_or_default();
    current.parent().unwrap_or(std::path::Path::new("."))
        .join("goopie-launcher-update")
}

// ── Previous-attempt result check ─────────────────────────────────────────────

/// Checks for a leftover result marker from a previous self-update attempt
/// (written by the elevated relaunch script spawned from `apply_update`) and
/// surfaces a native error dialog if it failed, instead of leaving the user
/// silently stuck on the old version with no explanation. Call once early on
/// startup; no-op if the marker is absent (the common case) or reports "OK".
#[cfg(windows)]
pub fn check_previous_update_result() {
    let path = std::env::temp_dir().join("goopie-update-result.txt");
    let Ok(contents) = std::fs::read_to_string(&path) else { return };
    let _ = std::fs::remove_file(&path);

    if let Some(msg) = contents.trim().strip_prefix("ERROR: ") {
        eprintln!("[launcher] previous self-update failed: {msg}");
        rfd::MessageDialog::new()
            .set_title("Goopie Launcher Update Failed")
            .set_description(format!(
                "The launcher couldn't finish updating and is still running the previous version.\n\n{msg}"
            ))
            .set_level(rfd::MessageLevel::Error)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
}

// ── Platform-specific apply ───────────────────────────────────────────────────

#[cfg(windows)]
fn apply_update(staging: &std::path::Path, new_version: &str) -> std::io::Result<()> {
    let current_exe = std::env::current_exe()?;

    let ps_quote = |p: &std::path::Path| p.display().to_string().replace('\'', "''");

    // Forward args (e.g. --local) to the relaunched process.
    let args_ps = std::env::args()
        .skip(1)
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let start_args = if args_ps.is_empty() {
        String::new()
    } else {
        format!(" -ArgumentList @({})", args_ps)
    };

    let reg_block = if new_version.is_empty() {
        String::new()
    } else {
        format!(
            "try {{ \
               $dir=(Split-Path -Parent '{dst}').TrimEnd('\\'); \
               $roots=@('HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',\
'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',\
'HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall'); \
               foreach ($r in $roots) {{ if (Test-Path $r) {{ foreach ($k in Get-ChildItem $r) {{ \
                 $loc=(Get-ItemProperty $k.PSPath).InstallLocation; \
                 if ($loc -and ($loc.TrimEnd('\\') -ieq $dir)) {{ \
                   Set-ItemProperty -Path $k.PSPath -Name DisplayVersion -Value '{ver}' }} \
               }} }} }} \
             }} catch {{}}\n",
            dst = ps_quote(&current_exe),
            ver = new_version.replace('\'', "''"),
        )
    };

    // `result_file` is how the elevated (or non-elevated) copy step reports
    // back to us: this process exits immediately after spawning the relaunch
    // script below, so we can't observe success/failure directly. Instead the
    // script writes "OK" or "ERROR: <message>" here, and the *next* launcher
    // start (`check_previous_update_result`) reads it and surfaces a dialog
    // instead of silently relaunching whatever binary ended up on disk.
    let result_file = std::env::temp_dir().join("goopie-update-result.txt");

    let elevate_script = std::env::temp_dir().join("goopie-elevate.ps1");
    std::fs::write(&elevate_script, format!(
        "$ErrorActionPreference='Stop'\n\
         $result='{result}'\n\
         try {{\n\
         \x20\x20$copied=$false\n\
         \x20\x20for ($i=0; $i -lt 5; $i++) {{\n\
         \x20\x20\x20\x20try {{ Copy-Item -Force '{src}' '{dst}'; $copied=$true; break }}\n\
         \x20\x20\x20\x20catch {{ Start-Sleep -Milliseconds 700 }}\n\
         \x20\x20}}\n\
         \x20\x20if (-not $copied) {{ Copy-Item -Force '{src}' '{dst}' }}\n\
         {reg}\
         \x20\x20Remove-Item -Force '{src}' -ErrorAction SilentlyContinue\n\
         \x20\x20Set-Content -Path $result -Value 'OK' -Encoding UTF8\n\
         }} catch {{\n\
         \x20\x20Set-Content -Path $result -Value (\"ERROR: \" + $_.Exception.Message) -Encoding UTF8\n\
         \x20\x20exit 1\n\
         }}\n",
        src = ps_quote(staging),
        dst = ps_quote(&current_exe),
        reg = reg_block,
        result = ps_quote(&result_file),
    ))?;

    let ps = format!(
        "$dst='{dst}'; $elev='{elev}'; $result='{result}'; \
         Start-Sleep -Milliseconds 1500; \
         Remove-Item -Force $result -ErrorAction SilentlyContinue; \
         $ok=$false; \
         try {{ $p = Start-Process powershell -Wait -WindowStyle Hidden -PassThru \
                -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-WindowStyle','Hidden','-File',$elev); \
                if ($p.ExitCode -eq 0) {{ $ok=$true }} }} catch {{}}; \
         if (-not $ok) {{ \
             try {{ $p = Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -PassThru \
                 -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-WindowStyle','Hidden','-File',$elev); \
                 if ($p.ExitCode -eq 0) {{ $ok=$true }} \
             }} catch {{ \
                 Set-Content -Path $result -Value (\"ERROR: update was not applied - elevation failed or was cancelled (\" + $_.Exception.Message + \")\") -Encoding UTF8 \
             }} \
         }}; \
         if ((-not $ok) -and -not (Test-Path $result)) {{ \
             Set-Content -Path $result -Value 'ERROR: update was not applied - the elevated update step exited unexpectedly' -Encoding UTF8 \
         }}; \
         Remove-Item -Force $elev -EA 0; \
         Start-Process -FilePath $dst{args}",
        dst = ps_quote(&current_exe),
        elev = ps_quote(&elevate_script),
        result = ps_quote(&result_file),
        args = start_args,
    );

    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;

    std::process::exit(0);
}

#[cfg(not(windows))]
fn apply_update(staging: &std::path::Path, _new_version: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let current_exe = replaceable_exe_path()?;
    let tmp = current_exe.with_extension("new");

    std::fs::copy(staging, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &current_exe)?;
    std::fs::remove_file(staging).ok();

    // Forward args (e.g. --local) to the relaunched process.
    let args: Vec<_> = std::env::args().skip(1).collect();
    std::process::Command::new(&current_exe).args(&args).spawn()?;
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression test for the bug where `is_target_asset` ignored CPU
    // architecture entirely: with both x86_64 and aarch64 Linux/Windows
    // assets published, an x86_64 host could match and download the
    // aarch64 asset (or vice versa) since both shared the "-linux-"/
    // "-windows-" substring, then fail to exec it (ENOEXEC) after
    // self-update replaced the binary — silently hanging the relaunch.
    #[test]
    fn is_target_asset_matches_only_host_architecture() {
        let host = host_release_arch();
        let other = if host == "x86_64" { "aarch64" } else { "x86_64" };

        #[cfg(not(windows))]
        {
            let matching = format!("Goopie-Launcher-linux-{host}.AppImage");
            let mismatched = format!("Goopie-Launcher-linux-{other}.AppImage");
            assert_eq!(is_target_asset(&matching), running_as_appimage());
            assert!(!is_target_asset(&mismatched));
        }

        #[cfg(windows)]
        {
            let matching = format!("Goopie-Launcher-windows-{host}.exe");
            let mismatched = format!("Goopie-Launcher-windows-{other}.exe");
            assert!(is_target_asset(&matching));
            assert!(!is_target_asset(&mismatched));
            assert!(!is_target_asset(&format!("Goopie-Launcher-windows-{host}-setup.exe")));
        }
    }

    #[test]
    fn auto_apply_requires_flag_update_and_no_running_game() {
        // The only combination that applies an update unattended is:
        // flag on + a newer release available + no game running.
        assert!(should_auto_apply(true, true, false));

        // Missing either precondition blocks it.
        assert!(!should_auto_apply(false, true, false)); // flag off
        assert!(!should_auto_apply(true, false, false)); // nothing newer

        // ...and a running game blocks it even with the flag on and an update
        // pending — this is the "wait for the game to close before auto-updating"
        // guarantee: applying mid-session would exit the launcher and orphan the
        // player's game.
        assert!(!should_auto_apply(true, true, true));
        assert!(!should_auto_apply(false, false, true));
    }

    // Verifies the wiring `maybe_auto_apply` relies on: an `AppState` reports a
    // game as running exactly while one is tracked, so the guard above actually
    // fires when the player has a game open. Uses a real throwaway child process
    // (`sleep`) to stand in for the game; not run on Windows (no `sleep` binary).
    #[test]
    fn version_is_newer_semver() {
        assert!(version_is_newer("1.3.0", "1.3.1"));
        assert!(version_is_newer("1.3.1", "1.4.0"));
        assert!(version_is_newer("1.3.1", "2.0.0"));
        assert!(version_is_newer("1.3.1", "9999.0.0"));
        assert!(!version_is_newer("1.3.1", "1.3.1"));
        assert!(!version_is_newer("1.3.1", "1.3.0"));
        assert!(!version_is_newer("1.3.1", "1.2.5"));
        assert!(!version_is_newer("2.0.0", "1.99.99"));
        assert!(!version_is_newer("1.3.1", "0.0.1"));
        assert!(!version_is_newer("1.3.1", ""));
    }

    #[cfg(not(windows))]
    #[test]
    fn game_is_running_reflects_tracked_session() {
        use crate::bridge::RunningGame;
        use std::time::Instant;

        let state = AppState::new();
        assert!(!game_is_running(&state), "nothing tracked ⇒ no game running");

        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn throwaway child");
        *state.running_game.lock().unwrap() = Some(RunningGame {
            session_id: 1,
            game: "test".into(),
            build: "test".into(),
            child,
            started_at: Instant::now(),
        });
        assert!(game_is_running(&state), "tracked session ⇒ game running");

        // Don't leave the throwaway child behind.
        let tracked = state.running_game.lock().unwrap().take();
        if let Some(mut running) = tracked {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
}
