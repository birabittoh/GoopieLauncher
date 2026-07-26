mod auth;
mod binfmt;
mod bridge;
mod launcher;
mod cloud_saves;
mod config;
mod archive;
mod discord;
mod drive;
mod download;
mod games;
mod extract;
mod mods;
mod offline_site;
mod paths;
mod platform;
mod proton;
mod achievements;
mod leaderboards;
mod playtime;
mod saves;
mod shortcuts;
mod vehicles;

use std::sync::Arc;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
    DragDropEvent, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

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

/// Whether the launcher was invoked with `--offline`: forces offline mode for
/// this run only (skips the `goopie.xyz` connectivity probe entirely). Unlike
/// the persisted offline-mode preference, this is not written to config — it
/// exists so the launcher can be started with a guaranteed-instant, no-network
/// startup even when the site is down, without touching the user's saved
/// preference.
fn offline_flag_requested() -> bool {
    std::env::args().skip(1).any(|a| a == "--offline")
}

/// Resolve the URL to load in the main window at startup.
///
/// Priority:
///   1. `--url <URL>` / `--local` / `GOOPIE_URL` — dev overrides, bypass offline logic.
///   2. `--offline` CLI flag, or the user's persisted offline-mode preference
///      (`config::get_offline_mode_preference`): honor it unconditionally — no probe,
///      no requests to `goopie.xyz` at all.
///   3. Otherwise (the user prefers online): probe `https://goopie.xyz` on every launch
///      and fall back to the embedded offline bundle if it's unreachable. This fallback
///      is purely transient — it's never written back to the persisted preference, so
///      the launcher returns to online mode automatically once connectivity is restored.
fn resolve_url() -> String {
    if let Some(url) = url_override() {
        return url;
    }
    let offline = offline_flag_requested()
        || config::get_offline_mode_preference()
        || !offline_site::probe_connectivity();
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
    parse_play_arg(&args)
}

/// Extract `--play <game>` from an argv slice.
fn parse_play_arg(argv: &[String]) -> Option<String> {
    parse_flag_arg(argv, "--play")
}

/// Extract the value following `flag` in an argv slice (1-indexed, skipping
/// argv[0] — the exe path — same as `parse_play_arg`).
fn parse_flag_arg(argv: &[String], flag: &str) -> Option<String> {
    let mut i = 1;
    while i < argv.len() {
        if argv[i] == flag {
            return argv.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

/// Blocks until the process identified by `pid` exits (or is already gone).
/// Used to notice when Steam kills the relay process it launched (via its
/// own "Close" button) out from under an in-progress game session — see
/// `relay_play_to_running_instance`.
#[cfg(windows)]
fn wait_for_pid_exit(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE};

    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return; // already exited, or we couldn't open it — either way, don't block
    }
    unsafe {
        WaitForSingleObject(handle, INFINITE);
        CloseHandle(handle);
    }
}

/// If this is a `--play <game>` launch and another instance is already
/// running, hand it off to that instance ourselves and block until the game
/// session ends, then return `true` (caller should exit immediately without
/// building a Tauri app of its own).
///
/// This deliberately bypasses `tauri_plugin_single_instance`'s own detection:
/// that plugin forwards the launch args via the same Win32 `WM_COPYDATA`
/// mechanism used below, but then calls `std::process::exit(0)` on this
/// process *immediately* — fine for a Desktop/Start Menu shortcut (Explorer
/// doesn't care how long the process it launched lives), but Steam tracks a
/// non-Steam game's "running" state purely by whether the process *it*
/// launched is still alive. An instant exit makes Steam show "Play" again a
/// few seconds into every session even though the game is actually running
/// (in a *different*, pre-existing process). Waiting here instead — using
/// the per-game marker file `bridge::running_marker_path` maintained by
/// `launch_and_track`/`monitor_running_game`/`kill_running_game` — keeps
/// Steam's tracked process alive for the game's actual duration.
///
/// Returns `false` (proceed with normal startup, becoming the primary
/// instance) when this isn't a `--play` launch, or no other instance is
/// currently running — `tauri_plugin_single_instance`'s own detection still
/// covers those cases as before.
#[cfg(windows)]
fn relay_play_to_running_instance(argv: &[String]) -> bool {
    use std::time::Duration;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SendMessageW, WM_COPYDATA};

    let Some(game) = parse_play_arg(argv) else { return false };

    // Same identifier/suffix scheme tauri-plugin-single-instance uses
    // internally to name its message-only window (see its
    // `platform_impl/windows.rs`) — kept in sync manually since the plugin
    // exposes no public API for finding that window. `identifier` here must
    // match `tauri.conf.json`'s `identifier`; the "semver" feature (which
    // would add a version suffix) isn't enabled for our dependency.
    const IDENTIFIER: &str = "xyz.goopie.launcher";
    let class_name = encode_wide_null(&format!("{IDENTIFIER}-sic"));
    let window_name = encode_wide_null(&format!("{IDENTIFIER}-siw"));

    let hwnd: HWND = unsafe { FindWindowW(class_name.as_ptr(), window_name.as_ptr()) };
    if hwnd.is_null() {
        return false;
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd = cwd.to_str().unwrap_or_default();
    // Tack our own PID on as an extra pseudo-arg (parsed on the receiving end
    // by `parse_flag_arg`, separately from `parse_play_arg`'s real-argv-only
    // use) so the primary can notice if *this* process — the one Steam
    // actually tracks — disappears before the game does. That happens when
    // the user clicks Steam's own "Close" button: Steam just terminates the
    // process it launched, which doesn't otherwise tell the primary (running
    // the real game in a separate child process) to stop anything.
    let mut relay_argv = argv.to_vec();
    relay_argv.push("--relay-pid".to_string());
    relay_argv.push(std::process::id().to_string());
    let args = relay_argv.join("|");
    let data = format!("{cwd}|{args}\0");
    let bytes = data.as_bytes();
    let cds = COPYDATASTRUCT {
        dwData: 1542, // WMCOPYDATA_SINGLE_INSTANCE_DATA, matching the plugin
        cbData: bytes.len() as _,
        lpData: bytes.as_ptr() as _,
    };
    unsafe { SendMessageW(hwnd, WM_COPYDATA, 0, &cds as *const _ as _) };

    let marker = bridge::running_marker_path(&game);
    // Wait for the primary to actually start the game — bail out (and just
    // exit) rather than hang forever if it never does (e.g. mods failed
    // validation, the build wasn't installed, ...).
    let mut waited = Duration::ZERO;
    while !marker.exists() && waited < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(300));
        waited += Duration::from_millis(300);
    }
    // Now wait for it to finish — no timeout; a long play session is normal.
    while marker.exists() {
        std::thread::sleep(Duration::from_millis(750));
    }
    true
}

