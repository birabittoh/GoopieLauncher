//! HTTP downloading with progress callbacks and SHA256 verification.
//!
//! Uses `reqwest::blocking` (synchronous, run on worker threads by callers).

use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// Callback invoked with `(bytes_downloaded, total_bytes)` during a download.
pub type ProgressCallback = Box<dyn Fn(u64, u64) + Send>;

#[derive(Debug)]
pub enum DownloadError {
    Network(reqwest::Error),
    Io(std::io::Error),
    Http(u16),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Network(e) => write!(f, "network error: {}", e),
            DownloadError::Io(e) => write!(f, "I/O error: {}", e),
            DownloadError::Http(code) => write!(f, "HTTP {}", code),
        }
    }
}

impl From<reqwest::Error> for DownloadError {
    fn from(e: reqwest::Error) -> Self {
        DownloadError::Network(e)
    }
}

impl From<std::io::Error> for DownloadError {
    fn from(e: std::io::Error) -> Self {
        DownloadError::Io(e)
    }
}

/// Download `url` to `dest_path`, calling `progress` with (downloaded, total) updates.
pub fn download_file(
    url: &str,
    dest_path: &str,
    progress: Option<&ProgressCallback>,
) -> Result<(), DownloadError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Goopie-Launcher/2")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;

    let mut resp = client.get(url).send()?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(DownloadError::Http(status));
    }

    let total = resp.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(dest_path)?;
    let mut downloaded: u64 = 0;
    let mut buf = vec![0u8; 65536];

    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        downloaded += n as u64;
        if let Some(cb) = progress {
            cb(downloaded, if total > 0 { total } else { downloaded });
        }
    }

    Ok(())
}

/// Fetch a URL to a `String` (e.g. for GitHub API JSON).
pub fn fetch_to_string(url: &str) -> Result<String, DownloadError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Goopie-Launcher/2")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;
    let resp = client.get(url).send()?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(DownloadError::Http(status));
    }
    Ok(resp.text()?)
}

/// Compute the SHA-256 hex digest of a local file.
pub fn sha256_file(path: &str) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex::encode(hasher.finalize()))
}
