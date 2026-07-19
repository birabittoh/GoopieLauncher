//! Leaderboard store reader for the Manage tab.
//!
//! On-disk layout (written by the ReXGlue runtime, see
//! `rex::system::LeaderboardManager`):
//!   <rex_user_folder>/<game>/leaderboards/<TITLE_ID_HEX8>.toml
//!
//! A single store file can carry multiple `[[boards]]` (one per view_id), each
//! with its own `rows`. A game can also accumulate more than one store file if
//! it shipped under different title ids over time (e.g. title updates), so the
//! Manage UI lets the user pick which file(s) to read.

use std::{fs, path::PathBuf};

use serde::Serialize;

use crate::paths;

/// Mirrors `rex::system::LeaderboardColumnType` (X_USER_DATA type byte).
/// The value is game-defined per column id — nothing on the store side knows
/// what a given column *means*, only how its bytes were laid out, so this is
/// as far as generic decoding can go without per-game column metadata.
fn column_type_name(type_id: i64) -> &'static str {
    match type_id {
        0 => "Context",
        1 => "Int32",
        2 => "Int64",
        3 => "Double",
        4 => "WString",
        5 => "Float",
        6 => "Binary",
        7 => "DateTime",
        _ => "Unknown",
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardColumn {
    pub id: u32,
    #[serde(rename = "type")]
    pub type_name: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardRow {
    pub xuid: String,
    pub gamertag: String,
    pub columns: Vec<LeaderboardColumn>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardBoard {
    pub title_id: String,
    pub view_id: u32,
    pub rows: Vec<LeaderboardRow>,
}

fn leaderboards_dir(game: &str) -> Option<PathBuf> {
    Some(paths::rex_user_folder()?.join(game).join("leaderboards"))
}

/// List the title ids (filenames minus `.toml`) available for a game, sorted.
pub fn list_leaderboard_files(game: &str) -> Vec<String> {
    let Some(dir) = leaderboards_dir(game) else { return vec![] };
    let Ok(entries) = fs::read_dir(&dir) else { return vec![] };

    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some("toml") {
                path.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

/// Parse the `boards` from the given title-id store files for a game.
/// Unknown/unreadable/unparseable files are skipped (graceful degradation).
pub fn get_leaderboards(game: &str, title_ids: Vec<String>) -> Vec<LeaderboardBoard> {
    let Some(dir) = leaderboards_dir(game) else { return vec![] };
    let mut out = Vec::new();

    for title_id in title_ids {
        // Filenames come straight from a directory listing on our side, but
        // guard against path traversal in case a caller passes one through directly.
        // Store files are normally named after their hex title id, but users can
        // rename/duplicate a file (e.g. to keep an old store around under a new
        // name) so any filesystem-safe stem is accepted here.
        if !valid_store_filename(&title_id) {
            continue;
        }
        let path = dir.join(format!("{}.toml", title_id));
        let Ok(content) = fs::read_to_string(&path) else { continue };
        let Ok(value) = content.parse::<toml::Value>() else { continue };
        let Some(toml::Value::Array(boards)) = value.get("boards").cloned() else { continue };

        for b in boards {
            let toml::Value::Table(board) = b else { continue };
            let view_id = board.get("view_id").and_then(|v| v.as_integer()).unwrap_or(0) as u32;

            let mut rows = Vec::new();
            if let Some(toml::Value::Array(row_vals)) = board.get("rows") {
                for r in row_vals {
                    let toml::Value::Table(row) = r else { continue };

                    // Written as a quoted 16-hex-digit XUID; tolerate a legacy bare integer.
                    let xuid = match row.get("xuid") {
                        Some(toml::Value::String(s)) => s.clone(),
                        Some(toml::Value::Integer(i)) => format!("{:016x}", i),
                        _ => String::new(),
                    };
                    let gamertag = row
                        .get("gamertag")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let mut columns = Vec::new();
                    if let Some(toml::Value::Array(col_vals)) = row.get("columns") {
                        for c in col_vals {
                            let toml::Value::Table(col) = c else { continue };
                            let id = col.get("id").and_then(|v| v.as_integer()).unwrap_or(0) as u32;
                            let type_id = col.get("type").and_then(|v| v.as_integer()).unwrap_or(1);
                            let type_name = column_type_name(type_id).to_string();
                            let value = match col.get("value") {
                                Some(toml::Value::Integer(i)) => serde_json::json!(i),
                                Some(toml::Value::Float(f)) => serde_json::json!(f),
                                Some(toml::Value::String(s)) => serde_json::json!(s),
                                _ => serde_json::Value::Null,
                            };
                            columns.push(LeaderboardColumn { id, type_name, value });
                        }
                    }

                    rows.push(LeaderboardRow { xuid, gamertag, columns });
                }
            }

            out.push(LeaderboardBoard { title_id: title_id.clone(), view_id, rows });
        }
    }

    out
}

fn valid_store_filename(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Merges every leaderboard store file present for `game` into one, so the
/// game sees a single combined leaderboard for the duration of this play
/// session. Every file being merged is first copied aside as a timestamped
/// backup, then all source files except the target are deleted and the
/// target is overwritten with the merged content. Call
/// [`restore_after_exit`] with the same `game` once the session ends to
/// bring the non-target files back.
///
/// `title_id_hint` is the title id configured in Edit Game, if any — the
/// actual filename the game writes to is trusted over any file that merely
/// *looks* like a hex title id, since a user can copy/rename a file to an
/// identical-looking name.
///
/// A no-op (nothing to merge) when fewer than two store files exist.
pub fn merge_all_for_launch(game: &str, title_id_hint: Option<&str>) {
    let title_ids = list_leaderboard_files(game);
    if title_ids.len() < 2 {
        return;
    }
    if let Err(e) = merge_leaderboard_files(game, title_ids, title_id_hint) {
        eprintln!("[leaderboards] merge-on-launch failed for {}: {}", game, e);
    }
}

/// Undoes [`merge_all_for_launch`]: every non-target file removed by the
/// merge is restored from its backup, and the exact rows that merge injected
/// into the target (per view_id, read straight out of the backups) are
/// subtracted back out of it — rather than overwriting the target wholesale,
/// which would also discard any new rows the game wrote to it this session.
/// Without this subtraction, re-merging on the next launch would re-inject
/// the same imported rows and duplicate them forever. All backups are
/// cleaned up afterward.
pub fn restore_after_exit(game: &str) {
    let Some(dir) = leaderboards_dir(game) else { return };
    let Ok(entries) = fs::read_dir(&dir) else { return };

    // Group backups by their original title id, keeping only the newest
    // (highest timestamp) copy of each in case stale backups ever linger.
    let mut latest_backup: std::collections::HashMap<String, (u64, PathBuf)> = std::collections::HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some((id, ts_str)) = name.split_once(".toml.backup-") else { continue };
        let Ok(ts) = ts_str.parse::<u64>() else { continue };
        if !valid_store_filename(id) {
            continue;
        }
        match latest_backup.get(id) {
            Some((existing_ts, _)) if *existing_ts >= ts => {
                let _ = fs::remove_file(&path);
            }
            _ => {
                if let Some((_, stale)) = latest_backup.insert(id.to_string(), (ts, path.clone())) {
                    let _ = fs::remove_file(stale);
                }
            }
        }
    }
    if latest_backup.is_empty() {
        return;
    }

    let live_ids: Vec<&String> = latest_backup.keys()
        .filter(|id| dir.join(format!("{}.toml", id)).exists())
        .collect();

    if let [target_id] = live_ids.as_slice() {
        // Every row each *non-target* backup held, grouped by view_id — this
        // is exactly what the merge injected into the target from that file,
        // and what must come back out of it below. The target's own backup
        // must not contribute here: those are the target's original rows,
        // not something merge added to it.
        let mut injected_by_view: std::collections::HashMap<i64, Vec<toml::Value>> = std::collections::HashMap::new();
        for (id, (_, path)) in &latest_backup {
            if id == *target_id {
                continue;
            }
            let Ok(content) = fs::read_to_string(path) else { continue };
            let Ok(value) = content.parse::<toml::Value>() else { continue };
            let Some(toml::Value::Array(boards)) = value.get("boards").cloned() else { continue };
            for b in boards {
                let toml::Value::Table(board) = b else { continue };
                let view_id = board.get("view_id").and_then(|v| v.as_integer()).unwrap_or(0);
                if let Some(toml::Value::Array(rows)) = board.get("rows") {
                    injected_by_view.entry(view_id).or_default().extend(rows.clone());
                }
            }
        }

        let target_path = dir.join(format!("{}.toml", target_id));
        if let Ok(content) = fs::read_to_string(&target_path) {
            if let Ok(toml::Value::Table(mut root)) = content.parse::<toml::Value>() {
                if let Some(toml::Value::Array(boards)) = root.get_mut("boards") {
                    for b in boards.iter_mut() {
                        let toml::Value::Table(board) = b else { continue };
                        let view_id = board.get("view_id").and_then(|v| v.as_integer()).unwrap_or(0);
                        let Some(to_remove) = injected_by_view.get(&view_id) else { continue };
                        let Some(toml::Value::Array(rows)) = board.get_mut("rows") else { continue };
                        for row in to_remove {
                            if let Some(pos) = rows.iter().position(|r| r == row) {
                                rows.remove(pos);
                            }
                        }
                    }
                }
                if let Ok(new_content) = toml::to_string_pretty(&toml::Value::Table(root)) {
                    let _ = fs::write(&target_path, new_content);
                }
            }
        }
    }

    for (id, (_, backup_path)) in latest_backup {
        let live_path = dir.join(format!("{}.toml", id));
        if live_path.exists() {
            let _ = fs::remove_file(&backup_path);
        } else {
            let _ = fs::rename(&backup_path, &live_path);
        }
    }
}

fn merge_leaderboard_files(game: &str, mut title_ids: Vec<String>, target_hint: Option<&str>) -> Result<String, String> {
    let dir = leaderboards_dir(game).ok_or("Could not determine the leaderboards folder")?;

    title_ids.retain(|id| valid_store_filename(id));
    title_ids.sort();
    title_ids.dedup();
    if title_ids.len() < 2 {
        return Err("Need at least two leaderboard files to merge".to_string());
    }

    // Prefer the configured title id (the file the game actually writes to)
    // over guessing from filenames — several files can look like a valid hex
    // title id if one was copied/renamed to imitate another.
    let target_id = target_hint
        .filter(|hint| valid_store_filename(hint))
        .and_then(|hint| title_ids.iter().find(|id| id.eq_ignore_ascii_case(hint)))
        .cloned()
        .or_else(|| {
            title_ids
                .iter()
                .find(|id| id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()))
                .cloned()
        })
        .unwrap_or_else(|| title_ids[0].clone());

    // Merge boards by view_id, concatenating each view's `rows` array as raw
    // toml::Value so row/column contents round-trip byte-for-byte.
    let mut merged: Vec<(i64, toml::value::Table)> = Vec::new();
    for title_id in &title_ids {
        let path = dir.join(format!("{}.toml", title_id));
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("Could not read {}.toml: {}", title_id, e))?;
        let value: toml::Value = content
            .parse()
            .map_err(|e| format!("Could not parse {}.toml: {}", title_id, e))?;
        let Some(toml::Value::Array(boards)) = value.get("boards").cloned() else { continue };

        for b in boards {
            let toml::Value::Table(board) = b else { continue };
            let view_id = board.get("view_id").and_then(|v| v.as_integer()).unwrap_or(0);
            let rows = board.get("rows").cloned().unwrap_or(toml::Value::Array(vec![]));
            let toml::Value::Array(rows) = rows else { continue };

            match merged.iter_mut().find(|(v, _)| *v == view_id) {
                Some((_, existing)) => {
                    if let Some(toml::Value::Array(existing_rows)) = existing.get_mut("rows") {
                        existing_rows.extend(rows);
                    }
                }
                None => {
                    let mut table = board.clone();
                    table.insert("rows".to_string(), toml::Value::Array(rows));
                    merged.push((view_id, table));
                }
            }
        }
    }

    let merged_value = toml::Value::Table({
        let mut root = toml::value::Table::new();
        root.insert(
            "boards".to_string(),
            toml::Value::Array(merged.into_iter().map(|(_, t)| toml::Value::Table(t)).collect()),
        );
        root
    });
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let merged_content = format!(
        "# Merged by GoopieLauncher for this play session (unix {}) from: {}.\n\
         # The pre-merge files were backed up alongside this one and are restored\n\
         # automatically when the game closes — do not edit this comment by hand.\n{}",
        timestamp,
        title_ids.join(", "),
        toml::to_string_pretty(&merged_value)
            .map_err(|e| format!("Could not serialize merged leaderboards: {}", e))?
    );

    // Back up every source file before touching anything on disk.
    for title_id in &title_ids {
        let src = dir.join(format!("{}.toml", title_id));
        // Deliberately not a `.toml` file: `list_leaderboard_files` (and thus
        // the ReXGlue runtime's own store lookup) only picks up `.toml`
        // files, so a backup with that extension would otherwise show up as
        // a selectable leaderboard file — or even get read as live game data.
        let backup = dir.join(format!("{}.toml.backup-{}", title_id, timestamp));
        fs::copy(&src, &backup).map_err(|e| format!("Could not back up {}.toml: {}", title_id, e))?;
    }

    // Only now overwrite the target and remove the other merged-in files.
    fs::write(dir.join(format!("{}.toml", target_id)), merged_content)
        .map_err(|e| format!("Could not write merged {}.toml: {}", target_id, e))?;
    for title_id in &title_ids {
        if title_id != &target_id {
            let _ = fs::remove_file(dir.join(format!("{}.toml", title_id)));
        }
    }

    Ok(target_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_store(dir: &std::path::Path, title_id: &str, content: &str) {
        fs::create_dir_all(dir).unwrap();
        let mut f = fs::File::create(dir.join(format!("{}.toml", title_id))).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn lists_and_parses_store_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        write_store(
            dir,
            "58410847",
            r#"
[[boards]]
title_id = 1481506375
view_id = 1

  [[boards.rows]]
  xuid = "b13ebabebabebabe"
  gamertag = "Player1"

    [[boards.rows.columns]]
    id = 1
    type = 1
    value = 42
"#,
        );

        let names = {
            let entries = fs::read_dir(dir).unwrap();
            let mut v: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(names, vec!["58410847".to_string()]);

        let content = fs::read_to_string(dir.join("58410847.toml")).unwrap();
        let value: toml::Value = content.parse().unwrap();
        let boards = value.get("boards").and_then(|v| v.as_array()).unwrap();
        assert_eq!(boards.len(), 1);
    }
}
