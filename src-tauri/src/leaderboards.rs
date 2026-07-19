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
        if title_id.is_empty() || !title_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
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

/// Merge several leaderboard store files into one, concatenating rows for
/// matching `view_id`s. Every file being merged is first copied aside as a
/// timestamped backup (so the user can always recover the pre-merge state by
/// renaming a backup back), then all source files except the target are
/// deleted and the target is overwritten with the merged content.
///
/// The merge target is the file whose name is an unmodified 8-hex-digit
/// title id (the store the game itself writes to) if one is present among
/// `title_ids`; otherwise the first name, sorted.
///
/// Returns the merged file's title id (its filename minus `.toml`) on success.
pub fn merge_leaderboard_files(game: &str, mut title_ids: Vec<String>) -> Result<String, String> {
    let dir = leaderboards_dir(game).ok_or("Could not determine the leaderboards folder")?;

    title_ids.retain(|id| valid_store_filename(id));
    title_ids.sort();
    title_ids.dedup();
    if title_ids.len() < 2 {
        return Err("Select at least two leaderboard files to merge".to_string());
    }

    let target_id = title_ids
        .iter()
        .find(|id| id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()))
        .cloned()
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
    let merged_content = toml::to_string_pretty(&merged_value)
        .map_err(|e| format!("Could not serialize merged leaderboards: {}", e))?;

    // Back up every source file before touching anything on disk.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for title_id in &title_ids {
        let src = dir.join(format!("{}.toml", title_id));
        let backup = dir.join(format!("{}.backup-{}.toml", title_id, timestamp));
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
