//! Localhost control server backing the web overlay. Serves the built React
//! bundle and a same-origin control surface: the overlay subscribes to
//! `/status/stream`, can read a one-shot `/status` snapshot, reads `/shows`,
//! `/stats`, `/history`, and POSTs `/pause` `/skip` `/prev` `/defer` `/seek`
//! `/volume` `/sub` `/audio` `/sync-now` `/fullscreen` `/library/*`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use shows_core::replica::Replica;
use shows_core::runner::Runner;
use shows_core::scan;
use shows_core::sync::Syncer;

use crate::player::Player;

#[derive(rust_embed::RustEmbed)]
#[folder = "../frontend/dist"]
struct Asset;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    Minimize,
    Maximize,
    Close,
}

pub type WindowActionCb = Box<dyn Fn(WindowAction) + Send + Sync>;
type FullscreenCb = Box<dyn Fn() + Send + Sync>;
type PipCb = Box<dyn Fn() + Send + Sync>;

pub struct ControlServer {
    dist_dir: Option<PathBuf>,
    playlists: Vec<String>,
    replica: Arc<Replica>,
    status: Mutex<serde_json::Map<String, Value>>,
    status_events: StatusBroadcaster,
    status_seq: AtomicU64,
    player: Mutex<Option<Arc<Player>>>,
    runner: Mutex<Option<Arc<Runner>>>,
    syncer: Mutex<Option<Arc<Syncer>>>,
    on_fullscreen: Mutex<Option<FullscreenCb>>,
    on_pip: Mutex<Option<PipCb>>,
    on_window_action: Mutex<Option<WindowActionCb>>,
}

impl ControlServer {
    pub fn new(
        dist_dir: Option<PathBuf>,
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
        status.insert("window_maximized".into(), json!(false));
        status.insert("window_fullscreen".into(), json!(false));
        status.insert("window_pip".into(), json!(false));
        Arc::new(ControlServer {
            dist_dir,
            playlists,
            replica,
            status: Mutex::new(status),
            status_events: StatusBroadcaster::default(),
            status_seq: AtomicU64::new(0),
            player: Mutex::new(None),
            runner: Mutex::new(None),
            syncer: Mutex::new(None),
            on_fullscreen: Mutex::new(None),
            on_pip: Mutex::new(None),
            on_window_action: Mutex::new(None),
        })
    }

    pub fn set_player(&self, p: Arc<Player>) {
        *self.player.lock().unwrap() = Some(p);
        self.notify();
    }
    pub fn set_runner(&self, r: Arc<Runner>) {
        *self.runner.lock().unwrap() = Some(r);
    }
    pub fn set_syncer(&self, s: Arc<Syncer>) {
        *self.syncer.lock().unwrap() = Some(s);
        self.notify();
    }
    pub fn set_on_fullscreen(&self, f: FullscreenCb) {
        *self.on_fullscreen.lock().unwrap() = Some(f);
    }
    pub fn set_on_pip(&self, f: PipCb) {
        *self.on_pip.lock().unwrap() = Some(f);
    }
    pub fn set_on_window_action(&self, f: WindowActionCb) {
        *self.on_window_action.lock().unwrap() = Some(f);
    }

    /// Merge an object's keys into the live status (what the runner pushes).
    pub fn push(&self, updates: Value) {
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
        if let Some(ref dist_dir) = self.dist_dir {
            std::fs::read(dist_dir.join("index.html")).unwrap_or_default()
        } else {
            Asset::get("index.html")
                .map(|file| file.data.into_owned())
                .unwrap_or_default()
        }
    }

