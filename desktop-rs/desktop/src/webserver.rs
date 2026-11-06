//! Control servers. The localhost one backs the web overlay and serves the built React
//! bundle and a same-origin control surface: the overlay subscribes to
//! `/status/stream`, can read a one-shot `/status` snapshot, reads `/shows`,
//! `/stats`, `/history`, and POSTs `/pause` `/skip` `/prev` `/defer` `/seek`
//! `/volume` `/sub` `/audio` `/fullscreen` `/stay-on-top` `/window/*`
//! `/library/*`.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use shows_core::remote_token::TokenChecker;
use shows_core::replica::Replica;
use shows_core::runner::{ControlOutcome, Runner};
use shows_core::scan;

use crate::player::Player;

#[cfg(not(debug_assertions))]
#[derive(rust_embed::RustEmbed)]
#[folder = "../frontend/dist"]
struct Asset;

fn embedded_asset(path: &str) -> Option<Vec<u8>> {
    #[cfg(not(debug_assertions))]
    { Asset::get(path).map(|file| file.data.into_owned()) }
    #[cfg(debug_assertions)]
    { let _ = path; None }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    Minimize,
    Maximize,
    Close,
    BeginDrag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPhase {
    Syncing,
    Fetching,
    Playing,
    Drained,
    Error,
}

impl StatusPhase {
    fn as_str(self) -> &'static str {
        match self {
            StatusPhase::Syncing => "syncing",
            StatusPhase::Fetching => "fetching",
            StatusPhase::Playing => "playing",
            StatusPhase::Drained => "drained",
            StatusPhase::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusRoundEntry {
    pub show_id: String,
    pub show_name: String,
    pub episode_id: String,
    pub playlist: String,
    pub order_value: u32,
    pub absolute_path: String,
}

#[derive(Debug, Clone)]
pub struct StatusRemovedShow {
    pub id: String,
    pub name: String,
    pub date_added: String,
    pub last_played_at: String,
}

#[derive(Debug, Clone)]
pub struct StatusFileSyncProblem {
    pub episode_id: String,
    pub show_name: String,
    pub source_path: String,
    pub local_path: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct StatusFileSync {
    pub copied: usize,
    pub cached: usize,
    pub missing: usize,
    pub failed: usize,
    pub summary: String,
    pub incomplete: bool,
    pub problems: Vec<StatusFileSyncProblem>,
}

#[derive(Debug, Clone)]
pub struct StatusPatch {
    fields: serde_json::Map<String, Value>,
}

impl StatusPatch {
    pub fn phase(phase: StatusPhase, message: impl Into<String>) -> StatusPatch {
        let mut fields = serde_json::Map::new();
        fields.insert("phase".into(), json!(phase.as_str()));
        fields.insert("message".into(), json!(message.into()));
        if !matches!(phase, StatusPhase::Error) {
            fields.insert("error_kind".into(), Value::Null);
            fields.insert("round_blocked".into(), json!(false));
        }
        StatusPatch { fields }
    }

    pub fn playing(round: Vec<StatusRoundEntry>, pos: usize, round_id: i64) -> StatusPatch {
        let mut patch =
            StatusPatch::phase(StatusPhase::Playing, format!("round of {}", round.len()));
        patch.fields.insert(
            "round".into(),
            Value::Array(
                round
                    .into_iter()
                    .map(|e| {
                        json!({
                            "show_id": e.show_id,
                            "show_name": e.show_name,
                            "episode_id": e.episode_id,
                            "playlist": e.playlist,
                            "order_value": e.order_value,
                            "absolute_path": e.absolute_path,
                        })
                    })
                    .collect(),
            ),
        );
        patch.fields.insert("round_pos".into(), json!(pos));
        patch.fields.insert("round_id".into(), json!(round_id));
        patch
    }

    pub fn advance(removed_shows: Vec<StatusRemovedShow>, advanced_count: usize) -> StatusPatch {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "last_advance".into(),
            json!({
                "advanced_count": advanced_count,
                "removed_shows": removed_shows
                    .into_iter()
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "name": r.name,
                            "date_added": r.date_added,
                            "last_played_at": r.last_played_at,
                        })
                    })
                    .collect::<Vec<_>>()
            }),
        );
        StatusPatch { fields }
    }

    pub fn drained() -> StatusPatch {
        let mut patch = StatusPatch::phase(StatusPhase::Drained, "every show finished");
        patch.fields.insert("round".into(), json!([]));
        patch.fields.insert("round_id".into(), Value::Null);
        patch
    }

    pub fn round_unplayable(message: impl Into<String>) -> StatusPatch {
        let mut patch = StatusPatch::phase(StatusPhase::Error, message);
        patch
            .fields
            .insert("error_kind".into(), json!("round_unplayable"));
        patch.fields.insert("round_blocked".into(), json!(true));
        patch.fields.insert("round_id".into(), Value::Null);
        patch
    }

    pub fn file_sync(report: StatusFileSync) -> StatusPatch {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "file_sync".into(),
            json!({
                "copied": report.copied,
                "cached": report.cached,
                "missing": report.missing,
                "failed": report.failed,
                "summary": report.summary,
                "incomplete": report.incomplete,
                "problems": report.problems
                    .into_iter()
                    .map(|p| {
                        json!({
                            "episode_id": p.episode_id,
                            "show_name": p.show_name,
                            "source_path": p.source_path,
                            "local_path": p.local_path,
                            "reason": p.reason,
                        })
                    })
                    .collect::<Vec<_>>()
            }),
        );
        StatusPatch { fields }
    }

    pub fn round_pos(pos: usize) -> StatusPatch {
        let mut fields = serde_json::Map::new();
        fields.insert("round_pos".into(), json!(pos));
        StatusPatch { fields }
    }

    pub fn window_state(maximized: bool, fullscreen: bool, on_top: bool) -> StatusPatch {
        let mut fields = serde_json::Map::new();
        fields.insert("window_maximized".into(), json!(maximized));
        fields.insert("window_fullscreen".into(), json!(fullscreen));
        fields.insert("window_on_top".into(), json!(on_top));
        StatusPatch { fields }
    }
}

pub type WindowActionCb = Box<dyn Fn(WindowAction) + Send + Sync>;
type FullscreenCb = Box<dyn Fn() -> Result<(), String> + Send + Sync>;
// Takes the desired pin state, not a flip — the overlay sends the target it
// wants so duplicate POSTs from one click converge instead of cancelling out.
type StayOnTopCb = Box<dyn Fn(bool) + Send + Sync>;
pub type PreviewCb = Box<dyn Fn() -> Result<shows_control::preview::VideoPreview, String> + Send + Sync>;

pub struct ControlServer {
    playlists: Vec<String>,
    replica: Arc<Replica>,
    status: Mutex<serde_json::Map<String, Value>>,
    status_events: StatusBroadcaster,
    status_seq: AtomicU64,
    player: Mutex<Option<Arc<Player>>>,
    runner: Mutex<Option<Arc<Runner>>>,
    on_fullscreen: Mutex<Option<FullscreenCb>>,
    on_stay_on_top: Mutex<Option<StayOnTopCb>>,
    on_window_action: Mutex<Option<WindowActionCb>>,
    on_preview: Mutex<Option<PreviewCb>>,
    preview_cache: Mutex<Option<(std::time::Instant, shows_control::preview::VideoPreview)>>,
    remote_tokens: TokenChecker,
}

