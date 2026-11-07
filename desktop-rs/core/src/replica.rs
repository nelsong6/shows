//! SQLite persistence for the authoritative NAS database.
//!
//! The engine ([`crate::engine`]) holds the domain logic over plain structs;
//! this layer loads rows into those structs, runs the engine, and persists the
//! results. Every desktop opens this same database, so SQLite commit order—not
//! a local dirty-row preference—determines the visible state.
//!
//! Concurrency: one connection behind a `Mutex`, since the runner thread, the
//! control-server thread, and the UI thread all touch it. Volume is tiny.
//!
//! Failure: the database lives on a network share, so every call can fail and
//! returns the error. A connection that dies is replaced, and while none can be
//! had each call fails at once. See `docs/feature-contracts/round-and-advance.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params,
    params_from_iter,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::engine::{self, Episode, Show};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS shows (
  id         TEXT PRIMARY KEY,
  playlist   TEXT NOT NULL,
  name       TEXT NOT NULL,
  root_path  TEXT NOT NULL,
  date_added TEXT,
  removed_at TEXT,
  updated_at TEXT NOT NULL,
  dirty      INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS episodes (
  id            TEXT PRIMARY KEY,
  show_id       TEXT NOT NULL,
  relative_path TEXT NOT NULL,
  position      INTEGER NOT NULL,
  watched_at    TEXT,
  resume_pos    REAL,
  updated_at    TEXT NOT NULL,
  dirty         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_episodes_show ON episodes(show_id);
CREATE TABLE IF NOT EXISTS watch_history (
  id            TEXT PRIMARY KEY,
  show_id       TEXT NOT NULL,
  episode_id    TEXT NOT NULL,
  relative_path TEXT,
  played_at     TEXT NOT NULL,
  dirty         INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS round_queue (
  episode_id     TEXT PRIMARY KEY,
  show_id        TEXT NOT NULL,
  play_order     INTEGER NOT NULL,
  state          TEXT NOT NULL, -- 'pending', 'playing', 'watched', 'deferred'
  playlist       TEXT NOT NULL,
  updated_at     TEXT NOT NULL,
  dirty          INTEGER NOT NULL DEFAULT 0
);
-- One now-playing entry per playlist (contract M3).
CREATE UNIQUE INDEX IF NOT EXISTS round_queue_one_playing ON round_queue(playlist) WHERE state='playing';
CREATE TABLE IF NOT EXISTS episode_playback_preferences (
  episode_id     TEXT PRIMARY KEY,
  subtitle_track TEXT,
  audio_track    TEXT,
  updated_at     TEXT NOT NULL
);
INSERT OR IGNORE INTO meta(key, value) VALUES('revision', '0');
CREATE TRIGGER IF NOT EXISTS shows_revision_insert AFTER INSERT ON shows BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS shows_revision_update AFTER UPDATE ON shows BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS shows_revision_delete AFTER DELETE ON shows BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS episodes_revision_insert AFTER INSERT ON episodes BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS episodes_revision_update AFTER UPDATE ON episodes BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS episodes_revision_delete AFTER DELETE ON episodes BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS history_revision_insert AFTER INSERT ON watch_history BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS history_revision_delete AFTER DELETE ON watch_history BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS queue_revision_insert AFTER INSERT ON round_queue BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS queue_revision_update AFTER UPDATE ON round_queue BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS queue_revision_delete AFTER DELETE ON round_queue BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS preferences_revision_insert AFTER INSERT ON episode_playback_preferences BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS preferences_revision_update AFTER UPDATE ON episode_playback_preferences BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
CREATE TRIGGER IF NOT EXISTS preferences_revision_delete AFTER DELETE ON episode_playback_preferences BEGIN
  UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision';
END;
";

/// The state a demoted `playing` entry returns to: the episode's own watched
/// status decides, so a revisited finished episode goes back to `watched`.
const DEMOTED_STATE: &str = "CASE WHEN EXISTS(SELECT 1 FROM episodes e \
     WHERE e.id=round_queue.episode_id AND e.watched_at IS NOT NULL) \
     THEN 'watched' ELSE 'pending' END";

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodePlaybackPreferences {
    pub subtitle_track: Option<String>,
    pub audio_track: Option<String>,
}

pub type DbError = rusqlite::Error;
pub type DbResult<T> = Result<T, DbError>;

pub struct Replica {
    path: String,
    /// `None` while the database can't be reached: calls then fail at once
    /// instead of waiting on the network, and [`Replica::revision`] reconnects.
    connection: Mutex<Option<Connection>>,
    reachable: AtomicBool,
    last_failure: Mutex<Option<String>>,
}

/// A connection that hit one of these is finished. Windows gives an open file's
/// handle up for good when its SMB session drops, so the connection keeps
/// failing after the share returns, while a new one works at once.
fn is_connection_failure(error: &DbError) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(
            ErrorCode::SystemIoFailure
                | ErrorCode::CannotOpen
                | ErrorCode::DatabaseBusy
                | ErrorCode::DatabaseLocked
                | ErrorCode::DatabaseCorrupt
                | ErrorCode::NotADatabase
                | ErrorCode::FileLockingProtocolFailed
                | ErrorCode::ReadOnly
                | ErrorCode::DiskFull
        )
    )
}

fn unreachable_error() -> DbError {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some("the database is unreachable".into()),
    )
}

