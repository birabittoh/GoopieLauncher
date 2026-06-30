//! Achievement system: extract metadata from an XEX XDBF resource and merge
//! with per-user unlock state tracked by the ReXGlue runtime.
//!
//! On-disk layout (written by the ReXGlue runtime):
//!   <rex_user_folder>/<game>/achievements/<TITLE_ID>.toml
//!
//! Unlock-state TOML format (new):
//!   [unlocked.<id>]
//!   filetime = <u64>
//!
//! Unlock-state TOML format (legacy, migrated automatically by the runtime):
//!   unlocked = [<id>, ...]
//!
//! Achievement definitions (names, descriptions, gamerscore, icons) live only
//! inside `assets/default.xex` as XDBF/SPA data and are extracted on demand.
//! Extracted definitions are cached to `assets/.achievements-cache.json` so
//! subsequent opens avoid re-decrypting the XEX.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};

use crate::{config, extract::xex, games, paths};

// ---------------------------------------------------------------------------
// Public data types returned over the bridge
// ---------------------------------------------------------------------------

/// Full achievement detail — returned by `getAchievements` for the Manage tab.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Achievement {
    pub id: u32,
    pub label: String,
    pub description: String,
    pub unachieved_description: String,
    /// PNG icon encoded as a `data:image/png;base64,...` string, or empty.
    pub icon_data_url: String,
    pub gamerscore: u32,
    pub flags: u32,
    pub unlocked: bool,
    /// Windows FILETIME of unlock (100-ns intervals since 1601-01-01), 0 if locked.
    pub unlock_filetime: u64,
}

/// Lightweight summary — returned by `getAchievementSummary` for profile totals.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementSummary {
    pub unlocked: u32,
    pub total: u32,
    pub earned_score: u32,
    pub total_score: u32,
}