impl ControlServer {
    pub fn new(
        replica: Arc<Replica>,
        playlists: Vec<String>,
    ) -> Arc<ControlServer> {
        let mut status = serde_json::Map::new();
        status.insert("phase".into(), json!("initializing"));
        status.insert("message".into(), json!("starting up"));
        status.insert("playlist".into(), json!(playlists.join(", ")));
        status.insert("round".into(), json!([]));
        status.insert("round_pos".into(), json!(0));
        status.insert("round_id".into(), json!(null));
        status.insert("error_kind".into(), Value::Null);
        status.insert("round_blocked".into(), json!(false));
        status.insert("window_maximized".into(), json!(false));
        status.insert("window_fullscreen".into(), json!(false));
        status.insert("window_on_top".into(), json!(false));
        Arc::new(ControlServer {
            playlists,
            replica,
            status: Mutex::new(status),
            status_events: StatusBroadcaster::default(),
            status_seq: AtomicU64::new(0),
            player: Mutex::new(None),
            runner: Mutex::new(None),
            on_fullscreen: Mutex::new(None),
            on_stay_on_top: Mutex::new(None),
            on_window_action: Mutex::new(None),
            on_preview: Mutex::new(None),
            preview_cache: Mutex::new(None),
            remote_tokens: TokenChecker::openbao(),
        })
    }

    pub fn set_player(&self, p: Arc<Player>) {
        *self.player.lock().unwrap() = Some(p);
        self.notify();
    }
    pub fn set_runner(&self, r: Arc<Runner>) {
        *self.runner.lock().unwrap() = Some(r);
    }
    pub fn set_on_fullscreen(&self, f: FullscreenCb) {
        *self.on_fullscreen.lock().unwrap() = Some(f);
    }
    pub fn set_on_preview(&self, f: PreviewCb) {
        *self.on_preview.lock().unwrap() = Some(f);
    }
    pub fn set_on_stay_on_top(&self, f: StayOnTopCb) {
        *self.on_stay_on_top.lock().unwrap() = Some(f);
    }
    pub fn set_on_window_action(&self, f: WindowActionCb) {
        *self.on_window_action.lock().unwrap() = Some(f);
    }

    pub fn push_status(&self, patch: StatusPatch) {
        self.push_raw(Value::Object(patch.fields));
    }

    /// Merge an object's keys into the live status. Prefer [`StatusPatch`];
    /// this remains for compositor callbacks that already produce window-state JSON.
    pub fn push_raw(&self, updates: Value) {
        let mut changed = false;
        if let Value::Object(map) = updates {
            let mut s = self.status.lock().unwrap();
            for (k, v) in map {
                s.insert(k, v);
            }
            changed = true;
        }
        if changed {
            self.notify();
        }
    }

    /// Broadcast the current status projection to open `/status/stream` clients.
    pub fn notify(&self) {
        let frame = self.status_frame(&self.status_json());
        self.status_events.broadcast(frame);
    }

    pub fn index_html(&self) -> Vec<u8> {
        embedded_asset("index.html").unwrap_or_default()
    }

    pub fn start(self: &Arc<Self>) -> u16 {
        let address = shows_control::control_addr().expect("invalid Shows control port");
        let server = Server::http(&address).expect("bind Shows loopback control server");
        let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
        self.serve(server, "control", Self::handle);
        match shows_control::remote_port().and_then(|remote| {
            Server::http(("0.0.0.0", remote)).map_err(|e| format!("port {remote}: {e}"))
        }) {
            Ok(remote) => self.serve(remote, "remote", Self::handle_remote),
            // Playback on this PC doesn't need it; a remote elsewhere reports it can't connect.
            Err(message) => log::error!("remote control port unavailable: {message}"),
        }
        port
    }

    fn serve(self: &Arc<Self>, server: Server, name: &str, handler: fn(&Arc<Self>, Request)) {
        let this = self.clone();
        let requests = format!("{name}-request");
        std::thread::Builder::new()
            .name(format!("{name}-server"))
            .spawn(move || {
                for request in server.incoming_requests() {
                    let req_server = this.clone();
                    std::thread::Builder::new()
                        .name(requests.clone())
                        .spawn(move || handler(&req_server, request))
                        .expect("spawn control request");
                }
            })
            .expect("spawn control server");
    }

    /// The network port: the remote's routes alone, each behind a token OpenBao signed.
    /// Library, window and overlay routes stay on the loopback port.
    fn handle_remote(self: &Arc<Self>, request: Request) {
        if request.headers().iter().any(|h| h.field.equiv("Origin")) {
            return respond_json(request, 403, &json!({"ok": false, "message": "remote controls require a native client"}));
        }
        let token = request.headers().iter()
            .find(|h| h.field.equiv("Authorization"))
            .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
            .unwrap_or("")
            .to_owned();
        if let Err(message) = self.remote_tokens.check(&token) {
            log::warn!("remote request refused from {:?}: {message}", request.remote_addr());
            return respond_json(request, 401, &json!({"ok": false, "message": format!("Shows refused this remote: {message}")}));
        }
        let method = request.method().clone();
        let path = request.url().split('?').next().unwrap_or("").to_string();
        match (&method, path.as_str()) {
            (Method::Get, "/remote/info") => respond_json(request, 200, &Self::remote_info()),
            (Method::Get, "/status") => respond_json(request, 200, &self.status_json()),
            (Method::Get, "/status/stream") => self.status_stream(request),
            (Method::Get, "/remote/preview") => self.video_preview(request),
            (Method::Post, "/remote/control") => self.remote_control(request),
            _ => respond_json(request, 404, &json!({"ok": false, "message": "not a remote route"})),
        }
    }

    fn remote_info() -> Value {
        json!({
            "application": "shows-desktop",
            "protocol": shows_control::PROTOCOL_VERSION,
            "build": option_env!("SHOWS_BUILD_SHA").unwrap_or("source"),
            "computer": std::env::var("COMPUTERNAME").unwrap_or_default(),
            "pid": std::process::id(),
        })
    }

    fn handle(self: &Arc<Self>, request: Request) {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();
        match (&method, path.as_str()) {
            (Method::Get, "/") => {
                respond(request, 200, self.index_html(), "text/html; charset=utf-8")
            }
            (Method::Get, p) if p.starts_with("/index") => {
                respond(request, 200, self.index_html(), "text/html; charset=utf-8")
            }
            (Method::Get, "/status") => respond_json(request, 200, &self.status_json()),
            (Method::Get, "/remote/info") => respond_json(request, 200, &Self::remote_info()),
            (Method::Get, "/status/stream") => self.status_stream(request),
            (Method::Get, "/remote/preview") => self.video_preview(request),
            (Method::Get, "/shows") => {
                let v = json!(self.replica.overlay_shows(&self.playlists));
                respond_json(request, 200, &v)
            }
            (Method::Get, "/stats") => {
                respond_json(request, 200, &self.replica.stats(&self.playlists))
            }
            (Method::Get, "/history") => {
                let show = query_param(&url, "show").unwrap_or_default();
                let v = json!(self.replica.show_history(&show));
                respond_json(request, 200, &v)
            }
            (Method::Get, "/health") => respond(request, 200, b"ok".to_vec(), "text/plain"),
            (Method::Get, "/library/browse") => {
                let target_path = query_param(&url, "path").unwrap_or_default();
                if target_path.is_empty() {
                    respond_json(request, 200, &json!(list_drives()));
                } else {
                    match read_directories(&target_path) {
                        Ok(dirs) => respond_json(request, 200, &json!(dirs)),
                        Err(e) => respond_json(request, 500, &json!({"error": e.to_string()})),
                    }
                }
            }
            (Method::Get, "/library/next-round") => self.library_next_round(request),
            (Method::Get, _) => match self.static_file(&path) {
                Some((data, ctype)) => respond(request, 200, data, &ctype),
                None => respond(request, 404, b"not found".to_vec(), "text/plain"),
            },
            (Method::Post, p) => self.handle_post(request, p, &url),
            _ => respond(request, 404, b"not found".to_vec(), "text/plain"),
        }
    }