fn open_connection(path: &str, flags: OpenFlags) -> DbResult<Connection> {
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Replica {
    /// A replica that must work now; for tests and tools.
    pub fn new(path: &str) -> Replica {
        let replica = Self::connect(path);
        assert!(
            replica.is_reachable(),
            "open shows database: {:?}",
            replica.failure()
        );
        replica
    }

    /// Open `path`. An unreachable database still gives a replica: it fails
    /// each call explicitly until [`Replica::revision`] gets a connection.
    pub fn connect(path: &str) -> Replica {
        let opened = open_connection(path, OpenFlags::default());
        let replica = Replica {
            path: path.to_string(),
            connection: Mutex::new(None),
            reachable: AtomicBool::new(false),
            last_failure: Mutex::new(None),
        };
        match opened {
            Ok(conn) => replica.install(conn),
            Err(e) => replica.record_failure(&e),
        }
        replica
    }

    /// Reconnections never create the file: a missing database is a failure,
    /// not an empty library.
    fn reopen(&self) -> DbResult<Connection> {
        open_connection(
            &self.path,
            OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_CREATE),
        )
    }

    fn install(&self, conn: Connection) {
        *lock(&self.connection) = Some(conn);
        *lock(&self.last_failure) = None;
        self.reachable.store(true, Ordering::SeqCst);
    }

    fn record_failure(&self, error: &DbError) {
        if self.reachable.swap(false, Ordering::SeqCst) {
            log::error!("database {} unreachable: {error}", self.path);
        }
        *lock(&self.last_failure) = Some(error.to_string());
    }

    pub fn is_reachable(&self) -> bool {
        self.reachable.load(Ordering::SeqCst)
    }

    /// Why the database can't be reached, while it can't.
    pub fn failure(&self) -> Option<String> {
        lock(&self.last_failure).clone()
    }

    /// Run `op` on the connection. A connection that fails is replaced once
    /// straight away, since a dropped session often comes right back; if that
    /// fails too the database is unreachable until [`Replica::revision`]
    /// reconnects. `op` may run twice: its uncommitted work rolled back with
    /// the failed connection.
    fn with_conn<T>(&self, mut op: impl FnMut(&mut Connection) -> DbResult<T>) -> DbResult<T> {
        let mut connection = lock(&self.connection);
        let conn = connection.as_mut().ok_or_else(unreachable_error)?;
        let error = match op(conn) {
            Err(e) if is_connection_failure(&e) => e,
            outcome => return outcome,
        };
        log::warn!("database connection failed: {error}");
        *connection = None;
        let mut fresh = match self.reopen() {
            Ok(fresh) => fresh,
            Err(e) => {
                self.record_failure(&e);
                return Err(e);
            }
        };
        let outcome = op(&mut fresh);
        match &outcome {
            Err(e) if is_connection_failure(e) => self.record_failure(e),
            _ => *connection = Some(fresh),
        }
        outcome
    }

    /// The shared change counter. This is also how an unreachable database is
    /// noticed coming back: it reconnects, outside the lock so other calls keep
    /// failing fast while the network decides.
    pub fn revision(&self) -> DbResult<i64> {
        let connected = lock(&self.connection).is_some();
        if !connected {
            match self.reopen() {
                Ok(conn) => {
                    self.install(conn);
                    log::info!("database {} reachable again", self.path);
                }
                Err(e) => {
                    self.record_failure(&e);
                    return Err(e);
                }
            }
        }
        self.with_conn(|conn| {
            conn.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='revision'", [], |r| r.get(0))
        })
    }

    #[cfg(test)]
    pub fn seed_test_shows(&self, shows: &[Show]) {
        let mut guard = self.connection.lock().unwrap();
        let conn = guard.as_mut().expect("test database is open");
        let tx = conn.transaction().expect("begin test seed");
        let updated_at = now();
        for show in shows {
            tx.execute(
                "INSERT INTO shows(id,playlist,name,root_path,date_added,removed_at,updated_at,dirty) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
                params![show.id, show.playlist, show.name, show.root_path, show.date_added, show.removed_at, updated_at],
            )
            .expect("seed show");
            for episode in &show.episodes {
                tx.execute(
                    "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
                    params![episode.id, show.id, episode.relative_path, episode.position, episode.watched_at, episode.resume_pos, updated_at],
                )
                .expect("seed episode");
            }
        }
        tx.commit().expect("commit test seed");
    }

    /// Lose the connection the way a dropped share does, for tests.
    #[cfg(test)]
    pub fn drop_connection_for_test(&self) {
        *lock(&self.connection) = None;
        self.record_failure(&unreachable_error());
    }

    // ── reads ───────────────────────────────────────────────────────────
    /// Non-removed shows in the given playlists, with episodes — the input to
    /// [`engine::next_round`] (pass several playlists for a cross-playlist round).
    pub fn active_shows(&self, playlists: &[String]) -> DbResult<Vec<Show>> {
        if playlists.is_empty() {
            return Ok(vec![]);
        }
        self.with_conn(|conn| {
            let marks = vec!["?"; playlists.len()].join(",");
            let sql =
                format!("SELECT id FROM shows WHERE removed_at IS NULL AND playlist IN ({marks})");
            let mut stmt = conn.prepare(&sql)?;
            let ids = stmt
                .query_map(params_from_iter(playlists.iter()), |r| r.get::<_, String>(0))?
                .collect::<DbResult<Vec<_>>>()?;
            load_shows(conn, &ids)
        })
    }

    pub fn all_shows(&self) -> DbResult<Vec<Show>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM shows WHERE removed_at IS NULL")?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<DbResult<Vec<_>>>()?;
            load_shows(conn, &ids)
        })
    }

    pub fn show(&self, show_id: &str) -> DbResult<Option<Show>> {
        self.with_conn(|conn| load_show(conn, show_id))
    }

    pub fn resume_pos(&self, episode_id: &str) -> DbResult<Option<f64>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT resume_pos FROM episodes WHERE id=?1",
                    params![episode_id],
                    |r| r.get::<_, Option<f64>>(0),
                )
                .optional()?
                .flatten())
        })
    }

    pub fn playback_preferences(&self, episode_id: &str) -> DbResult<EpisodePlaybackPreferences> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT subtitle_track, audio_track FROM episode_playback_preferences WHERE episode_id=?1",
                    params![episode_id],
                    |r| {
                        Ok(EpisodePlaybackPreferences {
                            subtitle_track: r.get(0)?,
                            audio_track: r.get(1)?,
                        })
                    },
                )
                .optional()?
                .unwrap_or_default())
        })
    }

    pub fn get_volume(&self) -> DbResult<Option<f64>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row("SELECT value FROM meta WHERE key = 'volume'", [], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
                .and_then(|s| s.parse().ok()))
        })
    }

    pub fn set_volume(&self, vol: f64) -> DbResult<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('volume', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![vol.to_string()],
            )?;
            Ok(())
        })
    }

    // ── mutations ───────────────────────────────────────────────────────
    /// Mark `(show_id, episode_id)` pairs watched via the engine; persist
    /// watched_at/removed_at, append history. Returns (newly-watched, removed ids).
    #[cfg(test)]
    pub fn advance(&self, entries: &[(String, String)]) -> DbResult<(usize, Vec<String>)> {
        self.advance_at(entries, &now())
    }

    /// [`Replica::advance`] with the time the episodes were watched, which
    /// differs from now when a mark waited out an outage.
    pub fn advance_at(
        &self,
        entries: &[(String, String)],
        at: &str,
    ) -> DbResult<(usize, Vec<String>)> {
        let mut by_show: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (sid, eid) in entries {
            by_show.entry(sid.clone()).or_default().push(eid.clone());
        }
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut advanced_total = 0;
            let mut removed = Vec::new();
            for (sid, eids) in &by_show {
                let Some(mut sh) = load_show(&tx, sid)? else {
                    continue;
                };
                let (history, n, tombstoned) = engine::advance(&mut sh, eids, at);
                if n == 0 {
                    continue;
                }
                for h in &history {
                    tx.execute(
                        "UPDATE episodes SET watched_at=?1, resume_pos=NULL, updated_at=?2, dirty=0 WHERE id=?3",
                        params![at, at, h.episode_id],
                    )?;
                    tx.execute(
                        "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty) VALUES(?1,?2,?3,?4,?5,0)",
                        params![Uuid::new_v4().to_string(), h.show_id, h.episode_id, h.relative_path, h.played_at],
                    )?;
                }
                if tombstoned {
                    tx.execute(
                        "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                        params![at, at, sid],
                    )?;
                    removed.push(sid.clone());
                }
                advanced_total += n;
            }
            tx.commit()?;
            Ok((advanced_total, removed))
        })
    }

    pub fn defer(&self, show_id: &str, episode_id: &str) -> DbResult<bool> {
        let now = now();
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(mut sh) = load_show(&tx, show_id)? else {
                return Ok(false);
            };
            if !engine::defer(&mut sh.episodes, episode_id) {
                return Ok(false);
            }
            let pos = sh
                .episodes
                .iter()
                .find(|e| e.id == episode_id)
                .map(|e| e.position)
                .expect("deferred episode present");
            tx.execute(
                "UPDATE episodes SET position=?1, updated_at=?2, dirty=0 WHERE id=?3",
                params![pos, now, episode_id],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    pub fn set_resume(&self, episode_id: &str, pos: Option<f64>) -> DbResult<()> {
        let now = now();
        self.with_conn(|conn| {
            let current = conn
                .query_row(
                    "SELECT resume_pos FROM episodes WHERE id=?1",
                    params![episode_id],
                    |r| r.get::<_, Option<f64>>(0),
                )
                .optional()?
                .flatten();
            if resume_pos_matches(current, pos) {
                return Ok(());
            }
            conn.execute(
                "UPDATE episodes SET resume_pos=?1, updated_at=?2, dirty=0 WHERE id=?3",
                params![pos, now, episode_id],
            )?;
            Ok(())
        })
    }

    pub fn set_subtitle_track(&self, episode_id: &str, sid: &str) -> DbResult<()> {
        self.set_playback_preference(episode_id, "subtitle_track", sid)
    }

    pub fn set_audio_track(&self, episode_id: &str, aid: &str) -> DbResult<()> {
        self.set_playback_preference(episode_id, "audio_track", aid)
    }

    fn set_playback_preference(&self, episode_id: &str, column: &str, value: &str) -> DbResult<()> {
        debug_assert!(matches!(column, "subtitle_track" | "audio_track"));
        let now = now();
        self.with_conn(|conn| {
            let current: Option<String> = conn
                .query_row(
                    &format!("SELECT {column} FROM episode_playback_preferences WHERE episode_id=?1"),
                    params![episode_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            if current.as_deref() == Some(value) {
                return Ok(());
            }
            conn.execute(
                &format!(
                    "INSERT INTO episode_playback_preferences(episode_id,{column},updated_at) VALUES(?1,?2,?3)
                     ON CONFLICT(episode_id) DO UPDATE SET {column}=excluded.{column}, updated_at=excluded.updated_at"
                ),
                params![episode_id, value, now],
            )?;
            Ok(())
        })
    }

    // ── library management ──────────────────────────────────────────────
    pub fn create_show(
        &self,
        playlist: &str,
        name: &str,
        root_path: &str,
        episodes: &[String],
    ) -> DbResult<String> {
        let now = now();
        let sid = Uuid::new_v4().to_string();
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO shows(id,playlist,name,root_path,date_added,removed_at,updated_at,dirty) VALUES(?1,?2,?3,?4,?5,NULL,?6,0)",
                params![sid, playlist, name, root_path, now, now],
            )?;
            for (i, rel) in episodes.iter().enumerate() {
                tx.execute(
                    "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty) VALUES(?1,?2,?3,?4,NULL,NULL,?5,0)",
                    params![Uuid::new_v4().to_string(), sid, rel, i as i64, now],
                )?;
            }
            tx.commit()?;
            Ok(sid.clone())
        })
    }

    pub fn update_show(
        &self,
        show_id: &str,
        name: Option<&str>,
        root_path: Option<&str>,
        playlist: Option<&str>,
    ) -> DbResult<bool> {
        let now = now();
        let mut sets: Vec<&str> = Vec::new();
        let mut vals: Vec<String> = Vec::new();
        if let Some(n) = name {
            sets.push("name=?");
            vals.push(n.to_string());
        }
        if let Some(r) = root_path {
            sets.push("root_path=?");
            vals.push(r.to_string());
        }
        if let Some(p) = playlist {
            sets.push("playlist=?");
            vals.push(p.to_string());
        }
        if sets.is_empty() {
            return Ok(false);
        }
        sets.push("updated_at=?");
        vals.push(now);
        vals.push(show_id.to_string()); // for WHERE id=?
        let sql = format!("UPDATE shows SET {} WHERE id=?", sets.join(", "));
        self.with_conn(|conn| Ok(conn.execute(&sql, params_from_iter(vals.iter()))? > 0))
    }

    /// Tombstone a show (removes it from rotation). Soft-delete — the engine
    /// skips removed shows.
    pub fn remove_show(&self, show_id: &str) -> DbResult<bool> {
        let now = now();
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                params![now, now, show_id],
            )?;
            Ok(n > 0)
        })
    }

    /// Mark all episodes of a show as watched, and tombstone/remove the show.
    pub fn mark_show_watched(&self, show_id: &str) -> DbResult<bool> {
        let now = now();
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let unwatched: Vec<(String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, relative_path FROM episodes WHERE show_id=?1 AND watched_at IS NULL",
                )?;
                stmt.query_map(params![show_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<DbResult<_>>()?
            };

            for (eid, rel) in &unwatched {
                tx.execute(
                    "UPDATE episodes SET watched_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                    params![now, now, eid],
                )?;
                tx.execute(
                    "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty) VALUES(?1,?2,?3,?4,?5,0)",
                    params![Uuid::new_v4().to_string(), show_id, eid, rel, now],
                )?;
            }

            let n = tx.execute(
                "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                params![now, now, show_id],
            )?;
            tx.commit()?;
            Ok(n > 0)
        })
    }

    /// Mark all episodes of a show as unwatched, reset resume positions, and restore the show.
    pub fn mark_show_unwatched(&self, show_id: &str) -> DbResult<()> {
        let now = now();
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE shows SET removed_at=NULL, updated_at=?1, dirty=0 WHERE id=?2",
                params![now, show_id],
            )?;
            tx.execute(
                "UPDATE episodes SET watched_at=NULL, resume_pos=NULL, updated_at=?1, dirty=0 WHERE show_id=?2",
                params![now, show_id],
            )?;
            tx.execute(
                "DELETE FROM watch_history WHERE show_id=?1",
                params![show_id],
            )?;
            tx.commit()
        })
    }

    pub fn rescan_episodes(&self, show_id: &str, sorted_rels: &[String]) -> DbResult<Vec<String>> {
        if sorted_rels.is_empty() {
            return Ok(Vec::new());
        }
        let now = now();
        self.with_conn(|conn| {
            let tx = conn.transaction()?;

            let existing: HashSet<String> = {
                let mut stmt = tx.prepare("SELECT relative_path FROM episodes WHERE show_id=?1")?;
                stmt.query_map(params![show_id], |r| r.get(0))?
                    .collect::<DbResult<_>>()?
            };

            let mut added_ids = Vec::new();

            for (i, rel) in sorted_rels.iter().enumerate() {
                if !existing.contains(rel) {
                    let new_id = Uuid::new_v4().to_string();
                    tx.execute(
                        "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty) VALUES(?1,?2,?3,?4,NULL,NULL,?5,0)",
                        params![new_id, show_id, rel, i as i64, now],
                    )?;
                    added_ids.push(new_id);
                } else {
                    tx.execute(
                        "UPDATE episodes SET position=?1, updated_at=?2, dirty=0 WHERE show_id=?3 AND relative_path=?4",
                        params![i as i64, now, show_id, rel],
                    )?;
                }
            }

            tx.commit()?;
            Ok(added_ids)
        })
    }

    pub fn mark_episodes_watched(&self, show_id: &str, episode_ids: &[String]) -> DbResult<()> {
        if episode_ids.is_empty() {
            return Ok(());
        }
        let now = now();
        self.with_conn(|conn| {
            let tx = conn.transaction()?;
            for eid in episode_ids {
                // Retrieve relative_path to insert into watch_history
                let rel: Option<String> = tx
                    .query_row(
                        "SELECT relative_path FROM episodes WHERE id=?1",
                        params![eid],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(rel) = rel.filter(|rel| !rel.is_empty()) else {
                    continue;
                };

                tx.execute(
                    "UPDATE episodes SET watched_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                    params![now, now, eid],
                )?;
                tx.execute(
                    "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty) VALUES(?1,?2,?3,?4,?5,0)",
                    params![Uuid::new_v4().to_string(), show_id, eid, rel, now],
                )?;
            }
            tx.commit()
        })
    }

    /// Relative paths already known for a show — for diffing a rescan.
    pub fn episode_paths(&self, show_id: &str) -> DbResult<HashSet<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT relative_path FROM episodes WHERE show_id=?1")?;
            stmt.query_map(params![show_id], |r| r.get::<_, String>(0))?
                .collect()
        })
    }

    pub fn get_round_queue(&self) -> DbResult<Vec<(String, String, i32, String, String, String, i32)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT episode_id, show_id, play_order, state, playlist, updated_at, dirty FROM round_queue ORDER BY play_order",
            )?;
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i32>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i32>(6)?,
                ))
            })?
            .collect()
        })
    }

    pub fn save_round_queue(
        &self,
        entries: &[(String, String, i32, String, String)],
        updated_at: &str,
    ) -> DbResult<()> {
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM round_queue", [])?;
            let mut playlists = HashSet::new();
            for entry in entries {
                playlists.insert(entry.4.clone());
                tx.execute(
                    "INSERT INTO round_queue (episode_id, show_id, play_order, state, playlist, updated_at, dirty) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                    params![entry.0, entry.1, entry.2, entry.3, entry.4, updated_at],
                )?;
            }
            for playlist in playlists {
                tx.execute(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                    params![format!("queue_updated:{playlist}"), updated_at],
                )?;
            }
            tx.commit()
        })
    }

    pub fn update_round_entry_state(
        &self,
        episode_id: &str,
        state: &str,
        playlist: &str,
    ) -> DbResult<()> {
        let now = now();
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE round_queue SET state=?1, updated_at=?2, dirty=0 WHERE episode_id=?3",
                params![state, now, episode_id],
            )?;
            tx.execute(
                "UPDATE round_queue SET updated_at=?1, dirty=0 WHERE playlist=?2",
                params![now, playlist],
            )?;
            tx.commit()
        })
    }

    /// Make `episode_id` the one `playing` entry (M3): the entry it replaces in
    /// any of `playlists` returns to `watched` or `pending` by its episode's
    /// watched status, in the same transaction.
    pub fn set_round_playing(&self, episode_id: &str, playlists: &[String]) -> DbResult<()> {
        let now = now();
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let marks = (3..playlists.len() + 3)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let mut args: Vec<&str> = vec![now.as_str(), episode_id];
            args.extend(playlists.iter().map(String::as_str));
            tx.execute(
                &format!(
                    "UPDATE round_queue SET state={DEMOTED_STATE}, updated_at=?1, dirty=0 \
                     WHERE state='playing' AND episode_id<>?2 AND playlist IN ({marks})"
                ),
                params_from_iter(args.iter()),
            )?;
            tx.execute(
                "UPDATE round_queue SET state='playing', updated_at=?1, dirty=0 WHERE episode_id=?2",
                params![now, episode_id],
            )?;
            for playlist in playlists {
                tx.execute(
                    "UPDATE round_queue SET updated_at=?1, dirty=0 WHERE playlist=?2",
                    params![now, playlist],
                )?;
            }
            tx.commit()
        })
    }

    pub fn remove_round_entry(&self, episode_id: &str) -> DbResult<Option<(String, String)>> {
        let now = now();
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let found: Option<(String, String)> = tx
                .query_row(
                    "SELECT show_id, playlist FROM round_queue WHERE episode_id=?1",
                    params![episode_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
            let Some((show_id, playlist)) = found else {
                return Ok(None);
            };
            tx.execute(
                "DELETE FROM round_queue WHERE episode_id=?1",
                params![episode_id],
            )?;
            tx.execute(
                "UPDATE round_queue SET updated_at=?1, dirty=0 WHERE playlist=?2",
                params![now, playlist],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                params![format!("queue_updated:{playlist}"), now],
            )?;
            tx.commit()?;
            Ok(Some((show_id, playlist)))
        })
    }
    // ── dashboard payloads ───────────────────────────────────────────────
    /// Active shows for the dashboard sidebar (no episodes).
    pub fn overlay_shows(&self, playlists: &[String]) -> DbResult<Vec<Value>> {
        Ok(self
            .active_shows(playlists)?
            .into_iter()
            .map(|s| {
                json!({
                    "id": s.id, "playlist": s.playlist, "name": s.name,
                    "root_path": s.root_path, "date_added": s.date_added.unwrap_or_default(),
                    "removed_at": s.removed_at,
                })
            })
            .collect())
    }

    /// Watch-history rows for a show, oldest first.
    pub fn show_history(&self, show_id: &str) -> DbResult<Vec<Value>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT episode_id, relative_path, played_at FROM watch_history WHERE show_id=?1 ORDER BY played_at",
            )?;
            stmt.query_map(params![show_id], |r| {
                Ok(json!({
                    "episode_id": r.get::<_, String>(0)?,
                    "relative_path": r.get::<_, Option<String>>(1)?,
                    "played_at": r.get::<_, String>(2)?,
                }))
            })?
            .collect()
        })
    }

    /// The "just finished" payload for a tombstoned show — name, date added, and
    /// last play time — for the overlay reveal.
    pub fn reveal(&self, show_id: &str) -> DbResult<Value> {
        self.with_conn(|conn| {
            let s: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT name, date_added FROM shows WHERE id=?1",
                    params![show_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let last: Option<String> = conn
                .query_row(
                    "SELECT MAX(played_at) FROM watch_history WHERE show_id=?1",
                    params![show_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            let (name, date_added) = s.unwrap_or_default();
            Ok(json!({
                "id": show_id,
                "name": name,
                "date_added": date_added.unwrap_or_default(),
                "last_played_at": last.unwrap_or_default(),
            }))
        })
    }

    /// Library + watch stats for the dashboard: totals, per-show progress,
    /// recent activity, and per-day heatmap.
    pub fn stats(&self, playlists: &[String]) -> DbResult<Value> {
        if playlists.is_empty() {
            return Ok(json!({}));
        }
        self.with_conn(|conn| {
            let marks = vec!["?"; playlists.len()].join(",");

            let mut stmt = conn.prepare(&format!(
                "SELECT id, name, playlist, removed_at FROM shows WHERE playlist IN ({marks})"
            ))?;
            let shows: Vec<(String, String, String, Option<String>)> = stmt
                .query_map(params_from_iter(playlists.iter()), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<DbResult<_>>()?;

            let mut name_by: HashMap<String, String> = HashMap::new();
            let mut per: Vec<PerShow> = Vec::new();
            let (mut ep_total, mut ep_watched, mut finished) = (0i64, 0i64, 0i64);
            for (id, name, playlist, removed_at) in &shows {
                name_by.insert(id.clone(), name.clone());
                let (total, w): (i64, i64) = conn.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(CASE WHEN watched_at IS NOT NULL THEN 1 ELSE 0 END),0) FROM episodes WHERE show_id=?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                ep_total += total;
                ep_watched += w;
                if removed_at.is_some() {
                    finished += 1;
                }
                per.push(PerShow {
                    name: name.clone(),
                    playlist: playlist.clone(),
                    watched: w,
                    total,
                    removed: removed_at.is_some(),
                });
            }

            // Recent + heatmap come from episodes.watched_at (pulled in /library), so
            // they reflect the FULL history, not just this client's local rows.
            let mut stmt = conn.prepare(&format!(
                "SELECT e.show_id, e.relative_path, e.watched_at FROM episodes e \
                 JOIN shows s ON e.show_id = s.id \
                 WHERE e.watched_at IS NOT NULL AND s.playlist IN ({marks}) \
                 ORDER BY e.watched_at DESC LIMIT 25"
            ))?;
            let recent: Vec<Value> = stmt
                .query_map(params_from_iter(playlists.iter()), |r| {
                    let sid: String = r.get(0)?;
                    let rp: String = r.get(1)?;
                    let wa: String = r.get(2)?;
                    Ok(json!({
                        "show": name_by.get(&sid).cloned().unwrap_or_else(|| "?".into()),
                        "relative_path": rp, "played_at": wa,
                    }))
                })?
                .collect::<DbResult<_>>()?;

            let cutoff = (chrono::Utc::now() - chrono::Duration::days(140)).to_rfc3339();
            let mut day_params: Vec<String> = playlists.to_vec();
            day_params.push(cutoff);
            let mut stmt = conn.prepare(&format!(
                "SELECT substr(e.watched_at,1,10) d, COUNT(*) c FROM episodes e \
                 JOIN shows s ON e.show_id = s.id \
                 WHERE e.watched_at IS NOT NULL AND s.playlist IN ({marks}) AND e.watched_at >= ? \
                 GROUP BY d"
            ))?;
            let mut by_day = serde_json::Map::new();
            for row in stmt.query_map(params_from_iter(day_params.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                let row = row?;
                by_day.insert(row.0, json!(row.1));
            }

            per.sort_by(|a, b| {
                b.ratio()
                    .partial_cmp(&a.ratio())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let active = shows.iter().filter(|s| s.3.is_none()).count();
            Ok(json!({
                "total_shows": shows.len(),
                "active_shows": active,
                "finished_shows": finished,
                "episodes_total": ep_total,
                "episodes_watched": ep_watched,
                "per_show": per.iter().map(PerShow::to_json).collect::<Vec<_>>(),
                "recent": recent,
                "by_day": Value::Object(by_day),
            }))
        })
    }
}
fn resume_pos_matches(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 0.5,
        (None, None) => true,
        _ => false,
    }
}

struct PerShow {
    name: String,
    playlist: String,
    watched: i64,
    total: i64,
    removed: bool,
}

impl PerShow {
    fn ratio(&self) -> f64 {
        if self.total > 0 {
            self.watched as f64 / self.total as f64
        } else {
            0.0
        }
    }
    fn to_json(&self) -> Value {
        json!({
            "name": self.name, "playlist": self.playlist,
            "watched": self.watched, "total": self.total, "removed": self.removed,
        })
    }
}

fn load_shows(conn: &Connection, ids: &[String]) -> DbResult<Vec<Show>> {
    let mut shows = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(show) = load_show(conn, id)? {
            shows.push(show);
        }
    }
    Ok(shows)
}

fn load_show(conn: &Connection, show_id: &str) -> DbResult<Option<Show>> {
    let base: Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT id,playlist,name,root_path,date_added,removed_at FROM shows WHERE id=?1",
            params![show_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((id, playlist, name, root_path, date_added, removed_at)) = base else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT id,relative_path,position,watched_at,resume_pos FROM episodes WHERE show_id=?1 ORDER BY position",
    )?;
    let episodes = stmt
        .query_map(params![id], |r| {
            Ok(Episode {
                id: r.get(0)?,
                relative_path: r.get(1)?,
                position: r.get(2)?,
                watched_at: r.get(3)?,
                resume_pos: r.get(4)?,
            })
        })?
        .collect::<DbResult<_>>()?;
    Ok(Some(Show {
        id,
        playlist,
        name,
        root_path,
        episodes,
        removed_at,
        date_added,
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{first_unwatched, next_round};
    use std::collections::BTreeSet;

    const T0: &str = "2026-01-01T00:00:00Z";

    fn lep(id: &str, rel: &str, pos: i64) -> Episode {
        Episode {
            id: id.into(),
            relative_path: rel.into(),
            position: pos,
            watched_at: None,
            resume_pos: None,
        }
    }

    fn seed(r: &Replica) {
        r.seed_test_shows(&[
            Show {
                id: "s1".into(),
                playlist: "nelson".into(),
                name: "S1".into(),
                root_path: "D:\\A".into(),
                episodes: vec![lep("a", "a.mkv", 0), lep("b", "b.mkv", 1)],
                removed_at: None,
                date_added: None,
            },
            Show {
                id: "s2".into(),
                playlist: "nelson".into(),
                name: "S2".into(),
                root_path: "D:\\B".into(),
                episodes: vec![lep("c", "c.mkv", 0)],
                removed_at: None,
                date_added: None,
            },
        ]);
    }

    fn ids(shows: &[Show]) -> BTreeSet<&str> {
        shows.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn separate_apps_observe_shared_commits_and_last_write_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shows.db");
        let path = path.to_string_lossy();
        let first = Replica::new(&path);
        let second = Replica::new(&path);
        let show_id = first.create_show("nelson", "original", r"S:\Shows\Original", &[]).unwrap();
        let after_create = second.revision().unwrap();

        assert_eq!(second.show(&show_id).unwrap().unwrap().name, "original");
        second.update_show(&show_id, Some("second app"), None, None).unwrap();
        assert_eq!(first.show(&show_id).unwrap().unwrap().name, "second app");
        assert!(first.revision().unwrap() > after_create);

        first.update_show(&show_id, Some("first app last"), None, None).unwrap();
        assert_eq!(second.show(&show_id).unwrap().unwrap().name, "first app last");
    }

    #[test]
    fn seed_and_next_round() {
        let r = Replica::new(":memory:");
        seed(&r);
        let shows = r.active_shows(&["nelson".into()]).unwrap();
        assert_eq!(ids(&shows), ["s1", "s2"].into_iter().collect());
        let rnd = next_round(&shows);
        let rids: BTreeSet<&str> = rnd.iter().map(|o| o.show_id.as_str()).collect();
        assert_eq!(rids, ["s1", "s2"].into_iter().collect());
        assert_eq!(
            rnd.iter().find(|o| o.show_id == "s1").unwrap().episode_id,
            "a"
        );
    }

    #[test]
    fn advance_persists_and_advances_pick() {
        let r = Replica::new(":memory:");
        seed(&r);
        let (n, removed) = r.advance(&[("s1".into(), "a".into())]).unwrap();
        assert_eq!(n, 1);
        assert!(removed.is_empty());
        let s1 = r.show("s1").unwrap().unwrap();
        assert!(
            s1.episodes
                .iter()
                .find(|e| e.id == "a")
                .unwrap()
                .watched_at
                .is_some()
        );
        let nr = next_round(&r.active_shows(&["nelson".into()]).unwrap());
        assert_eq!(
            nr.iter().find(|o| o.show_id == "s1").unwrap().episode_id,
            "b"
        );
    }

    #[test]
    fn advance_tombstones_drained_show() {
        let r = Replica::new(":memory:");
        seed(&r);
        let (n, removed) = r.advance(&[("s2".into(), "c".into())]).unwrap();
        assert_eq!(n, 1);
        assert_eq!(removed, ["s2"]);
        assert!(r.show("s2").unwrap().unwrap().removed_at.is_some());
        assert_eq!(
            ids(&r.active_shows(&["nelson".into()]).unwrap()),
            ["s1"].into_iter().collect()
        );
    }

    #[test]
    fn defer_persists_position_and_changes_pick() {
        let r = Replica::new(":memory:");
        seed(&r);
        assert!(r.defer("s1", "a").unwrap());
        let s1 = r.show("s1").unwrap().unwrap();
        assert_eq!(
            s1.episodes.iter().find(|e| e.id == "a").unwrap().position,
            2
        );
        assert_eq!(first_unwatched(&s1.episodes).unwrap().id, "b");
        r.advance(&[("s1".into(), "b".into())]).unwrap();
        assert!(!r.defer("s1", "b").unwrap()); // now watched -> no-op
    }

    #[test]
    fn set_resume_persists() {
        let r = Replica::new(":memory:");
        seed(&r);
        r.set_resume("a", Some(123.5)).unwrap();
        assert_eq!(r.resume_pos("a").unwrap(), Some(123.5));
    }

    #[test]
    fn episode_playback_preferences_are_local_to_episode() {
        let r = Replica::new(":memory:");
        seed(&r);
        r.set_subtitle_track("a", "no").unwrap();
        r.set_audio_track("a", "2").unwrap();

        let a = r.playback_preferences("a").unwrap();
        assert_eq!(a.subtitle_track.as_deref(), Some("no"));
        assert_eq!(a.audio_track.as_deref(), Some("2"));
        assert_eq!(
            r.playback_preferences("b").unwrap(),
            EpisodePlaybackPreferences::default()
        );
    }

    #[test]
    fn set_resume_within_tolerance_does_not_write() {
        let r = Replica::new(":memory:");
        seed(&r);
        r.set_resume("a", Some(123.5)).unwrap();
        let revision = r.revision().unwrap();

        r.set_resume("a", Some(123.6)).unwrap();

        assert_eq!(r.revision().unwrap(), revision);
    }

    #[test]
    fn advance_clears_resume_pos() {
        // A finished episode has no meaningful resume point; clearing it means
        // stepping back to it ('previous') replays from the start, not the end.
        let r = Replica::new(":memory:");
        seed(&r);
        r.set_resume("a", Some(123.5)).unwrap();
        r.advance(&[("s1".into(), "a".into())]).unwrap();
        assert_eq!(r.resume_pos("a").unwrap(), None);
    }

    #[test]
    fn create_show_is_in_round() {
        let r = Replica::new(":memory:");
        let sid = r.create_show(
            "nelson",
            "New",
            "D:\\New",
            &["e1.mkv".into(), "e2.mkv".into()],
        ).unwrap();
        assert!(!sid.is_empty());
        let me = r
            .active_shows(&["nelson".into()]).unwrap()
            .into_iter()
            .find(|s| s.id == sid)
            .unwrap();
        assert_eq!(me.episodes.len(), 2);
    }

    #[test]
    fn remove_show_tombstones() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into()]).unwrap();
        assert!(r.remove_show(&sid).unwrap());
        assert!(
            r.active_shows(&["nelson".into()]).unwrap()
                .iter()
                .all(|s| s.id != sid)
        );
    }

    #[test]
    fn remove_show_preserves_queued_round_entry() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into()]).unwrap();
        let show = r.show(&sid).unwrap().unwrap();
        let eid = show.episodes[0].id.clone();
        r.save_round_queue(
            &[(eid, sid.clone(), 0, "pending".into(), "nelson".into())],
            T0,
        ).unwrap();

        assert!(r.remove_show(&sid).unwrap());
        assert!(
            r.get_round_queue().unwrap()
                .iter()
                .any(|(_, show_id, ..)| show_id == &sid)
        );
    }

    #[test]
    fn rescan_episodes_appends_positions() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into(), "b.mkv".into()]).unwrap();
        assert_eq!(
            r.rescan_episodes(&sid, &["a.mkv".into(), "b.mkv".into(), "c.mkv".into()]).unwrap()
                .len(),
            1
        );
        let c = r
            .show(&sid).unwrap()
            .unwrap()
            .episodes
            .into_iter()
            .find(|e| e.relative_path == "c.mkv")
            .unwrap();
        assert_eq!(c.position, 2); // continues after a(0), b(1)
    }

    #[test]
    fn episode_paths_and_update_show() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into(), "b.mkv".into()]).unwrap();
        assert_eq!(
            r.episode_paths(&sid).unwrap(),
            ["a.mkv".to_string(), "b.mkv".to_string()]
                .into_iter()
                .collect()
        );
        assert!(r.update_show(&sid, Some("Y"), None, Some("couple")).unwrap());
        let s = r.show(&sid).unwrap().unwrap();
        assert_eq!(s.name, "Y");
        assert_eq!(s.playlist, "couple");
    }

    #[test]
    fn stats_counts_progress_and_finished() {
        let r = Replica::new(":memory:");
        let a = r.create_show("nelson", "A", "D:\\A", &["a1.mkv".into(), "a2.mkv".into()]).unwrap();
        let b = r.create_show("nelson", "B", "D:\\B", &["b1.mkv".into()]).unwrap();
        let a0 = r.show(&a).unwrap().unwrap().episodes[0].id.clone();
        let b0 = r.show(&b).unwrap().unwrap().episodes[0].id.clone();
        r.advance(&[(a, a0), (b, b0)]).unwrap(); // b drains -> finished
        let s = r.stats(&["nelson".into()]).unwrap();
        assert_eq!(s["total_shows"], 2);
        assert_eq!(s["episodes_total"], 3);
        assert_eq!(s["episodes_watched"], 2);
        assert_eq!(s["finished_shows"], 1);
        assert_eq!(s["recent"].as_array().unwrap().len(), 2);
        let per = s["per_show"].as_array().unwrap();
        let pa = per.iter().find(|p| p["name"] == "A").unwrap();
        assert_eq!(pa["watched"], 1);
        assert_eq!(pa["total"], 2);
        let pb = per.iter().find(|p| p["name"] == "B").unwrap();
        assert_eq!(pb["removed"], true);
    }

    #[test]
    fn remove_round_entry_preserves_rest() {
        let r = Replica::new(":memory:");
        let entries = vec![
            (
                "ep1".to_string(),
                "s1".to_string(),
                0,
                "pending".to_string(),
                "nelson".to_string(),
            ),
            (
                "ep2".to_string(),
                "s2".to_string(),
                1,
                "pending".to_string(),
                "nelson".to_string(),
            ),
        ];
        r.save_round_queue(&entries, "2026-05-31T00:00:00Z").unwrap();

        assert_eq!(
            r.remove_round_entry("ep1").unwrap(),
            Some(("s1".to_string(), "nelson".to_string()))
        );

        let q = r.get_round_queue().unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, "ep2");
        assert_eq!(q[0].6, 0);
        assert_eq!(
            r.remove_round_entry("ep2").unwrap(),
            Some(("s2".to_string(), "nelson".to_string()))
        );
        assert!(r.get_round_queue().unwrap().is_empty());
    }

    fn queue_states(r: &Replica) -> Vec<(String, String)> {
        r.get_round_queue().unwrap()
            .into_iter()
            .map(|(episode_id, _, _, state, ..)| (episode_id, state))
            .collect()
    }

    fn queue(r: &Replica, states: &[(&str, &str, &str)]) {
        let entries: Vec<_> = states
            .iter()
            .enumerate()
            .map(|(i, (ep, show, state))| {
                (
                    ep.to_string(),
                    show.to_string(),
                    i as i32,
                    state.to_string(),
                    "nelson".to_string(),
                )
            })
            .collect();
        r.save_round_queue(&entries, T0).unwrap();
    }

    #[test]
    fn set_round_playing_keeps_one_playing_and_restores_watched() {
        let r = Replica::new(":memory:");
        seed(&r);
        queue(&r, &[("a", "s1", "pending"), ("c", "s2", "pending")]);
        let playlists = ["nelson".to_string()];

        r.set_round_playing("a", &playlists).unwrap();
        r.advance(&[("s1".into(), "a".into())]).unwrap();
        // Revisit the finished episode, then move on.
        r.set_round_playing("a", &playlists).unwrap();
        r.set_round_playing("c", &playlists).unwrap();

        assert_eq!(
            queue_states(&r),
            vec![
                ("a".into(), "watched".into()),
                ("c".into(), "playing".into())
            ]
        );
    }

    #[test]
    fn second_playing_entry_is_rejected() {
        let r = Replica::new(":memory:");
        seed(&r);
        queue(&r, &[("a", "s1", "playing"), ("c", "s2", "pending")]);

        assert!(
            r.with_conn(|conn| {
                conn.execute(
                    "UPDATE round_queue SET state='playing' WHERE episode_id='c'",
                    [],
                )
            })
            .is_err()
        );
    }

    fn io_failure() -> DbError {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
            Some("disk I/O error".into()),
        )
    }

    fn on_disk(dir: &tempfile::TempDir) -> (Replica, std::path::PathBuf) {
        let path = dir.path().join("shows.db");
        let r = Replica::new(path.to_str().unwrap());
        seed(&r);
        (r, path)
    }

    #[test]
    fn a_dead_connection_is_replaced_and_the_call_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let (r, _) = on_disk(&dir);
        let mut calls = 0;

        let answer = r.with_conn(|conn| {
            calls += 1;
            if calls == 1 {
                return Err(io_failure());
            }
            conn.query_row("SELECT COUNT(*) FROM shows", [], |row| row.get::<_, i64>(0))
        });

        assert_eq!(calls, 2);
        assert_eq!(answer, Ok(2));
        assert!(r.is_reachable());
    }

    #[test]
    fn a_connection_that_stays_dead_makes_the_database_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let (r, _) = on_disk(&dir);

        let answer = r.with_conn(|_| Err::<(), _>(io_failure()));

        assert!(answer.is_err());
        assert!(!r.is_reachable());
        assert!(r.failure().unwrap().contains("disk I/O error"));
    }

    #[test]
    fn other_errors_keep_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let (r, _) = on_disk(&dir);
        let mut calls = 0;

        let answer = r.with_conn(|conn| {
            calls += 1;
            conn.execute("INSERT INTO shows(id) VALUES('broken')", [])
        });

        assert!(answer.is_err());
        assert_eq!(calls, 1);
        assert!(r.is_reachable());
    }

    #[test]
    fn reads_fail_while_unreachable_instead_of_looking_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (r, _) = on_disk(&dir);
        r.drop_connection_for_test();

        assert!(r.active_shows(&["nelson".to_string()]).is_err());
        assert!(r.get_round_queue().is_err());
        assert!(r.show("s1").is_err());
        assert!(r.stats(&["nelson".to_string()]).is_err());
    }

    #[test]
    fn revision_reconnects_once_the_database_is_back() {
        let dir = tempfile::tempdir().unwrap();
        let (r, path) = on_disk(&dir);
        let revision = r.revision().unwrap();
        let moved = dir.path().join("away.db");
        r.drop_connection_for_test();
        std::fs::rename(&path, &moved).unwrap();

        assert!(r.revision().is_err(), "a missing database is never created");
        assert!(!path.exists());
        assert!(!r.is_reachable());

        std::fs::rename(&moved, &path).unwrap();
        assert_eq!(r.revision(), Ok(revision));
        assert!(r.is_reachable());
        assert_eq!(r.failure(), None);
        assert_eq!(r.show("s1").unwrap().unwrap().id, "s1");
    }

    #[test]
    fn a_database_unreachable_at_startup_gives_a_working_replica_later() {
        let dir = tempfile::tempdir().unwrap();
        let (seeded, path) = on_disk(&dir);
        let revision = seeded.revision().unwrap();
        drop(seeded);
        let moved = dir.path().join("away").join("shows.db");
        let early = Replica::connect(moved.to_str().unwrap());
        assert!(!early.is_reachable());
        assert!(early.failure().is_some());

        std::fs::create_dir(moved.parent().unwrap()).unwrap();
        std::fs::rename(&path, &moved).unwrap();
        assert_eq!(early.revision(), Ok(revision));
    }
}
