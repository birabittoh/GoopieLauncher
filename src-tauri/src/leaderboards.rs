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
        if title_id.is_empty() || !title_id.chars().all(|c| c.is_ascii_hexdigit()) {
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