    fn handle_post(self: &Arc<Self>, mut request: Request, path: &str, _url: &str) {
        match path {
            "/remote/control" => self.remote_control(request),
            "/pause" => {
                let mut did_explicit = false;
                if let Some(query) = request.url().split('?').nth(1) {
                    for pair in query.split('&') {
                        if pair == "state=true" {
                            self.with_player(|p| p.set_pause(true));
                            did_explicit = true;
                        } else if pair == "state=false" {
                            self.with_player(|p| p.set_pause(false));
                            did_explicit = true;
                        }
                    }
                }
                if !did_explicit {
                    self.with_player(|p| p.toggle_pause());
                }
                respond_control_ok(
                    request,
                    "pause_updated",
                    if did_explicit {
                        "pause state updated"
                    } else {
                        "pause toggled"
                    },
                );
            }
            "/skip" => {
                self.respond_runner_outcome(request, |r| r.skip());
            }
            "/prev" => {
                self.respond_runner_outcome(request, |r| r.previous());
            }
            "/play-show" => {
                let b = read_body(&mut request);
                let Some(show_id) = b.get("show_id").and_then(Value::as_str) else {
                    return respond_json(
                        request,
                        400,
                        &json!({
                            "ok": false,
                            "status": "missing_show_id",
                            "message": "show_id is required",
                        }),
                    );
                };
                let Some(show) = self.replica.show(show_id) else {
                    return respond_json(
                        request,
                        404,
                        &json!({
                            "ok": false,
                            "status": "show_not_found",
                            "message": format!("show {show_id} was not found"),
                        }),
                    );
                };
                self.respond_runner_outcome(request, |r| r.play_show(&show.id));
            }
            "/library/mark-watched" => {
                let b = read_body(&mut request);
                let Some(id) = b.get("show_id").and_then(Value::as_str) else {
                    return respond_json(
                        request,
                        400,
                        &json!({
                            "ok": false,
                            "status": "missing_show_id",
                            "message": "show_id is required",
                        }),
                    );
                };
                self.replica.mark_show_watched(id);
                self.notify_database_change();
                respond_control_ok(request, "show_marked_watched", "show marked watched");
            }
            "/library/mark-unwatched" => {
                let b = read_body(&mut request);
                let Some(id) = b.get("show_id").and_then(Value::as_str) else {
                    return respond_json(
                        request,
                        400,
                        &json!({
                            "ok": false,
                            "status": "missing_show_id",
                            "message": "show_id is required",
                        }),
                    );
                };
                self.replica.mark_show_unwatched(id);
                self.notify_database_change();
                respond_control_ok(request, "show_marked_unwatched", "show marked unwatched");
            }
            "/defer" => {
                self.respond_runner_outcome(request, |r| r.defer());
            }
            "/round/remove-entry" => {
                let b = read_body(&mut request);
                let Some(episode_id) = b.get("episode_id").and_then(Value::as_str) else {
                    return respond_json(
                        request,
                        400,
                        &json!({
                            "ok": false,
                            "status": "missing_episode_id",
                            "message": "episode_id is required",
                        }),
                    );
                };
                let Some((_show_id, playlist)) = self.replica.remove_round_entry(episode_id) else {
                    return respond_json(
                        request,
                        404,
                        &json!({
                            "ok": false,
                            "status": "round_entry_not_found",
                            "message": format!("episode {episode_id} is not in the current round"),
                        }),
                    );
                };
                self.notify_database_change();
                respond_json(
                    request,
                    200,
                    &json!({
                        "ok": true,
                        "status": "round_entry_removed",
                        "message": format!("removed one {playlist} round entry; reload the round to apply it"),
                    }),
                );
            }
            "/round/reload" => {
                self.push_status(StatusPatch::phase(
                    StatusPhase::Fetching,
                    "reloading repaired round",
                ));
                self.respond_runner_outcome(request, |r| r.reload_round());
            }
            "/fullscreen" => {
                self.respond_fullscreen(request);
            }
            "/stay-on-top" => {
                // Idempotent set: the body carries the desired pin state
                // (`{"on_top": bool}`). The overlay sends the target it wants, so
                // a single click delivered as two POSTs lands on one state rather
                // than flipping twice and undoing itself.
                let b = read_body(&mut request);
                let on_top = b.get("on_top").and_then(Value::as_bool);
                let from = b.get("from").and_then(Value::as_bool);
                log::info!("/stay-on-top POST on_top={on_top:?} from={from:?}");
                if let Some(on_top) = on_top {
                    if let Some(cb) = self.on_stay_on_top.lock().unwrap().as_ref() {
                        cb(on_top);
                    }
                }
                respond_control_ok(request, "stay_on_top_set", "stay on top set");
            }
            "/window/minimize" => {
                if let Some(cb) = self.on_window_action.lock().unwrap().as_ref() {
                    cb(WindowAction::Minimize);
                }
                respond_control_ok(request, "window_minimized", "window minimized");
            }
            "/window/maximize" => {
                if let Some(cb) = self.on_window_action.lock().unwrap().as_ref() {
                    cb(WindowAction::Maximize);
                }
                respond_control_ok(request, "window_maximized", "window maximize toggled");
            }
            "/window/close" => {
                if let Some(cb) = self.on_window_action.lock().unwrap().as_ref() {
                    cb(WindowAction::Close);
                }
                respond_control_ok(request, "window_close_requested", "closing window");
            }
            "/window/begin-drag" => {
                if let Some(cb) = self.on_window_action.lock().unwrap().as_ref() {
                    cb(WindowAction::BeginDrag);
                }
                respond_control_ok(request, "window_drag_started", "window drag started");
            }
            "/seek" => {
                let b = read_body(&mut request);
                self.with_player(|p| {
                    if let Some(pct) = b.get("percent").and_then(Value::as_f64) {
                        p.seek_percent(pct);
                    } else if let Some(s) = b.get("seconds").and_then(Value::as_f64) {
                        p.seek_relative(s);
                    }
                });
                respond_control_ok(request, "seek_updated", "seek updated");
            }
            "/volume" => {
                let b = read_body(&mut request);
                if let Some(v) = b.get("volume").and_then(Value::as_f64) {
                    self.with_player(|p| p.set_volume(v));
                    self.replica.set_volume(v);
                }
                respond_control_ok(request, "volume_updated", "volume updated");
            }
            "/sub" => {
                let b = read_body(&mut request);
                let sid = if let Some(s) = b.get("sid").and_then(Value::as_str) {
                    s.to_string()
                } else if let Some(n) = b.get("sid").and_then(Value::as_i64) {
                    n.to_string()
                } else {
                    "no".to_string()
                };
                if let Some(runner) = self.runner.lock().unwrap().clone() {
                    respond_control_outcome(request, runner.set_current_subtitle_track(&sid));
                } else {
                    self.with_player(|p| p.set_sub(&sid));
                    respond_control_ok(request, "subtitle_updated", "subtitle track updated");
                }
            }
            "/audio" => {
                let b = read_body(&mut request);
                let aid = if let Some(s) = b.get("aid").and_then(Value::as_str) {
                    Some(s.to_string())
                } else {
                    b.get("aid").and_then(Value::as_i64).map(|n| n.to_string())
                };
                if let Some(aid) = aid {
                    if let Some(runner) = self.runner.lock().unwrap().clone() {
                        respond_control_outcome(request, runner.set_current_audio_track(&aid));
                    } else {
                        self.with_player(|p| p.set_audio(&aid));
                        respond_control_ok(request, "audio_updated", "audio track updated");
                    }
                } else {
                    respond_control_ok(request, "audio_updated", "audio track updated");
                }
            }
            "/library/add" => self.library_add(request),
            "/library/remove" => {
                let b = read_body(&mut request);
                let Some(id) = b.get("show_id").and_then(Value::as_str) else {
                    return respond_json(
                        request,
                        400,
                        &json!({
                            "ok": false,
                            "status": "missing_show_id",
                            "message": "show_id is required",
                        }),
                    );
                };
                let removed = self.replica.remove_show(id);
                if removed {
                    self.notify_database_change();
                    respond_control_ok(request, "show_removed", "show removed");
                } else {
                    respond_json(
                        request,
                        404,
                        &json!({
                            "ok": false,
                            "status": "show_not_found",
                            "message": format!("show {id} was not found"),
                        }),
                    );
                }
            }
            "/library/update" => {
                let b = read_body(&mut request);
                let Some(id) = b.get("show_id").and_then(Value::as_str) else {
                    return respond_json(
                        request,
                        400,
                        &json!({
                            "ok": false,
                            "status": "missing_show_id",
                            "message": "show_id is required",
                        }),
                    );
                };
                self.replica.update_show(
                    id,
                    b.get("name").and_then(Value::as_str),
                    b.get("root_path").and_then(Value::as_str),
                    b.get("playlist").and_then(Value::as_str),
                );
                self.notify_database_change();
                respond_control_ok(request, "show_updated", "show updated");
            }
            "/library/pick-folder" => self.library_pick_folder(request),
            "/library/preview" => self.library_preview(request),
            "/library/rescan" => self.library_rescan(request),
            "/library/rescan-watched" => self.library_rescan_watched(request),
            "/library/detect-unadded" => self.library_detect_unadded(request),
            "/library/detect-new-episodes" => self.library_detect_new_episodes(request),
            "/library/show-details" => self.library_show_details(request),
            _ => respond(request, 404, b"not found".to_vec(), "text/plain"),
        }
    }

