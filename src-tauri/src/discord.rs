//! Discord Rich Presence: reflects "Browsing games" vs "Playing <title>" on the
//! user's Discord profile.
//!
//! The single source of truth for browsing-vs-playing is `AppState::running_game`
//! (see `bridge::launch_and_track` / `bridge::monitor_running_game` /
//! `bridge::kill_running_game`, which call into this module at the relevant
//! transitions). This module just owns the Discord IPC connection and re-applies
//! the *desired* presence whenever it changes or the connection needs re-establishing.
//!
//! No async runtime exists in this codebase, so — like `offline_site`'s
//! connectivity monitor and `launcher`'s update monitor — this uses a plain
//! `std::thread` polling loop rather than `tokio`. All Discord IPC calls are
//! best-effort: Discord may not be installed or running, and every error is
//! swallowed (logged) rather than surfaced to the user.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use serde::Deserialize;

use crate::{config, paths, AppState};

/// The Discord Application Client ID for GoopieLauncher's Rich Presence
/// integration, baked in at build time — mirrors `auth::resolve_client_id`'s
/// pattern for the Google OAuth client ID.
///
/// Set `GOOPIE_DISCORD_CLIENT_ID` in the build environment before `cargo build`
/// / `cargo tauri build`. For local dev you can also export it at runtime — the
/// runtime value is used as a fallback when the compile-time one is absent.
///
/// Discord application/client IDs are public by design (every Rich Presence
/// card shows one), so baking one in isn't a secret leak; keeping it out of
/// the committed source is just about not hardcoding an environment-specific
/// identifier into code, matching how the OAuth client ID is handled.
fn discord_client_id() -> Option<String> {
    if let Some(id) = option_env!("GOOPIE_DISCORD_CLIENT_ID") {
        return Some(id.to_string());
    }
    std::env::var("GOOPIE_DISCORD_CLIENT_ID").ok()
}

/// The presence the launcher *wants* shown on Discord, independent of whether
/// an IPC connection currently exists to push it.
#[derive(Clone, PartialEq, Eq)]
enum Presence {
    Browsing,
    Playing {
        title: String,
        /// `None` when the game has no `iconUrl` of its own — in that case we
        /// simply omit `large_image`, and Discord falls back to the
        /// application's own default icon (set in the Developer Portal),
        /// rather than us guessing at a hardcoded Goopie logo URL.
        image: Option<String>,
        /// Unix seconds the game was launched, used to derive Discord's
        /// "elapsed" timer (`Timestamps::start`, which the protocol expects
        /// in Unix *milliseconds*).
        start_epoch_secs: u64,
    },
    /// No launcher-owned activity shown at all — used while a game that opted
    /// out of the launcher's Rich Presence (`discordPresenceEnabled !== true`)
    /// is running, so the launcher doesn't clobber a presence the game sets
    /// for itself.
    Hidden,
}

/// Owns the Discord IPC connection and the desired/last-applied presence;
/// lives in `AppState::discord`.
pub struct DiscordManager {
    client: Option<DiscordIpcClient>,
    desired: Presence,
    /// Mirrors the persisted `config::get_discord_presence_enabled` setting so
    /// `apply()` doesn't need to read the registry/INI on every call.
    enabled: bool,
    /// Tracks what's actually been pushed, so reconnecting doesn't need to
    /// resend an unchanged activity every monitor tick.
    last_applied: Option<Presence>,
    /// Whether we've already logged a missing `GOOPIE_DISCORD_CLIENT_ID` —
    /// logged once (not every monitor tick) since it's a build/dev-env
    /// misconfiguration, unlike "Discord isn't running", which is normal and
    /// stays silent.
    warned_missing_client_id: bool,
}

impl DiscordManager {
    pub fn new(enabled: bool) -> Self {
        Self {
            client: None,
            desired: Presence::Browsing,
            enabled,
            last_applied: None,
            warned_missing_client_id: false,
        }
    }

