//! The control contract shared by the desktop and terminal companion.
#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod preview;

pub const CONTROL_ADDR: &str = "127.0.0.1:45831";
/// The player's network port, for a remote on another PC. It serves only the remote's
/// routes, each needing a token OpenBao signed.
pub const REMOTE_PORT: u16 = 45832;
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: u64 = 1024 * 1024;

pub fn overlay_url(port: u16, development: Option<&str>, debug: bool) -> Result<String, String> {
    match (debug, development) {
        (false, None) => Ok(format!("http://127.0.0.1:{port}/")),
        (false, Some(_)) => Err("SHOWS_DEV_URL is supported only by debug builds".into()),
        (true, None) => Err("Start the live overlay with scripts/dev-shows.ps1 desktop".into()),
        (true, Some(value)) => {
            let uri: ureq::http::Uri = value.parse().map_err(|_| "invalid SHOWS_DEV_URL")?;
            let local = matches!(uri.host(), Some("localhost" | "127.0.0.1" | "[::1]"));
            if uri.scheme_str() != Some("http")
                || !local
                || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            {
                return Err("SHOWS_DEV_URL must be an HTTP loopback URL".into());
            }
            Ok(value.to_string())
        }
    }
}

pub fn control_addr() -> Result<String, String> {
    match std::env::var("SHOWS_CONTROL_PORT") {
        Ok(port) => {
            let port: u16 = port
                .parse()
                .map_err(|_| "SHOWS_CONTROL_PORT must be a port from 1 to 65535")?;
            if port == 0 {
                return Err("SHOWS_CONTROL_PORT cannot be zero".into());
            }
            Ok(format!("127.0.0.1:{port}"))
        }
        Err(std::env::VarError::NotPresent) => Ok(CONTROL_ADDR.into()),
        Err(_) => Err("SHOWS_CONTROL_PORT is not valid Unicode".into()),
    }
}

pub fn remote_port() -> Result<u16, String> {
    match std::env::var("SHOWS_REMOTE_PORT") {
        Ok(port) => port
            .parse()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| "SHOWS_REMOTE_PORT must be a port from 1 to 65535".to_string()),
        Err(std::env::VarError::NotPresent) => Ok(REMOTE_PORT),
        Err(_) => Err("SHOWS_REMOTE_PORT is not valid Unicode".into()),
    }
}

pub fn playback_ready(status: &Value) -> bool {
    let Some(pos) = status["round_pos"].as_u64() else {
        return false;
    };
    status["phase"] == "playing"
        && status["playback"]["time_pos"].is_number()
        && status["round"][pos as usize]["episode_id"].is_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status {},
    Queue {},
    Watch {},
    Preview {},
    Play {},
    Pause {},
    Skip {},
    Previous {},
    Defer {},
    Seek { seconds: f64 },
    Volume { level: f64 },
    Fullscreen {},
    PlayShow { show_id: String },
}

impl Command {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Seek { seconds } if !seconds.is_finite() || seconds.abs() > 86400.0 => {
                Err("seek must be a finite number between -86400 and 86400 seconds".into())
            }
            Self::Volume { level } if !level.is_finite() || !(0.0..=130.0).contains(level) => {
                Err("volume must be between 0 and 130".into())
            }
            Self::PlayShow { show_id } if show_id.is_empty() || show_id.len() > 256 => {
                Err("show_id must contain 1 to 256 bytes".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u64,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    Reply {
        id: u64,
        result: Result<Value, String>,
    },
    Status {
        status: Value,
    },
    Disconnected {
        message: String,
    },
    Heartbeat,
}

/// A bounded line reader for both SSH messages and SSE fields.
pub fn read_line(reader: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut line = String::new();
    let n = reader
        .take(MAX_FRAME + 1)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if n as u64 > MAX_FRAME {
        return Err("control message exceeds 1 MiB".into());
    }
    if n == 0 { Ok(None) } else { Ok(Some(line)) }
}

/// Gets a current token for each request to another PC; it renews its own.
pub type TokenSource = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

#[derive(Clone)]
pub struct Client {
    base: String,
    agent: ureq::Agent,
    token: Option<TokenSource>,
}

impl Client {
    pub fn connect() -> Result<Self, String> {
        Self::connect_to(&control_addr()?)
    }

    pub fn connect_to(address: &str) -> Result<Self, String> {
        let address: std::net::SocketAddr = address
            .parse()
            .map_err(|_| "control address must be a loopback socket address")?;
        if !address.ip().is_loopback() {
            return Err("control HTTP connections must stay on loopback".into());
        }
        Self::identified(format!("http://{address}"), None).map_err(|e| {
            format!("Shows is not available on this PC. Open the current Shows release first. {e}")
        })
    }

    /// Connects to the player on another PC, at `host:port`, with that PC's network port.
    pub fn connect_remote(authority: &str, token: TokenSource) -> Result<Self, String> {
        Self::identified(format!("http://{authority}"), Some(token))
    }

    fn identified(base: String, token: Option<TokenSource>) -> Result<Self, String> {
        let client = Self {
            base,
            agent: ureq::Agent::config_builder()
                .proxy(None)
                .max_redirects(0)
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(8)))
                .build()
                .into(),
            token,
        };
        let info = client.get("/remote/info")?;
        if info["application"] != "shows-desktop" || info["protocol"] != PROTOCOL_VERSION {
            return Err(format!("This listener is not a compatible Shows desktop: the client requires control protocol {PROTOCOL_VERSION}."));
        }
        Ok(client)
    }