    pub fn start(self: &Arc<Self>) -> u16 {
        let server = Server::http("127.0.0.1:0").expect("bind control server");
        let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
        let this = self.clone();
        std::thread::Builder::new()
            .name("control-server".into())
            .spawn(move || {
                for request in server.incoming_requests() {
                    let req_server = this.clone();
                    std::thread::Builder::new()
                        .name("control-request".into())
                        .spawn(move || req_server.handle(request))
                        .expect("spawn control request");
                }
            })
            .expect("spawn control server");
        port
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
            (Method::Get, "/status/stream") => self.status_stream(request),
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
                respond(request, 204, vec![], "text/plain");
            }
            "/skip" => {
                self.with_runner(|r| r.skip());
                respond(request, 204, vec![], "text/plain");
            }
            "/prev" => {
                self.with_runner(|r| r.previous());
                respond(request, 204, vec![], "text/plain");
            }
            "/play-show" => {
                let b = read_body(&mut request);
                if let Some(show_id) = b.get("show_id").and_then(Value::as_str) {
                    if let Some(show) = self.replica.show(show_id) {
                        self.with_runner(|r| {
                            r.play_show(&show.id);
                        });
                    }
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/library/mark-watched" => {
                let b = read_body(&mut request);
                if let Some(id) = b.get("show_id").and_then(Value::as_str) {
                    self.replica.mark_show_watched(id);
                    self.push_sync();
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/library/mark-unwatched" => {
                let b = read_body(&mut request);
                if let Some(id) = b.get("show_id").and_then(Value::as_str) {
                    self.replica.mark_show_unwatched(id);
                    self.push_sync();
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/defer" => {
                self.with_runner(|r| r.defer());
                respond(request, 204, vec![], "text/plain");
            }
            "/fullscreen" => {
                if let Some(cb) = self.on_fullscreen.lock().unwrap().as_ref() {
                    cb();
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/pip" => {
                if let Some(cb) = self.on_pip.lock().unwrap().as_ref() {
                    cb();
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/window/minimize" => {
                if let Some(cb) = self.on_window_action.lock().unwrap().as_ref() {
                    cb(WindowAction::Minimize);
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/window/maximize" => {
                if let Some(cb) = self.on_window_action.lock().unwrap().as_ref() {
                    cb(WindowAction::Maximize);
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/window/close" => {
                if let Some(cb) = self.on_window_action.lock().unwrap().as_ref() {
                    cb(WindowAction::Close);
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/sync-now" => {
                // Manual reconcile, off-thread so the response is instant.
                if let Some(s) = self.syncer.lock().unwrap().clone() {
                    let srv = self.clone();
                    std::thread::spawn(move || {
                        s.sync();
                        srv.notify();
                    });
                }
                respond(request, 204, vec![], "text/plain");
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
                respond(request, 204, vec![], "text/plain");
            }
            "/volume" => {
                let b = read_body(&mut request);
                if let Some(v) = b.get("volume").and_then(Value::as_f64) {
                    self.with_player(|p| p.set_volume(v));
                    self.replica.set_volume(v);
                }
                respond(request, 204, vec![], "text/plain");
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
                self.with_player(|p| p.set_sub(&sid));
                respond(request, 204, vec![], "text/plain");
            }
            "/audio" => {
                let b = read_body(&mut request);
                let aid = if let Some(s) = b.get("aid").and_then(Value::as_str) {
                    Some(s.to_string())
                } else {
                    b.get("aid").and_then(Value::as_i64).map(|n| n.to_string())
                };
                if let Some(aid) = aid {
                    self.with_player(|p| p.set_audio(&aid));
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/library/add" => self.library_add(request),
            "/library/remove" => {
                let b = read_body(&mut request);
                if let Some(id) = b.get("show_id").and_then(Value::as_str) {
                    self.replica.remove_show(id);
                    self.push_sync();
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/library/update" => {
                let b = read_body(&mut request);
                if let Some(id) = b.get("show_id").and_then(Value::as_str) {
                    self.replica.update_show(
                        id,
                        b.get("name").and_then(Value::as_str),
                        b.get("root_path").and_then(Value::as_str),
                        b.get("playlist").and_then(Value::as_str),
                    );
                    self.push_sync();
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/library/pick-folder" => self.library_pick_folder(request),
            "/library/preview" => self.library_preview(request),
            "/library/rescan" => self.library_rescan(request),
            "/library/detect-unadded" => self.library_detect_unadded(request),
            _ => respond(request, 404, b"not found".to_vec(), "text/plain"),
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
        self.push_sync();
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
            return respond_json(
                request,
                400,
                &json!({"error":"root_path is required"}),
            );
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
            let show_name = shows.iter().find(|s| s.id == r.show_id).map(|s| s.name.clone()).unwrap_or_default();
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
            return respond_json(
                request,
                400,
                &json!({"error":"parent_path is required"}),
            );
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
                            if let (Some(s_name), Some(p_name)) = (s_path.file_name(), path.file_name()) {
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

    fn library_remove(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let sid = b
            .get("show_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !sid.is_empty() {
            if self.replica.remove_show(&sid) {
                self.push_sync();
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
        let mut added = 0usize;
        if let Some(show) = self.replica.show(&sid) {
            let full_sorted: Vec<String> = scan::scan_episodes(&show.root_path);
            added = self.replica.rescan_episodes(&sid, &full_sorted);
            self.push_sync();
        }
        respond_json(request, 200, &json!({"added": added}));
    }

    fn status_json(&self) -> Value {
        let mut status = self.status.lock().unwrap().clone();
        if let Some(p) = self.player.lock().unwrap().as_ref() {
            status.insert("playback".into(), p.playback_state());
        }
        if let Some(s) = self.syncer.lock().unwrap().as_ref() {
            status.insert(
                "sync".into(),
                json!({"online": s.online(), "pending": s.pending()}),
            );
        }
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
    fn push_sync(&self) {
        if let Some(s) = self.syncer.lock().unwrap().clone() {
            s.push();
        }
        self.notify();
    }

    fn static_file(&self, rel: &str) -> Option<(Vec<u8>, String)> {
        let rel = rel.trim_start_matches('/');
        if let Some(ref dist_dir) = self.dist_dir {
            let full = dist_dir.join(rel).canonicalize().ok()?;
            let dist = dist_dir.canonicalize().ok()?;
            if !full.starts_with(&dist) {
                return None; // traversal guard
            }
            let data = std::fs::read(&full).ok()?;
            Some((data, content_type(&full)))
        } else {
            let file = Asset::get(rel)?;
            let data = file.data.into_owned();
            let ctype = content_type(Path::new(rel));
            Some((data, ctype))
        }
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

struct StatusStream {
    subscriber: Arc<StatusSubscriber>,
    buffer: Vec<u8>,
    offset: usize,
}

impl Read for StatusStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        while self.offset >= self.buffer.len() {
            let mut state = self.subscriber.state.lock().unwrap();
            while state.next.is_none() {
                let (guard, timeout) = self
                    .subscriber
                    .cv
                    .wait_timeout(state, Duration::from_secs(30))
                    .unwrap();
                state = guard;
                if timeout.timed_out() && state.next.is_none() {
                    state.next = Some(": keepalive\n\n".to_string());
                }
            }
            self.buffer = state.next.take().unwrap_or_default().into_bytes();
            self.offset = 0;
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

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static response header")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

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

        hub.broadcast("stale".to_string());
        hub.broadcast("latest".to_string());

        let mut buf = [0; 6];
        stream.read_exact(&mut buf).unwrap();

        assert_eq!(&buf, b"latest");
    }
}
