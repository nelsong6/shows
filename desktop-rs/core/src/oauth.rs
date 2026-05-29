//! auth.romaine.life user-login flow — PKCE + loopback: open the browser at
//! `/api/auth/cli/user-login`, catch the one-time code on a localhost listener,
//! exchange it at `/api/auth/cli/user-token` for the user's JWT.
//!
//! Shares the cache shape and location with the (now-retired) Go app
//! (`%APPDATA%\shows\token.json`, `{version, token, expires_at}`); `version`
//! gates cross-generation reuse.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const DEFAULT_AUTH_BASE_URL: &str = "https://auth.romaine.life";
const LOGIN_ENDPOINT: &str = "/api/auth/cli/user-login";
const TOKEN_ENDPOINT: &str = "/api/auth/cli/user-token";

/// Must match the cache version any other build writes, so a token written by
/// either is accepted by the other.
pub const CACHE_VERSION: i64 = 1;

const SIGNED_IN_HTML: &str = "<!doctype html><body style='background:#0a0a0a;color:#eee;\
font-family:monospace;padding:32px'><h2>shows: signed in</h2>\
<p>You can close this tab.</p></body>";

#[derive(Debug, Clone)]
pub struct Token {
    pub token: String,
    pub expires_at: i64,
    pub version: i64,
}

impl Token {
    pub fn expired(&self) -> bool {
        if self.token.is_empty() {
            return true;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        now + 60 >= self.expires_at // 60s skew margin, like the Go/Python clients
    }
}

fn b64url(raw: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(raw)
}

/// Random URL-safe string of at least `n_bytes` of entropy (sourced from v4
/// UUIDs, which draw from the OS CSPRNG).
fn rand_b64url(n_bytes: usize) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(n_bytes + 16);
    while buf.len() < n_bytes {
        buf.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    buf.truncate(n_bytes);
    b64url(&buf)
}

fn pkce_pair() -> (String, String) {
    let verifier = rand_b64url(32);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn cache_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))
        .unwrap_or_else(|| ".".into());
    PathBuf::from(base).join("shows").join("token.json")
}

fn load_token_at(path: &Path) -> Option<Token> {
    let text = std::fs::read_to_string(path).ok()?;
    let d: Value = serde_json::from_str(&text).ok()?;
    if d.get("version").and_then(Value::as_i64) != Some(CACHE_VERSION) {
        return None; // wrong/missing version (or a different generation) -> re-auth
    }
    Some(Token {
        token: d.get("token").and_then(Value::as_str).unwrap_or("").to_string(),
        expires_at: d.get("expires_at").and_then(Value::as_i64).unwrap_or(0),
        version: d.get("version").and_then(Value::as_i64).unwrap_or(0),
    })
}

fn save_token_at(path: &Path, tok: &Token) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(&json!({
        "version": tok.version, "token": tok.token, "expires_at": tok.expires_at,
    }))
    .map_err(|e| e.to_string())?;
    std::fs::write(path, body).map_err(|e| e.to_string())
}

pub fn load_cached_token() -> Option<Token> {
    load_token_at(&cache_path())
}

pub fn save_token(tok: &Token) -> Result<(), String> {
    save_token_at(&cache_path(), tok)
}

fn build_login_url(base_url: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let q = format!(
        "redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        urlencoding::encode(redirect_uri),
        urlencoding::encode(challenge),
        urlencoding::encode(state),
    );
    format!("{base_url}{LOGIN_ENDPOINT}?{q}")
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let v = urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_else(|_| v.to_string());
            Some((k.to_string(), v))
        })
        .collect()
}

/// Run the user-login flow: open the browser via `opener`, catch the one-time
/// code on a loopback listener, and exchange it for the user's JWT.
pub fn authenticate(
    auth_base_url: &str,
    opener: impl Fn(&str),
    timeout: Duration,
) -> Result<Token, String> {
    let (verifier, challenge) = pkce_pair();
    let state = rand_b64url(24);

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let login_url = build_login_url(auth_base_url, &redirect_uri, &challenge, &state);

    let st = state.clone();
    let handle = std::thread::spawn(move || accept_code(listener, st, timeout));
    opener(&login_url);
    let code = handle
        .join()
        .map_err(|_| "oauth: sign-in listener panicked".to_string())??;

    exchange_code(auth_base_url, &code, &verifier, &redirect_uri)
}

