//! Native Google OAuth — system-browser loopback + PKCE.
//!
//! The website cannot use `signInWithPopup` or `signInWithRedirect` inside the launcher's
//! webview because Google blocks OAuth from embedded user-agents (disallowed_useragent).
//! This module implements the correct desktop-app pattern:
//!
//!   1. Bind a TCP listener on a random loopback port.
//!   2. Build a Google auth URL with PKCE (S256) and open it in the system browser.
//!   3. Wait for the browser to redirect back with the authorization code.
//!   4. Exchange the code for tokens (no client secret — PKCE is the proof).
//!   5. Return the `access_token` to the caller.
//!
//! The website then calls `signInWithCredential(auth, GoogleAuthProvider.credential(null, accessToken))`.
//!
//! # Setup (one-time, outside this code)
//! In Google Cloud Console → APIs & Services → Credentials:
//!   - Create an OAuth client ID of type "Desktop app".
//!   - Copy the client ID (looks like `<numbers>.apps.googleusercontent.com`).
//! Bake it into release / CI builds by setting `GOOPIE_OAUTH_CLIENT_ID` in the build
//! environment before `cargo build` / `cargo tauri build`. For local dev you can also
//! export it in your shell at runtime. Desktop app client IDs are public by design.
//! No client secret is needed — the PKCE code verifier is the proof of possession.
//!
//! In Firebase Console → Authentication → Sign-in providers → Google:
//!   - Add the Desktop client ID to "Allowlist client IDs from external projects".
//!   - (Or just use the access_token path, which has no audience check.)
//!
//! The bridge wires this up as:
//!   `GoogleSignIn`          → fire-and-forget; immediately returns null.
//!   `getGoogleSignInResult` → poll; returns { status, accessToken?, message? }.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use sha2::{Digest, Sha256};

/// The Desktop OAuth client ID baked in at build time.
///
/// Set `GOOPIE_OAUTH_CLIENT_ID` in the build environment before `cargo build` / `cargo tauri build`.
/// For local dev you can also set it at runtime (as a shell export) — the runtime value is used
/// as a fallback when the compile-time one is absent.
///
/// Desktop app client IDs are public by design (Google explicitly documents this); the PKCE
/// verifier is the proof of possession, not the client ID.
fn resolve_client_id() -> Result<String, String> {
    if let Some(id) = option_env!("GOOPIE_OAUTH_CLIENT_ID") {
        return Ok(id.to_string());
    }
    std::env::var("GOOPIE_OAUTH_CLIENT_ID").map_err(|_| {
        "GOOPIE_OAUTH_CLIENT_ID must be set at build time (export before cargo build) \
         or at runtime. Create a Desktop-app OAuth client in Google Cloud Console."
            .to_string()
    })
}

/// Google requires the client secret in the token exchange even for Desktop app credentials
/// and even when PKCE is used. The secret for a Desktop app is considered public (you can't
/// protect it in a native binary), but it must still be sent. Find it in Google Cloud Console
/// → Credentials → click your Desktop app credential → Download JSON.
fn resolve_client_secret() -> Result<String, String> {
    if let Some(s) = option_env!("GOOPIE_OAUTH_CLIENT_SECRET") {
        return Ok(s.to_string());
    }
    std::env::var("GOOPIE_OAUTH_CLIENT_SECRET").map_err(|_| {
        "GOOPIE_OAUTH_CLIENT_SECRET must be set at build time or at runtime. \
         Find it in Google Cloud Console → Credentials → your Desktop app client → Download JSON."
            .to_string()
    })
}