#[cfg(windows)]
fn encode_wide_null(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Un-hide/un-minimize and focus the main window — used to restore it from
/// the tray, whether it was hidden (`CloseRequested` collapse) or just
/// minimized by the OS.
pub(crate) fn show_main_window(window: &tauri::WebviewWindow) {
    // If the window was last positioned on a monitor that's no longer
    // connected (e.g. undocked from an external display while collapsed to
    // tray), `current_monitor` returns `None` — the window is technically
    // "shown" but sits at off-screen coordinates with nothing to render it.
    // Re-center it in that case so it reappears on the primary display.
    if let Ok(None) = window.current_monitor() {
        let _ = window.center();
    }
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

/// Restore the main window from the tray and jump the website to `hash`
/// (e.g. `"#/library"`) via the same hash router used by in-page navigation.
fn navigate_and_show(app: &tauri::AppHandle, hash: &str) {
    if let Some(window) = app.get_webview_window("main") {
        show_main_window(&window);
        let js = format!("window.location.hash = '{}'", hash);
        let _ = window.eval(&js);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // If another instance is already running and this is a `--play` shortcut
    // launch, hand off to it and wait out the game session ourselves instead
    // of building a whole second Tauri app just to have
    // tauri_plugin_single_instance immediately forward-and-exit it — see
    // `relay_play_to_running_instance` for why that matters for Steam.
    #[cfg(windows)]
    {
        let argv: Vec<String> = std::env::args().collect();
        if relay_play_to_running_instance(&argv) {
            return;
        }
    }

    // Headless self-update check (no GUI): runs the update flow and exits. Must
    // happen before any Tauri setup so it works without a display.
    if self_update_check_requested() {
        launcher::run_self_update_check();
    }

    // Surface a native error dialog if the previous launch's self-update
    // attempt failed (see `apply_update`/`check_previous_update_result`) —
    // otherwise a failed elevated copy silently relaunches the old binary
    // with no indication anything went wrong.
    #[cfg(windows)]
    launcher::check_previous_update_result();

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

    // Tracks whether the drag session currently over the window is a real OS
    // file drag (Explorer, etc.) as opposed to an in-page HTML5 drag — e.g.
    // dragging a game cover `<img>` around the launcher's own UI. WebView2
    // still raises the native Enter/Over/Leave sequence for those, but
    // `Enter`'s `paths` is empty since there's no real file involved. Only
    // `Enter` carries `paths`; `Over` doesn't, so this flag lets `Over`
    // (which fires continuously while the drag is in progress) know which
    // kind of session it belongs to. Reset on Leave/Drop.
    let drag_has_files = Arc::new(std::sync::atomic::AtomicBool::new(false));

    tauri::Builder::default()
        // Must be the first plugin registered (per tauri-plugin-single-instance
        // docs) so it can intercept a second launch before anything else runs.
        // A second launch just focuses/restores the existing window instead of
        // spawning a duplicate instance — cross-platform (Windows + Linux; a
        // no-op on macOS, which already single-instances .app bundles).
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                show_main_window(&window);
                let Some(state) = app.try_state::<Arc<AppState>>() else { return };
                let state: &Arc<AppState> = state.inner();
                discord::set_window_visible(state, true);

                // If the second instance was launched with --play <game>
                // (i.e. a desktop shortcut), forward it to the already-running
                // window so the game actually starts instead of just showing
                // the launcher.
                if let Some(game) = parse_play_arg(&argv) {
                    *state.auto_play_game.lock().unwrap() = Some(game.clone());
                    state.exit_after_game.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = window.eval(&format!(
                        "window.dispatchEvent(new CustomEvent('goopie:auto-play', {{ detail: {{ game: '{}' }} }}))",
                        game.replace('\\', "\\\\").replace('\'', "\\'"),
                    ));

                    // This launch came through `relay_play_to_running_instance`
                    // (Steam or another shortcut, while we're already running) —
                    // watch its relay PID so that if Steam's "Close" button kills
                    // it out from under us, we close the actual game too, rather
                    // than leaving it running with Steam none the wiser.
                    #[cfg(windows)]
                    if let Some(pid) = parse_flag_arg(&argv, "--relay-pid").and_then(|s| s.parse::<u32>().ok()) {
                        let state_clone = Arc::clone(state);
                        std::thread::spawn(move || {
                            wait_for_pid_exit(pid);
                            bridge::kill_running_game_if_matches(&state_clone, &game);
                        });
                    }
                }
            }
        }))
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

            // Tray icon — always created (even if "collapse to tray" is off)
            // since it's also how a hidden window gets restored, and it's
            // harmless to show when the setting is disabled: closing just
            // quits normally in that case and the icon offers quick access.
            let show_item = MenuItem::with_id(app, "library", "Library", true, None::<&str>)?;
            let settings_item = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit Goopie Launcher", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &settings_item, &quit_item])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().expect("missing default window icon"))
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "library" => {
                        navigate_and_show(app, "#/library");
                        if let Some(state) = app.try_state::<Arc<AppState>>() {
                            discord::set_window_visible(state.inner(), true);
                        }
                    }
                    "settings" => {
                        navigate_and_show(app, "#/settings");
                        if let Some(state) = app.try_state::<Arc<AppState>>() {
                            discord::set_window_visible(state.inner(), true);
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                        if let Some(window) = tray.app_handle().get_webview_window("main") {
                            show_main_window(&window);
                            if let Some(state) = tray.app_handle().try_state::<Arc<AppState>>() {
                                discord::set_window_visible(state.inner(), true);
                            }
                        }
                    }
                })
                .build(app)?;

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

            // Discord Rich Presence: announce "Browsing games" immediately, then
            // keep retrying/re-applying in the background — this is what lets
            // presence appear once Discord is launched after the launcher (its
            // IPC pipe/socket doesn't exist until then).
            discord::set_browsing(&state_for_setup);
            discord::spawn_discord_monitor(Arc::clone(&state_for_setup));

            Ok(())
        })
        .on_window_event(move |window, event| {
            match event {
                WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                    drag_has_files.store(false, std::sync::atomic::Ordering::Relaxed);
                    if paths.is_empty() {
                        return; // in-page drag (e.g. a cover image), not a real file drop
                    }
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
                // Drives the "drop files to install" overlay: shown on
                // Enter/Over, cleared on Leave (Drop above also implies the
                // drag session ended, but the frontend clears on drop itself).
                // Only real file drags (see `drag_has_files` above) show it —
                // otherwise dragging a cover image around the launcher's own
                // UI would pop the overlay and, since that kind of in-page
                // drag never produces a window-level Leave, get stuck until
                // the next real file drag happened to pass over the window.
                WindowEvent::DragDrop(DragDropEvent::Enter { paths, .. }) => {
                    let has_files = !paths.is_empty();
                    drag_has_files.store(has_files, std::sync::atomic::Ordering::Relaxed);
                    if has_files {
                        for webview in window.webviews() {
                            let _ = webview.eval("window.dispatchEvent(new CustomEvent('goopie:dragenter'))");
                        }
                    }
                }
                WindowEvent::DragDrop(DragDropEvent::Over { .. }) => {
                    if drag_has_files.load(std::sync::atomic::Ordering::Relaxed) {
                        for webview in window.webviews() {
                            let _ = webview.eval("window.dispatchEvent(new CustomEvent('goopie:dragenter'))");
                        }
                    }
                }
                WindowEvent::DragDrop(DragDropEvent::Leave { .. }) => {
                    drag_has_files.store(false, std::sync::atomic::Ordering::Relaxed);
                    for webview in window.webviews() {
                        let _ = webview.eval("window.dispatchEvent(new CustomEvent('goopie:dragleave'))");
                    }
                }
                // Collapse to the tray instead of quitting: a *hidden* WebView2
                // window stops rendering entirely, which is what actually fixes
                // the reported idle-performance hit (a minimized window keeps
                // painting). Gated by the user's setting — off means the
                // default OS behavior (close = quit) applies unchanged.
                WindowEvent::CloseRequested { api, .. } => {
                    if config::get_collapse_to_tray() {
                        api.prevent_close();
                        let _ = window.hide();
                        if let Some(state) = window.app_handle().try_state::<Arc<AppState>>() {
                            discord::set_window_visible(state.inner(), false);
                        }
                    }
                }
                _ => {}
            }
        })
        // No tauri::command handlers — all native calls go through the bridge scheme.
        .invoke_handler(tauri::generate_handler![])
        .run(tauri::generate_context!())
        .expect("error while running Goopie Launcher");
}
