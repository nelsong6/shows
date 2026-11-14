//! Best-effort "is a newer build out?" check for stable builds.
//!
//! Forgejo requires a stored Git credential even for public release metadata.
//! Authentication never prompts. Missing credentials, offline requests, and
//! dev/source builds without an embedded version simply yield `None`.
//!
//! CI stamps the release version (`v0.1.N`, or `v0.1.N-prP.R` for a build that
//! isn't released) in at compile time via `SHOWS_VERSION`, from
//! pipeline-templates' release-version step; a local `cargo build` has none, so
//! it's treated as "version unknown" and skips the check. A release is offered
//! only when its number is higher than this build's.

use std::cmp::Ordering;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const RELEASES_LATEST: &str =
    "https://forgejo.romaine.life/api/v1/repos/nelsong6/shows/releases/latest";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub latest: String,
    pub current: String,
    pub url: String,
}

/// The release version this build was stamped with, or `None` on a source/dev run.
pub fn current_version() -> Option<String> {
    option_env!("SHOWS_VERSION")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn current_channel() -> &'static str {
    option_env!("SHOWS_UPDATE_CHANNEL").unwrap_or("dev")
}

/// A version's place in the series: its numbers, and whether it is a build that
/// isn't released (`v0.1.13-pr152.871`), which comes before `v0.1.13` itself.
/// `None` for anything that isn't `v` and dot-separated numbers.
fn release_number(version: &str) -> Option<(Vec<u64>, bool)> {
    let (base, unreleased) = match version.split_once('-') {
        Some((base, _)) => (base, true),
        None => (version, false),
    };
    let numbers = base
        .strip_prefix('v')?
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    Some((numbers, unreleased))
}

fn newer(latest: &(Vec<u64>, bool), current: &(Vec<u64>, bool)) -> bool {
    match latest.0.cmp(&current.0) {
        Ordering::Equal => current.1 && !latest.1,
        order => order == Ordering::Greater,
    }
}

/// Pure comparison: given the current version and the latest release tag+url,
/// return `UpdateInfo` when the release is numbered higher than this build,
/// else `None`. A tag that isn't a release number is never offered.
pub fn compare(current: &str, tag: &str, url: &str) -> Option<UpdateInfo> {
    let latest = release_number(tag)?;
    let mine = release_number(current)?;
    newer(&latest, &mine).then(|| UpdateInfo {
        latest: tag.to_string(),
        current: current.to_string(),
        url: url.to_string(),
    })
}

/// Compare this build to Forgejo's latest release. `None` on a dev build, when
/// offline/rate-limited, or when already up to date.
pub fn check() -> Option<UpdateInfo> {
    if current_channel() != "stable" {
        return None;
    }
    let current = current_version()?;
    let (tag, url) = fetch_latest_release()?;
    compare(&current, &tag, &url)
}

/// Fetch the latest release's `(tag_name, html_url)`. `None` on any failure —
/// this is best-effort and must never surface an error into playback.
fn fetch_latest_release() -> Option<(String, String)> {
    let token = forgejo_token()?;
    let mut resp = ureq::get(RELEASES_LATEST)
        .header("Accept", "application/json")
        .header("User-Agent", "shows-desktop")
        .header("Authorization", &format!("token {token}"))
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

fn forgejo_token() -> Option<String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "credential.interactive=never", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().ok()?;
    let written = child.stdin.take().is_some_and(|mut input| {
        input
            .write_all(b"protocol=https\nhost=forgejo.romaine.life\n\n")
            .is_ok()
    });
    if !written {
        stop_credential_query(&mut child);
        return None;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                stop_credential_query(&mut child);
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("password="))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn stop_credential_query(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill.exe")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_release_is_offered() {
        assert_eq!(
            compare("v0.1.4", "v0.1.12", "https://forgejo/rel"),
            Some(UpdateInfo {
                latest: "v0.1.12".into(),
                current: "v0.1.4".into(),
                url: "https://forgejo/rel".into(),
            })
        );
        assert!(compare("v0.1.40", "v0.2.0", "u").is_some());
    }

    #[test]
    fn the_same_or_an_older_release_is_not_offered() {
        assert_eq!(compare("v0.1.7", "v0.1.7", ""), None);
        assert_eq!(compare("v0.1.12", "v0.1.4", ""), None);
    }

    #[test]
    fn a_build_that_is_not_released_is_behind_its_own_number() {
        assert!(compare("v0.1.13-pr152.871", "v0.1.13", "u").is_some());
        assert_eq!(compare("v0.1.13-pr152.871", "v0.1.12", "u"), None);
        assert_eq!(compare("v0.1.13-run871", "v0.1.12", "u"), None);
    }

    #[test]
    fn a_tag_or_build_that_is_not_a_release_number_is_never_offered() {
        // desktop-<commit> is how releases were named before they were numbered.
        assert_eq!(compare("v0.1.4", "desktop-1a2b3c4", "u"), None);
        assert_eq!(compare("v0.1.4", "", "u"), None);
        assert_eq!(compare("v0.1.4", "v0.1.x", "u"), None);
        assert_eq!(compare("dev", "v0.1.9", "u"), None);
    }
}