    fn respond_fullscreen(&self, request: Request) {
        let result = self.on_fullscreen.lock().unwrap().as_ref()
            .ok_or_else(|| "The Shows window is unavailable".to_string())
            .and_then(|callback| callback());
        match result {
            Ok(()) => respond_control_ok(request, "fullscreen_requested", "fullscreen toggle requested; window status confirms the result"),
            Err(message) => respond_json(request, 503, &json!({"ok": false, "message": message})),
        }
    }

    fn video_preview(&self, request: Request) {
        if request.headers().iter().any(|h| h.field.equiv("Origin")) {
            return respond_json(request, 403, &json!({"ok":false,"message":"Video preview requires a native client"}));
        }
        let Ok(mut cache) = self.preview_cache.try_lock() else {
            return respond_json(request, 429, &json!({"ok":false,"message":"Video preview is busy"}));
        };
        if let Some((at, frame)) = cache.as_ref() {
            if at.elapsed() < shows_control::preview::INTERVAL {
                return respond_json(request, 200, &json!(frame));
            }
        }
        let result = self.on_preview.lock().unwrap().as_ref()
            .ok_or_else(|| "The target video window is unavailable".to_string())
            .and_then(|capture| capture());
        match result {
            Ok(frame) => {
                log::debug!("video preview: capture {:.2} ms, {} characters", frame.capture_ms, shows_control::preview::WIDTH * shows_control::preview::HEIGHT);
                if frame.capture_ms > 20.0 { log::warn!("Slow video preview capture: {:.2} ms", frame.capture_ms); }
                respond_json(request, 200, &json!(frame));
                *cache = Some((std::time::Instant::now(), frame));
            }
            Err(message) => {
                *cache = None;
                respond_json(request, 503, &json!({"ok":false,"message":message}));
            }
        }
    }

    fn remote_control(&self, mut request: Request) {
        use shows_control::Command;
        if request.headers().iter().any(|h| h.field.equiv("Origin")) ||
            !request.headers().iter().any(|h| h.field.equiv("Content-Type") && h.value.as_str() == "application/json") {
            return respond_json(request, 403, &json!({"ok": false, "message": "remote controls require a native JSON client"}));
        }
        let mut body = Vec::new();
        if request.as_reader().take(shows_control::MAX_FRAME + 1).read_to_end(&mut body).is_err() || body.len() as u64 > shows_control::MAX_FRAME {
            return respond_json(request, 413, &json!({"ok": false, "message": "control body unreadable or larger than 1 MiB"}));
        }
        let command: Command = match serde_json::from_slice(&body) {
            Ok(command) => command,
            Err(e) => return respond_json(request, 400, &json!({"ok": false, "message": e.to_string()})),
        };
        if let Err(message) = command.validate() {
            return respond_json(request, 400, &json!({"ok": false, "message": message}));
        }
        let status = self.status_json();
        if !matches!(&command, Command::Fullscreen {}) && !shows_control::playback_ready(&status) {
            return respond_json(request, 503, &json!({"ok": false, "message": "Shows has no playable episode loaded; check status"}));
        }
        log::info!("remote control: {command:?}");
        match command {
            Command::Fullscreen {} => self.respond_fullscreen(request),
            Command::Skip {} => self.respond_runner_outcome(request, |r| r.skip()),
            Command::Previous {} => self.respond_runner_outcome(request, |r| r.previous()),
            Command::Defer {} => self.respond_runner_outcome(request, |r| r.defer()),
            Command::PlayShow { show_id } => self.respond_runner_outcome(request, |r| r.play_show(&show_id)),
            Command::Play {} | Command::Pause {} | Command::Seek { .. } | Command::Volume { .. } => {
                let result = {
                    let player = self.player.lock().unwrap();
                    match player.as_ref() {
                        Some(p) => match command {
                            Command::Play {} => p.set_pause_confirmed(false),
                            Command::Pause {} => p.set_pause_confirmed(true),
                            Command::Seek { seconds } => p.seek_confirmed(seconds),
                            Command::Volume { level } => p.set_volume_confirmed(level),
                            _ => unreachable!(),
                        },
                        None => Err("player is unavailable".into()),
                    }
                };
                match result {
                    Ok(()) => {
                        if let Command::Volume { level } = command { self.replica.set_volume(level); }
                        self.notify();
                        respond_control_ok(request, "control_applied", "control applied by the target player");
                    }
                    Err(message) => respond_json(request, 503, &json!({"ok": false, "message": message})),
                }
            }
            _ => respond_json(request, 400, &json!({"ok": false, "message": "use the status and stream endpoints for reads"})),
        }
    }

