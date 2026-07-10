//! Round-robin playback loop — offline-first.
//!
//! The desktop is the engine: each round is computed locally from the SQLite
//! replica ([`crate::engine::next_round`]), advances/defers are applied to the
//! replica, and the syncer pushes those changes at smart moments. Playback never
//! blocks on the network — the loop runs entirely off the replica.
//!
//! ```text
//!   IDLE    -> next round from replica -> queue N -> PLAYING
//!   PLAYING -> each episode's natural end advances it (local); round drains -> IDLE
//!   IDLE    -> empty round -> DRAINED -> park
//! ```
//!
//! Advance is per-episode: an episode is marked watched the instant it plays to
//! its natural end (contract A1). A file that fails to load, or one closed /
//! skipped / deferred before it finishes, never reaches that point, so a
//! non-watch is never recorded as a watch.
//!
//! The runner is pure orchestration over the [`PlayerOps`] and [`SyncOps`]
//! traits, so it lives in the safe core and is fully tested with fakes; the
//! libmpv `Player` and the real `Syncer` implement the traits.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::engine;
use crate::replica::Replica;
use crate::sync::{SyncProgress, SyncProgressState};

/// A `threading.Event`-equivalent: settable, with a blocking `wait`.
pub struct StopFlag {
    state: Mutex<bool>,
    cv: Condvar,
}

impl StopFlag {
    pub fn new() -> Arc<StopFlag> {
        Arc::new(StopFlag {
            state: Mutex::new(false),
            cv: Condvar::new(),
        })
    }
    pub fn is_set(&self) -> bool {
        *self.state.lock().unwrap()
    }
    pub fn set(&self) {
        *self.state.lock().unwrap() = true;
        self.cv.notify_all();
    }
    /// Block until set.
    pub fn wait(&self) {
        let mut g = self.state.lock().unwrap();
        while !*g {
            g = self.cv.wait(g).unwrap();
        }
    }
    /// Block up to `d`; returns the set state afterward.
    pub fn wait_timeout(&self, d: Duration) -> bool {
        let g = self.state.lock().unwrap();
        if *g {
            return true;
        }
        let (g, _) = self.cv.wait_timeout(g, d).unwrap();
        *g
    }
}

/// Returned by [`PlayerOps::wait_for_round_end`] when mpv closed or the runner
/// was stopped — ends the loop.
pub struct PlayerShutdown(pub String);

/// Tracks the round boundary from mpv's `playlist-pos`. A round ends exactly
/// once — when the queued playlist is exhausted (pos -> -1) *after* it actually
/// started playing. The startup/idle -1 (mpv's observe fires once with the
/// initial value) and any -1 seen while already idle don't count, so skips and
/// back-navigation (playlist-prev) never miscount the boundary.
#[derive(Default)]
pub struct RoundEndTracker {
    active: bool,
    ended: u64,
}

impl RoundEndTracker {
    /// Feed a `playlist-pos` value (negative = exhausted/idle). Returns true
    /// when this transition is a real round end — the caller wakes the waiter.
    pub fn observe(&mut self, pos: i64) -> bool {
        if pos >= 0 {
            self.active = true;
            false
        } else if self.active {
            self.active = false;
            self.ended += 1;
            true
        } else {
            false
        }
    }

    /// How many rounds have ended so far. [`PlayerOps::wait_for_round_end`]
    /// waits for this to advance past the value captured when the round began.
    pub fn ended_count(&self) -> u64 {
        self.ended
    }
}

/// The playback operations the runner drives (implemented by the libmpv player;
/// faked in tests).
pub trait PlayerOps: Send + Sync {
    fn play(&self, path: &str, mode: &str);
    fn playlist_clear(&self);
    fn show_text(&self, text: &str, duration_ms: i64);
    fn skip(&self);
    fn previous(&self);
    fn time_pos(&self) -> Option<f64>;
    fn seek_absolute(&self, seconds: f64);
    fn set_sub(&self, sid: &str);
    fn set_audio(&self, aid: &str);
    fn set_playlist_pos(&self, idx: usize);
    /// Block until the queued round's playlist is exhausted (mpv goes idle /
    /// playlist-pos -> -1), or stop/shutdown. Detecting the boundary this way —
    /// not by counting end-file events — keeps in-round navigation (skip/prev)
    /// from miscounting the round.
    fn wait_for_round_end(&self, stop: &StopFlag) -> Result<(), PlayerShutdown>;
}

/// The sync operations the runner triggers (implemented by [`crate::sync::Syncer`]).
pub trait SyncOps: Send + Sync {
    fn sync(&self) -> bool;
    fn push(&self) -> bool;
    fn pending(&self) -> i64;
    fn online(&self) -> bool;
}

impl SyncOps for crate::sync::Syncer {
    fn sync(&self) -> bool {
        crate::sync::Syncer::sync(self)
    }
    fn push(&self) -> bool {
        crate::sync::Syncer::push(self)
    }
    fn pending(&self) -> i64 {
        crate::sync::Syncer::pending(self)
    }
    fn online(&self) -> bool {
        crate::sync::Syncer::online(self)
    }
}

const REQUEST_PUSH: u8 = 0b01;
const REQUEST_SYNC: u8 = 0b10;

fn start_sync_worker(syncer: Arc<dyn SyncOps>) -> (Sender<()>, Arc<AtomicU8>) {
    let (wake_tx, wake_rx) = channel::<()>();
    let pending = Arc::new(AtomicU8::new(0));
    let worker_pending = pending.clone();
    std::thread::Builder::new()
        .name("shows-sync-worker".into())
        .spawn(move || {
            while wake_rx.recv().is_ok() {
                let requested = worker_pending.swap(0, Ordering::AcqRel);
                if requested & REQUEST_SYNC != 0 {
                    syncer.sync();
                } else if requested & REQUEST_PUSH != 0 {
                    syncer.push();
                }
            }
        })
        .expect("spawn sync worker");
    (wake_tx, pending)
}

#[derive(Debug, Clone)]
pub struct RoundEntry {
    pub show_id: String,
    pub show_name: String,
    pub episode_id: String,
    pub absolute_path: String,
    pub nas_absolute_path: String,
    pub relative_path: String,
    pub order_value: u32,
    pub playlist: String,
    pub position: i64,
}

pub fn get_local_path(show_name: &str, relative_path: &str) -> std::path::PathBuf {
    let base = std::env::var("SHOWS_LOCAL_WATCHING_DIR")
        .unwrap_or_else(|_| r"d:\downloads\Watching".to_string());
    let mut path = std::path::PathBuf::from(base);
    path.push(show_name);
    for part in relative_path.split(|c| c == '\\' || c == '/') {
        if !part.is_empty() {
            path.push(part);
        }
    }
    path
}

fn comparable_path_key(path: &std::path::Path) -> String {
    let comparable = canonicalize_existing_ancestor(path);
    let mut key = comparable.to_string_lossy().replace('/', "\\");
    if let Some(rest) = key.strip_prefix(r"\\?\UNC\") {
        key = format!(r"\\{rest}");
    } else if let Some(rest) = key.strip_prefix(r"\\?\") {
        key = rest.to_string();
    }
    if cfg!(windows) {
        key.make_ascii_lowercase();
    }
    while key.ends_with('\\') && key.len() > 3 {
        key.pop();
    }
    key
}

