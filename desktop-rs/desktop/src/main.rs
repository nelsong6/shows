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

mod audio_monitor;
mod compositor;
mod gl;
mod mpv;
mod player;
mod webserver;

use std::sync::{Arc, mpsc::sync_channel};
use std::time::{Duration, Instant};

use shows_core::replica::Replica;
use shows_core::roundlogic::parse_playlists;
use shows_core::runner::{Callbacks, PlayerOps, Runner, StopFlag, SyncOps};
use shows_core::sync::Syncer;

use compositor::Compositor;
use player::Player;
use webserver::ControlServer;

struct NotifyingSyncer {
    inner: Arc<Syncer>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl NotifyingSyncer {
    fn new(inner: Arc<Syncer>, notify: Arc<dyn Fn() + Send + Sync>) -> NotifyingSyncer {
        NotifyingSyncer { inner, notify }
    }

    fn notify(&self) {
        (self.notify)();
    }
}

impl SyncOps for NotifyingSyncer {
    fn seed(&self) -> bool {
        let online = self.inner.seed();
        self.notify();
        online
    }

    fn push(&self) -> bool {
        let online = self.inner.push();
        self.notify();
        online
    }

    fn pending(&self) -> i64 {
        self.inner.pending()
    }

    fn online(&self) -> bool {
        self.inner.online()
    }
}

fn start_status_waker(
    server: Arc<ControlServer>,
    min_interval: Duration,
) -> Arc<dyn Fn() + Send + Sync> {
    let (tx, rx) = sync_channel::<()>(1);
    std::thread::Builder::new()
        .name("status-waker".into())
        .spawn(move || {
            let mut last_emit = Instant::now() - min_interval;
            while rx.recv().is_ok() {
                let elapsed = last_emit.elapsed();
                if elapsed < min_interval {
                    std::thread::sleep(min_interval - elapsed);
                }
                while rx.try_recv().is_ok() {}
                server.notify();
                last_emit = Instant::now();
            }
        })
        .expect("spawn status waker");

    Arc::new(move || {
        let _ = tx.try_send(());
    })
}
fn to_err(e: String) -> Box<dyn std::error::Error> {
    e.into()
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
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
        {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    builder.init();

    // Single-instance guard. A second copy would race the replica + control
    // server and silently drop watch updates; refuse and focus the running one.
    // (Kernel cleans up the mutex on process exit; no CloseHandle needed.)
    {
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
        };
        use windows::core::w;
        let _singleton =
            unsafe { CreateMutexW(None, false, w!("Local\\shows-desktop-singleton"))? };
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

    let playlists = parse_playlists(
        &std::env::var("SHOWS_PLAYLISTS").unwrap_or_default(),
        &["nelson"],
    );
    // Offline-first: the replica is the working copy; the syncer reconciles it.
    // SHOWS_REPLICA overrides the path (used for safe, throwaway test runs).
    let rp = std::env::var("SHOWS_REPLICA").unwrap_or_else(|_| replica_path());
    let replica = Arc::new(Replica::new(&rp));

    // SQLite-to-SQLite Sync: synchronizes the local replica with a database file on a network share/NAS.
    // Defaults to S:\shows.db, can be overridden by SHOWS_SHARED_DB.
    let shared_db_path = std::env::var("SHOWS_SHARED_DB")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| Some("S:\\shows.db".to_string()));
    let syncer = Arc::new(Syncer::new(replica.clone(), shared_db_path, playlists.clone()));

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
    if let Some(vol) = replica.get_volume() {
        player.set_volume(vol);
    }
    let compositor = Compositor::create(handle.clone(), &overlay_url)?;
    let _audio_monitor = audio_monitor::AudioMonitor::start(Arc::downgrade(&player))?;
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
        let (s_round, s_adv, s_drn, s_err) = (
            server.clone(),
            server.clone(),
            server.clone(),
            server.clone(),
        );
        Callbacks {
            on_round: Some(Box::new(move |round, pos| {
                let entries: Vec<_> = round
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "show_id": e.show_id, "show_name": e.show_name, "episode_id": e.episode_id,
                            "playlist": e.playlist, "order_value": e.order_value,
                            "absolute_path": e.absolute_path,
                        })
                    })
                    .collect();
                let round_id = round
                    .iter()
                    .map(|e| e.position)
                    .min()
                    .expect("round is not empty")
                    + 1;
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
    let runner_syncer = Arc::new(NotifyingSyncer::new(syncer.clone(), {
        let s = server.clone();
        Arc::new(move || s.notify())
    }));
    let runner = Arc::new(Runner::new(
        replica.clone(),
        runner_syncer as Arc<dyn SyncOps>,
        player.clone() as Arc<dyn PlayerOps>,
        playlists.clone(),
        stop.clone(),
        cb,
    ));

    server.set_player(player.clone());
    server.set_runner(runner.clone());
    server.set_syncer(syncer.clone());
    server.set_on_fullscreen(compositor.fullscreen_callback());
    server.set_on_pip(compositor.pip_callback());
    server.set_on_window_action(compositor.window_action_callback());

    let s_clone = server.clone();
    compositor.set_status_callback(Box::new(move |updates| {
        s_clone.push(updates);
    }));

    // Push initial window states
    server.push(serde_json::json!({
        "window_maximized": compositor.maximized(),
        "window_fullscreen": false,
        "window_pip": false,
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
        let auto_fit_needed =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(!c.has_saved()));
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
    {
        let wake_status = start_status_waker(server.clone(), Duration::from_millis(100));
        player.set_on_playback_change(Box::new(move || wake_status()));
    }

    // Round-robin runner.
    {
        let r = runner.clone();
        std::thread::Builder::new()
            .name("runner".into())
            .spawn(move || r.run())
            .expect("spawn runner");
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