    fn library_add(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let name = b
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let root = b
            .get("root_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let playlist = b
            .get("playlist")
            .and_then(Value::as_str)
            .unwrap_or("nelson")
            .trim()
            .to_string();
        if name.is_empty() || root.is_empty() {
            return respond_json(
                request,
                400,
                &json!({"error":"name and root_path are required"}),
            );
        }
        let eps = scan::scan_episodes(&root);
        if eps.is_empty() {
            return respond_json(
                request,
                400,
                &json!({"error":"no video files found under root_path"}),
            );
        }
        let sid = self.replica.create_show(&playlist, &name, &root, &eps);
        self.notify_database_change();
        respond_json(request, 200, &json!({"id": sid, "episodes": eps.len()}));
    }

    fn library_pick_folder(self: &Arc<Self>, request: Request) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            respond_json(request, 200, &json!({"path": path.to_string_lossy()}));
        } else {
            respond_json(request, 200, &json!({"path": null}));
        }
    }

    fn library_preview(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let root = b
            .get("root_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if root.is_empty() {
            return respond_json(request, 400, &json!({"error":"root_path is required"}));
        }
        let eps = scan::scan_episodes(&root);
        respond_json(request, 200, &json!({"episodes": eps}));
    }

    fn library_next_round(self: &Arc<Self>, request: Request) {
        // We assume playlist "nelson" for now, or take it from query params.
        // Let's just use the active shows for all playlists we know about, or default to nelson.
        let shows = self.replica.active_shows(&["nelson".to_string()]);
        let round = shows_core::engine::next_round(&shows);

        let mut results = Vec::new();
        for r in round {
            // Find the show name to return
            let show_name = shows
                .iter()
                .find(|s| s.id == r.show_id)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            results.push(json!({
                "show_id": r.show_id,
                "show_name": show_name,
                "episode_id": r.episode_id,
                "relative_path": r.absolute_path,
            }));
        }
        respond_json(request, 200, &json!({"next_round": results}));
    }

    fn library_detect_unadded(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let parent = b
            .get("parent_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if parent.is_empty() {
            return respond_json(request, 400, &json!({"error":"parent_path is required"}));
        }

        let mut unadded = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&parent) {
            let existing_shows = self.replica.all_shows();
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type() {
                    if file_type.is_dir() {
                        let path = entry.path();
                        let path_str = path.to_string_lossy().to_string();
                        // Check if any existing show has a matching folder name
                        // This handles the migration from D:\Downloads\Group-Nelson to S:\Group-Nelson
                        let is_added = existing_shows.iter().any(|s| {
                            let s_path = std::path::Path::new(&s.root_path);
                            if let (Some(s_name), Some(p_name)) =
                                (s_path.file_name(), path.file_name())
                            {
                                s_name == p_name
                            } else {
                                s_path == path.as_path()
                            }
                        });
                        if !is_added {
                            unadded.push(path_str);
                        }
                    }
                }
            }
        }
        unadded.sort();
        respond_json(request, 200, &json!({"unadded": unadded}));
    }

    fn library_detect_new_episodes(self: &Arc<Self>, request: Request) {
        let mut shows_with_new = Vec::new();
        let all_shows = self.replica.all_shows();

        for show in all_shows {
            let scanned: Vec<String> = scan::scan_episodes(&show.root_path);
            let existing = self.replica.episode_paths(&show.id);
            let mut new_eps = Vec::new();

            for ep in scanned {
                if !existing.contains(&ep) {
                    new_eps.push(ep);
                }
            }

            if !new_eps.is_empty() {
                shows_with_new.push(json!({
                    "show_id": show.id,
                    "show_name": show.name,
                    "new_episodes": new_eps,
                }));
            }
        }

        respond_json(request, 200, &json!({"shows": shows_with_new}));
    }

    fn library_remove(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let sid = b
            .get("show_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !sid.is_empty() {
            if self.replica.remove_show(&sid) {
                self.notify_database_change();
            }
        }
        respond_json(request, 200, &json!({}));
    }

    fn library_rescan(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let sid = b
            .get("show_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let specific_episodes: Option<Vec<String>> =
            b.get("episodes").and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });
        let mut added = 0usize;
        if let Some(show) = self.replica.show(&sid) {
            let full_sorted: Vec<String> =
                specific_episodes.unwrap_or_else(|| scan::scan_episodes(&show.root_path));
            let added_ids = self.replica.rescan_episodes(&sid, &full_sorted);
            added = added_ids.len();
            self.notify_database_change();
        }
        respond_json(request, 200, &json!({"added": added}));
    }

    fn library_rescan_watched(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let sid = b
            .get("show_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let specific_episodes: Option<Vec<String>> =
            b.get("episodes").and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });
        let mut added = 0usize;
        if let Some(show) = self.replica.show(&sid) {
            let full_sorted: Vec<String> =
                specific_episodes.unwrap_or_else(|| scan::scan_episodes(&show.root_path));
            let added_ids = self.replica.rescan_episodes(&sid, &full_sorted);
            added = added_ids.len();
            if added > 0 {
                self.replica.mark_episodes_watched(&sid, &added_ids);
            }
            self.notify_database_change();
        }
        respond_json(request, 200, &json!({"added": added}));
    }

    fn library_show_details(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let sid = b
            .get("show_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if let Some(show) = self.replica.show(&sid) {
            let history = self.replica.show_history(&sid);
            let first_unwatched_id =
                shows_core::engine::first_unwatched(&show.episodes).map(|e| e.id.clone());

            let mut all_episodes = Vec::new();
            let mut previous = Vec::new();
            let mut upcoming = Vec::new();

            for ep in &show.episodes {
                let ep_val = json!({
                    "id": ep.id,
                    "relative_path": ep.relative_path,
                    "position": ep.position,
                    "watched_at": ep.watched_at,
                    "resume_pos": ep.resume_pos,
                });

                all_episodes.push(ep_val.clone());

                if ep.watched_at.is_some() {
                    previous.push(ep_val.clone());
                } else {
                    if Some(&ep.id) != first_unwatched_id.as_ref() {
                        upcoming.push(ep_val);
                    }
                }
            }

            respond_json(
                request,
                200,
                &json!({
                    "show": {
                        "id": show.id,
                        "name": show.name,
                        "playlist": show.playlist,
                        "root_path": show.root_path,
                    },
                    "all_episodes": all_episodes,
                    "previous_episodes": previous,
                    "upcoming_episodes": upcoming,
                    "history": history,
                }),
            );
        } else {
            respond_json(request, 404, &json!({"error": "not found"}));
        }
    }

    fn status_json(&self) -> Value {
        let mut status = self.status.lock().unwrap().clone();
        if let Some(p) = self.player.lock().unwrap().as_ref() {
            status.insert("playback".into(), p.playback_state());
        }
        status.insert("alerts".into(), status_alerts(&status));
        Value::Object(status)
    }

    fn status_frame(&self, status: &Value) -> String {
        let id = self.status_seq.fetch_add(1, Ordering::Relaxed) + 1;
        sse_frame(id, "status", status)
    }

    fn status_stream(&self, request: Request) {
        let reader = self
            .status_events
            .subscribe(self.status_frame(&self.status_json()));
        let headers = vec![
            header("Content-Type", "text/event-stream; charset=utf-8"),
            header("Cache-Control", "no-store"),
            header("X-Accel-Buffering", "no"),
        ];
        let response =
            Response::new(StatusCode(200), headers, reader, None, None).with_chunked_threshold(0);
        let _ = request.respond(response);
    }

    fn with_player(&self, f: impl FnOnce(&Player)) {
        if let Some(p) = self.player.lock().unwrap().as_ref() {
            f(p);
        }
    }
    fn with_runner(&self, f: impl FnOnce(&Runner)) {
        if let Some(r) = self.runner.lock().unwrap().as_ref() {
            f(r);
        }
    }
    fn respond_runner_outcome(&self, request: Request, f: impl FnOnce(&Runner) -> ControlOutcome) {
        let outcome = self.runner.lock().unwrap().as_ref().map(|r| f(r.as_ref()));
        match outcome {
            Some(outcome) => respond_control_outcome(request, outcome),
            None => respond_json(
                request,
                503,
                &json!({
                    "ok": false,
                    "status": "runner_unavailable",
                    "message": "runner is not ready",
                }),
            ),
        }
    }
    fn notify_database_change(&self) {
        // Mutations are already committed to the authoritative NAS database.
        self.notify();
    }

    fn static_file(&self, rel: &str) -> Option<(Vec<u8>, String)> {
        let rel = rel.trim_start_matches('/');
        let data = embedded_asset(rel)?;
        Some((data, content_type(Path::new(rel))))
    }
}

#[derive(Default)]
struct StatusBroadcaster {
    subscribers: Mutex<Vec<Weak<StatusSubscriber>>>,
}

