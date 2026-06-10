//! Sync orchestration between the local replica and the shared SQLite database —
//! git-style: local-first, push at smart moments, pull to seed/reconcile,
//! last-write-wins.
//!
//! Connectivity is *inferred*, never polled: every push/pull attempt sets
//! `online` (success) or clears it (failure). A failed attempt flips offline and
//! stops; the manual "check connectivity" action or the next natural push
//! recovers it. The runner never blocks on the network — playback runs entirely
//! off the replica.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use rusqlite::Connection;
use crate::replica::{Replica, SCHEMA};
use crate::model::{LibraryEpisode, LibraryShow, LibraryQueue, RoundQueueEntry};

pub struct Syncer {
    replica: Arc<Replica>,
    shared_db_path: Option<String>,
    playlists: Vec<String>,
    online: AtomicBool,
}

impl Syncer {
    pub fn new(replica: Arc<Replica>, shared_db_path: Option<String>, playlists: Vec<String>) -> Syncer {
        Syncer {
            replica,
            shared_db_path,
            playlists,
            online: AtomicBool::new(true), // optimistic; the first failed attempt flips it
        }
    }

    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Pull the library and reconcile it into the replica (initial seed and later
    /// pulls). Returns the resulting online state.
    pub fn seed(&self) -> bool {
        if self.shared_db_path.is_none() {
            self.online.store(false, Ordering::Relaxed);
            return false;
        }
        match self.open_shared() {
            Ok(conn) => {
                match load_library_shows(&conn, &self.playlists) {
                    Ok(shows) => self.replica.merge_shows(&shows),
                    Err(e) => {
                        log::warn!("Failed to load shows from shared DB: {e}");
                        self.online.store(false, Ordering::Relaxed);
                        return false;
                    }
                }
                match load_library_queues(&conn, &self.playlists) {
                    Ok(queues) => self.replica.merge_queues(&queues),
                    Err(e) => {
                        log::warn!("Failed to load queues from shared DB: {e}");
                        self.online.store(false, Ordering::Relaxed);
                        return false;
                    }
                }
                self.online.store(true, Ordering::Relaxed);
                true
            }
            Err(e) => {
                log::warn!("Failed to open shared DB for seed: {e}");
                self.online.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    /// Push dirty records and clear their dirty flags on success. No-op when
    /// nothing is pending. Returns the resulting online state.
    pub fn push(&self) -> bool {
        if self.shared_db_path.is_none() {
            self.online.store(false, Ordering::Relaxed);
            return false;
        }
        let mut has_dirty = self.pending() > 0 || self.replica.dirty_queue().is_some();

        // Auto-seed: if local database has shows but the shared database is completely empty,
        // mark all local records dirty to write them to the shared database.
        if !self.replica.active_shows(&self.playlists).is_empty() {
            if let Ok(conn) = self.open_shared() {
                if let Ok(count) = conn.query_row("SELECT COUNT(*) FROM shows", [], |r| r.get::<_, i64>(0)) {
                    if count == 0 {
                        log::info!("Shared DB is empty; auto-marking local database as dirty to seed it.");
                        if let Err(e) = self.replica.mark_all_dirty() {
                            log::warn!("Failed to mark all records dirty for seeding: {e}");
                        } else {
                            has_dirty = true;
                        }
                    }
                }
            }
        }

        if !has_dirty {
            return self.online();
        }
        match self.open_shared() {
            Ok(mut conn) => {
                match self.replica.push_to_shared(&mut conn) {
                    Ok((show_ids, episode_ids, history_ids, queue_playlist)) => {
                        self.replica.mark_synced("shows", &show_ids);
                        self.replica.mark_synced("episodes", &episode_ids);
                        self.replica.mark_synced("watch_history", &history_ids);
                        if let Some(ref pl) = queue_playlist {
                            self.replica.mark_queue_synced(pl);
                        }
                        self.online.store(true, Ordering::Relaxed);
                        true
                    }
                    Err(e) => {
                        log::warn!("Push failed; {} change(s) stay queued: {e}", self.pending());
                        self.online.store(false, Ordering::Relaxed);
                        false
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to open shared DB for push: {e}");
                self.online.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    /// Push then pull — the manual "check connectivity" / reconcile action.
    pub fn sync(&self) -> bool {
        self.push();
        if self.online() {
            self.seed();
        }
        self.online()
    }

    /// Unpushed local changes — the git "ahead" count.
    pub fn pending(&self) -> i64 {
        let p = self.replica.pending();
        p.shows + p.episodes + p.history
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

fn load_library_shows(conn: &Connection, playlists: &[String]) -> Result<Vec<LibraryShow>, rusqlite::Error> {
    if playlists.is_empty() {
        return Ok(vec![]);
    }
    let marks = vec!["?"; playlists.len()].join(",");
    let sql = format!(
        "SELECT id, playlist, name, root_path, date_added, removed_at, updated_at FROM shows WHERE playlist IN ({})",
        marks
    );
    let mut stmt = conn.prepare(&sql)?;
    
    let params_vec: Vec<&dyn rusqlite::ToSql> = playlists.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let mut shows = stmt.query_map(rusqlite::params_from_iter(params_vec), |r| {
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
        show.episodes = ep_stmt.query_map([&show.id], |r| {
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

fn load_library_queues(conn: &Connection, playlists: &[String]) -> Result<Vec<LibraryQueue>, rusqlite::Error> {
    let mut queues = Vec::new();
    for pl in playlists {
        let mut stmt = conn.prepare(
            "SELECT episode_id, show_id, play_order, state, updated_at FROM round_queue WHERE playlist = ? ORDER BY play_order"
        )?;
        let mut entries = Vec::new();
        let mut max_updated_at = "1970-01-01T00:00:00Z".to_string();

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

        if !entries.is_empty() {
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
    use tempfile::NamedTempFile;
    use crate::replica::Replica;

    fn temp_db_path() -> String {
        NamedTempFile::new().unwrap().path().to_string_lossy().to_string()
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
    fn auto_seed_empty_shared_db() {
        let r = Arc::new(Replica::new(":memory:"));
        let shared_path = temp_db_path();
        
        r.create_show("nelson", "S1", "D:\\A", &["a.mkv".into()]);
        let show_id = r.active_shows(&["nelson".into()])[0].id.clone();
        r.mark_synced("shows", &[show_id.clone()]);
        let ep_ids: Vec<String> = r.show(&show_id).unwrap().episodes.iter().map(|e| e.id.clone()).collect();
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
}
