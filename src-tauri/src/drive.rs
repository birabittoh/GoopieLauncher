//! Minimal Google Drive REST client, scoped to the app's hidden
//! `appDataFolder` (see `auth::DRIVE_APPDATA_SCOPE`) — used by `cloud_saves.rs`
//! to store one zipped save file per game, tagged with a content hash and an
//! upload timestamp via Drive's `appProperties`.
//!
//! Deliberately hand-rolled (no Drive SDK crate, and `reqwest`'s "json"/
//! "multipart" cargo features aren't enabled — see `Cargo.toml`) to match the
//! existing manual-request style in `auth.rs`.

use serde_json::Value;

const FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";

/// A save file's Drive-side metadata, as read back from `appProperties`.
pub struct DriveFile {
    pub id: String,
    pub hash: Option<String>,
    pub updated_at: Option<u64>,
}

/// Look up the (at most one) file named `name` in the app's `appDataFolder`.
/// Returns `Ok(None)` if no such file exists yet (first sync for this game).
pub fn find_file(access_token: &str, name: &str) -> Result<Option<DriveFile>, String> {
    let query = format!("name = '{}' and trashed = false", escape_query_literal(name));
    let url = format!(
        "{}?spaces=appDataFolder&fields={}&q={}",
        FILES_URL,
        url_encode("files(id,appProperties)"),
        url_encode(&query),
    );

    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .map_err(|e| format!("Drive file lookup failed: {}", e))?;

    let json = parse_response(resp)?;
    let files = json["files"].as_array().cloned().unwrap_or_default();
    Ok(files.first().map(file_from_json))
}

/// Create (`existing_id` is `None`) or update the save file, tagging it with
/// `hash` and `updated_at` via `appProperties`. Returns the Drive file id.
pub fn upload(
    access_token: &str,
    existing_id: Option<&str>,
    name: &str,
    bytes: &[u8],
    hash: &str,
    updated_at: u64,
) -> Result<String, String> {
    let mut metadata = serde_json::json!({
        "appProperties": { "hash": hash, "updatedAt": updated_at.to_string() },
    });
    if existing_id.is_none() {
        // `name`/`parents` only matter (and are only accepted) on create —
        // Drive keeps both fixed across updates unless explicitly changed.
        metadata["name"] = Value::String(name.to_string());
        metadata["parents"] = serde_json::json!(["appDataFolder"]);
    }

    // multipart/related, built by hand (see module doc): part 1 is the JSON
    // metadata, part 2 is the raw zip bytes.
    const BOUNDARY: &str = "goopie-cloud-save-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{}\r\n", BOUNDARY).as_bytes());
    body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(metadata.to_string().as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{}\r\n", BOUNDARY).as_bytes());
    body.extend_from_slice(b"Content-Type: application/zip\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{}--", BOUNDARY).as_bytes());

    let client = reqwest::blocking::Client::new();
    let content_type = format!("multipart/related; boundary={}", BOUNDARY);
    let resp = if let Some(id) = existing_id {
        client
            .patch(format!("{}/{}?uploadType=multipart", UPLOAD_URL, id))
            .bearer_auth(access_token)
            .header("Content-Type", content_type)
            .body(body)
            .send()
    } else {
        client
            .post(format!("{}?uploadType=multipart", UPLOAD_URL))
            .bearer_auth(access_token)
            .header("Content-Type", content_type)
            .body(body)
            .send()
    }
    .map_err(|e| format!("Drive upload failed: {}", e))?;

    let json = parse_response(resp)?;
    json["id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No file id in Drive upload response".to_string())
}

/// Download a file's raw content by Drive file id.
pub fn download(access_token: &str, file_id: &str) -> Result<Vec<u8>, String> {
    let url = format!("{}/{}?alt=media", FILES_URL, file_id);
    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .map_err(|e| format!("Drive download request failed: {}", e))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .map_err(|e| format!("Drive download read failed: {}", e))?;
    if !status.is_success() {
        let msg = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|j| j["error"]["message"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return Err(format!("Drive download failed: {}", msg));
    }
    Ok(bytes.to_vec())
}

fn file_from_json(v: &Value) -> DriveFile {
    DriveFile {
        id: v["id"].as_str().unwrap_or_default().to_string(),
        hash: v["appProperties"]["hash"].as_str().map(|s| s.to_string()),
        updated_at: v["appProperties"]["updatedAt"].as_str().and_then(|s| s.parse().ok()),
    }
}

fn parse_response(resp: reqwest::blocking::Response) -> Result<Value, String> {
    let status = resp.status();
    let bytes = resp
        .bytes()
        .map_err(|e| format!("Drive response read failed: {}", e))?;
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Drive response is not valid JSON: {}", e))?;
    if !status.is_success() {
        let msg = json["error"]["message"].as_str().unwrap_or("unknown error");
        return Err(format!("Drive request failed ({}): {}", status.as_u16(), msg));
    }
    Ok(json)
}

/// Escape a string literal for use inside a Drive `q` search expression
/// (single quotes and backslashes must be backslash-escaped — see the Drive
/// API's search-query syntax docs).
fn escape_query_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Percent-encode a string for use in a URL query parameter value.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(char::from_digit(u32::from(b) >> 4, 16).unwrap().to_ascii_uppercase());
            out.push(char::from_digit(u32::from(b) & 0xf, 16).unwrap().to_ascii_uppercase());
        }
    }
    out
}