// ---------------------------------------------------------------------------
// Definition cache (stored alongside the XEX to avoid re-extracting)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct CacheKey {
    xex_len: u64,
    xex_mtime: u64,
    /// The launcher's configured UI language (`config::get_language()`) at
    /// extraction time. Strings are baked into the cache per-language, so a
    /// language switch in Settings invalidates the cache and re-resolves
    /// label/description/unachieved_description against the new language.
    lang: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedDef {
    id: u32,
    label: String,
    description: String,
    unachieved_description: String,
    icon_data_url: String,
    gamerscore: u32,
    flags: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DefinitionCache {
    key: CacheKey,
    title_id: String,
    definitions: Vec<CachedDef>,
}

fn xex_cache_key(xex_path: &Path, lang: u32) -> Option<CacheKey> {
    let meta = fs::metadata(xex_path).ok()?;
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(CacheKey { xex_len: len, xex_mtime: mtime, lang })
}

fn cache_path(assets_dir: &Path) -> PathBuf {
    assets_dir.join(".achievements-cache.json")
}

fn load_definition_cache(assets_dir: &Path, key: &CacheKey) -> Option<DefinitionCache> {
    let path = cache_path(assets_dir);
    let data = fs::read(&path).ok()?;
    let cache: DefinitionCache = serde_json::from_slice(&data).ok()?;
    // Invalidate if the xex was replaced/updated or the requested language changed.
    if cache.key != *key {
        return None;
    }
    Some(cache)
}

fn save_definition_cache(assets_dir: &Path, cache: &DefinitionCache) {
    let path = cache_path(assets_dir);
    if let Ok(data) = serde_json::to_vec(cache) {
        if let Ok(mut f) = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
        {
            let _ = f.write_all(&data);
        }
    }
}

// ---------------------------------------------------------------------------
// Definition extraction (XEX → XDBF → XACH + icons)
// ---------------------------------------------------------------------------

/// Extract achievement definitions from `assets/default.xex`.
/// Returns `None` if the XEX is missing or unparseable (graceful degradation).
fn extract_definitions(game_root: &Path) -> Option<DefinitionCache> {
    let assets_dir = game_root.join("assets");
    let xex_path   = assets_dir.join("default.xex");

    if !xex_path.exists() {
        return None;
    }

    // Resolve achievement strings against the launcher's configured UI
    // language, so switching language in Settings changes achievement text too.
    let requested_lang = config::get_language() as u32;
    let key = xex_cache_key(&xex_path, requested_lang)?;

    // Cache hit?
    if let Some(cached) = load_definition_cache(&assets_dir, &key) {
        return Some(cached);
    }

    // Parse the XEX.
    let xdbf = xex::load_xdbf(&xex_path)
        .map_err(|e| eprintln!("[achievements] load_xdbf failed for {}: {}", xex_path.display(), e))
        .ok()?;

    // Not every game ships translations for every language; fall back to the
    // XEX's own default language when the requested one isn't present.
    let lang = if xdbf.has_language(requested_lang) {
        requested_lang
    } else {
        xdbf.default_language()
    };
    let raw = xdbf.get_achievements(lang);

    let definitions: Vec<CachedDef> = raw
        .into_iter()
        .map(|a| {
            // Encode the per-achievement icon as a data URL.
            let icon_data_url = xdbf
                .get_image(a.image_id as u64)
                .map(|png| format!("data:image/png;base64,{}", B64.encode(png)))
                .unwrap_or_default();

            CachedDef {
                id: a.id,
                label: a.label,
                description: a.description,
                unachieved_description: a.unachieved_description,
                icon_data_url,
                gamerscore: a.gamerscore,
                flags: a.flags,
            }
        })
        .collect();

    let cache = DefinitionCache {
        key,
        title_id: xdbf.title_id.clone(),
        definitions,
    };

    save_definition_cache(&assets_dir, &cache);
    Some(cache)
}

// ---------------------------------------------------------------------------
// Unlock-state reading
// ---------------------------------------------------------------------------

/// Read the unlock-state TOML file for a game and return a map of id → filetime.
/// Returns an empty map when the file is missing (game never run, or no unlocks yet).
fn read_unlock_state(game: &str, title_id: &str) -> HashMap<u32, u64> {
    let Some(user_root) = paths::rex_user_folder() else {
        return HashMap::new();
    };
    let state_path = user_root
        .join(game)
        .join("achievements")
        .join(format!("{}.toml", title_id));

    read_unlock_state_at(&state_path)
}

/// Core parser — separated so unit tests can pass arbitrary paths.
fn read_unlock_state_at(path: &Path) -> HashMap<u32, u64> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };

    let mut map = HashMap::new();

    // Try parsing as a TOML value so we handle both formats.
    let value: toml::Value = match toml::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[achievements] failed to parse unlock state at {}: {}", path.display(), e);
            return map;
        }
    };

    // New format: [unlocked.<id>] tables with a `filetime` key.
    if let Some(toml::Value::Table(unlocked)) = value.get("unlocked") {
        for (id_str, entry) in unlocked {
            let Ok(id) = id_str.parse::<u32>() else { continue };
            let filetime = entry
                .get("filetime")
                .and_then(|v| v.as_integer())
                .map(|i| i as u64)
                .unwrap_or(1);
            map.insert(id, filetime);
        }
        return map;
    }

    // Legacy format: unlocked = [id, id, ...]
    if let Some(toml::Value::Array(ids)) = value.get("unlocked") {
        for v in ids {
            if let Some(id) = v.as_integer().map(|i| i as u32) {
                map.insert(id, 1); // no timestamp in legacy format
            }
        }
    }

    map
}

// ---------------------------------------------------------------------------
// Public bridge-facing functions
// ---------------------------------------------------------------------------

/// Full achievement list for the given game, with unlock state merged in.
/// Returns an empty vec on any failure (graceful degradation).
pub fn get_achievements(game: &str) -> Vec<Achievement> {
    let game_root = games::game_root(game);
    let Some(cache) = extract_definitions(&game_root) else {
        return vec![];
    };

    let unlocked = read_unlock_state(game, &cache.title_id);

    cache
        .definitions
        .into_iter()
        .map(|def| {
            let unlock_filetime = unlocked.get(&def.id).copied().unwrap_or(0);
            Achievement {
                id: def.id,
                label: def.label,
                description: def.description,
                unachieved_description: def.unachieved_description,
                icon_data_url: def.icon_data_url,
                gamerscore: def.gamerscore,
                flags: def.flags,
                unlocked: unlock_filetime > 0,
                unlock_filetime,
            }
        })
        .collect()
}