    fn decode(&self, mut response: ureq::http::Response<ureq::Body>) -> Result<Value, String> {
        let code = response.status();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_FRAME)
            .read_to_string()
            .map_err(|e| e.to_string())?;
        let value: Value =
            serde_json::from_str(&body).map_err(|e| format!("invalid Shows response: {e}"))?;
        if !code.is_success() || value.get("ok") == Some(&json!(false)) {
            return Err(value["message"]
                .as_str()
                .unwrap_or("Shows rejected the request")
                .to_string());
        }
        Ok(value)
    }

    fn bearer(&self) -> Result<Option<String>, String> {
        self.token
            .as_ref()
            .map(|token| token().map(|value| format!("Bearer {value}")))
            .transpose()
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let mut request = self.agent.get(format!("{}{path}", self.base));
        if let Some(value) = self.bearer()? {
            request = request.header("Authorization", value);
        }
        self.decode(request.call().map_err(|e| e.to_string())?)
    }

    pub fn execute(&self, command: &Command) -> Result<Value, String> {
        command.validate()?;
        match command {
            Command::Status {} => self.get("/status"),
            Command::Queue {} => Ok(self.get("/status")?["round"].clone()),
            Command::Preview {} => self.get("/remote/preview"),
            Command::Watch {} => Err("watch requires a status stream".into()),
            _ => {
                let mut request = self
                    .agent
                    .post(format!("{}/remote/control", self.base))
                    .header("Content-Type", "application/json");
                if let Some(value) = self.bearer()? {
                    request = request.header("Authorization", value);
                }
                let response = request
                    .send(serde_json::to_vec(command).map_err(|e| e.to_string())?)
                    .map_err(|e| {
                        format!("Control outcome is unknown; check status before retrying. {e}")
                    })?;
                self.decode(response)
            }
        }
    }

    pub fn watch(
        &self,
        mut on_frame: impl FnMut(Frame) -> Result<(), String>,
    ) -> Result<(), String> {
        // A body timeout would cap the entire SSE lifetime. The controller
        // instead watches activity, including the desktop's 30-second comments.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(8)))
            .build()
            .into();
        let mut request = agent.get(format!("{}/status/stream", self.base));
        if let Some(value) = self.bearer()? {
            request = request.header("Authorization", value);
        }
        let response = request.call().map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("Shows refused its status stream: {}", response.status()));
        }
        let mut reader = BufReader::new(response.into_body().into_reader());
        let mut event = String::new();
        let mut data = String::new();
        let mut last_heartbeat = std::time::Instant::now();
        while let Some(line) = read_line(&mut reader)? {
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if event == "status" && !data.is_empty() {
                    on_frame(Frame::Status {
                        status: serde_json::from_str(&data)
                            .map_err(|e| format!("invalid status event: {e}"))?,
                    })?;
                }
                event.clear();
                data.clear();
            } else if line.starts_with(':') && last_heartbeat.elapsed() >= Duration::from_secs(10) {
                on_frame(Frame::Heartbeat)?;
                last_heartbeat = std::time::Instant::now();
            } else if let Some(value) = line.strip_prefix("event:") {
                event = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
                if data.len() as u64 > MAX_FRAME {
                    return Err("status event exceeds 1 MiB".into());
                }
            }
        }
        Err("Shows closed its status stream; reconnect to the running instance".into())
    }
}

/// Resolve a user-entered show against the target's current round only.
pub fn resolve_show(status: &Value, query: &str) -> Result<String, String> {
    if query.trim().is_empty() {
        return Err("show name cannot be empty".into());
    }
    let round = status["round"]
        .as_array()
        .ok_or("The target has no active round")?;
    let exact: Vec<_> = round
        .iter()
        .filter(|entry| {
            entry["show_id"].as_str() == Some(query)
                || entry["show_name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(query))
        })
        .collect();
    let matches = if exact.is_empty() {
        let query = query.to_lowercase();
        round
            .iter()
            .filter(|entry| {
                entry["show_name"]
                    .as_str()
                    .is_some_and(|n| n.to_lowercase().contains(&query))
            })
            .collect()
    } else {
        exact
    };
    match matches.as_slice() {
        [entry] => entry["show_id"]
            .as_str()
            .map(String::from)
            .ok_or("Show has no ID".into()),
        [] => Err(format!(
            "No show matching {query:?} in the target's current round. Use queue to list it."
        )),
        _ => Err(format!(
            "{query:?} matches several shows; use the full name or show ID."
        )),
    }
}

#[cfg(test)]
mod tests;
