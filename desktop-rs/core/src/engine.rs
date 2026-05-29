//! The round/advance/defer engine — the desktop's local implementation of the
//! round-and-advance contract (`docs/feature-contracts/round-and-advance.md`).
//!
//! A faithful port of the Go server's pure store logic (`firstUnwatched` /
//! `applyAdvance` / `deferEpisode` / `allWatched` + `NextEpisodes`→`Sort`),
//! locked to the same contract by the ported vectors below. Pure: plain
//! structs, no SQLite / GUI / network. The replica layer loads rows, calls
//! these, and persists the results.

use crate::ordering::{sort_round, Candidate, Ordered};

#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub id: String,
    pub relative_path: String,
    pub position: i64,
    pub watched_at: Option<String>, // ISO-8601, or None if unwatched
    pub resume_pos: Option<f64>,    // seconds into the file, or None
}

#[derive(Debug, Clone, PartialEq)]
pub struct Show {
    pub id: String,
    pub playlist: String,
    pub name: String,
    pub root_path: String,
    pub episodes: Vec<Episode>,
    pub removed_at: Option<String>, // tombstone (all episodes watched)
    pub date_added: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRow {
    pub show_id: String,
    pub episode_id: String,
    pub relative_path: String,
    pub played_at: String,
}

/// The show's next pick: the lowest-position episode that isn't watched.
/// (On equal positions, the first in the slice wins — matching the reference.)
pub fn first_unwatched(episodes: &[Episode]) -> Option<&Episode> {
    episodes
        .iter()
        .filter(|e| e.watched_at.is_none())
        .min_by_key(|e| e.position)
}

pub fn all_watched(episodes: &[Episode]) -> bool {
    episodes.iter().all(|e| e.watched_at.is_some())
}

/// One episode per active show, in deterministic round order (contract I1).
///
/// Pass the active shows for one playlist for a single-playlist round, or the
/// union across playlists for a cross-playlist round (X1) — ordering keys on the
/// absolute path alone, so membership never changes an episode's place.
pub fn next_round(shows: &[Show]) -> Vec<Ordered> {
    let cands: Vec<Candidate> = shows
        .iter()
        .filter(|s| s.removed_at.is_none())
        .filter_map(|s| {
            first_unwatched(&s.episodes).map(|ep| Candidate {
                episode_id: ep.id.clone(),
                show_id: s.id.clone(),
                root_path: s.root_path.clone(),
                relative_path: ep.relative_path.clone(),
            })
        })
        .collect();
    sort_round(&cands)
}

/// Mark the named episodes watched on `show`, mirroring the server's
/// `applyAdvance`. Returns `(history rows, count newly watched, tombstoned?)`.
///
/// - I3 (idempotent): an already-watched episode is skipped — no re-mark, no
///   history row — so re-advancing is a no-op.
/// - I7 (per-episode skip): `episode_ids` may be any subset of the round.
/// - I5 (tombstone): if this drains the show's last unwatched episode, set
///   `removed_at` and return `true`.
///
/// Mutates `show` in place; the caller persists it + the returned history.
pub fn advance(show: &mut Show, episode_ids: &[String], now: &str) -> (Vec<HistoryRow>, usize, bool) {
    let show_id = show.id.clone();
    let mut history = Vec::new();
    let mut advanced = 0usize;
    for epid in episode_ids {
        for ep in show.episodes.iter_mut() {
            if &ep.id != epid {
                continue;
            }
            if ep.watched_at.is_some() {
                break; // I3
            }
            ep.watched_at = Some(now.to_string());
            advanced += 1;
            history.push(HistoryRow {
                show_id: show_id.clone(),
                episode_id: ep.id.clone(),
                relative_path: ep.relative_path.clone(),
                played_at: now.to_string(),
            });
            break;
        }
    }
    let removed = advanced > 0 && all_watched(&show.episodes);
    if removed {
        show.removed_at = Some(now.to_string());
    }
    (history, advanced, removed)
}

/// Re-roll a show's next pick: bump the named unwatched episode to the back of
/// its queue (position = max+1) without marking it watched (contract D1-D3).
/// Returns `false` (a no-op) if the episode is absent or already watched.
pub fn defer(episodes: &mut [Episode], episode_id: &str) -> bool {
    let mut idx: Option<usize> = None;
    let mut max_pos: i64 = 0;
    for (i, e) in episodes.iter().enumerate() {
        if e.position > max_pos {
            max_pos = e.position;
        }
        if e.id == episode_id {
            idx = Some(i);
        }
    }
    match idx {
        Some(i) if episodes[i].watched_at.is_none() => {
            episodes[i].position = max_pos + 1;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-05-28T12:00:00Z";
    const EARLIER: &str = "2026-05-28T11:00:00Z";

    fn ep(id: &str, pos: i64, watched: Option<&str>) -> Episode {
        Episode {
            id: id.to_string(),
            relative_path: format!("{id}.mkv"),
            position: pos,
            watched_at: watched.map(String::from),
            resume_pos: None,
        }
    }

    fn show(id: &str, playlist: &str, root: &str, eps: Vec<Episode>, removed_at: Option<&str>) -> Show {
        Show {
            id: id.into(),
            playlist: playlist.into(),
            name: id.to_uppercase(),
            root_path: root.into(),
            episodes: eps,
            removed_at: removed_at.map(String::from),
            date_added: None,
        }
    }

    // ── first_unwatched ──────────────────────────────────────────────
    #[test]
    fn first_unwatched_lowest_position() {
        let eps = [ep("c", 2, None), ep("a", 0, None), ep("b", 1, None)];
        assert_eq!(first_unwatched(&eps).unwrap().id, "a");
    }

    #[test]
    fn first_unwatched_skips_watched_even_lower() {
        let eps = [ep("a", 0, Some(NOW)), ep("b", 1, None)];
        assert_eq!(first_unwatched(&eps).unwrap().id, "b");
    }

    #[test]
    fn first_unwatched_none_when_all_watched_or_empty() {
        assert!(first_unwatched(&[ep("a", 0, Some(NOW)), ep("b", 1, Some(NOW))]).is_none());
        assert!(first_unwatched(&[]).is_none());
    }

    #[test]
    fn all_watched_works() {
        assert!(!all_watched(&[ep("a", 0, None), ep("b", 1, Some(NOW))]));
        assert!(all_watched(&[ep("a", 0, Some(NOW))]));
    }

    // ── advance (I3 / I5 / I7) ───────────────────────────────────────
    #[test]
    fn advance_subset_only_named() {
        let mut s = show("s1", "nelson", "D:\\A", vec![ep("a", 0, None), ep("b", 1, None)], None);
        let (history, n, removed) = advance(&mut s, &["a".into()], NOW);
        assert_eq!(n, 1);
        assert!(!removed);
        assert_eq!(s.episodes[0].watched_at.as_deref(), Some(NOW));
        assert!(s.episodes[1].watched_at.is_none());
        assert_eq!(history.iter().map(|h| h.episode_id.as_str()).collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn advance_idempotent() {
        let mut s = show("s1", "nelson", "D:\\A", vec![ep("a", 0, Some(EARLIER)), ep("b", 1, None)], None);
        let (history, n, removed) = advance(&mut s, &["a".into()], NOW);
        assert_eq!(n, 0);
        assert!(!removed);
        assert!(history.is_empty());
        assert_eq!(s.episodes[0].watched_at.as_deref(), Some(EARLIER)); // not overwritten
    }

    #[test]
    fn advance_tombstones_when_drained() {
        let mut s = show("s1", "nelson", "D:\\A", vec![ep("a", 0, Some(EARLIER)), ep("b", 1, None)], None);
        let (_, n, removed) = advance(&mut s, &["b".into()], NOW);
        assert_eq!(n, 1);
        assert!(removed);
        assert_eq!(s.removed_at.as_deref(), Some(NOW));
    }

    #[test]
    fn advance_whole_round_drains() {
        let mut s = show("s1", "nelson", "D:\\A", vec![ep("a", 0, None), ep("b", 1, None)], None);
        let (history, n, removed) = advance(&mut s, &["a".into(), "b".into()], NOW);
        assert_eq!(n, 2);
        assert!(removed);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn advance_unknown_id_noop() {
        let mut s = show("s1", "nelson", "D:\\A", vec![ep("a", 0, None)], None);
        let (_, n, removed) = advance(&mut s, &["nope".into()], NOW);
        assert_eq!(n, 0);
        assert!(!removed);
        assert!(s.episodes[0].watched_at.is_none());
    }

    // ── defer (D1-D3) ────────────────────────────────────────────────
    #[test]
    fn defer_bumps_and_changes_pick() {
        let mut eps = vec![ep("a", 0, None), ep("b", 1, None), ep("c", 2, None)];
        assert!(defer(&mut eps, "a"));
        assert_eq!(eps[0].position, 3);
        assert!(eps[0].watched_at.is_none());
        assert_eq!(first_unwatched(&eps).unwrap().id, "b");
    }

    #[test]
    fn defer_watched_or_absent_is_noop() {
        let mut eps = vec![ep("a", 0, Some(NOW)), ep("b", 1, None)];
        assert!(!defer(&mut eps, "a")); // watched
        assert!(!defer(&mut eps, "nope")); // absent
    }

    // ── next_round (I1 selection + ordering, X1 cross-playlist) ───────
    #[test]
    fn next_round_one_per_active_show_skips_drained_and_removed() {
        let shows = vec![
            show("s1", "nelson", "D:\\A", vec![ep("a", 0, None), ep("b", 1, Some(NOW))], None),
            show("s2", "nelson", "D:\\B", vec![ep("c", 0, Some(NOW))], None), // drained
            show("s3", "nelson", "D:\\C", vec![ep("d", 0, None)], Some(NOW)), // tombstoned
            show("s4", "couple", "D:\\D", vec![ep("e", 0, None)], None),      // other playlist
        ];
        let out = next_round(&shows);
        let picked: std::collections::BTreeSet<&str> = out.iter().map(|o| o.show_id.as_str()).collect();
        assert_eq!(picked, ["s1", "s4"].into_iter().collect());
    }

    #[test]
    fn next_round_deterministic_ascending_union() {
        let shows = vec![
            show("s1", "nelson", "D:\\A", vec![ep("a", 0, None)], None),
            show("s2", "couple", "D:\\B", vec![ep("b", 0, None)], None),
            show("s3", "nelson", "D:\\C", vec![ep("c", 0, None)], None),
        ];
        let out = next_round(&shows);
        let vals: Vec<u32> = out.iter().map(|o| o.order_value).collect();
        let mut sorted = vals.clone();
        sorted.sort_unstable();
        assert_eq!(vals, sorted); // round order is ascending by the hash key
    }
}
