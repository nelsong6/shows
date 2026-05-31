//! Localhost control server backing the web overlay. Serves the built React
//! bundle and a same-origin control surface: the overlay polls `/status`,
//! `/shows`, `/stats`, `/history`, and POSTs `/pause` `/skip` `/prev` `/defer`
//! `/seek` `/volume` `/sub` `/audio` `/sync-now` `/fullscreen` `/library/*`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use shows_core::replica::Replica;
use shows_core::runner::Runner;
use shows_core::scan;
use shows_core::sync::Syncer;

use crate::player::Player;

type FullscreenCb = Box<dyn Fn() + Send + Sync>;

pub struct ControlServer {
    dist_dir: PathBuf,
    playlists: Vec<String>,
    replica: Arc<Replica>,
    status: Mutex<serde_json::Map<String, Value>>,
    player: Mutex<Option<Arc<Player>>>,
    runner: Mutex<Option<Arc<Runner>>>,
    syncer: Mutex<Option<Arc<Syncer>>>,
    on_fullscreen: Mutex<Option<FullscreenCb>>,
}

impl ControlServer {
    pub fn new(dist_dir: PathBuf, replica: Arc<Replica>, playlists: Vec<String>) -> Arc<ControlServer> {
        let mut status = serde_json::Map::new();
        status.insert("phase".into(), json!("initializing"));
        status.insert("message".into(), json!("starting up"));
        status.insert("playlist".into(), json!(playlists.join(", ")));
        status.insert("round".into(), json!([]));
        status.insert("round_pos".into(), json!(0));
        status.insert("round_id".into(), json!(null));
        Arc::new(ControlServer {
            dist_dir,
            playlists,
            replica,
            status: Mutex::new(status),
            player: Mutex::new(None),
            runner: Mutex::new(None),
            syncer: Mutex::new(None),
            on_fullscreen: Mutex::new(None),
        })
    }

    pub fn set_player(&self, p: Arc<Player>) {
        *self.player.lock().unwrap() = Some(p);
    }
    pub fn set_runner(&self, r: Arc<Runner>) {
        *self.runner.lock().unwrap() = Some(r);
    }
    pub fn set_syncer(&self, s: Arc<Syncer>) {
        *self.syncer.lock().unwrap() = Some(s);
    }
    pub fn set_on_fullscreen(&self, f: FullscreenCb) {
        *self.on_fullscreen.lock().unwrap() = Some(f);
    }

    /// Merge an object's keys into the live status (what the runner pushes).
    pub fn push(&self, updates: Value) {
        if let Value::Object(map) = updates {
            let mut s = self.status.lock().unwrap();
            for (k, v) in map {
                s.insert(k, v);
            }
        }
    }

    pub fn index_html(&self) -> Vec<u8> {
        std::fs::read(self.dist_dir.join("index.html")).unwrap_or_default()
    }

    pub fn start(self: &Arc<Self>) -> u16 {
        let server = Server::http("127.0.0.1:0").expect("bind control server");
        let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
        let this = self.clone();
        std::thread::Builder::new()
            .name("control-server".into())
            .spawn(move || {
                for request in server.incoming_requests() {
                    this.handle(request);
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
            (Method::Get, "/") => respond(request, 200, self.index_html(), "text/html; charset=utf-8"),
            (Method::Get, p) if p.starts_with("/index") => {
                respond(request, 200, self.index_html(), "text/html; charset=utf-8")
            }
            (Method::Get, "/status") => respond_json(request, 200, &self.status_json()),
            (Method::Get, "/shows") => {
                let v = json!(self.replica.overlay_shows(&self.playlists));
                respond_json(request, 200, &v)
            }
            (Method::Get, "/stats") => respond_json(request, 200, &self.replica.stats(&self.playlists)),
            (Method::Get, "/history") => {
                let show = query_param(&url, "show").unwrap_or_default();
                let v = json!(self.replica.show_history(&show));
                respond_json(request, 200, &v)
            }
            (Method::Get, "/health") => respond(request, 200, b"ok".to_vec(), "text/plain"),
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
                self.with_player(|p| p.toggle_pause());
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
                        if let Some(ep) = shows_core::engine::first_unwatched(&show.episodes) {
                            self.with_runner(|r| {
                                r.play_episode(&show, ep);
                            });
                        }
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
            "/sync-now" => {
                // Manual reconcile, off-thread so the response is instant.
                if let Some(s) = self.syncer.lock().unwrap().clone() {
                    std::thread::spawn(move || {
                        s.sync();
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
                }
                respond(request, 204, vec![], "text/plain");
            }
            "/sub" => {
                let b = read_body(&mut request);
                let sid = b.get("sid").and_then(Value::as_str).unwrap_or("no").to_string();
                self.with_player(|p| p.set_sub(&sid));
                respond(request, 204, vec![], "text/plain");
            }
            "/audio" => {
                let b = read_body(&mut request);
                if let Some(aid) = b.get("aid").and_then(Value::as_str) {
                    self.with_player(|p| p.set_audio(aid));
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
            "/library/rescan" => self.library_rescan(request),
            _ => respond(request, 404, b"not found".to_vec(), "text/plain"),
        }
    }

    fn library_add(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let name = b.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let root = b.get("root_path").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let playlist = b.get("playlist").and_then(Value::as_str).unwrap_or("nelson").trim().to_string();
        if name.is_empty() || root.is_empty() {
            return respond_json(request, 400, &json!({"error":"name and root_path are required"}));
        }
        let eps = scan::scan_episodes(&root);
        if eps.is_empty() {
            return respond_json(request, 400, &json!({"error":"no video files found under root_path"}));
        }
        let sid = self.replica.create_show(&playlist, &name, &root, &eps);
        self.push_sync();
        respond_json(request, 200, &json!({"id": sid, "episodes": eps.len()}));
    }

    fn library_rescan(self: &Arc<Self>, mut request: Request) {
        let b = read_body(&mut request);
        let sid = b.get("show_id").and_then(Value::as_str).unwrap_or("").to_string();
        let mut added = 0usize;
        if let Some(show) = self.replica.show(&sid) {
            let known = self.replica.episode_paths(&sid);
            let new: Vec<String> = scan::scan_episodes(&show.root_path)
                .into_iter()
                .filter(|f| !known.contains(f))
                .collect();
            added = self.replica.add_episodes(&sid, &new);
            if added > 0 {
                self.push_sync();
            }
        }
        respond_json(request, 200, &json!({"added": added}));
    }

    fn status_json(&self) -> Value {
        let mut status = self.status.lock().unwrap().clone();
        if let Some(p) = self.player.lock().unwrap().as_ref() {
            status.insert("playback".into(), p.playback_state());
        }
        if let Some(s) = self.syncer.lock().unwrap().as_ref() {
            status.insert("sync".into(), json!({"online": s.online(), "pending": s.pending()}));
        }
        Value::Object(status)
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
    }

    fn static_file(&self, rel: &str) -> Option<(Vec<u8>, String)> {
        let rel = rel.trim_start_matches('/');
        let full = self.dist_dir.join(rel).canonicalize().ok()?;
        let dist = self.dist_dir.canonicalize().ok()?;
        if !full.starts_with(&dist) {
            return None; // traversal guard
        }
        let data = std::fs::read(&full).ok()?;
        Some((data, content_type(&full)))
    }
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
    // Minimal: the overlay only sends show ids (uuids), so a passthrough with
    // '+'->space is sufficient; percent-decoding via the std isn't available, so
    // ids (no special chars) pass through unchanged.
    s.replace('+', " ")
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
    respond(request, code, serde_json::to_vec(v).unwrap_or_default(), "application/json");
}