/// Lightweight achievement summary for the given game (used by profile totals).
/// Returns a zero summary when the game has no achievements or XEX.
pub fn get_achievement_summary(game: &str) -> AchievementSummary {
    let game_root = games::game_root(game);
    let Some(cache) = extract_definitions(&game_root) else {
        return AchievementSummary { unlocked: 0, total: 0, earned_score: 0, total_score: 0 };
    };

    let unlocked_map = read_unlock_state(game, &cache.title_id);

    let total = cache.definitions.len() as u32;
    let total_score: u32 = cache.definitions.iter().map(|d| d.gamerscore).sum();
    let unlocked = cache
        .definitions
        .iter()
        .filter(|d| unlocked_map.contains_key(&d.id))
        .count() as u32;
    let earned_score: u32 = cache
        .definitions
        .iter()
        .filter(|d| unlocked_map.contains_key(&d.id))
        .map(|d| d.gamerscore)
        .sum();

    AchievementSummary { unlocked, total, earned_score, total_score }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_achievements_installed_games() {
        let config_dir = crate::config::get_games_folder();
        let games_dir = std::path::PathBuf::from(&config_dir);
        if !games_dir.exists() {
            eprintln!("SKIP: games dir not found");
            return;
        }

        let mut found_any = false;
        if let Ok(entries) = std::fs::read_dir(&games_dir) {
            for entry in entries.flatten() {
                let game = entry.file_name().to_string_lossy().into_owned();
                let xex = entry.path().join("assets").join("default.xex");
                if !xex.exists() {
                    continue;
                }
                found_any = true;

                let achievements = get_achievements(&game);
                eprintln!("OK {}: {} achievements", game, achievements.len());

                for a in &achievements {
                    assert_ne!(a.id, 0);
                    assert!(!a.label.is_empty(), "achievement {} label is empty in {}", a.id, game);
                    assert!(a.gamerscore < 1000, "suspicious gamerscore {} in {}", a.gamerscore, game);
                }

                // Sanity-check (not a hard assertion): unlock-state ids should
                // normally appear in the definitions list. This can legitimately
                // drift — the on-disk unlock state reflects whatever build the
                // user last played, which may differ from the currently
                // installed default.xex (game updated/reverted since) — so we
                // only warn, never fail the test, on a mismatch.
                let game_root = games::game_root(&game);
                if let Some(cache) = extract_definitions(&game_root) {
                    let def_ids: std::collections::HashSet<u32> =
                        cache.definitions.iter().map(|d| d.id).collect();
                    let unlocked = read_unlock_state(&game, &cache.title_id);
                    for id in unlocked.keys() {
                        if !def_ids.contains(id) {
                            eprintln!(
                                "WARN unlock state has achievement id {} not in current definitions for {} (stale build?)",
                                id, game
                            );
                        }
                    }
                }
            }
        }
        if !found_any {
            eprintln!("SKIP: no games with default.xex found");
        }
    }

    #[test]
    fn test_read_unlock_state_new_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("achievements.toml");
        std::fs::write(
            &path,
            "# Achievement unlock state\n\
             [unlocked.10]\nfiletime = 134271593675495254\n\
             [unlocked.82]\nfiletime = 134271593408494738\n",
        )
        .unwrap();

        let map = read_unlock_state_at(&path);
        assert_eq!(map.get(&10), Some(&134271593675495254u64));
        assert_eq!(map.get(&82), Some(&134271593408494738u64));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_read_unlock_state_legacy_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("achievements.toml");
        std::fs::write(&path, "unlocked = [1, 2, 5]\n").unwrap();

        let map = read_unlock_state_at(&path);
        assert!(map.contains_key(&1));
        assert!(map.contains_key(&2));
        assert!(map.contains_key(&5));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn test_read_unlock_state_missing_file() {
        let map = read_unlock_state_at(Path::new("/nonexistent/achievements.toml"));
        assert!(map.is_empty());
    }
}