/// Canonicalize the longest *existing* ancestor of `path`, then re-append the
/// remaining (not-yet-existing) components lexically.
///
/// A bare `path.canonicalize()` is existence-dependent: it only succeeds once the
/// file is on disk (where, on Windows, it also expands 8.3 short names and
/// resolves symlinks/junctions). So a key built for a round file *before* it has
/// been synced (canonicalize fails → raw path) would not match the key built
/// while pruning *after* it exists (canonicalize succeeds → resolved path) — and
/// `prune_unused_files` would delete the file it just synced. Canonicalizing only
/// the stable existing prefix makes the key the same in both cases.
fn canonicalize_existing_ancestor(path: &std::path::Path) -> std::path::PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(canon) = cur.canonicalize() {
            let mut result = canon;
            for part in tail.iter().rev() {
                result.push(part);
            }
            return result;
        }
        let Some(name) = cur.file_name().map(|n| n.to_os_string()) else {
            return path.to_path_buf();
        };
        tail.push(name);
        if !cur.pop() {
            return path.to_path_buf();
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemovedShow {
    pub id: String,
    pub name: String,
    pub date_added: String,
    pub last_played_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct AdvanceResult {
    pub advanced_count: usize,
    pub removed_shows: Vec<RemovedShow>,
}

#[derive(Debug, Clone)]
pub struct FileSyncProblem {
    pub episode_id: String,
    pub show_name: String,
    pub source_path: String,
    pub local_path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct FileSyncReport {
    pub copied: usize,
    pub cached: usize,
    pub missing: usize,
    pub failed: usize,
    pub problems: Vec<FileSyncProblem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOutcome {
    Applied {
        status: &'static str,
        message: String,
    },
    Noop {
        status: &'static str,
        message: String,
    },
}

impl ControlOutcome {
    fn applied(status: &'static str, message: impl Into<String>) -> ControlOutcome {
        ControlOutcome::Applied {
            status,
            message: message.into(),
        }
    }

    fn noop(status: &'static str, message: impl Into<String>) -> ControlOutcome {
        ControlOutcome::Noop {
            status,
            message: message.into(),
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, ControlOutcome::Applied { .. })
    }

    pub fn status(&self) -> &'static str {
        match self {
            ControlOutcome::Applied { status, .. } | ControlOutcome::Noop { status, .. } => status,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            ControlOutcome::Applied { message, .. } | ControlOutcome::Noop { message, .. } => {
                message
            }
        }
    }
}

impl FileSyncReport {
    pub fn incomplete(&self) -> bool {
        self.missing > 0 || self.failed > 0
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.copied > 0 {
            parts.push(format!("{} copied", self.copied));
        }
        if self.cached > 0 {
            parts.push(format!("{} cached", self.cached));
        }
        if self.missing > 0 {
            parts.push(format!("{} missing", self.missing));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if parts.is_empty() {
            "no files needed".to_string()
        } else {
            parts.join(", ")
        }
    }
}

fn record_file_sync_problem(
    report: &mut FileSyncReport,
    entry: &RoundEntry,
    local_path: &std::path::Path,
    reason: impl Into<String>,
) {
    if report.problems.len() >= 5 {
        return;
    }
    report.problems.push(FileSyncProblem {
        episode_id: entry.episode_id.clone(),
        show_name: entry.show_name.clone(),
        source_path: entry.nas_absolute_path.clone(),
        local_path: local_path.to_string_lossy().into_owned(),
        reason: reason.into(),
    });
}

pub type OnRound = Box<dyn Fn(&[RoundEntry], usize) + Send + Sync>;
pub type OnAdvance = Box<dyn Fn(&AdvanceResult) + Send + Sync>;
pub type OnDrained = Box<dyn Fn() + Send + Sync>;
pub type OnError = Box<dyn Fn(&str) + Send + Sync>;
pub type OnFileSync = Box<dyn Fn(&FileSyncReport) + Send + Sync>;
pub type OnStatus = Box<dyn Fn(&str, &str) + Send + Sync>;
pub type OnSyncProgress = Box<dyn Fn(&SyncProgress) + Send + Sync>;

#[derive(Default)]
pub struct Callbacks {
    pub on_round: Option<OnRound>,
    pub on_advance: Option<OnAdvance>,
    pub on_drained: Option<OnDrained>,
    pub on_error: Option<OnError>,
    pub on_file_sync: Option<OnFileSync>,
    pub on_status: Option<OnStatus>,
    pub on_sync_progress: Option<OnSyncProgress>,
}

struct Inner {
    round: Option<Vec<RoundEntry>>,
    pos: usize,
    deferred: HashSet<String>,
    playing: Option<RoundEntry>,
    loaded_any: bool,
}

pub struct Runner {
    replica: Arc<Replica>,
    syncer: Arc<dyn SyncOps>,
    player: Arc<dyn PlayerOps>,
    playlists: Vec<String>,
    stop: Arc<StopFlag>,
    cb: Callbacks,
    inner: Mutex<Inner>,
    sync_wake: Sender<()>,
    sync_requests: Arc<AtomicU8>,
}

impl Runner {
    pub fn new(
        replica: Arc<Replica>,
        syncer: Arc<dyn SyncOps>,
        player: Arc<dyn PlayerOps>,
        playlists: Vec<String>,
        stop: Arc<StopFlag>,
        cb: Callbacks,
    ) -> Runner {
        let (sync_wake, sync_requests) = start_sync_worker(syncer.clone());
        Runner {
            replica,
            syncer,
            player,
            playlists,
            stop,
            cb,
            inner: Mutex::new(Inner {
                round: None,
                pos: 0,
                deferred: HashSet::new(),
                playing: None,
                loaded_any: false,
            }),
            sync_wake,
            sync_requests,
        }
    }

    pub fn run(&self) {
        self.emit_sync_progress(SyncProgress::new(
            "startup.plan",
            SyncProgressState::Started,
            "Planning local-first startup",
        ));
        let has_local_library = self.replica.has_library_data();
        self.emit_sync_progress(SyncProgress::new(
            "startup.plan",
            SyncProgressState::Succeeded,
            if has_local_library {
                "Local library is available; load it before shared-database sync"
            } else {
                "Local library is empty; seed it from the shared database before loading a round"
            },
        ));

        // The only synchronous network path is a genuinely empty first install:
        // without any local library there is nothing the offline engine can play.
        if !has_local_library {
            if let Some(cb) = &self.cb.on_status {
                cb("syncing", "seeding empty local library");
            }
            self.syncer.sync();
        }
        if let Some(cb) = &self.cb.on_status {
            cb("fetching", "loading local round");
        }
        self.run_loop(has_local_library);
    }

    fn run_loop(&self, mut background_startup_sync: bool) {
        let mut first_round = true;
        while !self.stop.is_set() {
            if first_round {
                self.emit_sync_progress(SyncProgress::new(
                    "local-round.load",
                    SyncProgressState::Started,
                    "Loading or selecting a round from the local replica",
                ));
            }
            let (mut round, mut pos) = self.load_round_from_db();
            if round.is_empty() {
                // A first-round queue is pushed only after local-round.load is
                // terminal. Otherwise a first install can start an async push
                // after origin.complete but before local completion, freezing
                // a dangling `started` event into the launch-scoped timeline.
                round = self.compute_and_save_new_round(!first_round);
                pos = 0;
            }
            if round.is_empty() && first_round && background_startup_sync {
                // The replica is established but has no local round for the
                // selected playlists. There is nothing useful to start, so wait
                // for one reconciliation and retry locally before declaring it
                // drained. A fire-and-forget worker would otherwise merge new
                // playable rows and leave the runner parked until restart.
                self.syncer.sync();
                background_startup_sync = false;
                (round, pos) = self.load_round_from_db();
                if round.is_empty() {
                    round = self.compute_and_save_new_round(false);
                    pos = 0;
                }
            }
            if round.is_empty() {
                if first_round {
                    self.emit_sync_progress(SyncProgress::new(
                        "local-round.load",
                        SyncProgressState::Skipped,
                        "The local library has no playable round",
                    ));
                }
                log::info!("playlists drained: {}", self.playlists.join(","));
                if let Some(cb) = &self.cb.on_drained {
                    cb();
                }
                self.stop.wait(); // park until shutdown
                return;
            }
            {
                let mut inner = self.inner.lock().unwrap();
                inner.round = Some(round.clone());
                inner.pos = pos;
                inner.deferred.clear();
                inner.playing = None;
                inner.loaded_any = false;
            }
            log::info!(
                "round loaded/queued: playlists={} episodes={} start_pos={} start_episode={}",
                self.playlists.join(","),
                round.len(),
                pos,
                round.get(pos).map(|e| e.episode_id.as_str()).unwrap_or("")
            );
            let file_cache_started = Instant::now();
            if first_round {
                self.emit_sync_progress(SyncProgress::new(
                    "file-cache.check",
                    SyncProgressState::Started,
                    format!("Checking {} round file(s) in the local cache", round.len()),
                ));
            }
            let file_sync = self.pull_and_prune_files(&round);
            if first_round {
                self.emit_sync_progress(
                    SyncProgress::new(
                        "file-cache.check",
                        if file_sync.incomplete() {
                            SyncProgressState::Failed
                        } else {
                            SyncProgressState::Succeeded
                        },
                        format!("Local round file check: {}", file_sync.summary()),
                    )
                    .with_duration(
                        file_cache_started
                            .elapsed()
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    ),
                );
            }
            if let Some(cb) = &self.cb.on_file_sync {
                cb(&file_sync);
            }
            self.queue_round(&round);
            self.player.set_playlist_pos(pos);
            if let Some(cb) = &self.cb.on_round {
                cb(&round, pos);
            }
            if first_round {
                self.emit_sync_progress(SyncProgress::new(
                    "local-round.load",
                    SyncProgressState::Succeeded,
                    format!("Loaded {} episode(s) from the local replica", round.len()),
                ));
                if background_startup_sync {
                    // Playback is now fully queued. Only now may the worker touch
                    // a sleeping origin, so no network wait can delay first play.
                    self.request_full_sync();
                } else if self.replica.dirty_queue().is_some() {
                    // First install (or the drained/retry path) already completed
                    // its origin pull. Flush the newly-created queue only after
                    // local-round.load closes the boot timeline.
                    self.request_push();
                }
                first_round = false;
            }

            let wait_res = self.player.wait_for_round_end(&self.stop);
            if self.stop.is_set() {
                return;
            }
            if let Err(PlayerShutdown(ref reason)) = wait_res {
                if reason == "interrupted" {
                    log::info!("runner wait interrupted, reloading round from db");
                    continue;
                }
                return;
            }
            self.player.playlist_clear();
            let loaded_any = {
                let mut inner = self.inner.lock().unwrap();
                inner.round = None;
                inner.loaded_any
            };
            if !loaded_any {
                log::error!("round produced no playable media; parking until restart");
                if let Some(cb) = &self.cb.on_error {
                    cb("no playable media — check that the show files are reachable");
                }
                self.stop.wait();
                return;
            }
            self.request_push(); // flush this round's advances without blocking playback
        }
    }

    fn emit_sync_progress(&self, progress: SyncProgress) {
        if let Some(cb) = &self.cb.on_sync_progress {
            cb(&progress);
        }
    }

    fn request_sync_work(&self, request: u8) {
        let previous = self.sync_requests.fetch_or(request, Ordering::AcqRel);
        if previous == 0 {
            let _ = self.sync_wake.send(());
        }
    }

    fn request_full_sync(&self) {
        self.request_sync_work(REQUEST_SYNC);
    }

    fn request_push(&self) {
        self.request_sync_work(REQUEST_PUSH);
    }

    // ── interactive controls (control-server thread) ─────────────────────
    pub fn set_pos(&self, i: usize) {
        self.inner.lock().unwrap().pos = i;
    }

    fn current(&self) -> Option<RoundEntry> {
        let inner = self.inner.lock().unwrap();
        let r = inner.round.as_ref()?;
        if inner.pos < r.len() {
            Some(r[inner.pos].clone())
        } else {
            None
        }
    }

    /// Skip the current episode: jump forward now and mark it watched (I7).
    pub fn skip(&self) -> ControlOutcome {
        let cur = self.current();
        self.player.skip();
        if let Some(cur) = cur {
            let show_name = cur.show_name.clone();
            self.apply_advance(&[cur]);
            self.request_push();
            ControlOutcome::applied("skipped", format!("skipped {show_name}"))
        } else {
            ControlOutcome::noop("no_current_episode", "nothing is currently selected")
        }
    }

    /// Step back to the previous entry in the current round. Navigation only:
    /// going back never marks anything watched, and replaying an already-watched
    /// entry is a no-op at its natural end (engine I3). The playlist-pos observer
    /// moves `pos` + the overlay's now-playing as mpv steps back; a finished
    /// episode replays from the start because [`apply_advance`] clears resume.
    pub fn previous(&self) -> ControlOutcome {
        if self.current().is_none() {
            return ControlOutcome::noop("no_current_episode", "nothing is currently selected");
        }
        self.player.previous();
        ControlOutcome::applied("previous", "stepped back")
    }

    /// Deliberately reload the current round from `round_queue`. Manual repair
    /// removes bad queue entries in the replica; this interrupts mpv so the run
    /// loop clears the in-memory playlist and rebuilds from the repaired queue.
    pub fn reload_round(&self) -> ControlOutcome {
        self.player.playlist_clear();
        ControlOutcome::applied("round_reload_requested", "reloading round")
    }

    /// Defer the current show's pick: bump it locally (D1-D3, not watched), push,
    /// and jump forward.
    pub fn defer(&self) -> ControlOutcome {
        let Some(cur) = self.current() else {
            return ControlOutcome::noop("no_current_episode", "nothing is currently selected");
        };
        if !self.replica.defer(&cur.show_id, &cur.episode_id) {
            log::warn!("defer no-op for {:?}", cur.show_name);
            return ControlOutcome::noop(
                "not_deferred",
                format!("{} could not be deferred", cur.show_name),
            );
        }
        let show_name = cur.show_name.clone();
        self.inner
            .lock()
            .unwrap()
            .deferred
            .insert(cur.episode_id.clone());
        self.replica
            .update_round_entry_state(&cur.episode_id, "deferred", &cur.playlist);
        self.request_push();
        self.player.skip();
        ControlOutcome::applied("deferred", format!("deferred {show_name}"))
    }

    /// Play a specific show immediately by navigating to it in the current round playlist.
    pub fn play_show(&self, show_id: &str) -> ControlOutcome {
        let raw = self.replica.get_round_queue();
        let filtered_raw: Vec<_> = raw
            .iter()
            .filter(|(_, _, _, _, playlist, _, _)| self.playlists.contains(playlist))
            .collect();

        let existing_pos = filtered_raw
            .iter()
            .position(|(_, s_id, _, _, _, _, _)| s_id == show_id);

        if let Some(idx) = existing_pos {
            // Reset previous playing episode to pending if it was playing in the DB
            let old_pos = self.inner.lock().unwrap().pos;
            if old_pos < filtered_raw.len() && old_pos != idx {
                let (old_ep_id, _, _, old_state, old_playlist, _, _) = filtered_raw[old_pos];
                if old_state == "playing" {
                    self.replica
                        .update_round_entry_state(old_ep_id, "pending", old_playlist);
                }
            }

            // Update local state and tell the player to navigate
            {
                let mut inner = self.inner.lock().unwrap();
                inner.pos = idx;
            }
            self.player.set_playlist_pos(idx);
            self.request_push();
            ControlOutcome::applied("playing_selected_show", format!("selected show {show_id}"))
        } else {
            log::warn!(
                "Manual play ignored: show {} is not in the current round queue",
                show_id
            );
            ControlOutcome::noop(
                "show_not_in_round",
                format!("show {show_id} is not in the current round"),
            )
        }
    }

    pub fn set_current_subtitle_track(&self, sid: &str) -> ControlOutcome {
        self.player.set_sub(sid);
        let Some(cur) = self.current() else {
            return ControlOutcome::noop("no_current_episode", "nothing is currently selected");
        };
        self.replica.set_subtitle_track(&cur.episode_id, sid);
        ControlOutcome::applied("subtitle_updated", "subtitle track updated")
    }

    pub fn set_current_audio_track(&self, aid: &str) -> ControlOutcome {
        self.player.set_audio(aid);
        let Some(cur) = self.current() else {
            return ControlOutcome::noop("no_current_episode", "nothing is currently selected");
        };
        self.replica.set_audio_track(&cur.episode_id, aid);
        ControlOutcome::applied("audio_updated", "audio track updated")
    }

    // ── playback callbacks (mpv event thread; keep fast + local) ─────────
    /// A queued file opened — mark it now-playing, note reachable media, restore
    /// its saved resume position and per-episode track choices.
    pub fn on_file_loaded(&self) {
        let Some(cur) = self.current() else {
            return;
        };
        {
            let mut inner = self.inner.lock().unwrap();
            inner.playing = Some(cur.clone());
            inner.loaded_any = true;
        }
        self.replica
            .update_round_entry_state(&cur.episode_id, "playing", &cur.playlist);
        let prefs = self.replica.playback_preferences(&cur.episode_id);
        if let Some(sid) = prefs.subtitle_track.as_deref() {
            self.player.set_sub(sid);
        }
        if let Some(aid) = prefs.audio_track.as_deref() {
            self.player.set_audio(aid);
        }
        if let Some(pos) = self.replica.resume_pos(&cur.episode_id) {
            if pos > 1.0 {
                self.player.seek_absolute(pos);
            }
        }
    }

    /// The now-playing episode reached its natural end — mark it watched (A1).
    /// Only a real EOF lands here; a load failure / skip / defer ends the file
    /// another way, so an unwatched episode is never advanced.
    pub fn on_natural_end(&self) {
        let cur = {
            let mut inner = self.inner.lock().unwrap();
            match inner.playing.take() {
                Some(c) if !inner.deferred.contains(&c.episode_id) => c,
                _ => return,
            }
        };
        let result = self.apply_advance(&[cur]);
        if let Some(cb) = &self.cb.on_advance {
            cb(&result);
        }
    }

    /// Persist the current episode's position to the replica (local; pushed on
    /// the next sync). Called periodically and on window close.
    pub fn save_resume(&self) {
        let Some(cur) = self.current() else {
            return;
        };
        if let Some(pos) = self.player.time_pos() {
            if pos > 1.0 {
                self.replica.set_resume(&cur.episode_id, Some(pos));
            }
        }
    }

    fn load_round_from_db(&self) -> (Vec<RoundEntry>, usize) {
        let raw = self.replica.get_round_queue();
        let mut round = Vec::new();
        let mut first_pending_pos = None;

        for (ep_id, show_id, play_order, state, playlist, _updated_at, _dirty) in raw {
            if !self.playlists.contains(&playlist) {
                continue;
            }
            let Some(show) = self.replica.show(&show_id) else {
                continue;
            };
            let Some(ep) = show.episodes.iter().find(|e| e.id == ep_id) else {
                continue;
            };
            let nas_absolute_path = crate::ordering::join_path(&show.root_path, &ep.relative_path);
            let absolute_path = get_local_path(&show.name, &ep.relative_path)
                .to_string_lossy()
                .into_owned();

            let entry = RoundEntry {
                show_id: show.id.clone(),
                show_name: show.name.clone(),
                episode_id: ep.id.clone(),
                absolute_path,
                nas_absolute_path,
                relative_path: ep.relative_path.clone(),
                order_value: play_order as u32,
                playlist: show.playlist.clone(),
                position: ep.position,
            };

            if state == "pending" || state == "playing" {
                if first_pending_pos.is_none() {
                    first_pending_pos = Some(round.len());
                }
            }
            round.push(entry);
        }

        match first_pending_pos {
            Some(pos) => (round, pos),
            None => (vec![], 0),
        }
    }

    fn compute_and_save_new_round(&self, push: bool) -> Vec<RoundEntry> {
        let round = self.fetch_round();
        if !round.is_empty() {
            let entries: Vec<(String, String, i32, String, String)> = round
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    (
                        r.episode_id.clone(),
                        r.show_id.clone(),
                        i as i32,
                        "pending".to_string(),
                        r.playlist.clone(),
                    )
                })
                .collect();
            let now = chrono::Utc::now().to_rfc3339();
            self.replica.save_round_queue(&entries, &now, true);
            if push {
                self.request_push();
            }
        }
        round
    }

    // ── round build / advance (all local; sync is best-effort) ───────────
    fn fetch_round(&self) -> Vec<RoundEntry> {
        let shows = self.replica.active_shows(&self.playlists);
        let ordered = engine::next_round(&shows);
        let name_by: std::collections::HashMap<&str, &str> = shows
            .iter()
            .map(|s| (s.id.as_str(), s.name.as_str()))
            .collect();
        let pl_by: std::collections::HashMap<&str, &str> = shows
            .iter()
            .map(|s| (s.id.as_str(), s.playlist.as_str()))
            .collect();
        let mut ep_pos_by = std::collections::HashMap::new();
        let mut ep_rel_path_by = std::collections::HashMap::new();
        for s in &shows {
            for ep in &s.episodes {
                ep_pos_by.insert(ep.id.as_str(), ep.position);
                ep_rel_path_by.insert(ep.id.as_str(), ep.relative_path.as_str());
            }
        }
        ordered
            .iter()
            .map(|o| {
                let position = ep_pos_by
                    .get(o.episode_id.as_str())
                    .copied()
                    .expect("episode position must exist");
                let relative_path = ep_rel_path_by
                    .get(o.episode_id.as_str())
                    .copied()
                    .expect("relative path must exist")
                    .to_string();
                let nas_absolute_path = o.absolute_path.clone();
                let show_name = name_by
                    .get(o.show_id.as_str())
                    .copied()
                    .expect("show name must exist")
                    .to_string();
                let absolute_path = get_local_path(&show_name, &relative_path)
                    .to_string_lossy()
                    .into_owned();
                RoundEntry {
                    show_name,
                    playlist: pl_by
                        .get(o.show_id.as_str())
                        .copied()
                        .expect("playlist must exist")
                        .to_string(),
                    show_id: o.show_id.clone(),
                    episode_id: o.episode_id.clone(),
                    absolute_path,
                    nas_absolute_path,
                    relative_path,
                    order_value: o.order_value,
                    position,
                }
            })
            .collect()
    }

    fn apply_advance(&self, entries: &[RoundEntry]) -> AdvanceResult {
        for entry in entries {
            self.replica
                .update_round_entry_state(&entry.episode_id, "watched", &entry.playlist);
        }
        let pairs: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.show_id.clone(), e.episode_id.clone()))
            .collect();
        let (advanced, removed_ids) = self.replica.advance(&pairs);
        let removed_shows = removed_ids
            .iter()
            .map(|sid| {
                let v = self.replica.reveal(sid);
                let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                RemovedShow {
                    id: s("id"),
                    name: s("name"),
                    date_added: s("date_added"),
                    last_played_at: s("last_played_at"),
                }
            })
            .collect();
        AdvanceResult {
            advanced_count: advanced,
            removed_shows,
        }
    }

    fn pull_and_prune_files(&self, round: &[RoundEntry]) -> FileSyncReport {
        let mut report = FileSyncReport::default();
        if cfg!(test) {
            return report;
        }

        let base_dir = std::env::var("SHOWS_LOCAL_WATCHING_DIR")
            .unwrap_or_else(|_| r"d:\downloads\Watching".to_string());
        let base_path = std::path::Path::new(&base_dir);

        // Canonicalize base path if it exists to make canonicalization comparisons exact
        let canonical_base = if base_path.exists() {
            base_path
                .canonicalize()
                .unwrap_or_else(|_| base_path.to_path_buf())
        } else {
            base_path.to_path_buf()
        };

        log::info!(
            "file sync start: playlists={} round_entries={} local_base={:?}",
            self.playlists.join(","),
            round.len(),
            canonical_base
        );
        self.player.show_text("Checking round cache...", 10000);

        let mut active_local_paths = std::collections::HashSet::new();

        for entry in round {
            // Build the local path starting from canonical_base if possible
            let mut local_path = canonical_base.clone();
            local_path.push(&entry.show_name);
            for part in entry.relative_path.split(|c| c == '\\' || c == '/') {
                if !part.is_empty() {
                    local_path.push(part);
                }
            }

            active_local_paths.insert(comparable_path_key(&local_path));

            if is_usable_cached_file(&local_path) {
                report.cached += 1;
                log::info!(
                    "file sync cached: show={} episode={} local={:?}",
                    entry.show_id,
                    entry.episode_id,
                    local_path
                );
                continue;
            }

            let nas_path = std::path::Path::new(&entry.nas_absolute_path);
            if std::fs::metadata(nas_path).is_err() {
                report.missing += 1;
                record_file_sync_problem(&mut report, entry, &local_path, "source file missing");
                log::error!(
                    "file sync missing: show={} episode={} source={:?} local={:?}",
                    entry.show_id,
                    entry.episode_id,
                    nas_path,
                    local_path
                );
                continue;
            }

            log::info!(
                "file sync copy: show={} episode={} source={:?} local={:?}",
                entry.show_id,
                entry.episode_id,
                nas_path,
                local_path
            );
            if let Some(parent) = local_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    report.failed += 1;
                    record_file_sync_problem(
                        &mut report,
                        entry,
                        &local_path,
                        format!("failed to create local folder: {e}"),
                    );
                    log::error!(
                        "file sync mkdir failed: show={} episode={} parent={:?} error={}",
                        entry.show_id,
                        entry.episode_id,
                        parent,
                        e
                    );
                    continue;
                }
            }

            self.player
                .show_text(&format!("Copying {}...", entry.show_name), 5000);
            if let Err(e) = std::fs::copy(nas_path, &local_path) {
                report.failed += 1;
                record_file_sync_problem(
                    &mut report,
                    entry,
                    &local_path,
                    format!("copy failed: {e}"),
                );
                log::error!(
                    "file sync copy failed: show={} episode={} source={:?} local={:?} error={}",
                    entry.show_id,
                    entry.episode_id,
                    nas_path,
                    local_path,
                    e
                );
            } else {
                report.copied += 1;
            }
        }

        // Prune unused files in the watching directory
        if base_path.exists() && base_path.is_dir() {
            prune_unused_files(base_path, &active_local_paths);
        }

        if report.incomplete() {
            let message = format!("Round file sync incomplete: {}", report.summary());
            log::warn!("{message}");
            self.player.show_text(&message, 8000);
        } else {
            let message = format!("Round files sync complete: {}", report.summary());
            log::info!("{message}");
            self.player.show_text(&message, 3000);
        }
        report
    }

    fn queue_round(&self, round: &[RoundEntry]) {
        for (i, ep) in round.iter().enumerate() {
            self.player.play(
                &ep.absolute_path,
                if i == 0 { "replace" } else { "append-play" },
            );
        }
    }
}

