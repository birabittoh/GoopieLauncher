//! Per-game play-time tracking: recorded locally by the launcher when a game
//! session ends, never synced to the cloud — mirrors how achievements are
//! stored (see `achievements.rs`).
//!
//! On-disk layout (`paths::playtime_file()`):
//!   { "games": { "<recompName>": { "totalSeconds": u64, "lastPlayedAt": u64 } } }

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::paths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GamePlaytime {
    total_seconds: u64,
    last_played_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PlaytimeStore {
    #[serde(default)]
    games: HashMap<String, GamePlaytime>,
}

fn load(path: &Path) -> PlaytimeStore {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(path: &Path, store: &PlaytimeStore) {
    if let Ok(s) = serde_json::to_string(store) {
        let _ = std::fs::write(path, s);
    }
}

/// Add `seconds` to `game`'s running total and bump its last-played time.
/// No-op for a zero-length session (e.g. a launch that failed immediately).
pub fn record_session(game: &str, seconds: u64) {
    record_session_at(&paths::playtime_file(), game, seconds);
}

/// Returns `{ totalSeconds, lastPlayedAt }` for `game`, or `null` if it has
/// never been played.
pub fn get_playtime(game: &str) -> Value {
    get_playtime_at(&paths::playtime_file(), game)
}

fn record_session_at(path: &Path, game: &str, seconds: u64) {
    if seconds == 0 {
        return;
    }
    let mut store = load(path);
    let entry = store.games.entry(game.to_string()).or_default();
    entry.total_seconds += seconds;
    entry.last_played_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    save(path, &store);
}

fn get_playtime_at(path: &Path, game: &str) -> Value {
    match load(path).games.get(game) {
        Some(entry) => json!({
            "totalSeconds": entry.total_seconds,
            "lastPlayedAt": entry.last_played_at,
        }),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("goopie-playtime-test-{}-{}.json", std::process::id(), name));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn missing_file_defaults_to_null() {
        let path = temp_file("missing");
        assert_eq!(get_playtime_at(&path, "SomeGame"), Value::Null);
    }

    #[test]
    fn accumulates_across_sessions() {
        let path = temp_file("accumulate");
        record_session_at(&path, "SomeGame", 30);
        record_session_at(&path, "SomeGame", 15);
        let got = get_playtime_at(&path, "SomeGame");
        assert_eq!(got["totalSeconds"], json!(45));
        assert!(got["lastPlayedAt"].as_u64().unwrap() > 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zero_length_session_is_noop() {
        let path = temp_file("noop");
        record_session_at(&path, "SomeGame", 0);
        assert_eq!(get_playtime_at(&path, "SomeGame"), Value::Null);
    }

    #[test]
    fn tracks_games_independently() {
        let path = temp_file("independent");
        record_session_at(&path, "GameA", 10);
        record_session_at(&path, "GameB", 20);
        assert_eq!(get_playtime_at(&path, "GameA")["totalSeconds"], json!(10));
        assert_eq!(get_playtime_at(&path, "GameB")["totalSeconds"], json!(20));
        let _ = std::fs::remove_file(&path);
    }
}
