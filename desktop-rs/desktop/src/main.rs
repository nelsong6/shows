//! shows-desktop — the Windows shell. Wires shows-core's offline-first engine to
//! a single DirectComposition window: libmpv video under a transparent WebView2
//! overlay (the React UI), with a localhost control server.
//!
//! Threading: the compositor + WebView2 + message loop run on the main thread;
//! the runner, control server, and mpv event pump run on background threads.
// Release: GUI subsystem — no console window flashes on launch. Debug keeps
// the console subsystem so `cargo run` prints logs interactively.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(dead_code)]
#![allow(unsafe_op_in_unsafe_fn)]

mod compositor;
mod gl;
mod mpv;
mod player;
mod webserver;

use std::sync::Arc;
use std::time::Duration;

use shows_core::apiclient::{Client, RefreshFn};
use shows_core::oauth;
use shows_core::replica::Replica;
use shows_core::roundlogic::parse_playlists;
use shows_core::runner::{Callbacks, PlayerOps, Runner, StopFlag, SyncOps};
use shows_core::sync::Syncer;

use compositor::Compositor;
use player::Player;
use webserver::ControlServer;

fn to_err(e: String) -> Box<dyn std::error::Error> {
    e.into()
}

/// Open a URL in the default browser (the oauth login flow).
fn open_browser(url: &str) {
    let _ = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
}

/// `%APPDATA%\shows\replica.db`.
fn replica_path() -> String {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let dir = std::path::Path::new(&base).join("shows");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("replica.db").to_string_lossy().into_owned()
}

