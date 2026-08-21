//! Serves game images (covers, header art, title/logo images) through the
//! `goopieimg` custom URI scheme, caching each one to disk on first fetch so
//! it keeps rendering when offline — see `offline_site.rs` for the sibling
//! `goopieoffline` scheme this mirrors.
//!
//! The frontend never talks to the original image host directly: it rewrites
//! `coverImage`/`headerImage`/`titleImage` URLs to `<img src="goopieimg://.../?url=<original>">`
//! (see `getCachedImageUrl` in `bridge/shim.js`). A request for a URL already
//! on disk is served straight from the cache; a cache miss downloads and
//! saves it before responding. Offline with no cached copy just 404s, which
//! renders as a broken image — the same degradation the frontend already
//! tolerates for missing art.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Base URL images are requested from, matching the platform-specific
/// custom-scheme convention used for `goopiebridge`/`goopieoffline`.
pub fn image_base_url() -> &'static str {
    if cfg!(windows) {
        "http://goopieimg.localhost/"
    } else {
        "goopieimg://localhost/"
    }
}

/// Handle one request arriving via the `goopieimg` custom URI scheme.
///
/// Expected request URI: `<image_base_url()>?url=<percent-encoded original URL>`.
pub fn handle_image_request(request: tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    let uri = request.uri();
    let url = uri
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("url="))
        .map(percent_decode)
        .unwrap_or_default();

    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return not_found();
    }

    let dir = crate::paths::image_cache_dir();
    let hash = hash_for(&url);

    if let Some(path) = find_cached(&dir, &hash) {
        if let Ok(bytes) = std::fs::read(&path) {
            let ct = content_type_for_bytes(&bytes);
            return ok(bytes, ct);
        }
    }

    // Cache miss: download synchronously — custom-scheme requests are
    // dispatched off the UI thread by the webview — then cache the result.
    match download_image(&url) {
        Some(bytes) => {
            let ct = content_type_for_bytes(&bytes);
            let ext = ext_for_content_type(ct);
            let _ = std::fs::write(dir.join(format!("{}.{}", hash, ext)), &bytes);
            ok(bytes, ct)
        }
        None => not_found(),
    }
}

fn hash_for(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hex::encode(hasher.finalize())
}

/// Find a cached file named `<hash>.<ext>` in `dir`, regardless of extension.
fn find_cached(dir: &Path, hash: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.strip_prefix(hash)
            .filter(|rest| rest.starts_with('.'))
            .map(|_| entry.path())
    })
}

fn download_image(url: &str) -> Option<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Goopie-Launcher/2")
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().ok().map(|b| b.to_vec())
}

fn content_type_for_bytes(bytes: &[u8]) -> &'static str {
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => "image/png",
        Ok(image::ImageFormat::Jpeg) => "image/jpeg",
        Ok(image::ImageFormat::Gif) => "image/gif",
        Ok(image::ImageFormat::WebP) => "image/webp",
        Ok(image::ImageFormat::Bmp) => "image/bmp",
        Ok(image::ImageFormat::Ico) => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn ext_for_content_type(ct: &str) -> &'static str {
    match ct {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/x-icon" => "ico",
        _ => "bin",
    }
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4 | lo) as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    out
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn ok(body: Vec<u8>, content_type: &str) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", content_type)
        .body(body)
        .unwrap()
}

fn not_found() -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(404)
        .header("Content-Type", "text/plain")
        .body(b"not found".to_vec())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_encoded_url() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fa%20b.png"),
            "https://example.com/a b.png"
        );
    }

    #[test]
    fn rejects_non_http_url() {
        let req = tauri::http::Request::builder()
            .uri("goopieimg://localhost/?url=file%3A%2F%2F%2Fetc%2Fpasswd")
            .body(Vec::new())
            .unwrap();
        let resp = handle_image_request(req);
        assert_eq!(resp.status(), 404);
    }

    #[test]
    fn hash_for_is_stable() {
        assert_eq!(hash_for("https://example.com/a.png"), hash_for("https://example.com/a.png"));
        assert_ne!(hash_for("https://example.com/a.png"), hash_for("https://example.com/b.png"));
    }
}
