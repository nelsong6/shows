//! SQLite persistence for the authoritative NAS database.
//!
//! The engine ([`crate::engine`]) holds the domain logic over plain structs;
//! this layer loads rows into those structs, runs the engine, and persists the
//! results. Every desktop opens this same database, so SQLite commit order—not
//! a local dirty-row preference—determines the visible state.
//!
//! Concurrency: one connection behind a `Mutex`, since the runner thread, the
//! control-server thread, and the UI thread all touch it. Volume is tiny.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
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

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodePlaybackPreferences {
    pub subtitle_track: Option<String>,
    pub audio_track: Option<String>,
}

pub struct Replica {
    db: Mutex<Connection>,
}

impl Replica {
    pub fn new(path: &str) -> Replica {
        Self::try_new(path).expect("open shows database")
    }

    pub fn try_new(path: &str) -> Result<Replica, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(15))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Replica {
            db: Mutex::new(conn),
        })
    }

    pub fn revision(&self) -> i64 {
        let conn = self.db.lock().unwrap();
        conn.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='revision'", [], |r| r.get(0))
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn seed_test_shows(&self, shows: &[Show]) {
        let mut conn = self.db.lock().unwrap();
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

    // ── reads ───────────────────────────────────────────────────────────
    /// Non-removed shows in the given playlists, with episodes — the input to
    /// [`engine::next_round`] (pass several playlists for a cross-playlist round).
    pub fn active_shows(&self, playlists: &[String]) -> Vec<Show> {
        if playlists.is_empty() {
            return vec![];
        }
        let conn = self.db.lock().unwrap();
        let marks = vec!["?"; playlists.len()].join(",");
        let sql =
            format!("SELECT id FROM shows WHERE removed_at IS NULL AND playlist IN ({marks})");
        let mut stmt = conn.prepare(&sql).expect("prep active");
        let ids: Vec<String> = stmt
            .query_map(params_from_iter(playlists.iter()), |r| r.get(0))
            .expect("query active")
            .filter_map(Result::ok)
            .collect();
        ids.iter().filter_map(|id| load_show(&conn, id)).collect()
    }

    pub fn all_shows(&self) -> Vec<Show> {
        let conn = self.db.lock().unwrap();
        let sql = "SELECT id FROM shows WHERE removed_at IS NULL";
        let mut stmt = conn.prepare(sql).expect("prep all_shows");
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .expect("query all_shows")
            .filter_map(Result::ok)
            .collect();
        ids.iter().filter_map(|id| load_show(&conn, id)).collect()
    }

    pub fn show(&self, show_id: &str) -> Option<Show> {
        let conn = self.db.lock().unwrap();
        load_show(&conn, show_id)
    }

    pub fn resume_pos(&self, episode_id: &str) -> Option<f64> {
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT resume_pos FROM episodes WHERE id=?1",
            params![episode_id],
            |r| r.get::<_, Option<f64>>(0),
        )
        .optional()
        .expect("resume_pos")
        .flatten()
    }

    pub fn playback_preferences(&self, episode_id: &str) -> EpisodePlaybackPreferences {
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT subtitle_track, audio_track FROM episode_playback_preferences WHERE episode_id=?1",
            params![episode_id],
            |r| {
                Ok(EpisodePlaybackPreferences {
                    subtitle_track: r.get(0)?,
                    audio_track: r.get(1)?,
                })
            },
        )
        .optional()
        .expect("playback_preferences")
        .unwrap_or_default()
    }

    pub fn get_volume(&self) -> Option<f64> {
        let conn = self.db.lock().unwrap();
        conn.query_row("SELECT value FROM meta WHERE key = 'volume'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
    }

    pub fn set_volume(&self, vol: f64) {
        let conn = self.db.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO meta (key, value) VALUES ('volume', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![vol.to_string()],
        );
    }

    // ── mutations ───────────────────────────────────────────────────────
    /// Mark `(show_id, episode_id)` pairs watched via the engine; persist
    /// watched_at/removed_at, append history. Returns (newly-watched, removed ids).
    pub fn advance(&self, entries: &[(String, String)]) -> (usize, Vec<String>) {
        let now = now();
        let mut by_show: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (sid, eid) in entries {
            by_show.entry(sid.clone()).or_default().push(eid.clone());
        }
        let mut conn = self.db.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin advance transaction");
        let mut advanced_total = 0;
        let mut removed = Vec::new();
        for (sid, eids) in &by_show {
            let Some(mut sh) = load_show(&tx, sid) else {
                continue;
            };
            let (history, n, tombstoned) = engine::advance(&mut sh, eids, &now);
            if n == 0 {
                continue;
            }
            for h in &history {
                tx.execute(
                    "UPDATE episodes SET watched_at=?1, resume_pos=NULL, updated_at=?2, dirty=0 WHERE id=?3",
                    params![now, now, h.episode_id],
                )
                .expect("mark watched");
                tx.execute(
                    "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty) VALUES(?1,?2,?3,?4,?5,0)",
                    params![Uuid::new_v4().to_string(), h.show_id, h.episode_id, h.relative_path, h.played_at],
                )
                .expect("append history");
            }
            if tombstoned {
                tx.execute(
                    "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                    params![now, now, sid],
                )
                .expect("tombstone");
                removed.push(sid.clone());
            }
            advanced_total += n;
        }
        tx.commit().expect("commit advance transaction");
        (advanced_total, removed)
    }

    pub fn defer(&self, show_id: &str, episode_id: &str) -> bool {
        let now = now();
        let mut conn = self.db.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin defer transaction");
        let Some(mut sh) = load_show(&tx, show_id) else {
            return false;
        };
        if !engine::defer(&mut sh.episodes, episode_id) {
            return false;
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
        )
        .expect("persist defer");
        tx.commit().expect("commit defer transaction");
        true
    }

    pub fn set_resume(&self, episode_id: &str, pos: Option<f64>) {
        let now = now();
        let conn = self.db.lock().unwrap();
        let current = conn
            .query_row(
                "SELECT resume_pos FROM episodes WHERE id=?1",
                params![episode_id],
                |r| r.get::<_, Option<f64>>(0),
            )
            .optional()
            .expect("lookup resume_pos")
            .flatten();
        if resume_pos_matches(current, pos) {
            return;
        }
        conn.execute(
            "UPDATE episodes SET resume_pos=?1, updated_at=?2, dirty=0 WHERE id=?3",
            params![pos, now, episode_id],
        )
        .expect("set_resume");
    }

    pub fn set_subtitle_track(&self, episode_id: &str, sid: &str) {
        self.set_playback_preference(episode_id, "subtitle_track", sid);
    }

    pub fn set_audio_track(&self, episode_id: &str, aid: &str) {
        self.set_playback_preference(episode_id, "audio_track", aid);
    }

    fn set_playback_preference(&self, episode_id: &str, column: &str, value: &str) {
        debug_assert!(matches!(column, "subtitle_track" | "audio_track"));
        let now = now();
        let conn = self.db.lock().unwrap();
        let current: Option<String> = conn
            .query_row(
                &format!("SELECT {column} FROM episode_playback_preferences WHERE episode_id=?1"),
                params![episode_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .expect("lookup playback preference")
            .flatten();
        if current.as_deref() == Some(value) {
            return;
        }
        conn.execute(
            &format!(
                "INSERT INTO episode_playback_preferences(episode_id,{column},updated_at) VALUES(?1,?2,?3)
                 ON CONFLICT(episode_id) DO UPDATE SET {column}=excluded.{column}, updated_at=excluded.updated_at"
            ),
            params![episode_id, value, now],
        )
        .expect("set playback preference");
    }

    // ── library management ──────────────────────────────────────────────
    pub fn create_show(
        &self,
        playlist: &str,
        name: &str,
        root_path: &str,
        episodes: &[String],
    ) -> String {
        let now = now();
        let sid = Uuid::new_v4().to_string();
        let mut conn = self.db.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin create show transaction");
        tx.execute(
            "INSERT INTO shows(id,playlist,name,root_path,date_added,removed_at,updated_at,dirty) VALUES(?1,?2,?3,?4,?5,NULL,?6,0)",
            params![sid, playlist, name, root_path, now, now],
        )
        .expect("create show");
        for (i, rel) in episodes.iter().enumerate() {
            tx.execute(
                "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty) VALUES(?1,?2,?3,?4,NULL,NULL,?5,0)",
                params![Uuid::new_v4().to_string(), sid, rel, i as i64, now],
            )
            .expect("create episode");
        }
        tx.commit().expect("commit create show transaction");
        sid
    }

    pub fn update_show(
        &self,
        show_id: &str,
        name: Option<&str>,
        root_path: Option<&str>,
        playlist: Option<&str>,
    ) -> bool {
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
            return false;
        }
        sets.push("updated_at=?");
        vals.push(now);
        vals.push(show_id.to_string()); // for WHERE id=?
        let sql = format!("UPDATE shows SET {} WHERE id=?", sets.join(", "));
        let conn = self.db.lock().unwrap();
        let n = conn
            .execute(&sql, params_from_iter(vals.iter()))
            .expect("update show");
        n > 0
    }

    /// Tombstone a show (removes it from rotation). Soft-delete — the engine
    /// skips removed shows.
    pub fn remove_show(&self, show_id: &str) -> bool {
        let now = now();
        let conn = self.db.lock().unwrap();
        let n = conn
            .execute(
                "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                params![now, now, show_id],
            )
            .expect("remove show");
        n > 0
    }

    /// Mark all episodes of a show as watched, and tombstone/remove the show.
    pub fn mark_show_watched(&self, show_id: &str) -> bool {
        let now = now();
        let conn = self.db.lock().unwrap();

        // Get all episodes that are unwatched
        let mut stmt = conn
            .prepare(
                "SELECT id, relative_path FROM episodes WHERE show_id=?1 AND watched_at IS NULL",
            )
            .expect("prep unwatched");
        let unwatched: Vec<(String, String)> = stmt
            .query_map(params![show_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query unwatched")
            .filter_map(Result::ok)
            .collect();

        for (eid, rel) in &unwatched {
            conn.execute(
                "UPDATE episodes SET watched_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                params![now, now, eid],
            )
            .expect("mark watched");
            conn.execute(
                "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty) VALUES(?1,?2,?3,?4,?5,0)",
                params![Uuid::new_v4().to_string(), show_id, eid, rel, now],
            )
            .expect("append history");
        }

        let n = conn
            .execute(
                "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                params![now, now, show_id],
            )
            .expect("tombstone");

        n > 0
    }

    /// Mark all episodes of a show as unwatched, reset resume positions, and restore the show.
    pub fn mark_show_unwatched(&self, show_id: &str) -> bool {
        let now = now();
        let conn = self.db.lock().unwrap();

        conn.execute(
            "UPDATE shows SET removed_at=NULL, updated_at=?1, dirty=0 WHERE id=?2",
            params![now, show_id],
        )
        .expect("restore show");

        conn.execute(
            "UPDATE episodes SET watched_at=NULL, resume_pos=NULL, updated_at=?1, dirty=0 WHERE show_id=?2",
            params![now, show_id],
        )
        .expect("reset episodes");

        conn.execute(
            "DELETE FROM watch_history WHERE show_id=?1",
            params![show_id],
        )
        .expect("clear history");

        true
    }

    pub fn rescan_episodes(&self, show_id: &str, sorted_rels: &[String]) -> Vec<String> {
        if sorted_rels.is_empty() {
            return Vec::new();
        }
        let now = now();
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction().expect("begin rescan tx");

        let existing: std::collections::HashSet<String> = {
            let mut stmt = tx
                .prepare("SELECT relative_path FROM episodes WHERE show_id=?1")
                .expect("prep paths");
            stmt.query_map(params![show_id], |r| r.get(0))
                .expect("query paths")
                .filter_map(Result::ok)
                .collect()
        };

        let mut added_ids = Vec::new();

        for (i, rel) in sorted_rels.iter().enumerate() {
            if !existing.contains(rel) {
                let new_id = Uuid::new_v4().to_string();
                tx.execute(
                    "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty) VALUES(?1,?2,?3,?4,NULL,NULL,?5,0)",
                    params![new_id, show_id, rel, i as i64, now],
                )
                .expect("add episode");
                added_ids.push(new_id);
            } else {
                tx.execute(
                    "UPDATE episodes SET position=?1, updated_at=?2, dirty=0 WHERE show_id=?3 AND relative_path=?4",
                    params![i as i64, now, show_id, rel],
                )
                .expect("update episode position");
            }
        }

        tx.commit().expect("commit rescan tx");
        added_ids
    }

    pub fn mark_episodes_watched(&self, show_id: &str, episode_ids: &[String]) {
        if episode_ids.is_empty() {
            return;
        }
        let now = now();
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction().expect("begin mark tx");
        for eid in episode_ids {
            // Retrieve relative_path to insert into watch_history
            let rel: String = {
                let mut stmt = tx
                    .prepare("SELECT relative_path FROM episodes WHERE id=?1")
                    .expect("prep rel");
                stmt.query_row(params![eid], |r| r.get(0))
                    .unwrap_or_default()
            };
            if rel.is_empty() {
                continue;
            }

            tx.execute(
                "UPDATE episodes SET watched_at=?1, updated_at=?2, dirty=0 WHERE id=?3",
                params![now, now, eid],
            )
            .expect("mark watched");
            tx.execute(
                "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty) VALUES(?1,?2,?3,?4,?5,0)",
                params![Uuid::new_v4().to_string(), show_id, eid, rel, now],
            )
            .expect("append history");
        }
        tx.commit().expect("commit mark tx");
    }

    /// Relative paths already known for a show — for diffing a rescan.
    pub fn episode_paths(&self, show_id: &str) -> HashSet<String> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT relative_path FROM episodes WHERE show_id=?1")
            .expect("prep paths");
        stmt.query_map(params![show_id], |r| r.get::<_, String>(0))
            .expect("query paths")
            .filter_map(Result::ok)
            .collect()
    }
    pub fn get_round_queue(&self) -> Vec<(String, String, i32, String, String, String, i32)> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT episode_id, show_id, play_order, state, playlist, updated_at, dirty FROM round_queue ORDER BY play_order")
            .expect("prep get_round_queue");
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
        })
        .expect("query round_queue")
        .filter_map(Result::ok)
        .collect()
    }

    pub fn save_round_queue(
        &self,
        entries: &[(String, String, i32, String, String)],
        updated_at: &str,
    ) {
        let mut conn = self.db.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin save_round_queue tx");
        tx.execute("DELETE FROM round_queue", [])
            .expect("delete round_queue");
        let mut playlists = HashSet::new();
        for entry in entries {
            playlists.insert(entry.4.clone());
            tx.execute(
                "INSERT INTO round_queue (episode_id, show_id, play_order, state, playlist, updated_at, dirty) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![entry.0, entry.1, entry.2, entry.3, entry.4, updated_at],
            )
            .expect("insert round_queue entry");
        }
        for playlist in playlists {
            tx.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                params![format!("queue_updated:{playlist}"), updated_at],
            )
            .expect("record queue timestamp");
        }
        tx.commit().expect("commit save_round_queue tx");
    }

    pub fn update_round_entry_state(&self, episode_id: &str, state: &str, playlist: &str) {
        let now = now();
        let mut conn = self.db.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin update_round_entry_state tx");
        tx.execute(
            "UPDATE round_queue SET state=?1, updated_at=?2, dirty=0 WHERE episode_id=?3",
            params![state, now, episode_id],
        )
        .expect("update round entry state");
        tx.execute(
            "UPDATE round_queue SET updated_at=?1, dirty=0 WHERE playlist=?2",
            params![now, playlist],
        )
        .expect("stamp playlist queue");
        tx.commit().expect("commit update_round_entry_state tx");
    }

    pub fn remove_round_entry(&self, episode_id: &str) -> Option<(String, String)> {
        let now = now();
        let mut conn = self.db.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin remove_round_entry tx");
        let found: Option<(String, String)> = tx
            .query_row(
                "SELECT show_id, playlist FROM round_queue WHERE episode_id=?1",
                params![episode_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .expect("lookup round entry");
        let (show_id, playlist) = found?;
        tx.execute(
            "DELETE FROM round_queue WHERE episode_id=?1",
            params![episode_id],
        )
        .expect("delete round entry");
        tx.execute(
            "UPDATE round_queue SET updated_at=?1, dirty=0 WHERE playlist=?2",
            params![now, playlist],
        )
        .expect("stamp repaired queue");
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
            params![format!("queue_updated:{playlist}"), now],
        )
        .expect("record repaired queue timestamp");
        tx.commit().expect("commit remove_round_entry tx");
        Some((show_id, playlist))
    }
    // ── dashboard payloads ───────────────────────────────────────────────
    /// Active shows for the dashboard sidebar (no episodes).
    pub fn overlay_shows(&self, playlists: &[String]) -> Vec<Value> {
        self.active_shows(playlists)
            .into_iter()
            .map(|s| {
                json!({
                    "id": s.id, "playlist": s.playlist, "name": s.name,
                    "root_path": s.root_path, "date_added": s.date_added.unwrap_or_default(),
                    "removed_at": s.removed_at,
                })
            })
            .collect()
    }

    /// Watch-history rows for a show, oldest first.
    pub fn show_history(&self, show_id: &str) -> Vec<Value> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT episode_id, relative_path, played_at FROM watch_history WHERE show_id=?1 ORDER BY played_at",
            )
            .expect("prep history");
        stmt.query_map(params![show_id], |r| {
            Ok(json!({
                "episode_id": r.get::<_, String>(0)?,
                "relative_path": r.get::<_, Option<String>>(1)?,
                "played_at": r.get::<_, String>(2)?,
            }))
        })
        .expect("query history")
        .filter_map(Result::ok)
        .collect()
    }

    /// The "just finished" payload for a tombstoned show — name, date added, and
    /// last play time — for the overlay reveal.
    pub fn reveal(&self, show_id: &str) -> Value {
        let conn = self.db.lock().unwrap();
        let s: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT name, date_added FROM shows WHERE id=?1",
                params![show_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .expect("reveal show");
        let last: Option<String> = conn
            .query_row(
                "SELECT MAX(played_at) FROM watch_history WHERE show_id=?1",
                params![show_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .expect("reveal last")
            .flatten();
        let (name, date_added) = s.unwrap_or_default();
        json!({
            "id": show_id,
            "name": name,
            "date_added": date_added.unwrap_or_default(),
            "last_played_at": last.unwrap_or_default(),
        })
    }

    /// Library + watch stats for the dashboard: totals, per-show progress,
    /// recent activity, and per-day heatmap.
    pub fn stats(&self, playlists: &[String]) -> Value {
        if playlists.is_empty() {
            return json!({});
        }
        let conn = self.db.lock().unwrap();
        let marks = vec!["?"; playlists.len()].join(",");

        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, name, playlist, removed_at FROM shows WHERE playlist IN ({marks})"
            ))
            .expect("prep shows");
        let shows: Vec<(String, String, String, Option<String>)> = stmt
            .query_map(params_from_iter(playlists.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .expect("query shows")
            .filter_map(Result::ok)
            .collect();

        let mut name_by: HashMap<String, String> = HashMap::new();
        let mut per: Vec<PerShow> = Vec::new();
        let (mut ep_total, mut ep_watched, mut finished) = (0i64, 0i64, 0i64);
        for (id, name, playlist, removed_at) in &shows {
            name_by.insert(id.clone(), name.clone());
            let (total, w): (i64, i64) = conn
                .query_row(
                    "SELECT COUNT(*), COALESCE(SUM(CASE WHEN watched_at IS NOT NULL THEN 1 ELSE 0 END),0) FROM episodes WHERE show_id=?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .expect("episode counts");
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
        let mut stmt = conn
            .prepare(&format!(
                "SELECT e.show_id, e.relative_path, e.watched_at FROM episodes e \
                 JOIN shows s ON e.show_id = s.id \
                 WHERE e.watched_at IS NOT NULL AND s.playlist IN ({marks}) \
                 ORDER BY e.watched_at DESC LIMIT 25"
            ))
            .expect("prep recent");
        let recent: Vec<Value> = stmt
            .query_map(params_from_iter(playlists.iter()), |r| {
                let sid: String = r.get(0)?;
                let rp: String = r.get(1)?;
                let wa: String = r.get(2)?;
                Ok(json!({
                    "show": name_by.get(&sid).cloned().unwrap_or_else(|| "?".into()),
                    "relative_path": rp, "played_at": wa,
                }))
            })
            .expect("query recent")
            .filter_map(Result::ok)
            .collect();

        let cutoff = (chrono::Utc::now() - chrono::Duration::days(140)).to_rfc3339();
        let mut day_params: Vec<String> = playlists.to_vec();
        day_params.push(cutoff);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT substr(e.watched_at,1,10) d, COUNT(*) c FROM episodes e \
                 JOIN shows s ON e.show_id = s.id \
                 WHERE e.watched_at IS NOT NULL AND s.playlist IN ({marks}) AND e.watched_at >= ? \
                 GROUP BY d"
            ))
            .expect("prep by_day");
        let mut by_day = serde_json::Map::new();
        for row in stmt
            .query_map(params_from_iter(day_params.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .expect("query by_day")
            .filter_map(Result::ok)
        {
            by_day.insert(row.0, json!(row.1));
        }

        per.sort_by(|a, b| {
            b.ratio()
                .partial_cmp(&a.ratio())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let active = shows.iter().filter(|s| s.3.is_none()).count();
        json!({
            "total_shows": shows.len(),
            "active_shows": active,
            "finished_shows": finished,
            "episodes_total": ep_total,
            "episodes_watched": ep_watched,
            "per_show": per.iter().map(PerShow::to_json).collect::<Vec<_>>(),
            "recent": recent,
            "by_day": Value::Object(by_day),
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

fn load_show(conn: &Connection, show_id: &str) -> Option<Show> {
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
        .optional()
        .expect("load show");
    let (id, playlist, name, root_path, date_added, removed_at) = base?;
    let mut stmt = conn
        .prepare("SELECT id,relative_path,position,watched_at,resume_pos FROM episodes WHERE show_id=?1 ORDER BY position")
        .expect("prep episodes");
    let episodes = stmt
        .query_map(params![id], |r| {
            Ok(Episode {
                id: r.get(0)?,
                relative_path: r.get(1)?,
                position: r.get(2)?,
                watched_at: r.get(3)?,
                resume_pos: r.get(4)?,
            })
        })
        .expect("query episodes")
        .filter_map(Result::ok)
        .collect();
    Some(Show {
        id,
        playlist,
        name,
        root_path,
        episodes,
        removed_at,
        date_added,
    })
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
        let show_id = first.create_show("nelson", "original", r"S:\Shows\Original", &[]);
        let after_create = second.revision();

        assert_eq!(second.show(&show_id).unwrap().name, "original");
        second.update_show(&show_id, Some("second app"), None, None);
        assert_eq!(first.show(&show_id).unwrap().name, "second app");
        assert!(first.revision() > after_create);

        first.update_show(&show_id, Some("first app last"), None, None);
        assert_eq!(second.show(&show_id).unwrap().name, "first app last");
    }

    #[test]
    fn seed_and_next_round() {
        let r = Replica::new(":memory:");
        seed(&r);
        let shows = r.active_shows(&["nelson".into()]);
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
        let (n, removed) = r.advance(&[("s1".into(), "a".into())]);
        assert_eq!(n, 1);
        assert!(removed.is_empty());
        let s1 = r.show("s1").unwrap();
        assert!(
            s1.episodes
                .iter()
                .find(|e| e.id == "a")
                .unwrap()
                .watched_at
                .is_some()
        );
        let nr = next_round(&r.active_shows(&["nelson".into()]));
        assert_eq!(
            nr.iter().find(|o| o.show_id == "s1").unwrap().episode_id,
            "b"
        );
    }

    #[test]
    fn advance_tombstones_drained_show() {
        let r = Replica::new(":memory:");
        seed(&r);
        let (n, removed) = r.advance(&[("s2".into(), "c".into())]);
        assert_eq!(n, 1);
        assert_eq!(removed, ["s2"]);
        assert!(r.show("s2").unwrap().removed_at.is_some());
        assert_eq!(
            ids(&r.active_shows(&["nelson".into()])),
            ["s1"].into_iter().collect()
        );
    }

    #[test]
    fn defer_persists_position_and_changes_pick() {
        let r = Replica::new(":memory:");
        seed(&r);
        assert!(r.defer("s1", "a"));
        let s1 = r.show("s1").unwrap();
        assert_eq!(
            s1.episodes.iter().find(|e| e.id == "a").unwrap().position,
            2
        );
        assert_eq!(first_unwatched(&s1.episodes).unwrap().id, "b");
        r.advance(&[("s1".into(), "b".into())]);
        assert!(!r.defer("s1", "b")); // now watched -> no-op
    }

    #[test]
    fn set_resume_persists() {
        let r = Replica::new(":memory:");
        seed(&r);
        r.set_resume("a", Some(123.5));
        assert_eq!(r.resume_pos("a"), Some(123.5));
    }

    #[test]
    fn episode_playback_preferences_are_local_to_episode() {
        let r = Replica::new(":memory:");
        seed(&r);
        r.set_subtitle_track("a", "no");
        r.set_audio_track("a", "2");

        let a = r.playback_preferences("a");
        assert_eq!(a.subtitle_track.as_deref(), Some("no"));
        assert_eq!(a.audio_track.as_deref(), Some("2"));
        assert_eq!(
            r.playback_preferences("b"),
            EpisodePlaybackPreferences::default()
        );
    }

    #[test]
    fn set_resume_within_tolerance_does_not_write() {
        let r = Replica::new(":memory:");
        seed(&r);
        r.set_resume("a", Some(123.5));
        let revision = r.revision();

        r.set_resume("a", Some(123.6));

        assert_eq!(r.revision(), revision);
    }

    #[test]
    fn advance_clears_resume_pos() {
        // A finished episode has no meaningful resume point; clearing it means
        // stepping back to it ('previous') replays from the start, not the end.
        let r = Replica::new(":memory:");
        seed(&r);
        r.set_resume("a", Some(123.5));
        r.advance(&[("s1".into(), "a".into())]);
        assert_eq!(r.resume_pos("a"), None);
    }

    #[test]
    fn create_show_is_in_round() {
        let r = Replica::new(":memory:");
        let sid = r.create_show(
            "nelson",
            "New",
            "D:\\New",
            &["e1.mkv".into(), "e2.mkv".into()],
        );
        assert!(!sid.is_empty());
        let me = r
            .active_shows(&["nelson".into()])
            .into_iter()
            .find(|s| s.id == sid)
            .unwrap();
        assert_eq!(me.episodes.len(), 2);
    }

    #[test]
    fn remove_show_tombstones() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into()]);
        assert!(r.remove_show(&sid));
        assert!(
            r.active_shows(&["nelson".into()])
                .iter()
                .all(|s| s.id != sid)
        );
    }

    #[test]
    fn remove_show_preserves_queued_round_entry() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into()]);
        let show = r.show(&sid).unwrap();
        let eid = show.episodes[0].id.clone();
        r.save_round_queue(
            &[(eid, sid.clone(), 0, "pending".into(), "nelson".into())],
            T0,
        );

        assert!(r.remove_show(&sid));
        assert!(
            r.get_round_queue()
                .iter()
                .any(|(_, show_id, ..)| show_id == &sid)
        );
    }

    #[test]
    fn rescan_episodes_appends_positions() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into(), "b.mkv".into()]);
        assert_eq!(
            r.rescan_episodes(&sid, &["a.mkv".into(), "b.mkv".into(), "c.mkv".into()])
                .len(),
            1
        );
        let c = r
            .show(&sid)
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
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into(), "b.mkv".into()]);
        assert_eq!(
            r.episode_paths(&sid),
            ["a.mkv".to_string(), "b.mkv".to_string()]
                .into_iter()
                .collect()
        );
        assert!(r.update_show(&sid, Some("Y"), None, Some("couple")));
        let s = r.show(&sid).unwrap();
        assert_eq!(s.name, "Y");
        assert_eq!(s.playlist, "couple");
    }

    #[test]
    fn stats_counts_progress_and_finished() {
        let r = Replica::new(":memory:");
        let a = r.create_show("nelson", "A", "D:\\A", &["a1.mkv".into(), "a2.mkv".into()]);
        let b = r.create_show("nelson", "B", "D:\\B", &["b1.mkv".into()]);
        let a0 = r.show(&a).unwrap().episodes[0].id.clone();
        let b0 = r.show(&b).unwrap().episodes[0].id.clone();
        r.advance(&[(a, a0), (b, b0)]); // b drains -> finished
        let s = r.stats(&["nelson".into()]);
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
        r.save_round_queue(&entries, "2026-05-31T00:00:00Z");

        assert_eq!(
            r.remove_round_entry("ep1"),
            Some(("s1".to_string(), "nelson".to_string()))
        );

        let q = r.get_round_queue();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, "ep2");
        assert_eq!(q[0].6, 0);
        assert_eq!(
            r.remove_round_entry("ep2"),
            Some(("s2".to_string(), "nelson".to_string()))
        );
        assert!(r.get_round_queue().is_empty());
    }
}