impl StatusBroadcaster {
    fn subscribe(&self, initial: String) -> StatusStream {
        let subscriber = Arc::new(StatusSubscriber {
            state: Mutex::new(StatusSubscriberState {
                next: Some(initial),
            }),
            cv: Condvar::new(),
        });
        self.subscribers
            .lock()
            .unwrap()
            .push(Arc::downgrade(&subscriber));
        StatusStream {
            subscriber,
            buffer: Vec::new(),
            offset: 0,
            flush_pending: false,
        }
    }

    fn broadcast(&self, frame: String) {
        let live = {
            let mut subscribers = self.subscribers.lock().unwrap();
            let mut live = Vec::new();
            subscribers.retain(|weak| {
                if let Some(subscriber) = weak.upgrade() {
                    live.push(subscriber);
                    true
                } else {
                    false
                }
            });
            live
        };

        for subscriber in live {
            subscriber.replace(frame.clone());
        }
    }
}

struct StatusSubscriber {
    state: Mutex<StatusSubscriberState>,
    cv: Condvar,
}

impl StatusSubscriber {
    fn replace(&self, frame: String) {
        self.state.lock().unwrap().next = Some(frame);
        self.cv.notify_one();
    }
}

struct StatusSubscriberState {
    next: Option<String>,
}

/// A >1 KiB SSE comment used purely to force a socket flush. tiny_http buffers
/// the response in a 1024-byte `BufWriter` and never flushes a streaming body
/// (`raw_print` "does not flush the writer"), so a sub-1 KiB frame sits unsent
/// until enough following bytes overflow the buffer — which, on an idle stream,
/// may be never (e.g. unpin while the video is paused: no playback frames
/// follow, the overlay never sees the change, the titlebar never returns).
/// Writing this oversized comment right after a frame overflows the buffer and
/// pushes the frame to the socket at once. Clients ignore lines starting ':'.
fn flush_comment() -> Vec<u8> {
    let mut v = Vec::with_capacity(1203);
    v.push(b':');
    v.resize(1201, b' ');
    v.extend_from_slice(b"\n\n");
    v
}

struct StatusStream {
    subscriber: Arc<StatusSubscriber>,
    buffer: Vec<u8>,
    offset: usize,
    // True once a real frame has been handed out and not yet flushed: the next
    // read, if no further frame is queued, emits `flush_comment()` to push it.
    flush_pending: bool,
}

impl Read for StatusStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        while self.offset >= self.buffer.len() {
            let mut state = self.subscriber.state.lock().unwrap();
            if let Some(frame) = state.next.take() {
                // A frame is queued: deliver it. It will be flushed either by the
                // bytes of the following frame or, if the stream then idles, by
                // the flush comment below.
                drop(state);
                self.buffer = frame.into_bytes();
                self.offset = 0;
                self.flush_pending = !self.buffer.is_empty();
                break;
            }
            if self.flush_pending {
                // Just delivered a frame and nothing follows yet — force it out
                // to the socket now instead of letting it sit in the BufWriter.
                self.flush_pending = false;
                drop(state);
                self.buffer = flush_comment();
                self.offset = 0;
                break;
            }
            // Genuinely idle: block for the next frame, or a keepalive on timeout.
            let (mut guard, timeout) = self
                .subscriber
                .cv
                .wait_timeout(state, Duration::from_secs(30))
                .unwrap();
            if timeout.timed_out() && guard.next.is_none() {
                guard.next = Some(": keepalive\n\n".to_string());
            }
        }

        let n = out.len().min(self.buffer.len() - self.offset);
        out[..n].copy_from_slice(&self.buffer[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
    }
}

fn sse_frame(id: u64, event: &str, value: &Value) -> String {
    let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let data = data.replace('\n', "\ndata: ");
    format!("id: {id}\nevent: {event}\ndata: {data}\n\n")
}

fn content_type(p: &Path) -> String {
    let ct = match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    };
    ct.to_string()
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split('?').nth(1)?;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key {
            Some(urlencoding_decode(v))
        } else {
            None
        }
    })
}

fn urlencoding_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    result.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        if bytes[i] == b'+' {
            result.push(b' ');
        } else {
            result.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

fn list_drives() -> Vec<Value> {
    let mut drives = Vec::new();
    for c in b'A'..=b'Z' {
        let drive_root = format!("{}:\\", c as char);
        if Path::new(&drive_root).exists() {
            drives.push(json!({
                "name": format!("{}:", c as char),
                "path": drive_root,
                "is_drive": true
            }));
        }
    }
    drives
}

#[cfg(target_os = "windows")]
fn is_hidden(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    if let Ok(metadata) = path.metadata() {
        let attributes = metadata.file_attributes();
        // FILE_ATTRIBUTE_HIDDEN = 0x2, FILE_ATTRIBUTE_SYSTEM = 0x4
        (attributes & 0x6) != 0
    } else {
        false
    }
}

#[cfg(not(target_os = "windows"))]
fn is_hidden(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        name.starts_with('.')
    } else {
        false
    }
}

fn read_directories(path: &str) -> std::io::Result<Vec<Value>> {
    let mut dirs = Vec::new();
    let entries = std::fs::read_dir(path)?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if is_hidden(&p) {
                continue;
            }
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                dirs.push(json!({
                    "name": name,
                    "path": p.to_string_lossy().to_string(),
                }));
            }
        }
    }
    dirs.sort_by(|a, b| {
        let name_a = a["name"].as_str().unwrap_or("").to_lowercase();
        let name_b = b["name"].as_str().unwrap_or("").to_lowercase();
        name_a.cmp(&name_b)
    });
    Ok(dirs)
}

fn read_body(request: &mut Request) -> Value {
    let mut s = String::new();
    if request.as_reader().read_to_string(&mut s).is_ok() && !s.is_empty() {
        serde_json::from_str(&s).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    }
}

fn status_alerts(status: &serde_json::Map<String, Value>) -> Value {
    let mut alerts = Vec::new();
    let round_unplayable = status
        .get("error_kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "round_unplayable");
    if let Some(file_sync) = status.get("file_sync").and_then(Value::as_object) {
        if file_sync
            .get("incomplete")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let summary = file_sync
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("some files were not ready");
            let mut detail_parts = Vec::new();
            if let Some(problems) = file_sync.get("problems").and_then(Value::as_array) {
                for problem in problems.iter().take(3) {
                    let show = problem
                        .get("show_name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown show");
                    let reason = problem
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("file unavailable");
                    let source = problem
                        .get("source_path")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if source.is_empty() {
                        detail_parts.push(format!("{show}: {reason}"));
                    } else {
                        detail_parts.push(format!("{show}: {reason} ({source})"));
                    }
                }
            }
            let detail = if detail_parts.is_empty() {
                "Check the show paths in library or open the log for exact file paths.".to_string()
            } else {
                detail_parts.join("\n")
            };
            if round_unplayable {
                alerts.push(json!({
                    "level": "danger",
                    "title": "Round has no playable media",
                    "message": status
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("No files in this round could be opened."),
                    "detail": detail,
                }));
                return Value::Array(alerts);
            }
            alerts.push(json!({
                "level": "warning",
                "title": "Round file sync incomplete",
                "message": summary,
                "detail": detail,
            }));
        }
    }

    Value::Array(alerts)
}

fn respond(request: Request, code: u16, body: Vec<u8>, ctype: &str) {
    let mut resp = Response::from_data(body).with_status_code(code);
    if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()) {
        resp = resp.with_header(h);
    }
    if let Ok(h) = Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]) {
        resp = resp.with_header(h);
    }
    let _ = request.respond(resp);
}

fn respond_json(request: Request, code: u16, v: &Value) {
    respond(
        request,
        code,
        serde_json::to_vec(v).unwrap_or_default(),
        "application/json",
    );
}

