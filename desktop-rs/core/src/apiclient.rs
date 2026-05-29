//! shows.romaine.life HTTP client — the desktop's link to the durable origin
//! under the offline-first design. The whole conversation is two calls:
//! `get_library` (pull the library to seed/reconcile the replica) and
//! `post_sync` (push locally-changed records, last-write-wins). It threads a
//! bearer JWT through every request; on 401 it invokes a refresh hook once and
//! retries (the in-place token refresh the Go client does).
//!
//! HTTP is behind the [`HttpSend`] trait so the request shaping and the 401
//! retry are unit-tested without a network; the real transport is ureq.

use std::sync::Mutex;

use serde_json::{json, Value};

use crate::model::LibraryShow;

pub const DEFAULT_BASE_URL: &str = "https://shows.romaine.life";

#[derive(Debug)]
pub struct ApiError(pub String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ApiError {}

pub type RefreshFn = Box<dyn Fn() -> String + Send + Sync>;

/// The transport seam: send a request and return `(status, body_text)`, or an
/// `Err` for a transport-level failure (which the caller treats as offline).
pub trait HttpSend: Send + Sync {
    fn send(
        &self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<&Value>,
    ) -> Result<(u16, String), String>;
}

pub struct Client {
    base_url: String,
    token: Mutex<String>,
    refresh: Option<RefreshFn>,
    http: Box<dyn HttpSend>,
}

impl Client {
    pub fn new(token: impl Into<String>, base_url: impl Into<String>, refresh: Option<RefreshFn>) -> Client {
        let base = base_url.into();
        Client::with_http(
            token,
            if base.is_empty() { DEFAULT_BASE_URL.to_string() } else { base },
            refresh,
            Box::new(UreqHttp),
        )
    }

    pub fn with_http(
        token: impl Into<String>,
        base_url: impl Into<String>,
        refresh: Option<RefreshFn>,
        http: Box<dyn HttpSend>,
    ) -> Client {
        Client {
            base_url: base_url.into(),
            token: Mutex::new(token.into()),
            refresh,
            http,
        }
    }

    pub fn token(&self) -> String {
        self.token.lock().unwrap().clone()
    }

    fn do_req(&self, method: &str, path: &str, body: Option<&Value>) -> Result<String, ApiError> {
        let url = format!("{}{}", self.base_url, path);
        let tok = self.token();
        let (mut status, mut text) = self.http.send(method, &url, &tok, body).map_err(ApiError)?;
        // 401 -> refresh once, retry. A persistent error surfaces as ApiError.
        if status == 401 {
            if let Some(refresh) = &self.refresh {
                let new_token = refresh();
                *self.token.lock().unwrap() = new_token.clone();
                let (s, t) = self.http.send(method, &url, &new_token, body).map_err(ApiError)?;
                status = s;
                text = t;
            }
        }
        if status >= 300 {
            return Err(ApiError(format!("{method} {path}: {status} {}", text.trim())));
        }
        Ok(text)
    }

    /// Pull the full library (shows + embedded episodes, incl. removed) for
    /// seeding/reconciling the local replica.
    pub fn get_library(&self, playlists: &[String]) -> Result<Vec<LibraryShow>, ApiError> {
        let q = urlencoding::encode(&playlists.join(",")).into_owned();
        let text = self.do_req("GET", &format!("/api/library?playlists={q}"), None)?;
        let data: Value = serde_json::from_str(&text).map_err(|e| ApiError(e.to_string()))?;
        match data.get("shows") {
            Some(shows) if !shows.is_null() => {
                serde_json::from_value(shows.clone()).map_err(|e| ApiError(e.to_string()))
            }
            _ => Ok(vec![]),
        }
    }

    /// Push locally-changed records; the server upserts last-write-wins. Caller
    /// passes the replica's dirty rows (already shaped to the wire contract).
    pub fn post_sync(&self, shows: Vec<Value>, episodes: Vec<Value>, history: Vec<Value>) -> Result<(), ApiError> {
        let body = json!({ "shows": shows, "episodes": episodes, "history": history });
        self.do_req("POST", "/api/sync", Some(&body))?;
        Ok(())
    }
}

/// The production transport over ureq.
struct UreqHttp;

impl HttpSend for UreqHttp {
    fn send(&self, method: &str, url: &str, token: &str, body: Option<&Value>) -> Result<(u16, String), String> {
        let auth = format!("Bearer {token}");
        let result = match method {
            "GET" => ureq::get(url).header("Authorization", &auth).call(),
            "POST" => {
                let payload = body.map(|b| b.to_string()).unwrap_or_default();
                ureq::post(url)
                    .header("Authorization", &auth)
                    .header("Content-Type", "application/json")
                    .send(payload.as_bytes())
            }
            other => return Err(format!("unsupported method {other}")),
        };
        match result {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let text = resp.body_mut().read_to_string().unwrap_or_default();
                Ok((status, text))
            }
            // ureq treats 4xx/5xx as an error by default; recover the status so
            // the client can run its 401-refresh / error logic.
            Err(ureq::Error::StatusCode(code)) => Ok((code, String::new())),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
pub(crate) struct FnHttp<F>(pub F);

#[cfg(test)]
impl<F> HttpSend for FnHttp<F>
where
    F: Fn(&str, &str, &str, Option<&Value>) -> (u16, String) + Send + Sync,
{
    fn send(&self, method: &str, url: &str, token: &str, body: Option<&Value>) -> Result<(u16, String), String> {
        Ok((self.0)(method, url, token, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client<F>(handler: F, refresh: Option<RefreshFn>) -> Client
    where
        F: Fn(&str, &str, &str, Option<&Value>) -> (u16, String) + Send + Sync + 'static,
    {
        Client::with_http("tok", "https://example.test", refresh, Box::new(FnHttp(handler)))
    }

    #[test]
    fn get_library_query_and_parse() {
        let c = client(
            |method, url, token, _body| {
                assert_eq!(method, "GET");
                assert!(url.starts_with("https://example.test/api/library?playlists="));
                assert!(url.contains("nelson%2Ccouple")); // comma percent-encoded
                assert_eq!(token, "tok");
                (
                    200,
                    json!({"shows": [
                        {"id":"s1","playlist":"nelson","name":"S1","root_path":"D:\\A","updated_at":"t"},
                        {"id":"s2","playlist":"nelson","name":"S2","root_path":"D:\\B","updated_at":"t"}
                    ]})
                    .to_string(),
                )
            },
            None,
        );
        let out = c.get_library(&["nelson".into(), "couple".into()]).unwrap();
        assert_eq!(out.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["s1", "s2"]);
    }

    #[test]
    fn get_library_missing_key_is_empty() {
        let c = client(|_, _, _, _| (200, "{}".into()), None);
        assert!(c.get_library(&["nelson".into()]).unwrap().is_empty());
    }

    #[test]
    fn post_sync_body_shape() {
        let seen: std::sync::Arc<Mutex<Option<Value>>> = Default::default();
        let seen2 = seen.clone();
        let c = client(
            move |method, url, _token, body| {
                assert_eq!(method, "POST");
                assert!(url.ends_with("/api/sync"));
                *seen2.lock().unwrap() = body.cloned();
                (204, String::new())
            },
            None,
        );
        c.post_sync(vec![json!({"id":"s1"})], vec![json!({"id":"e1"})], vec![json!({"id":"h1"})])
            .unwrap();
        assert_eq!(
            seen.lock().unwrap().clone().unwrap(),
            json!({"shows":[{"id":"s1"}],"episodes":[{"id":"e1"}],"history":[{"id":"h1"}]})
        );
    }

    #[test]
    fn refreshes_token_once_and_retries_on_401() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = calls.clone();
        let c = client(
            move |_method, _url, token, _body| {
                let n = calls2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    assert_eq!(token, "tok");
                    (401, json!({"error":"expired"}).to_string())
                } else {
                    assert_eq!(token, "tok2"); // refreshed
                    (200, json!({"shows": []}).to_string())
                }
            },
            Some(Box::new(|| "tok2".to_string())),
        );
        c.get_library(&["nelson".into()]).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(c.token(), "tok2");
    }

    #[test]
    fn persistent_error_raises_apierror() {
        let c = client(|_, _, _, _| (500, "boom".into()), None);
        let err = c.get_library(&["nelson".into()]).unwrap_err();
        assert!(err.to_string().contains("500"));
    }
}