/// `%APPDATA%\shows\shows.log`.
fn log_path() -> std::path::PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let dir = std::path::Path::new(&base).join("shows");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("shows.log")
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Logging: dev → stderr (so `cargo run` shows it live); release → append
    // to `%APPDATA%\shows\shows.log` (the same place the Python build wrote,
    // so existing user-support muscle memory still finds it). Release has no
    // console, so stderr would be dropped on the floor without this.
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if cfg!(not(debug_assertions)) {
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    builder.init();

    // Single-instance guard. A second copy would race the replica + control
    // server and silently drop watch updates; refuse and focus the running one.
    // (Kernel cleans up the mutex on process exit; no CloseHandle needed.)
    {
        use windows::core::w;
        use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };
        let _singleton = unsafe { CreateMutexW(None, false, w!("Local\\shows-desktop-singleton"))? };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            log::warn!("another shows-desktop is already running — focusing it and exiting");
            if let Ok(existing) = unsafe { FindWindowW(w!("shows-desktop"), None) } {
                unsafe {
                    let _ = ShowWindow(existing, SW_RESTORE);
                    let _ = SetForegroundWindow(existing);
                }
            }
            return Ok(());
        }
    }

    let playlists = parse_playlists(&std::env::var("SHOWS_PLAYLISTS").unwrap_or_default(), &["nelson"]);
    let base_url = std::env::var("SHOWS_BASE_URL").unwrap_or_else(|_| shows_core::apiclient::DEFAULT_BASE_URL.to_string());
    let auth_base = std::env::var("SHOWS_AUTH_BASE").unwrap_or_else(|_| oauth::DEFAULT_AUTH_BASE_URL.to_string());

    // Token: a SHOWS_TOKEN env override (offline / testing) or the PKCE login flow.
    let token = match std::env::var("SHOWS_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => oauth::ensure_token(&auth_base, open_browser).map_err(to_err)?.token,
    };
    let auth_base2 = auth_base.clone();
    let refresh: RefreshFn = Box::new(move || {
        oauth::ensure_token(&auth_base2, open_browser).map(|t| t.token).unwrap_or_default()
    });
    let client = Arc::new(Client::new(token, base_url, Some(refresh)));

    // Offline-first: the replica is the working copy; the syncer reconciles it.
    // SHOWS_REPLICA overrides the path (used for safe, throwaway test runs).
    let rp = std::env::var("SHOWS_REPLICA").unwrap_or_else(|_| replica_path());
    let replica = Arc::new(Replica::new(&rp));
    let syncer = Arc::new(Syncer::new(replica.clone(), client, playlists.clone()));

    // Control server serves the React overlay + the control surface. Picking
    // a dist dir: explicit env override > in-tree source dir (dev) > embedded (release).
    let dist_dir = std::env::var("SHOWS_DIST_DIR")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            if cfg!(debug_assertions) {
                Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../frontend/dist"))
            } else {
                None
            }
        });
    let server = ControlServer::new(dist_dir, replica.clone(), playlists.clone());
    let port = server.start();
    let overlay_url = format!("http://127.0.0.1:{port}/");
    log::info!("control server on {overlay_url}");

    // mpv + player + compositor. The render context (created in the compositor)
    // must exist before the runner loads a file, or video comes up blank.
    let api = mpv::Api::load().map_err(to_err)?;
    let handle = Arc::new(mpv::Handle::create(api).map_err(to_err)?);
    let player = Player::new(handle.clone());
    let compositor = Compositor::create(handle.clone(), &overlay_url)?;
    log::info!("compositor + render context ready");

    // Test hook: play a file directly to verify the GPU video path (the runner
    // drives real playback; this just exercises mpv -> GL -> DComp).
    if let Ok(v) = std::env::var("SHOWS_TEST_VIDEO") {
        if !v.is_empty() {
            log::info!("SHOWS_TEST_VIDEO: {v}");
            player.play(&v, "replace");
        }
    }

    // Runner pushes phase/round/advance into the control server's status.
    let stop = StopFlag::new();
    let cb = {
        let (s_round, s_adv, s_drn, s_err) = (server.clone(), server.clone(), server.clone(), server.clone());
        Callbacks {
            on_round: Some(Box::new(move |round, pos| {
                let entries: Vec<_> = round
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "show_id": e.show_id, "show_name": e.show_name, "episode_id": e.episode_id,
                            "playlist": e.playlist, "order_value": e.order_value,
                        })
                    })
                    .collect();
                let round_id = round.iter().map(|e| e.position).min().expect("round is not empty") + 1;
                s_round.push(serde_json::json!({
                    "phase": "playing", "message": format!("round of {}", round.len()),
                    "round": entries, "round_pos": pos, "round_id": round_id,
                }));
            })),
            on_advance: Some(Box::new(move |res| {
                let removed: Vec<_> = res
                    .removed_shows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id, "name": r.name, "date_added": r.date_added, "last_played_at": r.last_played_at,
                        })
                    })
                    .collect();
                s_adv.push(serde_json::json!({
                    "last_advance": {"advanced_count": res.advanced_count, "removed_shows": removed}
                }));
            })),
            on_drained: Some(Box::new(move || {
                s_drn.push(serde_json::json!({"phase":"drained","message":"every show finished","round":[],"round_id":null}));
            })),
            on_error: Some(Box::new(move |e| {
                s_err.push(serde_json::json!({"phase":"error","message":e}));
            })),
        }
    };
    let runner = Arc::new(Runner::new(
        replica.clone(),
        syncer.clone() as Arc<dyn SyncOps>,
        player.clone() as Arc<dyn PlayerOps>,
        playlists.clone(),
        stop.clone(),
        cb,
    ));

    server.set_player(player.clone());
    server.set_runner(runner.clone());
    server.set_syncer(syncer.clone());
    server.set_on_fullscreen(compositor.fullscreen_callback());
    server.set_on_window_action(compositor.window_action_callback());

    let s_clone = server.clone();
    compositor.set_status_callback(Box::new(move |updates| {
        s_clone.push(updates);
    }));

    // Push initial window states
    server.push(serde_json::json!({
        "window_maximized": compositor.maximized(),
        "window_fullscreen": false,
    }));

    // Player events (mpv's thread) -> runner.
    {
        let r = runner.clone();
        player.set_on_natural_end(Box::new(move || r.on_natural_end()));
    }
    {
        let r = runner.clone();
        let p = player.clone();
        let c = compositor;
        let auto_fit_needed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(!c.has_saved()));
        player.set_on_file_loaded(Box::new(move || {
            r.on_file_loaded();
            if auto_fit_needed.swap(false, std::sync::atomic::Ordering::Relaxed) {
                if let Some((vw, vh)) = p.video_dimensions() {
                    log::info!("Auto-fitting window to video resolution: {vw}x{vh}");
                    c.auto_fit(vw, vh);
                }
            }
        }));
    }
    {
        let (r, s) = (runner.clone(), server.clone());
        player.set_on_pos(Box::new(move |i| {
            r.set_pos(i);
            s.push(serde_json::json!({"round_pos": i}));
        }));
    }

    // Round-robin runner.
    {
        let r = runner.clone();
        std::thread::Builder::new().name("runner".into()).spawn(move || r.run()).expect("spawn runner");
    }
    // Resume-saver: persist position periodically so a crash keeps your place.
    {
        let (r, st) = (runner.clone(), stop.clone());
        std::thread::Builder::new()
            .name("resume-saver".into())
            .spawn(move || {
                while !st.wait_timeout(Duration::from_secs(15)) {
                    r.save_resume();
                }
            })
            .expect("spawn resume-saver");
    }

    // On window close: capture the resume point, flush queued changes, stop.
    {
        let (r, s, st) = (runner.clone(), syncer.clone(), stop.clone());
        compositor.set_quit(Box::new(move || {
            r.save_resume();
            s.push();
            st.set();
        }));
    }

    compositor.run(); // message loop — blocks until the window closes
    Ok(())
}
