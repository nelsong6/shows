//! Best-effort "is a newer build out?" check for stable builds.
//!
//! Forgejo requires a stored Git credential even for public release metadata.
//! Authentication never prompts. Missing credentials, offline requests, and
//! dev/source builds without an embedded SHA simply yield `None`.
//!
//! The build SHA is stamped in at compile time via the `SHOWS_BUILD_SHA` env var
//! (set by `.forgejo/workflows/build-desktop.yaml`); a dev `cargo build` has none,
//! so it's treated as "version unknown" and skips the check.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const RELEASES_LATEST: &str =
    "https://forgejo.romaine.life/api/v1/repos/nelsong6/shows/releases/latest";
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

pub fn current_channel() -> &'static str {
    option_env!("SHOWS_UPDATE_CHANNEL").unwrap_or("dev")
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

/// Compare this build to Forgejo's latest release. `None` on a dev build, when
/// offline/rate-limited, or when already up to date.
pub fn check() -> Option<UpdateInfo> {
    if current_channel() != "stable" {
        return None;
    }
    let current = current_sha()?;
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
