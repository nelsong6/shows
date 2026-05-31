//! Sync orchestration between the local replica and the server origin —
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

use serde_json::Value;

use crate::apiclient::Client;
use crate::replica::Replica;

/// A replica row shaped for the wire: drop the local-only `dirty` flag.
fn wire(row: &Value) -> Value {
    let mut out = row.clone();
    if let Some(map) = out.as_object_mut() {
        map.remove("dirty");
    }
    out
}

fn ids(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str).map(String::from))
        .collect()
}

pub struct Syncer {
    replica: Arc<Replica>,
    client: Arc<Client>,
    playlists: Vec<String>,
    online: AtomicBool,
}

impl Syncer {
    pub fn new(replica: Arc<Replica>, client: Arc<Client>, playlists: Vec<String>) -> Syncer {
        Syncer {
            replica,
            client,
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
        match self.client.get_library(&self.playlists) {
            Ok((shows, queues)) => {
                self.replica.merge_shows(&shows);
                self.replica.merge_queues(&queues);
                self.online.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                log::warn!("pull failed; staying on local replica: {e}");
                self.online.store(false, Ordering::Relaxed);
            }
        }
        self.online()
    }

    /// Push dirty records and clear their dirty flags on success. No-op when
    /// nothing is pending. Returns the resulting online state.
    pub fn push(&self) -> bool {
        let d = self.replica.dirty();
        let q = self.replica.dirty_queue();
        if d.shows.is_empty() && d.episodes.is_empty() && d.history.is_empty() && q.is_none() {
            return self.online();
        }
        let shows: Vec<Value> = d.shows.iter().map(wire).collect();
        let episodes: Vec<Value> = d.episodes.iter().map(wire).collect();
        let history: Vec<Value> = d.history.iter().map(wire).collect();
        match self.client.post_sync(shows, episodes, history, q.clone()) {
            Ok(()) => {
                self.replica.mark_synced("shows", &ids(&d.shows));
                self.replica.mark_synced("episodes", &ids(&d.episodes));
                self.replica.mark_synced("watch_history", &ids(&d.history));
                if let Some(ref q_val) = q {
                    if let Some(pl) = q_val.get("playlist").and_then(Value::as_str) {
                        self.replica.mark_queue_synced(pl);
                    }
                }
                self.online.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                log::warn!("push failed; {} change(s) stay queued: {e}", self.pending());
                self.online.store(false, Ordering::Relaxed);
            }
        }
        self.online()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apiclient::FnHttp;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    const T0: &str = "2026-01-01T00:00:00Z";

    fn lib() -> Value {
        json!([{
            "id":"s1","playlist":"nelson","name":"S1","root_path":"D:\\A","updated_at":T0,
            "episodes":[
                {"id":"a","relative_path":"a.mkv","position":0,"updated_at":T0},
                {"id":"b","relative_path":"b.mkv","position":1,"updated_at":T0}
            ]
        }])
    }

    fn syncer<F>(r: Arc<Replica>, handler: F) -> Syncer
    where
        F: Fn(&str, &str, &str, Option<&Value>) -> (u16, String) + Send + Sync + 'static,
    {
        let c = Arc::new(Client::with_http("tok", "https://x.test", None, Box::new(FnHttp(handler))));
        Syncer::new(r, c, vec!["nelson".into()])
    }

    #[test]
    fn seed_pulls_into_replica() {
        let r = Arc::new(Replica::new(":memory:"));
        let s = syncer(r.clone(), |_m, url, _t, _b| {
            assert!(url.contains("/api/library"));
            assert!(url.contains("playlists=nelson"));
            (200, json!({ "shows": lib() }).to_string())
        });
        assert!(s.seed());
        assert!(s.online());
        let ids: BTreeSet<String> = r.active_shows(&["nelson".into()]).iter().map(|sh| sh.id.clone()).collect();
        assert_eq!(ids, ["s1".to_string()].into_iter().collect());
    }

    #[test]
    fn seed_failure_goes_offline() {
        let r = Arc::new(Replica::new(":memory:"));
        let s = syncer(r, |_m, _u, _t, _b| (500, "boom".into()));
        assert!(!s.seed());
        assert!(!s.online());
    }

    #[test]
    fn push_sends_dirty_stripped_then_clears() {
        let r = Arc::new(Replica::new(":memory:"));
        let seen: Arc<Mutex<Option<Value>>> = Default::default();
        let seen2 = seen.clone();
        let s = syncer(r.clone(), move |_m, url, _t, body| {
            if url.contains("/api/library") {
                return (200, json!({ "shows": lib() }).to_string());
            }
            if url.contains("/api/sync") {
                *seen2.lock().unwrap() = body.cloned();
                return (204, String::new());
            }
            (404, String::new())
        });
        s.seed();
        r.advance(&[("s1".into(), "a".into())]); // a watched + a history row (s1 not drained: b remains)
        assert_eq!(s.pending(), 2);
        assert!(s.push());
        assert_eq!(s.pending(), 0);
        let body = seen.lock().unwrap().clone().unwrap();
        let eps = body["episodes"].as_array().unwrap();
        assert!(eps.iter().any(|e| e["id"] == "a" && e.get("watched_at").is_some_and(|w| !w.is_null())));
        assert_eq!(body["history"].as_array().unwrap().len(), 1);
        assert!(eps[0].get("dirty").is_none()); // local-only flag stripped from the wire
    }

    #[test]
    fn push_failure_keeps_changes_and_goes_offline() {
        let r = Arc::new(Replica::new(":memory:"));
        let s = syncer(r.clone(), |_m, url, _t, _b| {
            if url.contains("/api/library") {
                (200, json!({ "shows": lib() }).to_string())
            } else {
                (503, String::new())
            }
        });
        s.seed();
        r.advance(&[("s1".into(), "a".into())]);
        let before = s.pending();
        assert!(!s.push());
        assert!(!s.online());
        assert_eq!(s.pending(), before); // nothing lost — stays queued for next time
    }

    #[test]
    fn push_noop_when_clean_stays_online() {
        let r = Arc::new(Replica::new(":memory:"));
        let s = syncer(r, |_m, _u, _t, _b| (200, json!({ "shows": lib() }).to_string()));
        s.seed();
        assert!(s.push());
        assert!(s.online());
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn seed_and_push_round_queue() {
        let r = Arc::new(Replica::new(":memory:"));
        let seen_sync: Arc<Mutex<Option<Value>>> = Default::default();
        let seen_sync2 = seen_sync.clone();
        
        let s = syncer(r.clone(), move |_method, url, _token, body| {
            if url.contains("/api/library") {
                return (
                    200,
                    json!({
                        "shows": lib(),
                        "queues": [
                            {
                                "playlist": "nelson",
                                "updated_at": T0,
                                "entries": [
                                    {"episode_id": "a", "show_id": "s1", "play_order": 0, "state": "pending"}
                                ]
                            }
                        ]
                    })
                    .to_string(),
                );
            }
            if url.contains("/api/sync") {
                *seen_sync2.lock().unwrap() = body.cloned();
                return (204, String::new());
            }
            (404, String::new())
        });
        
        // Pull library -> seeds both shows and queues
        assert!(s.seed());
        let q = r.get_round_queue();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, "a");
        assert_eq!(q[0].6, 0); // dirty = 0
        
        // Modify state locally -> marks queue dirty
        r.update_round_entry_state("a", "playing", "nelson");
        let q_dirty = r.dirty_queue();
        assert!(q_dirty.is_some());
        
        // Push -> pushes dirty queue
        assert!(s.push());
        let body = seen_sync.lock().unwrap().clone().unwrap();
        assert_eq!(body["queue"]["playlist"], "nelson");
        assert_eq!(body["queue"]["entries"][0]["episode_id"], "a");
        assert_eq!(body["queue"]["entries"][0]["state"], "playing");
        
        // Pushed queue marked clean
        assert!(r.dirty_queue().is_none());
    }
}
