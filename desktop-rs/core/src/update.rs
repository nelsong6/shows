//! Best-effort "is a newer build out?" check, run once at launch.
//!
//! The repo is public, so GitHub's releases API needs no auth. This never
//! raises into playback: any problem (offline, rate-limited, or a dev/source
//! run with no embedded SHA) just yields `None` and the overlay shows no banner.
//!
//! The build SHA is stamped in at compile time via the `SHOWS_BUILD_SHA` env var
//! (set by `.github/workflows/build-desktop.yaml`); a dev `cargo build` has none,
//! so it's treated as "version unknown" and skips the check.

const RELEASES_LATEST: &str = "https://api.github.com/repos/nelsong6/shows/releases/latest";
const TAG_PREFIX: &str = "desktop-"; // release tags are desktop-<short-sha>

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub latest: String,
    pub current: String,
    pub url: String,
}

/// The short SHA this build was stamped with, or `None` on a source/dev run.
pub fn current_sha() -> Option<String> {
    option_env!("SHOWS_BUILD_SHA")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Pure comparison: given the current SHA and the latest release tag+url, return
/// `UpdateInfo` when a different (newer) release exists, else `None`.
pub fn compare(current: &str, tag: &str, url: &str) -> Option<UpdateInfo> {
    let latest = tag.strip_prefix(TAG_PREFIX).unwrap_or(tag);
    if !latest.is_empty() && latest != current {
        Some(UpdateInfo {
            latest: latest.to_string(),
            current: current.to_string(),
            url: url.to_string(),
        })
    } else {
        None
    }
}

/// Compare this build to GitHub's latest release. `None` on a dev build, when
/// offline/rate-limited, or when already up to date.
pub fn check() -> Option<UpdateInfo> {
    let current = current_sha()?;
    let (tag, url) = fetch_latest_release()?;
    compare(&current, &tag, &url)
}

/// Fetch the latest release's `(tag_name, html_url)`. `None` on any failure —
/// this is best-effort and must never surface an error into playback.
fn fetch_latest_release() -> Option<(String, String)> {
    let mut resp = ureq::get(RELEASES_LATEST)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "shows-desktop") // GitHub's API requires a User-Agent
        .call()
        .ok()?;
    let body = resp.body_mut().read_to_string().ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = json.get("tag_name")?.as_str()?.to_string();
    let url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some((tag, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_available() {
        assert_eq!(
            compare("old1234", "desktop-new5678", "https://gh/rel"),
            Some(UpdateInfo {
                latest: "new5678".into(),
                current: "old1234".into(),
                url: "https://gh/rel".into(),
            })
        );
    }

    #[test]
    fn up_to_date_is_none() {
        assert_eq!(compare("same999", "desktop-same999", ""), None);
    }

    #[test]
    fn empty_latest_is_none() {
        // a tag that's only the prefix (or empty) yields no comparable version
        assert_eq!(compare("x", "desktop-", "u"), None);
        assert_eq!(compare("x", "", "u"), None);
    }

    #[test]
    fn unprefixed_tag_compared_whole() {
        assert_eq!(
            compare("x", "v2", "u"),
            Some(UpdateInfo {
                latest: "v2".into(),
                current: "x".into(),
                url: "u".into(),
            })
        );
    }
}
