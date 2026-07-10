//! Sync orchestration between the local replica and the shared SQLite database —
//! git-style: local-first, push at smart moments, pull to seed/reconcile,
//! last-write-wins.
//!
//! Connectivity is *inferred*, never polled: every push/pull attempt sets
//! `online` (success) or clears it (failure). A failed attempt flips offline and
//! stops; the manual "check connectivity" action or the next natural push
//! recovers it. The runner never blocks on the network — playback runs entirely
//! off the replica.

use crate::model::{LibraryEpisode, LibraryQueue, LibraryShow, RoundQueueEntry};
use crate::replica::{Replica, SCHEMA};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingBreakdown {
    pub shows: i64,
    pub episodes: i64,
    pub history: i64,
    pub queue: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncProgressState {
    Started,
    Succeeded,
    Skipped,
    Failed,
}

impl SyncProgressState {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncProgressState::Started => "started",
            SyncProgressState::Succeeded => "succeeded",
            SyncProgressState::Skipped => "skipped",
            SyncProgressState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncProgress {
    pub stage: String,
    pub state: SyncProgressState,
    pub message: String,
    pub duration_ms: Option<u64>,
    pub pending: Option<PendingBreakdown>,
}

impl SyncProgress {
    pub fn new(
        stage: impl Into<String>,
        state: SyncProgressState,
        message: impl Into<String>,
    ) -> SyncProgress {
        SyncProgress {
            stage: stage.into(),
            state,
            message: message.into(),
            duration_ms: None,
            pending: None,
        }
    }

    pub fn with_duration(mut self, duration_ms: u64) -> SyncProgress {
        self.duration_ms = Some(duration_ms);
        self
    }

    pub fn with_pending(mut self, pending: PendingBreakdown) -> SyncProgress {
        self.pending = Some(pending);
        self
    }
}

pub type SyncProgressCallback = Arc<dyn Fn(SyncProgress) + Send + Sync>;

impl PendingBreakdown {
    pub fn total(&self) -> i64 {
        self.shows + self.episodes + self.history + self.queue
    }
}

pub struct Syncer {
    replica: Arc<Replica>,
    shared_db_path: Option<String>,
    playlists: Vec<String>,
    online: AtomicBool,
    last_error: Mutex<Option<String>>,
    progress_callback: Mutex<Option<SyncProgressCallback>>,
    remote_operation: Mutex<()>,
}

impl Syncer {
    pub fn new(
        replica: Arc<Replica>,
        shared_db_path: Option<String>,
        playlists: Vec<String>,
    ) -> Syncer {
        Syncer {
            replica,
            shared_db_path,
            playlists,
            online: AtomicBool::new(true), // optimistic; the first failed attempt flips it
            last_error: Mutex::new(None),
            progress_callback: Mutex::new(None),
            remote_operation: Mutex::new(()),
        }
    }

    pub fn set_progress_callback(&self, callback: Option<SyncProgressCallback>) {
        *self.progress_callback.lock().unwrap() = callback;
    }

    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    pub fn shared_db_path(&self) -> Option<String> {
        self.shared_db_path.clone()
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    fn mark_online(&self) {
        self.online.store(true, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = None;
    }

    fn mark_offline(&self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("{message}");
        self.online.store(false, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(message);
    }

    fn emit(&self, progress: SyncProgress) {
        log::info!(
            "sync progress: stage={} state={} message={}",
            progress.stage,
            progress.state.as_str(),
            progress.message
        );
        let callback = self.progress_callback.lock().unwrap().clone();
        if let Some(callback) = callback {
            callback(progress);
        }
    }

    /// Pull the library and reconcile it into the replica (initial seed and later
    /// pulls). Returns the resulting online state.
    pub fn seed(&self) -> bool {
        let _operation = self.remote_operation.lock().unwrap();
        let Some(conn) = self.connect("seed") else {
            return false;
        };
        self.pull_with_connection(&conn)
    }

    /// Push dirty records and clear their dirty flags on success. No-op when
    /// nothing is pending. Returns the resulting online state.
    pub fn push(&self) -> bool {
        let _operation = self.remote_operation.lock().unwrap();
        let Some(mut conn) = self.connect("push") else {
            return false;
        };
        self.push_with_connection(&mut conn)
    }

    /// Push then pull over one serialized shared connection. Opening a sleeping
    /// mapped drive is the expensive operation; a sync run must never pay for it
    /// three times or race another remote operation.
    pub fn sync(&self) -> bool {
        let _operation = self.remote_operation.lock().unwrap();
        let started = Instant::now();
        let Some(mut conn) = self.connect("sync") else {
            let pending = self.pending_breakdown();
            self.emit(
                SyncProgress::new(
                    "origin.push",
                    SyncProgressState::Skipped,
                    "Push was not attempted because the shared database connection failed",
                )
                .with_pending(pending.clone()),
            );
            self.emit(SyncProgress::new(
                "origin.pull",
                SyncProgressState::Skipped,
                "Pull was not attempted because the shared database connection failed",
            ));
            self.emit(
                SyncProgress::new(
                    "origin.complete",
                    SyncProgressState::Failed,
                    "Startup sync could not connect to the shared database",
                )
                .with_duration(elapsed_ms(started))
                .with_pending(pending),
            );
            return false;
        };

        let pushed = self.push_with_connection(&mut conn);
        let pulled = if pushed {
            self.pull_with_connection(&conn)
        } else {
            self.emit(SyncProgress::new(
                "origin.pull",
                SyncProgressState::Skipped,
                "Pull was not attempted because the local push failed",
            ));
            false
        };
        let succeeded = pushed && pulled;
        self.emit(
            SyncProgress::new(
                "origin.complete",
                if succeeded {
                    SyncProgressState::Succeeded
                } else {
                    SyncProgressState::Failed
                },
                if succeeded {
                    "Startup sync completed"
                } else {
                    "Startup sync failed; local playback remains available"
                },
            )
            .with_duration(elapsed_ms(started))
            .with_pending(self.pending_breakdown()),
        );
        succeeded
    }

    /// Unpushed local changes — the git "ahead" count.
    pub fn pending(&self) -> i64 {
        self.pending_breakdown().total()
    }

    pub fn pending_breakdown(&self) -> PendingBreakdown {
        let p = self.replica.pending();
        PendingBreakdown {
            shows: p.shows,
            episodes: p.episodes,
            history: p.history,
            queue: i64::from(self.replica.dirty_queue().is_some()),
        }
    }

    fn connect(&self, purpose: &str) -> Option<Connection> {
        let path = self.shared_db_path.as_deref().unwrap_or("(not configured)");
        let started = Instant::now();
        self.emit(SyncProgress::new(
            "origin.connect",
            SyncProgressState::Started,
            format!("Opening shared database at {path}"),
        ));
        match self.open_shared() {
            Ok(conn) => {
                self.emit(
                    SyncProgress::new(
                        "origin.connect",
                        SyncProgressState::Succeeded,
                        format!("Opened shared database at {path}"),
                    )
                    .with_duration(elapsed_ms(started)),
                );
                Some(conn)
            }
            Err(e) => {
                let message = format!("Failed to open shared DB for {purpose}: {e}");
                self.mark_offline(message.clone());
                self.emit(
                    SyncProgress::new("origin.connect", SyncProgressState::Failed, message)
                        .with_duration(elapsed_ms(started)),
                );
                None
            }
        }
    }

    fn push_with_connection(&self, conn: &mut Connection) -> bool {
        let started = Instant::now();
        let mut pending = self.pending_breakdown();
        self.emit(
            SyncProgress::new(
                "origin.push",
                SyncProgressState::Started,
                format!("Checking {} pending local change(s)", pending.total()),
            )
            .with_pending(pending.clone()),
        );

        if self.replica.has_library_data() {
            let origin_count =
                match conn.query_row("SELECT COUNT(*) FROM shows", [], |r| r.get::<_, i64>(0)) {
                    Ok(count) => count,
                    Err(e) => {
                        let message = format!("Failed to inspect shared database before push: {e}");
                        self.mark_offline(message.clone());
                        self.emit(
                            SyncProgress::new("origin.push", SyncProgressState::Failed, message)
                                .with_duration(elapsed_ms(started))
                                .with_pending(pending),
                        );
                        return false;
                    }
                };
            if origin_count == 0 {
                log::info!("Shared DB is empty; marking the local library dirty to seed it.");
                if let Err(e) = self.replica.mark_all_dirty() {
                    let message = format!("Failed to prepare local library for origin seed: {e}");
                    self.mark_offline(message.clone());
                    self.emit(
                        SyncProgress::new("origin.push", SyncProgressState::Failed, message)
                            .with_duration(elapsed_ms(started))
                            .with_pending(pending),
                    );
                    return false;
                }
                pending = self.pending_breakdown();
            }
        }

        if pending.total() == 0 {
            self.mark_online();
            self.emit(
                SyncProgress::new(
                    "origin.push",
                    SyncProgressState::Skipped,
                    "No local changes needed pushing",
                )
                .with_duration(elapsed_ms(started))
                .with_pending(pending),
            );
            return true;
        }

        match self.replica.push_to_shared(conn) {
            Ok(()) => {
                self.mark_online();
                self.emit(
                    SyncProgress::new(
                        "origin.push",
                        SyncProgressState::Succeeded,
                        format!("Pushed {} local change(s)", pending.total()),
                    )
                    .with_duration(elapsed_ms(started))
                    .with_pending(pending),
                );
                true
            }
            Err(e) => {
                let remaining = self.pending_breakdown();
                let message = format!(
                    "Push failed; {} change(s) stay queued: {e}",
                    remaining.total()
                );
                self.mark_offline(message.clone());
                self.emit(
                    SyncProgress::new("origin.push", SyncProgressState::Failed, message)
                        .with_duration(elapsed_ms(started))
                        .with_pending(remaining),
                );
                false
            }
        }
    }

    fn pull_with_connection(&self, conn: &Connection) -> bool {
        let pull_started = Instant::now();
        self.emit(SyncProgress::new(
            "origin.pull",
            SyncProgressState::Started,
            format!("Reading shared library for {}", self.playlists.join(", ")),
        ));

        let shows = match load_library_shows(conn, &self.playlists) {
            Ok(shows) => shows,
            Err(e) => {
                let message = format!("Failed to load shows from shared DB: {e}");
                self.mark_offline(message.clone());
                self.emit(
                    SyncProgress::new("origin.pull", SyncProgressState::Failed, message)
                        .with_duration(elapsed_ms(pull_started)),
                );
                return false;
            }
        };
        let queues = match load_library_queues(conn, &self.playlists) {
            Ok(queues) => queues,
            Err(e) => {
                let message = format!("Failed to load queues from shared DB: {e}");
                self.mark_offline(message.clone());
                self.emit(
                    SyncProgress::new("origin.pull", SyncProgressState::Failed, message)
                        .with_duration(elapsed_ms(pull_started)),
                );
                return false;
            }
        };
        let show_count = shows.len();
        let episode_count: usize = shows.iter().map(|show| show.episodes.len()).sum();
        let queue_count: usize = queues.iter().map(|queue| queue.entries.len()).sum();
        self.emit(
            SyncProgress::new(
                "origin.pull",
                SyncProgressState::Succeeded,
                format!(
                    "Read {show_count} show(s), {episode_count} episode(s), and {queue_count} queue entry(s)"
                ),
            )
            .with_duration(elapsed_ms(pull_started)),
        );

        let merge_started = Instant::now();
        self.emit(SyncProgress::new(
            "replica.merge",
            SyncProgressState::Started,
            "Merging shared changes into the local replica",
        ));
        self.replica.merge_shows(&shows);
        self.replica.merge_queues(&queues);
        self.mark_online();
        self.emit(
            SyncProgress::new(
                "replica.merge",
                SyncProgressState::Succeeded,
                format!("Merged {show_count} show(s) into the local replica"),
            )
            .with_duration(elapsed_ms(merge_started)),
        );
        true
    }

    fn open_shared(&self) -> Result<Connection, rusqlite::Error> {
        let path = self.shared_db_path.as_ref().ok_or_else(|| {
            rusqlite::Error::InvalidPath(std::path::PathBuf::from("No shared DB path configured"))
        })?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(conn)
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn load_library_shows(
    conn: &Connection,
    playlists: &[String],
) -> Result<Vec<LibraryShow>, rusqlite::Error> {
    if playlists.is_empty() {
        return Ok(vec![]);
    }
    let marks = vec!["?"; playlists.len()].join(",");
    let sql = format!(
        "SELECT id, playlist, name, root_path, date_added, removed_at, updated_at FROM shows WHERE playlist IN ({})",
        marks
    );
    let mut stmt = conn.prepare(&sql)?;

    let params_vec: Vec<&dyn rusqlite::ToSql> = playlists
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let mut shows = stmt
        .query_map(rusqlite::params_from_iter(params_vec), |r| {
            Ok(LibraryShow {
                id: r.get(0)?,
                playlist: r.get(1)?,
                name: r.get(2)?,
                root_path: r.get(3)?,
                date_added: r.get(4)?,
                removed_at: r.get(5)?,
                updated_at: r.get(6)?,
                episodes: vec![],
            })
        })?
        .collect::<Result<Vec<LibraryShow>, _>>()?;

    for show in &mut shows {
        let mut ep_stmt = conn.prepare(
            "SELECT id, relative_path, position, watched_at, resume_pos, updated_at FROM episodes WHERE show_id = ? ORDER BY position"
        )?;
        show.episodes = ep_stmt
            .query_map([&show.id], |r| {
                Ok(LibraryEpisode {
                    id: r.get(0)?,
                    relative_path: r.get(1)?,
                    position: r.get(2)?,
                    watched_at: r.get(3)?,
                    resume_pos: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<LibraryEpisode>, _>>()?;
    }

    Ok(shows)
}

fn load_library_queues(
    conn: &Connection,
    playlists: &[String],
) -> Result<Vec<LibraryQueue>, rusqlite::Error> {
    let mut queues = Vec::new();
    for pl in playlists {
        let meta_updated: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [format!("queue_updated:{pl}")],
                |r| r.get(0),
            )
            .optional()?;
        let mut stmt = conn.prepare(
            "SELECT episode_id, show_id, play_order, state, updated_at FROM round_queue WHERE playlist = ? ORDER BY play_order"
        )?;
        let mut entries = Vec::new();
        let mut max_updated_at = meta_updated
            .clone()
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

        let rows = stmt.query_map([pl], |r| {
            let ep_id: String = r.get(0)?;
            let show_id: String = r.get(1)?;
            let play_order: i32 = r.get(2)?;
            let state: String = r.get(3)?;
            let updated_at: String = r.get(4)?;
            Ok((ep_id, show_id, play_order, state, updated_at))
        })?;

        for row in rows {
            let (episode_id, show_id, play_order, state, updated_at) = row?;
            if updated_at > max_updated_at {
                max_updated_at = updated_at;
            }
            entries.push(RoundQueueEntry {
                episode_id,
                show_id,
                play_order,
                state,
            });
        }

        if !entries.is_empty() || meta_updated.is_some() {
            queues.push(LibraryQueue {
                playlist: pl.clone(),
                updated_at: max_updated_at,
                entries,
            });
        }
    }
    Ok(queues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replica::Replica;
    use tempfile::NamedTempFile;

    fn temp_db_path() -> String {
        NamedTempFile::new()
            .unwrap()
            .path()
            .to_string_lossy()
            .to_string()
    }

    fn missing_parent_db_path() -> String {
        let dir = tempfile::tempdir().unwrap();
        dir.path()
            .join("missing-parent")
            .join("shows.db")
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn seed_pulls_into_replica() {
        let r = Arc::new(Replica::new(":memory:"));
        let shared_path = temp_db_path();

        let shared_db = Replica::new(&shared_path);
        shared_db.create_show("nelson", "S1", "D:\\A", &["a.mkv".into(), "b.mkv".into()]);

        let s = Syncer::new(r.clone(), Some(shared_path), vec!["nelson".into()]);
        assert!(s.seed());
        assert!(s.online());

        let active = r.active_shows(&["nelson".into()]);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "S1");
    }

    #[test]
    fn seed_failure_records_last_error_and_source_path() {
        let r = Arc::new(Replica::new(":memory:"));
        let shared_path = missing_parent_db_path();
        let s = Syncer::new(r, Some(shared_path.clone()), vec!["nelson".into()]);

        assert!(!s.seed());
        assert!(!s.online());
        assert_eq!(s.shared_db_path(), Some(shared_path));
        assert!(
            s.last_error()
                .is_some_and(|e| e.contains("Failed to open shared DB for seed"))
        );
    }

    #[test]
    fn push_sends_dirty_and_clears() {
        let r = Arc::new(Replica::new(":memory:"));
        let shared_path = temp_db_path();

        r.create_show("nelson", "S1", "D:\\A", &["a.mkv".into()]);
        assert_eq!(r.pending().shows, 1);

        let s = Syncer::new(r.clone(), Some(shared_path.clone()), vec!["nelson".into()]);
        assert!(s.push());
        assert_eq!(s.pending(), 0);

        let shared_db = Replica::new(&shared_path);
        let active = shared_db.active_shows(&["nelson".into()]);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "S1");
    }

    #[test]
    fn pending_breakdown_matches_total() {
        let r = Arc::new(Replica::new(":memory:"));
        let show_id = r.create_show("nelson", "S1", "D:\\A", &["a.mkv".into()]);
        let episode_id = r.show(&show_id).unwrap().episodes[0].id.clone();
        r.save_round_queue(
            &[(episode_id, show_id, 0, "pending".into(), "nelson".into())],
            "2026-01-01T00:00:00Z",
            true,
        );

        let s = Syncer::new(r, Some(temp_db_path()), vec!["nelson".into()]);
        let pending = s.pending_breakdown();

        assert_eq!(pending.shows, 1);
        assert_eq!(pending.episodes, 1);
        assert_eq!(pending.queue, 1);
        assert_eq!(s.pending(), pending.total());
    }

    #[test]
    fn push_empty_repaired_queue_deletes_shared_queue() {
        let r = Arc::new(Replica::new(":memory:"));
        let shared_path = temp_db_path();
        let shared_db = Replica::new(&shared_path);
        let entries = [(
            "ep1".to_string(),
            "show1".to_string(),
            0,
            "pending".to_string(),
            "nelson".to_string(),
        )];

        r.save_round_queue(&entries, "2026-01-01T00:00:00Z", false);
        shared_db.save_round_queue(&entries, "2026-01-01T00:00:00Z", false);
        assert!(r.remove_round_entry("ep1").is_some());

        let s = Syncer::new(r.clone(), Some(shared_path.clone()), vec!["nelson".into()]);
        assert!(s.push());

        let shared_db = Replica::new(&shared_path);
        assert!(shared_db.get_round_queue().is_empty());
        assert_eq!(s.pending_breakdown().queue, 0);
    }

    #[test]
    fn seed_empty_repaired_queue_deletes_stale_local_queue() {
        let r = Arc::new(Replica::new(":memory:"));
        let shared_path = temp_db_path();
        let shared_db = Replica::new(&shared_path);
        let entries = [(
            "ep1".to_string(),
            "show1".to_string(),
            0,
            "pending".to_string(),
            "nelson".to_string(),
        )];

        r.save_round_queue(&entries, "2026-01-01T00:00:00Z", false);
        shared_db.save_round_queue(&entries, "2026-01-01T00:00:00Z", false);
        assert!(shared_db.remove_round_entry("ep1").is_some());
        shared_db.mark_queue_synced("nelson");

        let s = Syncer::new(r.clone(), Some(shared_path), vec!["nelson".into()]);
        assert!(s.seed());

        assert!(r.get_round_queue().is_empty());
    }

    #[test]
    fn push_without_dirty_still_refreshes_connectivity() {
        let r = Arc::new(Replica::new(":memory:"));
        let dir = tempfile::tempdir().unwrap();
        let shared_path = dir.path().join("missing-parent").join("shows.db");
        let s = Syncer::new(
            r,
            Some(shared_path.to_string_lossy().to_string()),
            vec!["nelson".into()],
        );

        assert!(!s.push());
        assert!(s.last_error().is_some());

        std::fs::create_dir_all(shared_path.parent().unwrap()).unwrap();
        assert!(s.push());
        assert!(s.online());
        assert!(s.last_error().is_none());
    }

    #[test]
    fn auto_seed_empty_shared_db() {
        let r = Arc::new(Replica::new(":memory:"));
        let shared_path = temp_db_path();

        r.create_show("nelson", "S1", "D:\\A", &["a.mkv".into()]);
        let show_id = r.active_shows(&["nelson".into()])[0].id.clone();
        r.mark_synced("shows", &[show_id.clone()]);
        let ep_ids: Vec<String> = r
            .show(&show_id)
            .unwrap()
            .episodes
            .iter()
            .map(|e| e.id.clone())
            .collect();
        r.mark_synced("episodes", &ep_ids);

        assert_eq!(r.pending().shows, 0);
        assert_eq!(r.pending().episodes, 0);

        let s = Syncer::new(r.clone(), Some(shared_path.clone()), vec!["nelson".into()]);
        assert!(s.push());
        assert_eq!(s.pending(), 0);

        let shared_db = Replica::new(&shared_path);
        let active = shared_db.active_shows(&["nelson".into()]);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "S1");
        assert_eq!(active[0].episodes.len(), 1);
    }

    #[test]
    fn full_sync_reuses_one_connection_and_emits_ordered_progress() {
        let local = Arc::new(Replica::new(":memory:"));
        let shared_path = temp_db_path();
        let shared = Replica::new(&shared_path);
        shared.create_show("nelson", "S1", "D:\\A", &["a.mkv".into()]);

        let syncer = Syncer::new(local.clone(), Some(shared_path), vec!["nelson".into()]);
        let events: Arc<Mutex<Vec<(String, SyncProgressState)>>> = Default::default();
        syncer.set_progress_callback(Some({
            let events = events.clone();
            Arc::new(move |progress| {
                events
                    .lock()
                    .unwrap()
                    .push((progress.stage, progress.state));
            })
        }));

        assert!(syncer.sync());

        let events = events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                ("origin.connect".into(), SyncProgressState::Started),
                ("origin.connect".into(), SyncProgressState::Succeeded),
                ("origin.push".into(), SyncProgressState::Started),
                ("origin.push".into(), SyncProgressState::Skipped),
                ("origin.pull".into(), SyncProgressState::Started),
                ("origin.pull".into(), SyncProgressState::Succeeded),
                ("replica.merge".into(), SyncProgressState::Started),
                ("replica.merge".into(), SyncProgressState::Succeeded),
                ("origin.complete".into(), SyncProgressState::Succeeded),
            ]
        );
        assert_eq!(
            events
                .iter()
                .filter(|(stage, state)| {
                    stage == "origin.connect" && *state == SyncProgressState::Started
                })
                .count(),
            1,
            "a full push/pull sync must open the shared database exactly once"
        );
        assert_eq!(local.active_shows(&["nelson".into()]).len(), 1);
    }

    #[test]
    fn failed_connection_reports_skipped_remote_work_before_completion() {
        let local = Arc::new(Replica::new(":memory:"));
        let syncer = Syncer::new(local, Some(missing_parent_db_path()), vec!["nelson".into()]);
        let events: Arc<Mutex<Vec<(String, SyncProgressState)>>> = Default::default();
        syncer.set_progress_callback(Some({
            let events = events.clone();
            Arc::new(move |progress| {
                events
                    .lock()
                    .unwrap()
                    .push((progress.stage, progress.state));
            })
        }));

        assert!(!syncer.sync());
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ("origin.connect".into(), SyncProgressState::Started),
                ("origin.connect".into(), SyncProgressState::Failed),
                ("origin.push".into(), SyncProgressState::Skipped),
                ("origin.pull".into(), SyncProgressState::Skipped),
                ("origin.complete".into(), SyncProgressState::Failed),
            ]
        );
    }
}