fn control_outcome_json(outcome: &ControlOutcome) -> Value {
    json!({
        "ok": outcome.ok(),
        "status": outcome.status(),
        "message": outcome.message(),
    })
}

fn respond_control_outcome(request: Request, outcome: ControlOutcome) {
    let code = if outcome.ok() { 200 } else { 409 };
    respond_json(request, code, &control_outcome_json(&outcome));
}

fn respond_control_ok(request: Request, status: &str, message: &str) {
    respond_json(
        request,
        200,
        &json!({
            "ok": true,
            "status": status,
            "message": message,
        }),
    );
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static response header")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn remote_fullscreen_needs_a_window_but_not_a_loaded_episode() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for available in [None, Some(false), Some(true)] {
            let control = ControlServer::new(Arc::new(Replica::new(":memory:")), vec![]);
            let calls = Arc::new(AtomicUsize::new(0));
            if let Some(success) = available {
                let calls = calls.clone();
                control.set_on_fullscreen(Box::new(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if success { Ok(()) } else { Err("UI message failed".into()) }
                }));
            }
            let revision = control.replica.revision();
            let listener = Server::http("127.0.0.1:0").unwrap();
            let address = listener.server_addr().to_string();
            let copy = control.clone();
            let worker = std::thread::spawn(move || {
                for _ in 0..2 { copy.handle(listener.recv_timeout(Duration::from_secs(3)).unwrap().unwrap()); }
            });
            let client = shows_control::Client::connect_to(&address).unwrap();
            let result = client.execute(&shows_control::Command::Fullscreen {});
            assert_eq!(result.is_ok(), available == Some(true));
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(available.is_some()));
            assert_eq!(control.status_json()["window_fullscreen"], false);
            assert_eq!(control.replica.revision(), revision);
            worker.join().unwrap();
        }
    }

    #[test]
    fn native_remote_reports_unavailable_player_without_mutating_shared_state() {
        let control = ControlServer::new(Arc::new(Replica::new(":memory:")), vec!["nelson".into()]);
        let revision = control.replica.revision();
        let listener = Server::http("127.0.0.1:0").unwrap();
        let address = listener.server_addr().to_string();
        let copy = control.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..2 {
                copy.handle(listener.recv_timeout(Duration::from_secs(3)).unwrap().unwrap());
            }
        });
        let client = shows_control::Client::connect_to(&address).unwrap();
        assert!(client.execute(&shows_control::Command::Play {}).unwrap_err().contains("no playable episode"));
        worker.join().unwrap();
        assert_eq!(control.replica.revision(), revision);
    }

    #[test]
    fn preview_reads_are_bounded_cached_and_do_not_change_playback_or_database() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let control = ControlServer::new(Arc::new(Replica::new(":memory:")), vec![]);
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_calls = calls.clone();
        control.set_on_preview(Box::new(move || {
            capture_calls.fetch_add(1, Ordering::SeqCst);
            shows_control::preview::VideoPreview::from_rgba(&vec![0; 160 * 90 * 4], 16.0 / 9.0, 1.0)
        }));
        let before = control.status_json();
        let revision = control.replica.revision();
        let listener = Server::http("127.0.0.1:0").unwrap();
        let address = listener.server_addr().to_string();
        let copy = control.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..3 { copy.handle(listener.recv_timeout(Duration::from_secs(3)).unwrap().unwrap()); }
        });
        let client = shows_control::Client::connect_to(&address).unwrap();
        let first = client.execute(&shows_control::Command::Preview {}).unwrap();
        let second = client.execute(&shows_control::Command::Preview {}).unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.status_json(), before);
        assert_eq!(control.replica.revision(), revision);
        worker.join().unwrap();
    }

    #[test]
    fn the_network_port_serves_only_the_remotes_routes_and_only_with_a_token() {
        use std::io::Write;
        let control = ControlServer::new(Arc::new(Replica::new(":memory:")), vec![]);
        let listener = Server::http("127.0.0.1:0").unwrap();
        let address = listener.server_addr().to_string();
        let requests = [
            ("GET /status", "", "401"),
            ("GET /status", "Authorization: Bearer forged\r\n", "401"),
            ("POST /window/close", "", "401"),
            ("GET /status", "Origin: https://example.com\r\n", "403"),
        ];
        let copy = control.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..requests.len() {
                copy.handle_remote(listener.recv_timeout(Duration::from_secs(3)).unwrap().unwrap());
            }
        });
        for (line, headers, code) in requests {
            let mut socket = std::net::TcpStream::connect(&address).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            write!(socket, "{line} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}Content-Length: 0\r\n\r\n").unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {code}")), "{line}: {response}");
        }
        worker.join().unwrap();
    }

    #[test]
    fn a_valid_token_reaches_the_remotes_routes_and_nothing_else() {
        use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
        use std::io::Write;
        const KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgKL/xHHDaM1YrKnv2
mDuMrlN4kCyK6H6Zctd4R+n5AAmhRANCAAS5N+qDIf54e0j5JWN5q2/oG8JJm9YN
RXcElwx4NzzIjreNOTuPo860Vm9cKijgXPCxamwY+f4Lt32uD/NqurAW
-----END PRIVATE KEY-----
";
        let mut control = ControlServer::new(Arc::new(Replica::new(":memory:")), vec![]);
        Arc::get_mut(&mut control).unwrap().remote_tokens = TokenChecker::with_fetch(Box::new(|| {
            serde_json::from_value(json!({"keys":[{"kty":"EC","crv":"P-256","use":"sig","alg":"ES256","kid":"k1",
                "x":"uTfqgyH-eHtI-SVjeatv6BvCSZvWDUV3BJcMeDc8yI4","y":"t405O4-jzrRWb1wqKOBc8LFqbBj5_gu3fa4P82q6sBY"}]}))
                .map_err(|e| e.to_string())
        }));
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("k1".into());
        let claims = json!({"aud":"shows","iss":shows_core::remote_token::ISSUER,"exp":chrono::Utc::now().timestamp() + 300});
        let token = encode(&header, &claims, &EncodingKey::from_ec_pem(KEY.as_bytes()).unwrap()).unwrap();
        let listener = Server::http("127.0.0.1:0").unwrap();
        let address = listener.server_addr().to_string();
        let requests = [
            ("GET /remote/info", "200"),
            ("GET /status", "200"),
            ("POST /window/close", "404"),
            ("POST /library/add", "404"),
            ("GET /library/browse", "404"),
            ("GET /", "404"),
        ];
        let copy = control.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..requests.len() {
                copy.handle_remote(listener.recv_timeout(Duration::from_secs(3)).unwrap().unwrap());
            }
        });
        for (line, code) in requests {
            let mut socket = std::net::TcpStream::connect(&address).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            write!(socket, "{line} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n").unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {code}")), "{line}: {response}");
        }
        worker.join().unwrap();
    }

    #[test]
    fn remote_rejects_browser_requests_before_applying_controls() {
        use std::io::Write;
        for headers in ["Content-Type: text/plain\r\n", "Content-Type: application/json\r\nOrigin: https://example.com\r\n"] {
            let control = ControlServer::new(Arc::new(Replica::new(":memory:")), vec![]);
            let listener = Server::http("127.0.0.1:0").unwrap();
            let address = listener.server_addr().to_string();
            let worker = std::thread::spawn(move || {
                control.handle(listener.recv_timeout(Duration::from_secs(3)).unwrap().unwrap());
            });
            let mut socket = std::net::TcpStream::connect(address).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let body = r#"{"action":"skip"}"#;
            write!(socket, "POST /remote/control HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            assert!(response.starts_with("HTTP/1.1 403"));
            worker.join().unwrap();
        }
    }

    #[test]
    fn sse_frame_names_status_event_with_json_payload() {
        let frame = sse_frame(7, "status", &json!({"phase": "playing", "round_pos": 2}));

        assert!(frame.starts_with("id: 7\nevent: status\n"));
        assert!(frame.contains("data: {\"phase\":\"playing\",\"round_pos\":2}\n\n"));
    }

    #[test]
    fn status_stream_starts_with_initial_snapshot() {
        let hub = StatusBroadcaster::default();
        let mut stream = hub.subscribe("initial".to_string());
        let mut buf = [0; 7];

        stream.read_exact(&mut buf).unwrap();

        assert_eq!(&buf, b"initial");
    }

    #[test]
    fn status_stream_coalesces_pending_updates_to_latest() {
        let hub = StatusBroadcaster::default();
        let mut stream = hub.subscribe("first".to_string());
        let mut initial = [0; 5];
        stream.read_exact(&mut initial).unwrap();
        assert_eq!(&initial, b"first");

        hub.broadcast("stale".to_string());
        hub.broadcast("latest".to_string());

        // A frame is already queued when we read, so it is delivered directly
        // (no flush comment); the single slot has coalesced "stale" to "latest".
        let mut buf = [0; 6];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"latest");
    }

    #[test]
    fn status_stream_flushes_idle_frame_with_oversized_comment() {
        // Regression for the "titlebar never returns on unpin while paused" bug:
        // tiny_http's 1024-byte BufWriter holds a frame unsent until following
        // bytes overflow it. When the stream would otherwise idle after a frame,
        // the reader emits a >1 KiB comment that forces that frame to the socket.
        let hub = StatusBroadcaster::default();
        let mut stream = hub.subscribe("frame".to_string());
        let mut frame = [0; 5];
        stream.read_exact(&mut frame).unwrap();
        assert_eq!(&frame, b"frame");

        let flush = flush_comment();
        assert!(flush.len() > 1024, "comment must exceed the 1024-byte BufWriter");
        assert!(flush.starts_with(b":") && flush.ends_with(b"\n\n"));
        let mut buf = vec![0u8; flush.len()];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(buf, flush);
    }

    #[test]
    fn identical_shell_state_is_rebroadcast_as_an_acknowledgement() {
        let server = ControlServer::new(Arc::new(Replica::new(":memory:")),
            vec!["nelson".to_string()],
        );

        server.push_raw(json!({"window_on_top": false}));
        let after_first = server.status_seq.load(Ordering::Relaxed);
        server.push_raw(json!({"window_on_top": false}));
        let after_second = server.status_seq.load(Ordering::Relaxed);

        assert_eq!(after_second, after_first + 1);
        assert_eq!(server.status_json()["window_on_top"], false);
    }

    #[test]
    fn control_outcome_json_reports_noop_status() {
        let value = control_outcome_json(&ControlOutcome::Noop {
            status: "show_not_in_round",
            message: "show s1 is not in the current round".into(),
        });

        assert_eq!(value["ok"], false);
        assert_eq!(value["status"], "show_not_in_round");
        assert_eq!(value["message"], "show s1 is not in the current round");
    }

    #[test]
    fn initial_status_snapshot_has_frontend_contract_fields() {
        let server = ControlServer::new(Arc::new(Replica::new(":memory:")),
            vec!["nelson".to_string()],
        );
        let value = server.status_json();

        assert_eq!(value["phase"], "initializing");
        assert_eq!(value["message"], "starting up");
        assert_eq!(value["playlist"], "nelson");
        assert!(value["round"].is_array());
        assert_eq!(value["round_pos"], 0);
        assert!(value["round_id"].is_null());
        assert!(value["error_kind"].is_null());
        assert_eq!(value["round_blocked"], false);
        assert_eq!(value["window_maximized"], false);
        assert_eq!(value["window_fullscreen"], false);
        assert_eq!(value["window_on_top"], false);
        assert!(value.get("startup_sync").is_none());
        assert!(value.get("sync").is_none());
        assert!(value["alerts"].is_array());
    }

    #[test]
    fn non_error_phase_patch_clears_blocked_round_state() {
        let patch = StatusPatch::phase(StatusPhase::Fetching, "loading round");
        let value = Value::Object(patch.fields);

        assert_eq!(value["phase"], "fetching");
        assert!(value["error_kind"].is_null());
        assert_eq!(value["round_blocked"], false);
    }

    #[test]
    fn status_alerts_include_file_sync_problem_examples() {
        let mut status = serde_json::Map::new();
        status.insert(
            "file_sync".into(),
            json!({
                "incomplete": true,
                "summary": "37 copied, 1 missing",
                "problems": [{
                    "show_name": "Example",
                    "source_path": "S:\\missing.mkv",
                    "local_path": "D:\\Downloads\\Watching\\Example\\missing.mkv",
                    "reason": "source file missing"
                }]
            }),
        );

        let alerts = status_alerts(&status);
        assert_eq!(alerts[0]["level"], "warning");
        assert!(alerts[0]["detail"].as_str().unwrap().contains("Example"));
    }

    #[test]
    fn status_alerts_promote_unplayable_round_file_sync() {
        let mut status = serde_json::Map::new();
        status.insert("error_kind".into(), json!("round_unplayable"));
        status.insert(
            "message".into(),
            json!("no playable media - check that the show files are reachable"),
        );
        status.insert(
            "file_sync".into(),
            json!({
                "incomplete": true,
                "summary": "1 missing",
                "problems": [{
                    "show_name": "Example",
                    "source_path": "S:\\missing.mkv",
                    "local_path": "D:\\Downloads\\Watching\\Example\\missing.mkv",
                    "reason": "source file missing"
                }]
            }),
        );

        let alerts = status_alerts(&status);
        assert_eq!(alerts[0]["level"], "danger");
        assert_eq!(alerts[0]["title"], "Round has no playable media");
        assert!(
            alerts[0]["detail"]
                .as_str()
                .unwrap()
                .contains("S:\\missing.mkv")
        );
        assert_eq!(alerts.as_array().unwrap().len(), 1);
    }

    #[test]
    fn status_patch_playing_has_required_round_fields() {
        let patch = StatusPatch::playing(
            vec![StatusRoundEntry {
                show_id: "s1".into(),
                show_name: "Show".into(),
                episode_id: "e1".into(),
                playlist: "nelson".into(),
                order_value: 7,
                absolute_path: "D:\\Watching\\Show\\e1.mkv".into(),
            }],
            0,
            3,
        );
        let value = Value::Object(patch.fields);

        assert_eq!(value["phase"], "playing");
        assert_eq!(value["message"], "round of 1");
        assert_eq!(value["round_pos"], 0);
        assert_eq!(value["round_id"], 3);
        assert_eq!(value["round"][0]["show_id"], "s1");
        assert_eq!(value["round_blocked"], false);
        assert!(value["error_kind"].is_null());
    }

    #[test]
    fn status_patch_round_unplayable_marks_blocked_round() {
        let patch = StatusPatch::round_unplayable("no playable media");
        let value = Value::Object(patch.fields);

        assert_eq!(value["phase"], "error");
        assert_eq!(value["message"], "no playable media");
        assert_eq!(value["error_kind"], "round_unplayable");
        assert_eq!(value["round_blocked"], true);
        assert!(value["round_id"].is_null());
    }

    #[test]
    fn status_patch_drained_clears_round_identity() {
        let patch = StatusPatch::drained();
        let value = Value::Object(patch.fields);

        assert_eq!(value["phase"], "drained");
        assert_eq!(value["round"].as_array().unwrap().len(), 0);
        assert!(value["round_id"].is_null());
    }
}