/// Perform the full system-browser PKCE flow and return the Google `access_token`.
///
/// This blocks the calling thread until the user completes sign-in (or 5 min elapses).
/// Always call from a background thread via `std::thread::spawn`.
pub fn google_sign_in() -> Result<String, String> {
    let client_id = resolve_client_id()?;
    let client_secret = resolve_client_secret()?;

    // Bind a loopback listener on a random OS-assigned port.
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Could not bind loopback listener: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Could not get listener address: {}", e))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{}", port);

    // PKCE: generate verifier + S256 challenge.
    let verifier = make_verifier();
    let challenge = make_challenge(&verifier);
    let state_nonce = make_nonce();

    // Build the Google authorization URL.
    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth\
         ?response_type=code\
         &client_id={}\
         &redirect_uri={}\
         &scope=openid%20email%20profile\
         &code_challenge={}\
         &code_challenge_method=S256\
         &state={}\
         &access_type=offline",
        url_encode(&client_id),
        url_encode(&redirect_uri),
        challenge,
        state_nonce,
    );

    // Open the system browser (same `open` crate used by OpenExternalLink).
    open::that(&auth_url).map_err(|e| format!("Could not open system browser: {}", e))?;

    // Wait for the browser to redirect back (5-minute timeout).
    // We spin up a separate thread so we can time-box the blocking accept().
    let (tx, rx) = mpsc::channel();
    let listener_clone = listener
        .try_clone()
        .map_err(|e| format!("Listener clone failed: {}", e))?;
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener_clone.accept() {
            let _ = tx.send(stream);
        }
    });

    let mut stream = rx
        .recv_timeout(Duration::from_secs(300))
        .map_err(|_| "Sign-in timed out — browser was not completed within 5 minutes".to_string())?;

    // Read the HTTP request-line ("GET /?code=...&state=... HTTP/1.1\r\n").
    let request_line = {
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line);
        line
    };

    // Respond immediately so the browser tab shows a confirmation message.
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/html; charset=utf-8\r\n\
          Connection: close\r\n\r\n\
          <html><head><title>Goopie Launcher</title></head>\
          <body style=\"font-family:sans-serif;text-align:center;padding:3em;color:#eee;background:#1a1a2e\">\
          <h2 style=\"color:#a78bfa\">Sign-in complete!</h2>\
          <p>You can close this browser tab and return to Goopie Launcher.</p>\
          </body></html>",
    );
    drop(stream);

    // Parse query parameters from the request-line.
    let params = parse_query_from_request_line(&request_line);

    // Validate the state nonce (CSRF protection).
    if params.get("state").map(String::as_str) != Some(state_nonce.as_str()) {
        return Err("OAuth state mismatch — possible CSRF attack; sign-in aborted".to_string());
    }

    // Surface any error Google sent back (e.g. user clicked "Deny").
    if let Some(err) = params.get("error") {
        return Err(format!("Google declined sign-in: {}", err));
    }

    let code = params
        .get("code")
        .cloned()
        .ok_or_else(|| "No authorization code in the Google redirect".to_string())?;

    // Exchange the authorization code for tokens.
    exchange_code(&code, &client_id, &client_secret, &redirect_uri, &verifier)
}

// ── Token exchange ────────────────────────────────────────────────────────────

fn exchange_code(
    code: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<String, String> {
    // Build the form body manually (avoids needing reqwest's "form" feature).
    // Google requires client_secret even for Desktop app PKCE flows.
    let body = format!(
        "code={}&client_id={}&client_secret={}&redirect_uri={}&grant_type=authorization_code&code_verifier={}",
        url_encode(code),
        url_encode(client_id),
        url_encode(client_secret),
        url_encode(redirect_uri),
        url_encode(verifier),
    );

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(|e| format!("Token exchange request failed: {}", e))?;

    let status = resp.status();
    // Read as bytes; parse with serde_json directly (avoids needing reqwest's "json" feature).
    let bytes = resp
        .bytes()
        .map_err(|e| format!("Token exchange read failed: {}", e))?;
    let json: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Token exchange response is not valid JSON: {}", e))?;

    if !status.is_success() {
        let msg = json["error_description"]
            .as_str()
            .or_else(|| json["error"].as_str())
            .unwrap_or("unknown error");
        return Err(format!("Token exchange failed ({}): {}", status.as_u16(), msg));
    }

    json["access_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No access_token in token exchange response".to_string())
}

// ── PKCE helpers ──────────────────────────────────────────────────────────────

/// Generate a PKCE code verifier: 32 random bytes → base64url (43 chars, all unreserved).
fn make_verifier() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .hash(&mut h);
    std::process::id().hash(&mut h);
    let a = h.finish();
    let b = a.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let c = b ^ b.wrapping_shr(17);
    let d = c.wrapping_mul(0x6c62_272e_07bb_0142);
    // 32 bytes of pseudo-random data
    let bytes: Vec<u8> = [a, b, c, d]
        .iter()
        .flat_map(|n| n.to_le_bytes())
        .collect();
    base64url(&bytes)
}

/// Compute S256 challenge: BASE64URL(SHA256(verifier)).
fn make_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url(&hasher.finalize())
}

/// Generate a short random state nonce for CSRF protection.
fn make_nonce() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        .hash(&mut h);
    format!("{:016x}", h.finish())
}

/// URL-safe base64 without padding (RFC 4648 §5).
fn base64url(bytes: &[u8]) -> String {
    const CHARS: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4 + 2) / 3);
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i] as usize;
        let b1 = if i + 1 < bytes.len() { bytes[i + 1] as usize } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i + 2] as usize } else { 0 };
        out.push(CHARS[b0 >> 2] as char);
        out.push(CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char);
        if i + 1 < bytes.len() {
            out.push(CHARS[((b1 & 15) << 2) | (b2 >> 6)] as char);
        }
        if i + 2 < bytes.len() {
            out.push(CHARS[b2 & 63] as char);
        }
        i += 3;
    }
    out
}

// ── URL / HTTP helpers ────────────────────────────────────────────────────────

/// Percent-encode a string for use in a query parameter value.
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

/// Extract query parameters from an HTTP request-line such as:
/// `GET /?code=XYZ&state=ABC HTTP/1.1`
fn parse_query_from_request_line(line: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if let Some(path) = line.split_whitespace().nth(1) {
        if let Some(query) = path.splitn(2, '?').nth(1) {
            for pair in query.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    map.insert(url_decode(k), url_decode(v));
                }
            }
        }
    }
    map
}

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(decoded) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(decoded as char);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { ' ' } else { bytes[i] as char });
        i += 1;
    }
    out
}
