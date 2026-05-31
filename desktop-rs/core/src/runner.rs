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
        Arc::new(StopFlag { state: Mutex::new(false), cv: Condvar::new() })
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
    pub order_value: u32,
    pub playlist: String,
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

pub type OnRound = Box<dyn Fn(&[RoundEntry]) + Send + Sync>;
pub type OnAdvance = Box<dyn Fn(&AdvanceResult) + Send + Sync>;
pub type OnDrained = Box<dyn Fn() + Send + Sync>;
pub type OnError = Box<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub struct Callbacks {
    pub on_round: Option<OnRound>,
    pub on_advance: Option<OnAdvance>,
    pub on_drained: Option<OnDrained>,
    pub on_error: Option<OnError>,
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
        self.syncer.seed(); // pull/reconcile once before the first local round
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
            log::info!("round loaded/queued: {} episodes, starting at pos {}", round.len(), pos);
            self.queue_round(&round);
            self.player.set_playlist_pos(pos);
            if let Some(cb) = &self.cb.on_round {
                cb(&round);
            }
            if pos < round.len() {
                let active = &round[pos];
                self.player.show_text(&format!("{}   ({}/{})", active.show_name, pos + 1, round.len()), 4000);
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
    pub fn skip(&self) {
        let cur = self.current();
        self.player.skip();
        if let Some(cur) = cur {
            self.apply_advance(&[cur]);
            self.syncer.push();
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
    }

    /// Defer the current show's pick: bump it locally (D1-D3, not watched), push,
    /// and jump forward.
    pub fn defer(&self) {
        let Some(cur) = self.current() else {
            return;
        };
        if !self.replica.defer(&cur.show_id, &cur.episode_id) {
            log::warn!("defer no-op for {:?}", cur.show_name);
            return;
        }
        self.inner.lock().unwrap().deferred.insert(cur.episode_id.clone());
        self.replica.update_round_entry_state(&cur.episode_id, "deferred", &cur.playlist);
        self.syncer.push();
        self.player.skip();
    }

    /// Play a specific show's episode immediately, replacing the current round playlist.
    pub fn play_episode(&self, show: &crate::engine::Show, ep: &crate::engine::Episode) {
        let raw = self.replica.get_round_queue();
        let mut entries = Vec::new();
        for (ep_id, show_id, _play_order, state, playlist, _updated_at, _dirty) in raw {
            if show_id != show.id {
                entries.push((ep_id, show_id, state, playlist));
            }
        }
        let manual_entry = (ep.id.clone(), show.id.clone(), "pending".to_string(), show.playlist.clone());
        entries.insert(0, manual_entry);
        let db_entries: Vec<(String, String, i32, String, String)> = entries
            .into_iter()
            .enumerate()
            .map(|(i, (ep_id, show_id, state, playlist))| {
                (ep_id, show_id, i as i32, state, playlist)
            })
            .collect();
        let now = chrono::Utc::now().to_rfc3339();
        self.replica.save_round_queue(&db_entries, &now, true);
        self.syncer.push();
        self.player.playlist_clear();
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
        self.replica.update_round_entry_state(&cur.episode_id, "playing", &cur.playlist);
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
            
            let entry = RoundEntry {
                show_id: show.id.clone(),
                show_name: show.name.clone(),
                episode_id: ep.id.clone(),
                absolute_path: crate::ordering::join_path(&show.root_path, &ep.relative_path),
                order_value: play_order as u32,
                playlist: show.playlist.clone(),
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
        let name_by: std::collections::HashMap<&str, &str> =
            shows.iter().map(|s| (s.id.as_str(), s.name.as_str())).collect();
        let pl_by: std::collections::HashMap<&str, &str> =
            shows.iter().map(|s| (s.id.as_str(), s.playlist.as_str())).collect();
        ordered
            .iter()
            .map(|o| RoundEntry {
                show_name: name_by.get(o.show_id.as_str()).copied().unwrap_or("").to_string(),
                playlist: pl_by.get(o.show_id.as_str()).copied().unwrap_or("").to_string(),
                show_id: o.show_id.clone(),
                episode_id: o.episode_id.clone(),
                absolute_path: o.absolute_path.clone(),
                order_value: o.order_value,
            })
            .collect()
    }

    fn apply_advance(&self, entries: &[RoundEntry]) -> AdvanceResult {
        for entry in entries {
            self.replica.update_round_entry_state(&entry.episode_id, "watched", &entry.playlist);
        }
        let pairs: Vec<(String, String)> =
            entries.iter().map(|e| (e.show_id.clone(), e.episode_id.clone())).collect();
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
        AdvanceResult { advanced_count: advanced, removed_shows }
    }

    fn queue_round(&self, round: &[RoundEntry]) {
        for (i, ep) in round.iter().enumerate() {
            self.player.play(&ep.absolute_path, if i == 0 { "replace" } else { "append-play" });
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
        fn set_playlist_pos(&self, _idx: usize) {}
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
    fn skip_advances_current_locally() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(r.clone(), p.clone(), &["nelson"], StopFlag::new(), Callbacks::default());
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        run.skip();
        assert_eq!(p.skips.load(Ordering::SeqCst), 1);
        assert!(watched(&r, &cur.episode_id).is_some());
    }

    #[test]
    fn defer_bumps_without_watching() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(r.clone(), p.clone(), &["nelson"], StopFlag::new(), Callbacks::default());
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        run.defer();
        let ep = r.show("s1").unwrap().episodes.into_iter().find(|e| e.id == cur.episode_id).unwrap();
        assert!(ep.watched_at.is_none() && ep.position == 2);
        assert!(run.inner.lock().unwrap().deferred.contains(&cur.episode_id));
        assert_eq!(p.skips.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn previous_steps_back_without_watching() {
        let r = replica(&[show("s1", "nelson", &["a", "b"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(r.clone(), p.clone(), &["nelson"], StopFlag::new(), Callbacks::default());
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        run.previous();
        assert_eq!(p.prevs.load(Ordering::SeqCst), 1);
        // navigation only — stepping back marks nothing watched
        assert!(watched(&r, &cur.episode_id).is_none());
    }

    #[test]
    fn no_round_is_safe() {
        let r = replica(&[show("s1", "nelson", &["a"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(r.clone(), p.clone(), &["nelson"], StopFlag::new(), Callbacks::default());
        run.defer(); // no round in progress -> no-op
        run.skip(); // skip still nudges the player
        assert_eq!(p.skips.load(Ordering::SeqCst), 1);
        assert!(r.show("s1").unwrap().episodes[0].watched_at.is_none());
    }

    // ── per-episode advance (the "watch what's next" model) ───────────
    #[test]
    fn each_finished_episode_is_marked_watched() {
        let r = replica(&[show("s1", "nelson", &["a"]), show("s2", "nelson", &["b"])]);
        let p = Arc::new(FakePlayer::default());
        let stop = StopFlag::new();
        let run = runner(r.clone(), p.clone(), &["nelson"], stop.clone(), Callbacks::default());
        let (run2, stop2) = (run.clone(), stop.clone());
        p.set_on_wait(Box::new(move || {
            let n = run2.inner.lock().unwrap().round.as_ref().map(Vec::len).unwrap_or(0);
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
        let r = replica(&[show("s1", "nelson", &["simpsons"]), show("s2", "nelson", &["malcolm"])]);
        let p = Arc::new(FakePlayer::default());
        let stop = StopFlag::new();
        let run = runner(r.clone(), p.clone(), &["nelson"], stop.clone(), Callbacks::default());
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
        let run = runner(r.clone(), p.clone(), &["nelson"], stop.clone(), Callbacks::default());
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
        let run = runner(r.clone(), p.clone(), &["nelson"], StopFlag::new(), Callbacks::default());
        set_round(&run);
        run.set_pos(0);
        run.on_file_loaded();
        let cur = run.current().unwrap();
        run.defer(); // bumps + marks deferred + player.skip()
        run.on_natural_end(); // a stray EOF for the deferred entry -> no-op
        let ep = r.show("s1").unwrap().episodes.into_iter().find(|e| e.id == cur.episode_id).unwrap();
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
        assert!(errors.lock().unwrap().first().is_some_and(|e| e.contains("no playable media")));
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
        let run = runner(r, Arc::new(FakePlayer::default()), &["nelson", "couple"], StopFlag::new(), Callbacks::default());
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
        let run = runner(r.clone(), p, &["nelson"], StopFlag::new(), Callbacks::default());
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
        let run = runner(r.clone(), p.clone(), &["nelson"], StopFlag::new(), Callbacks::default());
        set_round(&run);
        run.set_pos(0);
        let cur = run.current().unwrap();
        r.set_resume(&cur.episode_id, Some(200.0));
        run.on_file_loaded();
        assert_eq!(*p.seeked.lock().unwrap(), Some(200.0));
        assert_eq!(run.inner.lock().unwrap().playing.as_ref().unwrap().episode_id, cur.episode_id);
    }

    #[test]
    fn play_episode_injects_and_interrupts() {
        let r = replica(&[show("s1", "nelson", &["a", "b"]), show("s2", "nelson", &["c"])]);
        let p = Arc::new(FakePlayer::default());
        let run = runner(r.clone(), p.clone(), &["nelson"], StopFlag::new(), Callbacks::default());
        
        // Compute and save a round in the database
        let round = run.fetch_round();
        assert_eq!(round.len(), 2);
        let entries: Vec<(String, String, i32, String, String)> = round
            .iter()
            .enumerate()
            .map(|(i, r)| {
                (r.episode_id.clone(), r.show_id.clone(), i as i32, "pending".to_string(), r.playlist.clone())
            })
            .collect();
        r.save_round_queue(&entries, "2026-05-31T00:00:00Z", false);
        
        // Manual play for s1's "b" (which is not currently the first pick - "a" is first pick)
        let s1 = r.show("s1").unwrap();
        let ep_b = s1.episodes.iter().find(|e| e.id == "b").unwrap();
        
        // Run play_episode
        run.play_episode(&s1, &ep_b);
        
        // Verify database: "b" must be prepended (play_order = 0), and duplicate "s1" entry "a" must be removed.
        let q = r.get_round_queue();
        // Since s1's "a" was removed, the queue should have "b" at 0, and "c" at 1.
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].0, "b");
        assert_eq!(q[0].2, 0); // play_order = 0
        assert_eq!(q[1].0, "c");
        assert_eq!(q[1].2, 1); // play_order = 1
        
        // Check that dirty flag is set on the queue
        assert_eq!(q[0].6, 1);
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
}