/// Accept loopback connections until the `/callback` carries a matching state +
/// code, the state mismatches/code is missing, or the deadline passes.
fn accept_code(listener: TcpListener, state: String, timeout: Duration) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err("oauth: sign-in window timed out".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let mut line = String::new();
                {
                    let mut reader = BufReader::new(&stream);
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                }
                let target = line.split_whitespace().nth(1).unwrap_or("");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                if path != "/callback" {
                    write_response(&mut stream, "404 Not Found", "text/plain", "not found");
                    continue;
                }
                let params = parse_query(query);
                if params.get("state").map(String::as_str) != Some(state.as_str()) {
                    write_response(&mut stream, "400 Bad Request", "text/plain", "state mismatch");
                    return Err("oauth: state mismatch on callback".into());
                }
                match params.get("code") {
                    Some(code) if !code.is_empty() => {
                        write_response(&mut stream, "200 OK", "text/html; charset=utf-8", SIGNED_IN_HTML);
                        return Ok(code.clone());
                    }
                    _ => {
                        write_response(&mut stream, "400 Bad Request", "text/plain", "missing code");
                        return Err("oauth: callback missing code".into());
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn exchange_code(auth_base_url: &str, code: &str, verifier: &str, redirect_uri: &str) -> Result<Token, String> {
    let body = json!({
        "grant_type": "authorization_code",
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": redirect_uri,
    })
    .to_string();
    let resp = ureq::post(&format!("{auth_base_url}{TOKEN_ENDPOINT}"))
        .header("Content-Type", "application/json")
        .send(body.as_bytes());
    let mut resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(c)) => return Err(format!("oauth: token exchange returned {c}")),
        Err(e) => return Err(format!("oauth: {e}")),
    };
    let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let data: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if let Some(tok) = data.get("token").and_then(Value::as_str) {
        if !tok.is_empty() {
            return Ok(Token {
                token: tok.to_string(),
                expires_at: data.get("expires_at").and_then(Value::as_i64).unwrap_or(0),
                version: CACHE_VERSION,
            });
        }
    }
    let err = data.get("error").and_then(Value::as_str).unwrap_or("token exchange failed");
    match data.get("error_description").and_then(Value::as_str) {
        Some(desc) => Err(format!("oauth: {err}: {desc}")),
        None => Err(format!("oauth: {err}")),
    }
}

/// Return a cached, unexpired token if present; otherwise run the login flow and
/// cache the result.
pub fn ensure_token(auth_base_url: &str, opener: impl Fn(&str)) -> Result<Token, String> {
    if let Some(cached) = load_cached_token() {
        if !cached.expired() {
            return Ok(cached);
        }
    }
    let tok = authenticate(auth_base_url, opener, Duration::from_secs(600))?;
    save_token(&tok)?;
    Ok(tok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_is_urlsafe_no_pad() {
        assert_eq!(b64url(b"hello"), "aGVsbG8"); // "aGVsbG8=" without padding
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let (verifier, challenge) = pkce_pair();
        assert_eq!(challenge, b64url(&Sha256::digest(verifier.as_bytes())));
        assert!(verifier.len() >= 43); // 32 bytes -> 43 url-safe base64 chars
    }

    #[test]
    fn token_expired_logic() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!(Token { token: String::new(), expires_at: now + 99999, version: 1 }.expired()); // empty
        assert!(!Token { token: "x".into(), expires_at: now + 99999, version: 1 }.expired()); // far future
        assert!(Token { token: "x".into(), expires_at: now - 10, version: 1 }.expired()); // past
    }

    #[test]
    fn token_cache_round_trip_and_version_gate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        save_token_at(&path, &Token { token: "jwt".into(), expires_at: 123456, version: CACHE_VERSION }).unwrap();
        let loaded = load_token_at(&path).unwrap();
        assert_eq!(loaded.token, "jwt");
        assert_eq!(loaded.expires_at, 123456);
        // a wrong cache version is rejected
        std::fs::write(&path, r#"{"version":999,"token":"x","expires_at":1}"#).unwrap();
        assert!(load_token_at(&path).is_none());
        // a missing file is None
        assert!(load_token_at(&dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn login_url_carries_pkce_params() {
        let u = build_login_url("https://auth.x", "http://127.0.0.1:5/callback", "CH", "ST");
        assert!(u.starts_with("https://auth.x/api/auth/cli/user-login?"));
        assert!(u.contains("code_challenge=CH"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("state=ST"));
        assert!(u.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fcallback"));
    }

    #[test]
    fn parse_query_decodes_values() {
        let p = parse_query("code=abc%2F123&state=xyz");
        assert_eq!(p.get("code").unwrap(), "abc/123");
        assert_eq!(p.get("state").unwrap(), "xyz");
    }
}
