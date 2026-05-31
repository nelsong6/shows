//! Local SQLite replica — the desktop's working copy of the library under the
//! offline-first design. All reads/writes for playback happen here; the server
//! is a durable origin we sync with (git-style: local changes are marked
//! `dirty` and pushed at smart moments; pulls reconcile last-write-wins by
//! `updated_at`).
//!
//! The engine ([`crate::engine`]) holds the domain logic over plain structs;
//! this layer loads rows into those structs, runs the engine, and persists the
//! results — marking touched rows dirty and bumping `updated_at`.
//!
//! Concurrency: one connection behind a `Mutex`, since the runner thread, the
//! control-server thread, and the UI thread all touch it. Volume is tiny.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::engine::{self, Episode, Show};
use crate::model::{LibraryEpisode, LibraryShow, LibraryQueue};

const SCHEMA: &str = "
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
";

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn parse_dt(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s).ok()
}

/// True if timestamp `a` is strictly newer than `b` (None == oldest).
fn newer(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(a), Some(b)) => match (parse_dt(a), parse_dt(b)) {
            (Some(da), Some(db)) => da > db,
            _ => a > b, // fall back to lexicographic if either is unparseable
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub shows: i64,
    pub episodes: i64,
    pub history: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dirty {
    pub shows: Vec<Value>,
    pub episodes: Vec<Value>,
    pub history: Vec<Value>,
}

pub struct Replica {
    db: Mutex<Connection>,
}

impl Replica {
    pub fn new(path: &str) -> Replica {
        let conn = Connection::open(path).expect("open replica db");
        conn.execute_batch(SCHEMA).expect("apply schema");
        Replica { db: Mutex::new(conn) }
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
        let sql = format!("SELECT id FROM shows WHERE removed_at IS NULL AND playlist IN ({marks})");
        let mut stmt = conn.prepare(&sql).expect("prep active");
        let ids: Vec<String> = stmt
            .query_map(params_from_iter(playlists.iter()), |r| r.get(0))
            .expect("query active")
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

    // ── mutations (local-first: mark dirty + bump updated_at) ────────────
    /// Mark `(show_id, episode_id)` pairs watched via the engine; persist
    /// watched_at/removed_at, append history. Returns (newly-watched, removed ids).
    pub fn advance(&self, entries: &[(String, String)]) -> (usize, Vec<String>) {
        let now = now();
        let mut by_show: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (sid, eid) in entries {
            by_show.entry(sid.clone()).or_default().push(eid.clone());
        }
        let conn = self.db.lock().unwrap();
        let mut advanced_total = 0;
        let mut removed = Vec::new();
        for (sid, eids) in &by_show {
            let Some(mut sh) = load_show(&conn, sid) else {
                continue;
            };
            let (history, n, tombstoned) = engine::advance(&mut sh, eids, &now);
            if n == 0 {
                continue;
            }
            for h in &history {
                conn.execute(
                    "UPDATE episodes SET watched_at=?1, updated_at=?2, dirty=1 WHERE id=?3",
                    params![now, now, h.episode_id],
                )
                .expect("mark watched");
                conn.execute(
                    "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty) VALUES(?1,?2,?3,?4,?5,1)",
                    params![Uuid::new_v4().to_string(), h.show_id, h.episode_id, h.relative_path, h.played_at],
                )
                .expect("append history");
            }
            if tombstoned {
                conn.execute(
                    "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=1 WHERE id=?3",
                    params![now, now, sid],
                )
                .expect("tombstone");
                removed.push(sid.clone());
            }
            advanced_total += n;
        }
        (advanced_total, removed)
    }

    pub fn defer(&self, show_id: &str, episode_id: &str) -> bool {
        let now = now();
        let conn = self.db.lock().unwrap();
        let Some(mut sh) = load_show(&conn, show_id) else {
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
        conn.execute(
            "UPDATE episodes SET position=?1, updated_at=?2, dirty=1 WHERE id=?3",
            params![pos, now, episode_id],
        )
        .expect("persist defer");
        true
    }

    pub fn set_resume(&self, episode_id: &str, pos: Option<f64>) {
        let now = now();
        let conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE episodes SET resume_pos=?1, updated_at=?2, dirty=1 WHERE id=?3",
            params![pos, now, episode_id],
        )
        .expect("set_resume");
    }

    // ── library management (local-first; syncs up like any change) ───────
    pub fn create_show(&self, playlist: &str, name: &str, root_path: &str, episodes: &[String]) -> String {
        let now = now();
        let sid = Uuid::new_v4().to_string();
        let conn = self.db.lock().unwrap();
        conn.execute(
            "INSERT INTO shows(id,playlist,name,root_path,date_added,removed_at,updated_at,dirty) VALUES(?1,?2,?3,?4,?5,NULL,?6,1)",
            params![sid, playlist, name, root_path, now, now],
        )
        .expect("create show");
        for (i, rel) in episodes.iter().enumerate() {
            conn.execute(
                "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty) VALUES(?1,?2,?3,?4,NULL,NULL,?5,1)",
                params![Uuid::new_v4().to_string(), sid, rel, i as i64, now],
            )
            .expect("create episode");
        }
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
        sets.push("dirty=1");
        vals.push(show_id.to_string()); // for WHERE id=?
        let sql = format!("UPDATE shows SET {} WHERE id=?", sets.join(", "));
        let conn = self.db.lock().unwrap();
        let n = conn.execute(&sql, params_from_iter(vals.iter())).expect("update show");
        n > 0
    }

    /// Tombstone a show (removes it from rotation). Soft-delete — the engine
    /// skips removed shows; the tombstone syncs up.
    pub fn remove_show(&self, show_id: &str) -> bool {
        let now = now();
        let conn = self.db.lock().unwrap();
        let n = conn
            .execute(
                "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=1 WHERE id=?3",
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
            .prepare("SELECT id, relative_path FROM episodes WHERE show_id=?1 AND watched_at IS NULL")
            .expect("prep unwatched");
        let unwatched: Vec<(String, String)> = stmt
            .query_map(params![show_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query unwatched")
            .filter_map(Result::ok)
            .collect();
            
        for (eid, rel) in &unwatched {
            conn.execute(
                "UPDATE episodes SET watched_at=?1, updated_at=?2, dirty=1 WHERE id=?3",
                params![now, now, eid],
            )
            .expect("mark watched");
            conn.execute(
                "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty) VALUES(?1,?2,?3,?4,?5,1)",
                params![Uuid::new_v4().to_string(), show_id, eid, rel, now],
            )
            .expect("append history");
        }
        
        let n = conn.execute(
            "UPDATE shows SET removed_at=?1, updated_at=?2, dirty=1 WHERE id=?3",
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
            "UPDATE shows SET removed_at=NULL, updated_at=?1, dirty=1 WHERE id=?2",
            params![now, show_id],
        )
        .expect("restore show");
        
        conn.execute(
            "UPDATE episodes SET watched_at=NULL, resume_pos=NULL, updated_at=?1, dirty=1 WHERE show_id=?2",
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

    /// Append new episodes to a show, positions continuing from max+1 (the
    /// new-episode-detection path). Returns how many were added.
    pub fn add_episodes(&self, show_id: &str, rels: &[String]) -> usize {
        if rels.is_empty() {
            return 0;
        }
        let now = now();
        let conn = self.db.lock().unwrap();
        let start: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), -1) FROM episodes WHERE show_id=?1",
                params![show_id],
                |r| r.get(0),
            )
            .expect("max position");
        for (i, rel) in rels.iter().enumerate() {
            conn.execute(
                "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty) VALUES(?1,?2,?3,?4,NULL,NULL,?5,1)",
                params![Uuid::new_v4().to_string(), show_id, rel, start + 1 + i as i64, now],
            )
            .expect("add episode");
        }
        rels.len()
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

    // ── seed / reconcile (pull): upsert last-write-wins ──────────────────
    /// Upsert shows + episodes pulled from the server. Last-write-wins by
    /// `updated_at`; a locally-dirty row is kept (local unsynced change wins
    /// until it's pushed — git-style). Used for the initial seed and later pulls.
    pub fn merge_shows(&self, shows: &[LibraryShow]) {
        let conn = self.db.lock().unwrap();
        for s in shows {
            let existing: Option<(String, i64)> = conn
                .query_row(
                    "SELECT updated_at, dirty FROM shows WHERE id=?1",
                    params![s.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .expect("lookup show");
            match existing {
                None => {
                    conn.execute(
                        "INSERT INTO shows(id,playlist,name,root_path,date_added,removed_at,updated_at,dirty) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
                        params![s.id, s.playlist, s.name, s.root_path, s.date_added, s.removed_at, s.updated_at],
                    )
                    .expect("insert merged show");
                }
                Some((cur_updated, dirty)) => {
                    if dirty == 0 && newer(Some(&s.updated_at), Some(&cur_updated)) {
                        conn.execute(
                            "UPDATE shows SET playlist=?1,name=?2,root_path=?3,date_added=?4,removed_at=?5,updated_at=?6,dirty=0 WHERE id=?7",
                            params![s.playlist, s.name, s.root_path, s.date_added, s.removed_at, s.updated_at, s.id],
                        )
                        .expect("update merged show");
                    }
                }
            }
            for e in &s.episodes {
                merge_episode(&conn, &s.id, e);
            }
        }
    }

    pub fn merge_queues(&self, queues: &[LibraryQueue]) {
        let conn = self.db.lock().unwrap();
        for q in queues {
            merge_queue_impl(&conn, q);
        }
    }

    // ── sync push: dirty rows out, then clear ────────────────────────────
    /// Count of unpushed local changes — the git "ahead" number.
    pub fn pending(&self) -> Pending {
        let conn = self.db.lock().unwrap();
        Pending {
            shows: count_dirty(&conn, "shows"),
            episodes: count_dirty(&conn, "episodes"),
            history: count_dirty(&conn, "watch_history"),
        }
    }

    /// The records to push upstream.
    pub fn dirty(&self) -> Dirty {
        let conn = self.db.lock().unwrap();
        Dirty {
            shows: dirty_shows(&conn),
            episodes: dirty_episodes(&conn),
            history: dirty_history(&conn),
        }
    }

    /// Clear dirty on rows confirmed pushed.
    pub fn mark_synced(&self, table: &str, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let marks = vec!["?"; ids.len()].join(",");
        let conn = self.db.lock().unwrap();
        conn.execute(
            &format!("UPDATE {table} SET dirty=0 WHERE id IN ({marks})"),
            params_from_iter(ids.iter()),
        )
        .expect("mark synced");
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

    pub fn save_round_queue(&self, entries: &[(String, String, i32, String, String)], updated_at: &str, dirty: bool) {
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction().expect("begin save_round_queue tx");
        tx.execute("DELETE FROM round_queue", []).expect("delete round_queue");
        let dirty_val = if dirty { 1 } else { 0 };
        for entry in entries {
            tx.execute(
                "INSERT INTO round_queue (episode_id, show_id, play_order, state, playlist, updated_at, dirty) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![entry.0, entry.1, entry.2, entry.3, entry.4, updated_at, dirty_val],
            )
            .expect("insert round_queue entry");
        }
        tx.commit().expect("commit save_round_queue tx");
    }

    pub fn update_round_entry_state(&self, episode_id: &str, state: &str, playlist: &str) {
        let now = now();
        let conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE round_queue SET state=?1, updated_at=?2, dirty=1 WHERE episode_id=?3",
            params![state, now, episode_id],
        )
        .expect("update round entry state");
        conn.execute(
            "UPDATE round_queue SET updated_at=?1, dirty=1 WHERE playlist=?2",
            params![now, playlist],
        )
        .expect("mark other playlist entries dirty");
    }

    pub fn dirty_queue(&self) -> Option<Value> {
        let conn = self.db.lock().unwrap();
        let dirty_pl: Option<(String, String)> = conn
            .query_row(
                "SELECT playlist, updated_at FROM round_queue WHERE dirty=1 LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .expect("lookup dirty playlist");
        let (playlist, updated_at) = dirty_pl?;
        
        let mut stmt = conn
            .prepare("SELECT episode_id, show_id, play_order, state FROM round_queue WHERE playlist=?1 ORDER BY play_order")
            .expect("prep dirty_queue query");
        let entries: Vec<Value> = stmt
            .query_map(params![playlist], |r| {
                Ok(json!({
                    "episode_id": r.get::<_, String>(0)?,
                    "show_id": r.get::<_, String>(1)?,
                    "play_order": r.get::<_, i32>(2)?,
                    "state": r.get::<_, String>(3)?,
                }))
            })
            .expect("query dirty queue entries")
            .filter_map(Result::ok)
            .collect();
        
        Some(json!({
            "playlist": playlist,
            "updated_at": updated_at,
            "entries": entries,
        }))
    }

    pub fn mark_queue_synced(&self, playlist: &str) {
        let conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE round_queue SET dirty=0 WHERE playlist=?1",
            params![playlist],
        )
        .expect("mark queue synced");
    }

    // ── dashboard payloads (read-only, offline-capable) ──────────────────
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

    /// Library + watch stats for the dashboard, computed from the replica (works
    /// offline): totals, per-show progress, recent activity, per-day heatmap.
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

        per.sort_by(|a, b| b.ratio().partial_cmp(&a.ratio()).unwrap_or(std::cmp::Ordering::Equal));
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
    let base: Option<(String, String, String, String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT id,playlist,name,root_path,date_added,removed_at FROM shows WHERE id=?1",
            params![show_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
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

fn merge_episode(conn: &Connection, show_id: &str, e: &LibraryEpisode) {
    let existing: Option<(String, i64)> = conn
        .query_row(
            "SELECT updated_at, dirty FROM episodes WHERE id=?1",
            params![e.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .expect("lookup episode");
    match existing {
        None => {
            conn.execute(
                "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
                params![e.id, show_id, e.relative_path, e.position, e.watched_at, e.resume_pos, e.updated_at],
            )
            .expect("insert merged episode");
        }
        Some((cur_updated, dirty)) => {
            if dirty == 0 && newer(Some(&e.updated_at), Some(&cur_updated)) {
                conn.execute(
                    "UPDATE episodes SET relative_path=?1,position=?2,watched_at=?3,resume_pos=?4,updated_at=?5,dirty=0 WHERE id=?6",
                    params![e.relative_path, e.position, e.watched_at, e.resume_pos, e.updated_at, e.id],
                )
                .expect("update merged episode");
            }
        }
    }
}

fn merge_queue_impl(conn: &Connection, q: &LibraryQueue) {
    let local: Option<(String, i64)> = conn
        .query_row(
            "SELECT COALESCE(MAX(updated_at), ''), COALESCE(SUM(dirty), 0) FROM round_queue WHERE playlist=?1",
            params![q.playlist],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .expect("lookup local queue stats");
        
    let should_overwrite = match local {
        None => true,
        Some((cur_updated, dirty_count)) => {
            dirty_count == 0 && newer(Some(&q.updated_at), Some(cur_updated.as_str()))
        }
    };
    
    if should_overwrite {
        conn.execute("DELETE FROM round_queue WHERE playlist=?1", params![q.playlist])
            .expect("delete old round_queue");
        for entry in &q.entries {
            conn.execute(
                "INSERT INTO round_queue (episode_id, show_id, play_order, state, playlist, updated_at, dirty) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![entry.episode_id, entry.show_id, entry.play_order, entry.state, q.playlist, q.updated_at],
            )
            .expect("insert round_queue entry");
        }
    }
}

fn count_dirty(conn: &Connection, table: &str) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE dirty=1"),
        [],
        |r| r.get(0),
    )
    .expect("count dirty")
}

fn dirty_shows(conn: &Connection) -> Vec<Value> {
    let mut stmt = conn
        .prepare("SELECT id,playlist,name,root_path,date_added,removed_at,updated_at FROM shows WHERE dirty=1")
        .expect("prep dirty shows");
    stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "playlist": r.get::<_, String>(1)?,
            "name": r.get::<_, String>(2)?,
            "root_path": r.get::<_, String>(3)?,
            "date_added": r.get::<_, Option<String>>(4)?,
            "removed_at": r.get::<_, Option<String>>(5)?,
            "updated_at": r.get::<_, String>(6)?,
        }))
    })
    .expect("query dirty shows")
    .filter_map(Result::ok)
    .collect()
}

fn dirty_episodes(conn: &Connection) -> Vec<Value> {
    let mut stmt = conn
        .prepare("SELECT id,show_id,relative_path,position,watched_at,resume_pos,updated_at FROM episodes WHERE dirty=1")
        .expect("prep dirty episodes");
    stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "show_id": r.get::<_, String>(1)?,
            "relative_path": r.get::<_, String>(2)?,
            "position": r.get::<_, i64>(3)?,
            "watched_at": r.get::<_, Option<String>>(4)?,
            "resume_pos": r.get::<_, Option<f64>>(5)?,
            "updated_at": r.get::<_, String>(6)?,
        }))
    })
    .expect("query dirty episodes")
    .filter_map(Result::ok)
    .collect()
}

fn dirty_history(conn: &Connection) -> Vec<Value> {
    let mut stmt = conn
        .prepare("SELECT id,show_id,episode_id,relative_path,played_at FROM watch_history WHERE dirty=1")
        .expect("prep dirty history");
    stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "show_id": r.get::<_, String>(1)?,
            "episode_id": r.get::<_, String>(2)?,
            "relative_path": r.get::<_, Option<String>>(3)?,
            "played_at": r.get::<_, String>(4)?,
        }))
    })
    .expect("query dirty history")
    .filter_map(Result::ok)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{first_unwatched, next_round};
    use std::collections::BTreeSet;

    const OLD: &str = "2025-01-01T00:00:00Z";
    const T0: &str = "2026-01-01T00:00:00Z";
    const NEW: &str = "2027-01-01T00:00:00Z";

    fn lep(id: &str, rel: &str, pos: i64, updated: &str) -> LibraryEpisode {
        LibraryEpisode {
            id: id.into(),
            relative_path: rel.into(),
            position: pos,
            updated_at: updated.into(),
            ..Default::default()
        }
    }

    fn seed(r: &Replica) {
        r.merge_shows(&[
            LibraryShow {
                id: "s1".into(),
                playlist: "nelson".into(),
                name: "S1".into(),
                root_path: "D:\\A".into(),
                updated_at: T0.into(),
                episodes: vec![lep("a", "a.mkv", 0, T0), lep("b", "b.mkv", 1, T0)],
                ..Default::default()
            },
            LibraryShow {
                id: "s2".into(),
                playlist: "nelson".into(),
                name: "S2".into(),
                root_path: "D:\\B".into(),
                updated_at: T0.into(),
                episodes: vec![lep("c", "c.mkv", 0, T0)],
                ..Default::default()
            },
        ]);
    }

    fn ids(shows: &[Show]) -> BTreeSet<&str> {
        shows.iter().map(|s| s.id.as_str()).collect()
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
        assert_eq!(rnd.iter().find(|o| o.show_id == "s1").unwrap().episode_id, "a");
    }

    #[test]
    fn advance_persists_marks_dirty_and_advances_pick() {
        let r = Replica::new(":memory:");
        seed(&r);
        let (n, removed) = r.advance(&[("s1".into(), "a".into())]);
        assert_eq!(n, 1);
        assert!(removed.is_empty());
        let s1 = r.show("s1").unwrap();
        assert!(s1.episodes.iter().find(|e| e.id == "a").unwrap().watched_at.is_some());
        let nr = next_round(&r.active_shows(&["nelson".into()]));
        assert_eq!(nr.iter().find(|o| o.show_id == "s1").unwrap().episode_id, "b");
        let p = r.pending();
        assert!(p.episodes >= 1 && p.history == 1);
    }

    #[test]
    fn advance_tombstones_drained_show() {
        let r = Replica::new(":memory:");
        seed(&r);
        let (n, removed) = r.advance(&[("s2".into(), "c".into())]);
        assert_eq!(n, 1);
        assert_eq!(removed, ["s2"]);
        assert!(r.show("s2").unwrap().removed_at.is_some());
        assert_eq!(ids(&r.active_shows(&["nelson".into()])), ["s1"].into_iter().collect());
    }

    #[test]
    fn defer_persists_position_and_changes_pick() {
        let r = Replica::new(":memory:");
        seed(&r);
        assert!(r.defer("s1", "a"));
        let s1 = r.show("s1").unwrap();
        assert_eq!(s1.episodes.iter().find(|e| e.id == "a").unwrap().position, 2);
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
    fn merge_lww_keeps_dirty_local_but_takes_newer_clean() {
        let r = Replica::new(":memory:");
        seed(&r);
        r.advance(&[("s1".into(), "a".into())]); // episode a now dirty (locally watched)
        // An OLDER server view must NOT clobber the dirty local episode...
        r.merge_shows(&[LibraryShow {
            id: "s1".into(),
            playlist: "nelson".into(),
            name: "S1".into(),
            root_path: "D:\\A".into(),
            updated_at: T0.into(),
            episodes: vec![lep("a", "OLD.mkv", 0, OLD)],
            ..Default::default()
        }]);
        assert_eq!(
            r.show("s1").unwrap().episodes.iter().find(|e| e.id == "a").unwrap().relative_path,
            "a.mkv"
        );
        // ...but a NEWER server update to the (clean) show row does win.
        r.merge_shows(&[LibraryShow {
            id: "s1".into(),
            playlist: "nelson".into(),
            name: "S1NEW".into(),
            root_path: "D:\\A".into(),
            updated_at: NEW.into(),
            episodes: vec![],
            ..Default::default()
        }]);
        assert_eq!(r.show("s1").unwrap().name, "S1NEW");
    }

    #[test]
    fn mark_synced_clears_pending() {
        let r = Replica::new(":memory:");
        seed(&r);
        r.advance(&[("s1".into(), "a".into())]);
        let d = r.dirty();
        let ep_ids: Vec<String> = d.episodes.iter().map(|e| e["id"].as_str().unwrap().to_string()).collect();
        let h_ids: Vec<String> = d.history.iter().map(|h| h["id"].as_str().unwrap().to_string()).collect();
        r.mark_synced("episodes", &ep_ids);
        r.mark_synced("watch_history", &h_ids);
        let p = r.pending();
        assert_eq!(p.episodes, 0);
        assert_eq!(p.history, 0);
    }

    #[test]
    fn create_show_is_dirty_and_in_round() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "New", "D:\\New", &["e1.mkv".into(), "e2.mkv".into()]);
        assert!(!sid.is_empty());
        let me = r.active_shows(&["nelson".into()]).into_iter().find(|s| s.id == sid).unwrap();
        assert_eq!(me.episodes.len(), 2);
        let p = r.pending();
        assert_eq!(p.shows, 1);
        assert_eq!(p.episodes, 2);
    }

    #[test]
    fn remove_show_tombstones() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into()]);
        assert!(r.remove_show(&sid));
        assert!(r.active_shows(&["nelson".into()]).iter().all(|s| s.id != sid));
    }

    #[test]
    fn add_episodes_appends_positions() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into(), "b.mkv".into()]);
        assert_eq!(r.add_episodes(&sid, &["c.mkv".into()]), 1);
        let c = r.show(&sid).unwrap().episodes.into_iter().find(|e| e.relative_path == "c.mkv").unwrap();
        assert_eq!(c.position, 2); // continues after a(0), b(1)
    }

    #[test]
    fn episode_paths_and_update_show() {
        let r = Replica::new(":memory:");
        let sid = r.create_show("nelson", "X", "D:\\X", &["a.mkv".into(), "b.mkv".into()]);
        assert_eq!(
            r.episode_paths(&sid),
            ["a.mkv".to_string(), "b.mkv".to_string()].into_iter().collect()
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
    fn round_queue_helpers_and_merge() {
        let r = Replica::new(":memory:");
        
        // Initially empty
        assert!(r.get_round_queue().is_empty());
        assert!(r.dirty_queue().is_none());
        
        // Save queue
        let entries = vec![
            ("ep1".to_string(), "show1".to_string(), 0, "pending".to_string(), "nelson".to_string()),
            ("ep2".to_string(), "show2".to_string(), 1, "pending".to_string(), "nelson".to_string()),
        ];
        r.save_round_queue(&entries, "2026-05-31T00:00:00Z", true);
        
        let q = r.get_round_queue();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].0, "ep1");
        assert_eq!(q[1].0, "ep2");
        assert_eq!(q[0].6, 1); // dirty = 1
        
        // Check dirty queue serializes correctly
        let dq = r.dirty_queue().unwrap();
        assert_eq!(dq["playlist"], "nelson");
        assert_eq!(dq["entries"].as_array().unwrap().len(), 2);
        
        // Mark synced
        r.mark_queue_synced("nelson");
        assert!(r.dirty_queue().is_none());
        
        // Update entry state
        r.update_round_entry_state("ep1", "playing", "nelson");
        let q2 = r.get_round_queue();
        assert_eq!(q2[0].3, "playing");
        assert_eq!(q2[0].6, 1); // dirty = 1 again
        assert_eq!(q2[1].6, 1); // whole playlist queue marked dirty
        
        // Merge queue (should win if newer/clean)
        use crate::model::RoundQueueEntry;
        r.mark_queue_synced("nelson");
        let incoming = LibraryQueue {
            playlist: "nelson".to_string(),
            updated_at: "2026-06-01T00:00:00Z".to_string(),
            entries: vec![
                RoundQueueEntry {
                    episode_id: "ep1".to_string(),
                    show_id: "show1".to_string(),
                    play_order: 0,
                    state: "watched".to_string(),
                }
            ],
        };
        r.merge_queues(&[incoming]);
        let q3 = r.get_round_queue();
        assert_eq!(q3.len(), 1);
        assert_eq!(q3[0].3, "watched");
        assert_eq!(q3[0].6, 0); // dirty = 0
    }
}