    /// Make sure `self.client` is connected, lazily creating/connecting it.
    /// Returns `false` if a connection isn't currently possible — silently
    /// for the common "Discord isn't running" case, but logged once for a
    /// missing `GOOPIE_DISCORD_CLIENT_ID`, since that's a misconfiguration
    /// rather than something to tolerate quietly.
    fn ensure_connected(&mut self) -> bool {
        if self.client.is_some() {
            return true;
        }
        let Some(client_id) = discord_client_id() else {
            if !self.warned_missing_client_id {
                eprintln!(
                    "[discord] GOOPIE_DISCORD_CLIENT_ID is not set — Rich Presence disabled. \
                     Export it before running cargo build / cargo tauri dev."
                );
                self.warned_missing_client_id = true;
            }
            return false;
        };
        let mut client = DiscordIpcClient::new(&client_id);
        if client.connect().is_err() {
            return false;
        }
        self.client = Some(client);
        // A fresh connection has no activity set yet; force the next apply().
        self.last_applied = None;
        true
    }

    /// Push `self.desired` to Discord if connected, enabled, and changed since
    /// the last successful push. Drops the client on any IPC error so the next
    /// call re-establishes the connection (covers Discord restarting/closing).
    fn apply(&mut self) {
        if !self.enabled {
            return;
        }
        if !self.ensure_connected() {
            return;
        }
        if self.last_applied.as_ref() == Some(&self.desired) {
            return;
        }

        // No `large_image`/`large_text` here: omitting them entirely makes
        // Discord fall back to the application's own default icon (set in the
        // Developer Portal) — both while browsing and for games with no
        // `iconUrl` of their own.
        let activity = match &self.desired {
            Presence::Browsing => Some(activity::Activity::new().details("Browsing games")),
            Presence::Playing { title, image, start_epoch_secs } => {
                let mut act = activity::Activity::new()
                    .details(format!("Playing {}", title))
                    .timestamps(activity::Timestamps::new().start((*start_epoch_secs as i64) * 1000));
                if let Some(image) = image {
                    act = act.assets(
                        activity::Assets::new()
                            .large_image(image.as_str())
                            .large_text(title.as_str()),
                    );
                }
                Some(act)
            }
            // The running game owns its own Rich Presence (if any); the
            // launcher must not show anything on top of it.
            Presence::Hidden => None,
        };

        let Some(client) = self.client.as_mut() else { return };
        let result = match activity {
            Some(activity) => client.set_activity(activity),
            None => client.clear_activity(),
        };
        match result {
            Ok(()) => self.last_applied = Some(self.desired.clone()),
            Err(e) => {
                eprintln!("[discord] failed to set activity, will reconnect: {}", e);
                self.client = None;
            }
        }
    }

    /// Disable presence: clear whatever's currently shown (best-effort) and
    /// stop pushing further updates until re-enabled.
    fn disable(&mut self) {
        self.enabled = false;
        if let Some(client) = self.client.as_mut() {
            let _ = client.clear_activity();
        }
        self.last_applied = None;
    }

    fn enable(&mut self) {
        self.enabled = true;
        self.last_applied = None; // force a re-push of the current desired presence
        self.apply();
    }
}

/// Set the desired presence to "Browsing games" (no game running) and push it.
pub fn set_browsing(state: &Arc<AppState>) {
    let mut mgr = state.discord.lock().unwrap();
    mgr.desired = Presence::Browsing;
    mgr.apply();
}

/// Clear the launcher's own presence and push it — used while a running game
/// has opted out of the launcher's Rich Presence (it sets its own instead).
pub fn set_hidden(state: &Arc<AppState>) {
    let mut mgr = state.discord.lock().unwrap();
    mgr.desired = Presence::Hidden;
    mgr.apply();
}

