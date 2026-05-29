//! Deterministic-random round ordering — bit-identical to the Go server and the
//! legacy `play_ordered_show.ps1`, so the desktop and server agree on round
//! order.
//!
//! Contract: hash the absolute path with SHA-256, take the first four hex chars
//! of the digest, parse as a u32. Sort ascending by that value, ties broken by
//! `episode_id`. See `docs/feature-contracts/round-and-advance.md`.

use sha2::{Digest, Sha256};

/// Backslash always — the hash input must match the legacy Windows paths
/// regardless of the OS this runs on. NOT a platform path join.
const PATH_SEPARATOR: char = '\\';

/// Join a show's root path with an episode's relative path the same way the
/// legacy importer did: strip stray separators, then join with a backslash.
pub fn join_path(root_path: &str, relative_path: &str) -> String {
    let r = root_path.trim_end_matches(['\\', '/']);
    let p = relative_path.trim_start_matches(['\\', '/']);
    format!("{r}{PATH_SEPARATOR}{p}")
}

/// The deterministic-random sort key for an absolute path.
///
/// `int(sha256_hexdigest[:4], 16)` in the Python/Go reference. The first four
/// hex chars of the digest are exactly the first two digest bytes, big-endian,
/// so this is that value with no string round-trip.
pub fn order_value(absolute_path: &str) -> u32 {
    let digest = Sha256::digest(absolute_path.as_bytes());
    ((digest[0] as u32) << 8) | (digest[1] as u32)
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub episode_id: String,
    pub show_id: String,
    pub root_path: String,
    pub relative_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ordered {
    pub episode_id: String,
    pub show_id: String,
    pub absolute_path: String,
    pub order_value: u32,
}

/// Resolve each candidate's absolute path + order value, then sort ascending by
/// `(order_value, episode_id)`.
pub fn sort_round(candidates: &[Candidate]) -> Vec<Ordered> {
    let mut out: Vec<Ordered> = candidates
        .iter()
        .map(|c| {
            let absolute_path = join_path(&c.root_path, &c.relative_path);
            let order_value = order_value(&absolute_path);
            Ordered {
                episode_id: c.episode_id.clone(),
                show_id: c.show_id.clone(),
                absolute_path,
                order_value,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.order_value
            .cmp(&b.order_value)
            .then_with(|| a.episode_id.cmp(&b.episode_id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_value_matches_canonical_sha256() {
        // Canonical values from the legacy SHA-256 (Get-FileHash-equivalent):
        // first 4 hex chars of the digest parsed base-16. Locks bit-exactness
        // with the Go server and play_ordered_show.ps1.
        assert_eq!(order_value("D:\\Shows\\foo\\bar.mkv"), 52416); // ccc0
        assert_eq!(order_value("D:\\Shows\\zzz.mkv"), 2883); // 0b43
        assert_eq!(order_value("a"), 51863); // ca97
    }

    #[test]
    fn join_path_normalizes_separators() {
        assert_eq!(
            join_path("D:\\Shows\\", "\\foo\\bar.mkv"),
            "D:\\Shows\\foo\\bar.mkv"
        );
        assert_eq!(
            join_path("D:\\Shows", "foo\\bar.mkv"),
            "D:\\Shows\\foo\\bar.mkv"
        );
        assert_eq!(join_path("D:/Shows/", "/x"), "D:/Shows\\x");
    }

    #[test]
    fn sort_round_orders_by_value_then_episode_id() {
        let cands = vec![
            Candidate {
                episode_id: "e-zzz".into(),
                show_id: "s".into(),
                root_path: "D:\\Shows".into(),
                relative_path: "zzz.mkv".into(), // 2883
            },
            Candidate {
                episode_id: "e-foo".into(),
                show_id: "s".into(),
                root_path: "D:\\Shows".into(),
                relative_path: "foo\\bar.mkv".into(), // 52416
            },
        ];
        let out = sort_round(&cands);
        assert_eq!(out[0].episode_id, "e-zzz");
        assert_eq!(out[0].order_value, 2883);
        assert_eq!(out[1].episode_id, "e-foo");
        assert_eq!(out[1].order_value, 52416);
    }

    #[test]
    fn sort_round_breaks_ties_by_episode_id() {
        let mk = |eid: &str| Candidate {
            episode_id: eid.into(),
            show_id: "s".into(),
            root_path: "D:\\Shows".into(),
            relative_path: "same.mkv".into(),
        };
        let out = sort_round(&[mk("e-b"), mk("e-a")]);
        assert_eq!(out[0].episode_id, "e-a");
        assert_eq!(out[1].episode_id, "e-b");
    }
}
