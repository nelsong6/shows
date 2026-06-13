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
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::engine;
use crate::replica::Replica;

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
    fn set_playlist_pos(&self, idx: usize);
    /// Block until the queued round's playlist is exhausted (mpv goes idle /
    /// playlist-pos -> -1), or stop/shutdown. Detecting the boundary this way —
    /// not by counting end-file events — keeps in-round navigation (skip/prev)
    /// from miscounting the round.
    fn wait_for_round_end(&self, stop: &StopFlag) -> Result<(), PlayerShutdown>;
}

/// The sync operations the runner triggers (implemented by [`crate::sync::Syncer`]).
pub trait SyncOps: Send + Sync {
    fn seed(&self) -> bool;
    fn push(&self) -> bool;
    fn pending(&self) -> i64;
    fn online(&self) -> bool;
}

impl SyncOps for crate::sync::Syncer {
    fn seed(&self) -> bool {
        crate::sync::Syncer::seed(self)
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
    let comparable = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
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

#[derive(Default)]
pub struct Callbacks {
    pub on_round: Option<OnRound>,
    pub on_advance: Option<OnAdvance>,
    pub on_drained: Option<OnDrained>,
    pub on_error: Option<OnError>,
    pub on_file_sync: Option<OnFileSync>,
    pub on_status: Option<OnStatus>,
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
        }
    }

    pub fn run(&self) {
        if let Some(cb) = &self.cb.on_status {
            cb("syncing", "syncing shared database");
        }
        self.syncer.push(); // push any pending local changes or trigger auto-seed on startup
        self.syncer.seed(); // pull/reconcile once before the first local round
        if let Some(cb) = &self.cb.on_status {
            cb("fetching", "loading round");
        }
        self.run_loop();
    }

    fn run_loop(&self) {
        while !self.stop.is_set() {
            let (mut round, mut pos) = self.load_round_from_db();
            if round.is_empty() {
                round = self.compute_and_save_new_round();
                pos = 0;
            }
            if round.is_empty() {
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
                "round loaded/queued: {} episodes, starting at pos {}",
                round.len(),
                pos
            );
            let file_sync = self.pull_and_prune_files(&round);
            if let Some(cb) = &self.cb.on_file_sync {
                cb(&file_sync);
            }
            self.queue_round(&round);
            self.player.set_playlist_pos(pos);
            if let Some(cb) = &self.cb.on_round {
                cb(&round, pos);
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
            self.syncer.push(); // flush this round's advances (smart moment)
        }
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
            self.syncer.push();
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
    pub fn previous(&self) {
        self.player.previous();
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
        self.syncer.push();
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
            self.syncer.push();
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

    // ── playback callbacks (mpv event thread; keep fast + local) ─────────
    /// A queued file opened — mark it now-playing, note reachable media, restore
    /// its saved resume position.
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

    fn compute_and_save_new_round(&self) -> Vec<RoundEntry> {
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
            self.syncer.push();
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

        log::info!("Starting file sync for new round to {:?}", canonical_base);
        self.player
            .show_text("Syncing new round files to SSD...", 10000);

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

            // Check if source file exists before trying to copy
            let nas_path = std::path::Path::new(&entry.nas_absolute_path);
            if !nas_path.exists() {
                if local_path.exists() {
                    report.cached += 1;
                    log::warn!(
                        "Source file does not exist or NAS is offline, using cached copy: {:?}",
                        nas_path
                    );
                } else {
                    report.missing += 1;
                    record_file_sync_problem(
                        &mut report,
                        entry,
                        &local_path,
                        "source file missing",
                    );
                    log::error!(
                        "Source file does not exist and no cached copy is available: {:?}",
                        nas_path
                    );
                }
                continue;
            }

            if local_path.exists() {
                let src_metadata = std::fs::metadata(nas_path);
                let dst_metadata = std::fs::metadata(&local_path);
                if let (Ok(src_m), Ok(dst_m)) = (src_metadata, dst_metadata) {
                    if src_m.len() == dst_m.len() {
                        report.cached += 1;
                        log::info!(
                            "File already exists and size matches, skipping copy: {:?}",
                            local_path
                        );
                        continue;
                    }
                }
            }

            log::info!("Copying from NAS: {:?} -> {:?}", nas_path, local_path);
            if let Some(parent) = local_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    report.failed += 1;
                    record_file_sync_problem(
                        &mut report,
                        entry,
                        &local_path,
                        format!("failed to create local folder: {e}"),
                    );
                    log::error!("Failed to create directory {:?}: {}", parent, e);
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
                    "Failed to copy file from {:?} to {:?}: {}",
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
        time: Mutex<Option<f64>>,
        seeked: Mutex<Option<f64>>,
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
        fn playlist_clear(&self) {}
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
        seeds: AtomicUsize,
        pushes: AtomicUsize,
    }
    impl SyncOps for StubSyncer {
        fn seed(&self) -> bool {
            self.seeds.fetch_add(1, Ordering::SeqCst);
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
        Arc::new(Runner::new(
            r,
            Arc::new(StubSyncer::default()),
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
    fn startup_reports_sync_then_round_loading() {
        let r = replica(&[]);
        let p = Arc::new(FakePlayer::default());
        let stop = StopFlag::new();
        let statuses: Arc<Mutex<Vec<(String, String)>>> = Default::default();
        let (statuses2, stop2) = (statuses.clone(), stop.clone());
        let cb = Callbacks {
            on_status: Some(Box::new(move |phase, message| {
                statuses2
                    .lock()
                    .unwrap()
                    .push((phase.to_string(), message.to_string()));
            })),
            on_drained: Some(Box::new(move || {
                stop2.set();
            })),
            ..Default::default()
        };
        let run = runner(r, p, &["nelson"], stop, cb);

        run.run();

        let statuses = statuses.lock().unwrap();
        assert_eq!(
            statuses[0],
            ("syncing".into(), "syncing shared database".into())
        );
        assert_eq!(statuses[1], ("fetching".into(), "loading round".into()));
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
        run.previous();
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
        assert_eq!(run.play_show("s1").status(), "show_not_in_round");
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
        run.run_loop();
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
        run.run_loop();
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
        run.run_loop();
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
        run.run_loop();
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
        run.run_loop();
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