fn is_usable_cached_file(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
}

fn prune_unused_files(dir: &std::path::Path, active_paths: &std::collections::HashSet<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_unused_files(&path, active_paths);
            // If directory is now empty, delete it
            if let Ok(mut rd) = std::fs::read_dir(&path) {
                if rd.next().is_none() {
                    let _ = std::fs::remove_dir(&path);
                }
            }
        } else if path.is_file() {
            let cmp_path = comparable_path_key(&path);
            if !active_paths.contains(&cmp_path) {
                log::info!("Pruning old show file: {:?}", path);
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LibraryEpisode, LibraryShow};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const T0: &str = "2026-01-01T00:00:00Z";

    #[derive(Default)]
    struct FakePlayer {
        skips: AtomicUsize,
        prevs: AtomicUsize,
        clears: AtomicUsize,
        time: Mutex<Option<f64>>,
        seeked: Mutex<Option<f64>>,
        sub: Mutex<Option<String>>,
        audio: Mutex<Option<String>>,
        playlist_pos: Mutex<Option<usize>>,
        on_wait: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }
    impl FakePlayer {
        fn set_on_wait(&self, f: Box<dyn Fn() + Send + Sync>) {
            *self.on_wait.lock().unwrap() = Some(f);
        }
    }
    impl PlayerOps for FakePlayer {
        fn play(&self, _p: &str, _m: &str) {}
        fn playlist_clear(&self) {
            self.clears.fetch_add(1, Ordering::SeqCst);
        }
        fn show_text(&self, _t: &str, _d: i64) {}
        fn skip(&self) {
            self.skips.fetch_add(1, Ordering::SeqCst);
        }
        fn previous(&self) {
            self.prevs.fetch_add(1, Ordering::SeqCst);
        }
        fn time_pos(&self) -> Option<f64> {
            *self.time.lock().unwrap()
        }
        fn seek_absolute(&self, seconds: f64) {
            *self.seeked.lock().unwrap() = Some(seconds);
        }
        fn set_sub(&self, sid: &str) {
            *self.sub.lock().unwrap() = Some(sid.to_string());
        }
        fn set_audio(&self, aid: &str) {
            *self.audio.lock().unwrap() = Some(aid.to_string());
        }
        fn set_playlist_pos(&self, idx: usize) {
            *self.playlist_pos.lock().unwrap() = Some(idx);
        }
        fn wait_for_round_end(&self, _stop: &StopFlag) -> Result<(), PlayerShutdown> {
            if let Some(f) = self.on_wait.lock().unwrap().as_ref() {
                f();
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct StubSyncer {
        syncs: AtomicUsize,
        pushes: AtomicUsize,
    }
    impl SyncOps for StubSyncer {
        fn sync(&self) -> bool {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            true
        }
        fn push(&self) -> bool {
            self.pushes.fetch_add(1, Ordering::SeqCst);
            true
        }
        fn pending(&self) -> i64 {
            0
        }
        fn online(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct BlockGate {
        state: Mutex<(bool, bool)>, // (entered, released)
        changed: Condvar,
    }

    impl BlockGate {
        fn enter_and_wait(&self) {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap();
            }
        }

        fn wait_until_entered(&self, timeout: Duration) -> bool {
            let state = self.state.lock().unwrap();
            let (state, _) = self
                .changed
                .wait_timeout_while(state, timeout, |state| !state.0)
                .unwrap();
            state.0
        }

        fn release(&self) {
            let mut state = self.state.lock().unwrap();
            state.1 = true;
            self.changed.notify_all();
        }
    }

    struct GatedSyncer {
        sync_gate: Option<Arc<BlockGate>>,
        push_gate: Option<Arc<BlockGate>>,
        seed: Option<(Arc<Replica>, Vec<LibraryShow>)>,
        syncs: AtomicUsize,
        pushes: AtomicUsize,
    }

    impl GatedSyncer {
        fn blocking_sync(gate: Arc<BlockGate>) -> GatedSyncer {
            GatedSyncer {
                sync_gate: Some(gate),
                push_gate: None,
                seed: None,
                syncs: AtomicUsize::new(0),
                pushes: AtomicUsize::new(0),
            }
        }

        fn blocking_seed(
            gate: Arc<BlockGate>,
            replica: Arc<Replica>,
            shows: Vec<LibraryShow>,
        ) -> GatedSyncer {
            GatedSyncer {
                sync_gate: Some(gate),
                push_gate: None,
                seed: Some((replica, shows)),
                syncs: AtomicUsize::new(0),
                pushes: AtomicUsize::new(0),
            }
        }

        fn blocking_push(gate: Arc<BlockGate>) -> GatedSyncer {
            GatedSyncer {
                sync_gate: None,
                push_gate: Some(gate),
                seed: None,
                syncs: AtomicUsize::new(0),
                pushes: AtomicUsize::new(0),
            }
        }
    }

    impl SyncOps for GatedSyncer {
        fn sync(&self) -> bool {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.sync_gate {
                gate.enter_and_wait();
            }
            if let Some((replica, shows)) = &self.seed {
                replica.merge_shows(shows);
            }
            true
        }

        fn push(&self) -> bool {
            self.pushes.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.push_gate {
                gate.enter_and_wait();
            }
            true
        }

        fn pending(&self) -> i64 {
            0
        }

        fn online(&self) -> bool {
            true
        }
    }

    fn show(sid: &str, playlist: &str, eps: &[&str]) -> LibraryShow {
        LibraryShow {
            id: sid.into(),
            playlist: playlist.into(),
            name: sid.into(),
            root_path: format!("D:\\{sid}"),
            updated_at: T0.into(),
            episodes: eps
                .iter()
                .enumerate()
                .map(|(i, e)| LibraryEpisode {
                    id: (*e).into(),
                    relative_path: format!("{e}.mkv"),
                    position: i as i64,
                    updated_at: T0.into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn replica(shows: &[LibraryShow]) -> Arc<Replica> {
        let r = Arc::new(Replica::new(":memory:"));
        r.merge_shows(shows);
        r
    }

    fn watched(r: &Replica, episode_id: &str) -> Option<String> {
        for sid in ["s1", "s2"] {
            if let Some(s) = r.show(sid) {
                for e in s.episodes {
                    if e.id == episode_id {
                        return e.watched_at;
                    }
                }
            }
        }
        None
    }

    fn runner(
        r: Arc<Replica>,
        player: Arc<dyn PlayerOps>,
        playlists: &[&str],
        stop: Arc<StopFlag>,
        cb: Callbacks,
    ) -> Arc<Runner> {
        runner_with_syncer(
            r,
            Arc::new(StubSyncer::default()),
            player,
            playlists,
            stop,
            cb,
        )
    }

    fn runner_with_syncer(
        r: Arc<Replica>,
        syncer: Arc<dyn SyncOps>,
        player: Arc<dyn PlayerOps>,
        playlists: &[&str],
        stop: Arc<StopFlag>,
        cb: Callbacks,
    ) -> Arc<Runner> {
        Arc::new(Runner::new(
            r,
            syncer,
            player,
            playlists.iter().map(|s| s.to_string()).collect(),
            stop,
            cb,
        ))
    }

    fn set_round(runner: &Runner) {
        let round = runner.fetch_round();
        runner.inner.lock().unwrap().round = Some(round);
    }

    fn play_sim(runner: &Runner, i: usize) {
        runner.set_pos(i);
        runner.on_file_loaded();
        runner.on_natural_end();
    }

    // ── interactive controls ─────────────────────────────────────────
    #[test]
    fn established_startup_reports_local_progress_before_background_sync() {
        let r = replica(&[show("s1", "nelson", &["a"])]);
        let p = Arc::new(FakePlayer::default());
        let stop = StopFlag::new();
        let syncer = Arc::new(StubSyncer::default());
        let progress: Arc<Mutex<Vec<(String, SyncProgressState)>>> = Default::default();
        let (progress2, stop2) = (progress.clone(), stop.clone());
        let cb = Callbacks {
            on_sync_progress: Some(Box::new(move |event| {
                progress2
                    .lock()
                    .unwrap()
                    .push((event.stage.clone(), event.state));
            })),
            on_round: Some(Box::new(move |_, _| {
                stop2.set();
            })),
            ..Default::default()
        };
        let run = runner_with_syncer(r, syncer.clone(), p, &["nelson"], stop, cb);

        run.run();

        assert_eq!(
            *progress.lock().unwrap(),
            vec![
                ("startup.plan".into(), SyncProgressState::Started),
                ("startup.plan".into(), SyncProgressState::Succeeded),
                ("local-round.load".into(), SyncProgressState::Started),
                ("file-cache.check".into(), SyncProgressState::Started),
                ("file-cache.check".into(), SyncProgressState::Succeeded),
                ("local-round.load".into(), SyncProgressState::Succeeded),
            ]
        );
        for _ in 0..100 {
            if syncer.syncs.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(syncer.syncs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn established_replica_does_not_wait_for_blocking_startup_sync() {
        let r = replica(&[show("s1", "nelson", &["a"])]);
        let gate = Arc::new(BlockGate::default());
        let syncer = Arc::new(GatedSyncer::blocking_sync(gate.clone()));
        let stop = StopFlag::new();
        let (round_tx, round_rx) = std::sync::mpsc::channel();
        let stop2 = stop.clone();
        let cb = Callbacks {
            on_round: Some(Box::new(move |_, _| {
                let _ = round_tx.send(());
                stop2.set();
            })),
            ..Default::default()
        };
        let run = runner_with_syncer(
            r,
            syncer,
            Arc::new(FakePlayer::default()),
            &["nelson"],
            stop,
            cb,
        );
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            run.run();
            let _ = done_tx.send(());
        });

        round_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("local round should load before origin sync finishes");
        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        let returned_before_sync = done_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        gate.release();
        if !returned_before_sync {
            let _ = done_rx.recv_timeout(Duration::from_secs(1));
        }
        handle.join().unwrap();
        assert!(
            returned_before_sync,
            "runner startup was blocked by the background shared-database sync"
        );
    }

    #[test]
    fn established_drained_replica_retries_after_sync_adds_selected_playlist() {
        let r = replica(&[show("other", "other", &["old"])]);
        let gate = Arc::new(BlockGate::default());
        let syncer = Arc::new(GatedSyncer::blocking_seed(
            gate.clone(),
            r.clone(),
            vec![show("s1", "nelson", &["a"])],
        ));
        let stop = StopFlag::new();
        let drained = Arc::new(AtomicUsize::new(0));
        let drained2 = drained.clone();
        let (round_tx, round_rx) = std::sync::mpsc::channel();
        let stop2 = stop.clone();
        let cb = Callbacks {
            on_round: Some(Box::new(move |round, _| {
                let _ = round_tx.send(round.len());
                stop2.set();
            })),
            on_drained: Some(Box::new(move || {
                drained2.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        };
        let run = runner_with_syncer(
            r,
            syncer.clone(),
            Arc::new(FakePlayer::default()),
            &["nelson"],
            stop,
            cb,
        );
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            run.run();
            let _ = done_tx.send(());
        });

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        assert_eq!(
            round_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "no round should load before reconciliation finishes"
        );
        assert_eq!(drained.load(Ordering::SeqCst), 0);

        gate.release();
        assert_eq!(round_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.join().unwrap();
        assert_eq!(syncer.syncs.load(Ordering::SeqCst), 1);
        assert_eq!(drained.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn first_install_defers_new_queue_push_until_local_terminal() {
        let r = replica(&[]);
        let push_gate = Arc::new(BlockGate::default());
        let syncer = Arc::new(GatedSyncer {
            sync_gate: None,
            push_gate: Some(push_gate.clone()),
            seed: Some((r.clone(), vec![show("s1", "nelson", &["a"])])),
            syncs: AtomicUsize::new(0),
            pushes: AtomicUsize::new(0),
        });
        let stop = StopFlag::new();
        let pushes_at_local_terminal = Arc::new(AtomicUsize::new(usize::MAX));
        let observed = pushes_at_local_terminal.clone();
        let syncer2 = syncer.clone();
        let stop2 = stop.clone();
        let cb = Callbacks {
            on_sync_progress: Some(Box::new(move |event| {
                if event.stage == "local-round.load"
                    && event.state == SyncProgressState::Succeeded
                {
                    observed.store(syncer2.pushes.load(Ordering::SeqCst), Ordering::SeqCst);
                    stop2.set();
                }
            })),
            ..Default::default()
        };
        let run = runner_with_syncer(
            r,
            syncer.clone(),
            Arc::new(FakePlayer::default()),
            &["nelson"],
            stop,
            cb,
        );

        run.run();

        assert_eq!(syncer.syncs.load(Ordering::SeqCst), 1);
        assert_eq!(
            pushes_at_local_terminal.load(Ordering::SeqCst),
            0,
            "the queue push must not start before local-round.load is terminal"
        );
        assert!(push_gate.wait_until_entered(Duration::from_secs(1)));
        push_gate.release();
    }

    #[test]
    fn empty_replica_waits_for_seed_before_loading_first_round() {
        let r = replica(&[]);
        let gate = Arc::new(BlockGate::default());
        let syncer = Arc::new(GatedSyncer::blocking_seed(
            gate.clone(),
            r.clone(),
            vec![show("s1", "nelson", &["a"])],
        ));
        let stop = StopFlag::new();
        let (round_tx, round_rx) = std::sync::mpsc::channel();
        let stop2 = stop.clone();
        let cb = Callbacks {
            on_round: Some(Box::new(move |_, _| {
                let _ = round_tx.send(());
                stop2.set();
            })),
            ..Default::default()
        };
        let run = runner_with_syncer(
            r,
            syncer.clone(),
            Arc::new(FakePlayer::default()),
            &["nelson"],
            stop,
            cb,
        );
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            run.run();
            let _ = done_tx.send(());
        });

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        assert_eq!(
            round_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "an empty replica must not select a round before its synchronous seed"
        );
        gate.release();
        round_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("seeded local round should load");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("runner should stop after the test round callback");
        handle.join().unwrap();
        assert_eq!(syncer.syncs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn automatic_control_push_does_not_block_on_remote_io() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let gate = Arc::new(BlockGate::default());
        let syncer = Arc::new(GatedSyncer::blocking_push(gate.clone()));
        let run = runner_with_syncer(
            r,
            syncer,
            Arc::new(FakePlayer::default()),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let outcome = run.skip();
            let _ = done_tx.send(outcome.ok());
        });

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        let returned_before_push = done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or(false);
        gate.release();
        handle.join().unwrap();
        assert!(
            returned_before_push,
            "control path waited for its automatic remote push"
        );
    }

    #[test]
    fn skip_advances_current_locally() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        let outcome = run.skip();
        assert!(outcome.ok());
        assert_eq!(outcome.status(), "skipped");
        assert_eq!(p.skips.load(Ordering::SeqCst), 1);
        assert!(watched(&r, &cur.episode_id).is_some());
    }

    #[test]
    fn defer_bumps_without_watching() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        let outcome = run.defer();
        assert!(outcome.ok());
        assert_eq!(outcome.status(), "deferred");
        let ep = r
            .show("s1")
            .unwrap()
            .episodes
            .into_iter()
            .find(|e| e.id == cur.episode_id)
            .unwrap();
        assert!(ep.watched_at.is_none() && ep.position == 2);
        assert!(run.inner.lock().unwrap().deferred.contains(&cur.episode_id));
        assert_eq!(p.skips.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn previous_steps_back_without_watching() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        let outcome = run.previous();
        assert!(outcome.ok());
        assert_eq!(outcome.status(), "previous");
        assert_eq!(p.prevs.load(Ordering::SeqCst), 1);
        // navigation only — stepping back marks nothing watched
        assert!(watched(&r, &cur.episode_id).is_none());
    }

    #[test]
    fn local_path_mapping_matches_env_or_default() {
        let show_name = "Dr. Katz";
        let relative_path = "S01\\E01.mkv";

        let path = get_local_path(show_name, relative_path);
        assert!(path.ends_with(std::path::Path::new("Dr. Katz").join("S01").join("E01.mkv")));
    }

    #[cfg(windows)]
    #[test]
    fn comparable_path_key_normalizes_windows_case() {
        let upper =
            comparable_path_key(std::path::Path::new(r"D:\Downloads\Watching\Show\E01.mkv"));
        let lower =
            comparable_path_key(std::path::Path::new(r"d:\downloads\watching\show\e01.mkv"));

        assert_eq!(upper, lower);
    }

    #[cfg(windows)]
    #[test]
    fn comparable_path_key_normalizes_verbatim_windows_prefix() {
        let normal =
            comparable_path_key(std::path::Path::new(r"D:\Downloads\Watching\Show\E01.mkv"));
        let verbatim = comparable_path_key(std::path::Path::new(
            r"\\?\D:\Downloads\Watching\Show\E01.mkv",
        ));

        assert_eq!(normal, verbatim);
    }

    #[test]
    fn prune_keeps_file_marked_active_before_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let active_file = base.join("Show").join("S01").join("E01.mkv");
        let stale_file = base.join("Stale").join("old.mkv");

        let active_key = comparable_path_key(&active_file);
        std::fs::create_dir_all(active_file.parent().unwrap()).unwrap();
        std::fs::write(&active_file, b"active").unwrap();
        std::fs::create_dir_all(stale_file.parent().unwrap()).unwrap();
        std::fs::write(&stale_file, b"stale").unwrap();

        prune_unused_files(base, &std::collections::HashSet::from([active_key]));

        assert!(active_file.exists());
        assert!(!stale_file.exists());
    }

    #[test]
    fn comparable_path_key_is_stable_across_file_existence() {
        // The key must not change once the file is created: prune records the
        // "active" key before a round file is synced, then recomputes it while
        // walking the cache after it exists. If those differ, the just-synced
        // file is deleted as "unused" — the real defect behind the Windows-only
        // CI failure (canonicalize() resolves 8.3 short names only once on disk).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Show").join("S01");
        std::fs::create_dir_all(&dir).unwrap();
        // A redundant ".." so a bare canonicalize() (once the file exists) would
        // resolve to a different string than the raw form used before it exists.
        let file = dir.join("..").join("S01").join("E01.mkv");

        let before = comparable_path_key(&file);
        std::fs::write(&file, b"x").unwrap();
        let after = comparable_path_key(&file);

        assert_eq!(before, after);
    }

    #[test]
    fn cache_hit_accepts_only_non_empty_local_files() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.mkv");
        let empty = tmp.path().join("empty.mkv");
        let cached = tmp.path().join("cached.mkv");

        std::fs::write(&empty, b"").unwrap();
        std::fs::write(&cached, b"cached bytes").unwrap();

        assert!(!is_usable_cached_file(&missing));
        assert!(!is_usable_cached_file(&empty));
        assert!(is_usable_cached_file(&cached));
    }

    #[test]
    fn round_entry_contains_nas_and_relative_paths() {
        let r = replica(&[show("s1", "nelson", &["ep-a"])]);
        let p = Arc::new(FakePlayer::default());

        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        let round = run.fetch_round();

        assert_eq!(round.len(), 1);
        let entry = &round[0];
        assert_eq!(entry.show_name, "s1");
        assert_eq!(entry.relative_path, "ep-a.mkv");
        assert_eq!(entry.nas_absolute_path, r"D:\s1\ep-a.mkv");
        let expected_suffix = std::path::Path::new("s1")
            .join("ep-a.mkv")
            .to_string_lossy()
            .into_owned();
        assert!(entry.absolute_path.ends_with(&expected_suffix));
    }

    #[test]
    fn no_round_is_safe() {
        let r = replica(&[show("s1", "nelson", &["a"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        run.defer(); // no round in progress -> no-op
        run.skip(); // skip still nudges the player
        assert_eq!(p.skips.load(Ordering::SeqCst), 1);
        assert!(r.show("s1").unwrap().episodes[0].watched_at.is_none());
    }

    #[test]
    fn controls_report_noop_without_current_round() {
        let r = replica(&[show("s1", "nelson", &["a"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(r, p, &["nelson"], StopFlag::new(), Callbacks::default());

        assert_eq!(run.defer().status(), "no_current_episode");
        assert_eq!(run.previous().status(), "no_current_episode");
        assert_eq!(run.play_show("s1").status(), "show_not_in_round");
    }

    #[test]
    fn reload_round_interrupts_player_so_loop_can_rebuild_from_queue() {
        let r = replica(&[show("s1", "nelson", &["a"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r,
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );

        let outcome = run.reload_round();

        assert!(outcome.ok());
        assert_eq!(outcome.status(), "round_reload_requested");
        assert_eq!(p.clears.load(Ordering::SeqCst), 1);
    }

    // ── per-episode advance (the "watch what's next" model) ───────────
    #[test]
    fn each_finished_episode_is_marked_watched() {
        let r = replica(&[show("s1", "nelson", &["a"]), show("s2", "nelson", &["b"])]);
        let p = Arc::new(FakePlayer::default());
        let stop = StopFlag::new();
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            stop.clone(),
            Callbacks::default(),
        );
        let (run2, stop2) = (run.clone(), stop.clone());
        p.set_on_wait(Box::new(move || {
            let n = run2
                .inner
                .lock()
                .unwrap()
                .round
                .as_ref()
                .map(Vec::len)
                .unwrap_or(0);
            for i in 0..n {
                play_sim(&run2, i);
            }
            stop2.set();
        }));
        run.run_loop(false);
        assert!(watched(&r, "a").is_some());
        assert!(watched(&r, "b").is_some());
    }

    #[test]
    fn unfinished_episode_is_not_watched() {
        // Watch the Simpsons to the end, turn off before Malcolm plays: Simpsons
        // is done; Malcolm wasn't watched, so it stays the next pick.
        let r = replica(&[
            show("s1", "nelson", &["simpsons"]),
            show("s2", "nelson", &["malcolm"]),
        ]);
        let p = Arc::new(FakePlayer::default());
        let stop = StopFlag::new();
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            stop.clone(),
            Callbacks::default(),
        );
        let (run2, stop2) = (run.clone(), stop.clone());
        p.set_on_wait(Box::new(move || {
            let round = run2.inner.lock().unwrap().round.clone().unwrap_or_default();
            for (i, e) in round.iter().enumerate() {
                if e.show_id == "s1" {
                    play_sim(&run2, i);
                }
            }
            stop2.set();
        }));
        run.run_loop(false);
        assert!(watched(&r, "simpsons").is_some());
        assert!(watched(&r, "malcolm").is_none());
    }

    #[test]
    fn failed_load_is_not_watched() {
        let r = replica(&[show("s1", "nelson", &["a"]), show("s2", "nelson", &["b"])]);
        let p = Arc::new(FakePlayer::default());
        let stop = StopFlag::new();
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            stop.clone(),
            Callbacks::default(),
        );
        let (run2, stop2) = (run.clone(), stop.clone());
        p.set_on_wait(Box::new(move || {
            let round = run2.inner.lock().unwrap().round.clone().unwrap_or_default();
            for (i, e) in round.iter().enumerate() {
                if e.show_id == "s1" {
                    play_sim(&run2, i); // s1 plays; s2's file "fails to load"
                }
            }
            stop2.set();
        }));
        run.run_loop(false);
        assert!(watched(&r, "a").is_some());
        assert!(watched(&r, "b").is_none());
    }

    #[test]
    fn deferred_episode_is_not_advanced_on_finish() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        run.on_file_loaded();
        let cur = run.current().unwrap();
        run.defer(); // bumps + marks deferred + player.skip()
        run.on_natural_end(); // a stray EOF for the deferred entry -> no-op
        let ep = r
            .show("s1")
            .unwrap()
            .episodes
            .into_iter()
            .find(|e| e.id == cur.episode_id)
            .unwrap();
        assert!(ep.watched_at.is_none() && ep.position == 2);
    }

    #[test]
    fn round_with_no_playable_media_parks() {
        let r = replica(&[show("s1", "nelson", &["a"]), show("s2", "nelson", &["b"])]);
        let p = Arc::new(FakePlayer::default()); // no on_wait -> nothing opens
        let stop = StopFlag::new();
        let errors: Arc<Mutex<Vec<String>>> = Default::default();
        let advances = Arc::new(AtomicUsize::new(0));
        let (errs2, stop2, adv2) = (errors.clone(), stop.clone(), advances.clone());
        let cb = Callbacks {
            on_advance: Some(Box::new(move |_res| {
                adv2.fetch_add(1, Ordering::SeqCst);
            })),
            on_error: Some(Box::new(move |e| {
                errs2.lock().unwrap().push(e.to_string());
                stop2.set();
            })),
            ..Default::default()
        };
        let run = runner(r.clone(), p, &["nelson"], stop, cb);
        run.run_loop(false);
        assert!(
            errors
                .lock()
                .unwrap()
                .first()
                .is_some_and(|e| e.contains("no playable media"))
        );
        assert_eq!(advances.load(Ordering::SeqCst), 0);
        assert!(watched(&r, "a").is_none() && watched(&r, "b").is_none());
    }

    #[test]
    fn drained_calls_on_drained() {
        let r = Arc::new(Replica::new(":memory:")); // empty library
        let stop = StopFlag::new();
        let drained = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (d2, stop2) = (drained.clone(), stop.clone());
        let cb = Callbacks {
            on_drained: Some(Box::new(move || {
                d2.store(true, Ordering::SeqCst);
                stop2.set();
            })),
            ..Default::default()
        };
        let run = runner(r, Arc::new(FakePlayer::default()), &["nelson"], stop, cb);
        run.run_loop(false);
        assert!(drained.load(Ordering::SeqCst));
    }

    #[test]
    fn cross_playlist_round_spans_playlists() {
        let r = replica(&[show("s1", "nelson", &["a"]), show("s2", "couple", &["b"])]);
        let run = runner(
            r,
            Arc::new(FakePlayer::default()),
            &["nelson", "couple"],
            StopFlag::new(),
            Callbacks::default(),
        );
        let rnd = run.fetch_round();
        let ids: BTreeSet<&str> = rnd.iter().map(|e| e.show_id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"].into_iter().collect());
    }

    // ── resume ─────────────────────────────────────────────────────────
    #[test]
    fn save_resume_persists_position() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        *p.time.lock().unwrap() = Some(100.0);
        let run = runner(
            r.clone(),
            p,
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        run.save_resume();
        assert_eq!(r.resume_pos(&cur.episode_id), Some(100.0));
    }

    #[test]
    fn on_file_loaded_seeks_to_resume() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        r.set_resume(&cur.episode_id, Some(200.0));
        run.on_file_loaded();
        assert_eq!(*p.seeked.lock().unwrap(), Some(200.0));
        assert_eq!(
            run.inner
                .lock()
                .unwrap()
                .playing
                .as_ref()
                .unwrap()
                .episode_id,
            cur.episode_id
        );
    }

    #[test]
    fn track_controls_persist_for_current_episode() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();

        assert_eq!(
            run.set_current_subtitle_track("no").status(),
            "subtitle_updated"
        );
        assert_eq!(run.set_current_audio_track("2").status(), "audio_updated");
        assert_eq!(*p.sub.lock().unwrap(), Some("no".into()));
        assert_eq!(*p.audio.lock().unwrap(), Some("2".into()));

        let prefs = r.playback_preferences(&cur.episode_id);
        assert_eq!(prefs.subtitle_track.as_deref(), Some("no"));
        assert_eq!(prefs.audio_track.as_deref(), Some("2"));
    }

    #[test]
    fn on_file_loaded_applies_episode_track_preferences() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        r.set_subtitle_track(&cur.episode_id, "3");
        r.set_audio_track(&cur.episode_id, "1");

        run.on_file_loaded();

        assert_eq!(*p.sub.lock().unwrap(), Some("3".into()));
        assert_eq!(*p.audio.lock().unwrap(), Some("1".into()));
    }

    // ── round-end detection (playlist-pos -> -1) ───────────────────────
    #[test]
    fn startup_idle_minus_one_is_not_a_round_end() {
        // mpv's observe fires once with the initial value; before anything
        // plays that value is -1. It must not count as a round ending.
        let mut t = RoundEndTracker::default();
        assert!(!t.observe(-1));
        assert_eq!(t.ended_count(), 0);
    }

    #[test]
    fn playing_then_exhausted_ends_exactly_one_round() {
        let mut t = RoundEndTracker::default();
        assert!(!t.observe(0)); // first entry
        assert!(!t.observe(1)); // second entry
        assert!(t.observe(-1)); // playlist exhausted -> round end
        assert_eq!(t.ended_count(), 1);
    }

    #[test]
    fn back_navigation_does_not_miscount_the_round() {
        // skip forward then step back (playlist-prev) revisits earlier
        // positions; only the final exhaustion ends the round.
        let mut t = RoundEndTracker::default();
        for pos in [0, 1, 2, 1, 0, 1, 2] {
            assert!(!t.observe(pos));
        }
        assert!(t.observe(-1));
        assert_eq!(t.ended_count(), 1);
    }

    #[test]
    fn repeated_idle_minus_one_counts_only_once() {
        // After exhaustion mpv may report -1 again while idle; the round
        // boundary is edge-triggered, so the extra -1 is ignored.
        let mut t = RoundEndTracker::default();
        assert!(!t.observe(0));
        assert!(t.observe(-1));
        assert!(!t.observe(-1));
        assert!(!t.observe(-1));
        assert_eq!(t.ended_count(), 1);
    }

    #[test]
    fn consecutive_rounds_each_end_once() {
        let mut t = RoundEndTracker::default();
        assert!(!t.observe(0));
        assert!(t.observe(-1));
        assert!(!t.observe(0)); // next round starts
        assert!(t.observe(-1));
        assert_eq!(t.ended_count(), 2);
    }

    #[test]
    fn play_episode_navigates_within_existing_round() {
        let r = replica(&[
            show("s1", "nelson", &["a", "b"]),
            show("s2", "nelson", &["c"]),
        ]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(
            r.clone(),
            p.clone(),
            &["nelson"],
            StopFlag::new(),
            Callbacks::default(),
        );

        // Compute and save round
        let round = run.fetch_round();
        assert_eq!(round.len(), 2);

        let idx_c = round.iter().position(|r| r.episode_id == "c").unwrap();
        let idx_a = round.iter().position(|r| r.episode_id == "a").unwrap();

        let entries: Vec<(String, String, i32, String, String)> = round
            .iter()
            .enumerate()
            .map(|(i, r)| {
                (
                    r.episode_id.clone(),
                    r.show_id.clone(),
                    i as i32,
                    if i == idx_a {
                        "playing".to_string()
                    } else {
                        "pending".to_string()
                    },
                    r.playlist.clone(),
                )
            })
            .collect();
        r.save_round_queue(&entries, "2026-05-31T00:00:00Z", false);

        // Populate inner.round and inner.pos (start playing "a")
        {
            let mut inner = run.inner.lock().unwrap();
            inner.round = Some(round.clone());
            inner.pos = idx_a;
        }

        // Manual play for s2's "c" (which is in the existing queue at idx_c)
        let s2 = r.show("s2").unwrap();

        let outcome = run.play_show(&s2.id);
        assert!(outcome.ok());
        assert_eq!(outcome.status(), "playing_selected_show");

        // Verify that:
        // 1. inner.pos is now idx_c
        assert_eq!(run.inner.lock().unwrap().pos, idx_c);
        // 2. FakePlayer received set_playlist_pos(idx_c)
        assert_eq!(*p.playlist_pos.lock().unwrap(), Some(idx_c));
        // 3. Database queue was NOT modified (order matches the round computed)
        let q = r.get_round_queue();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].0, round[0].episode_id);
        assert_eq!(q[1].0, round[1].episode_id);
        // 4. s1's "a" state was reset from "playing" to "pending"
        assert_eq!(q[idx_a].3, "pending");
    }
}