/// Set the desired presence to "Playing <title>" and push it.
///
/// `recomp_name` is the internal game id (`RunningGame::game`); the display
/// title and icon are resolved from the cached games catalogue.
pub fn set_playing(state: &Arc<AppState>, recomp_name: &str, start_epoch_secs: u64) {
    let (title, icon_url) = lookup_game_presentation(recomp_name);
    let mut mgr = state.discord.lock().unwrap();
    mgr.desired = Presence::Playing {
        title,
        image: icon_url,
        start_epoch_secs,
    };
    mgr.apply();
}

/// Enable/disable presence at runtime (the Settings toggle), persisting the
/// choice via `config::set_discord_presence_enabled`.
pub fn set_enabled(state: &Arc<AppState>, enabled: bool) {
    config::set_discord_presence_enabled(enabled);
    let mut mgr = state.discord.lock().unwrap();
    if enabled {
        mgr.enable();
    } else {
        mgr.disable();
    }
}

/// Current Unix-epoch seconds, for stamping a `Playing` presence's start time.
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Spawn a background thread that periodically re-applies the desired
/// presence — this is what picks up a Discord client that was launched
/// *after* GoopieLauncher (the lazy `connect()` in `apply()` only succeeds
/// once Discord's IPC pipe/socket actually exists).
pub fn spawn_discord_monitor(state: Arc<AppState>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(15));
        let mut mgr = state.discord.lock().unwrap();
        mgr.apply();
    });
}

// ── Games-cache lookup ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CachedGame {
    #[serde(rename = "recompName")]
    recomp_name: String,
    title: String,
    #[serde(rename = "iconUrl")]
    icon_url: Option<String>,
    /// Mirrors `Game.discordPresenceEnabled` from the website's data model.
    /// Missing/`false` = the game handles its own Rich Presence and the
    /// launcher should show nothing while it runs; only `Some(true)` opts in
    /// to the launcher's "Playing <title>" presence.
    #[serde(rename = "discordPresenceEnabled")]
    discord_presence_enabled: Option<bool>,
}

#[derive(Deserialize)]
struct GamesCache {
    games: Vec<CachedGame>,
}

/// Read the games catalogue cached on disk at `paths::games_cache_file()` —
/// the same file read by the `getCachedGamesData` bridge command — and find
/// the entry for `recomp_name`, if any. `None` covers the cache being
/// missing, unparsable, or simply not (yet) containing this game.
fn find_cached_game(recomp_name: &str) -> Option<CachedGame> {
    let cache: GamesCache = std::fs::read_to_string(paths::games_cache_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    cache.games.into_iter().find(|g| g.recomp_name == recomp_name)
}

/// Resolve `recomp_name` to a `(display title, image URL)` pair from the
/// cached games catalogue.
///
/// The image is the game's `iconUrl` (purpose-built for shortcuts/presence,
/// set in the website's game editor) when non-empty, and `None` otherwise —
/// callers treat `None` as "show the default Goopie icon" rather than
/// guessing at cover art. Falls back to `recomp_name` itself as the title,
/// and `None` for the image, when the cache is missing, unparsable, or
/// simply doesn't have an entry for this game (e.g. it was just removed from
/// the catalogue, or the cache hasn't been written yet).
fn lookup_game_presentation(recomp_name: &str) -> (String, Option<String>) {
    match find_cached_game(recomp_name) {
        Some(g) => {
            let image = g.icon_url.filter(|s| !s.is_empty());
            (g.title, image)
        }
        None => (recomp_name.to_string(), None),
    }
}

/// Whether the launcher should show its own Rich Presence while
/// `recomp_name` is running. Default is `false` (off) — for a missing cache
/// entry, an unset flag, or an explicit `false` — since most games are
/// assumed to set their own presence and the launcher must not clobber it.
/// Only an explicit `discordPresenceEnabled: true` in the website's game data
/// opts in.
pub fn presence_enabled_for_game(recomp_name: &str) -> bool {
    find_cached_game(recomp_name)
        .and_then(|g| g.discord_presence_enabled)
        .unwrap_or(false)
}
