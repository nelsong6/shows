//! What the palette's info view shows. Identity comes from CI's build stamp and the
//! running process; a value missing or unreadable arrives as a sentence saying so.
//! The version is release-version's (pipeline-templates), `v0.1.N` on a release.
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const DEVELOPMENT: &str = "development build, not from CI";
const NO_CHECKOUT: &str = "no checkout on this PC";
const REPOS: &str = r"D:\repos";

#[derive(Clone, Copy)]
struct Stamp<'a> {
    repo: Option<&'a str>,
    server: Option<&'a str>,
    version: Option<&'a str>,
    built: Option<&'a str>,
}

const BUILD: Stamp = Stamp {
    repo: option_env!("GITHUB_REPOSITORY"),
    server: option_env!("GITHUB_SERVER_URL"),
    version: option_env!("VERSION"),
    built: option_env!("BUILD_TIME"),
};

fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn executable(path: io::Result<PathBuf>) -> String {
    match path {
        Ok(path) => path.display().to_string(),
        Err(error) => format!("could not read the running executable's path: {error}"),
    }
}

fn checkout(root: &Path, repo: Option<&str>) -> String {
    let name = present(repo)
        .and_then(|repo| repo.rsplit('/').next())
        .unwrap_or("shows");
    let path = root.join(name);
    match path.join(".git").try_exists() {
        Ok(true) => path.display().to_string(),
        Ok(false) => NO_CHECKOUT.into(),
        Err(error) => format!("could not check {}: {error}", path.display()),
    }
}

fn facts(stamp: Stamp, exe: io::Result<PathBuf>, repos: &Path) -> Value {
    let repo = present(stamp.repo);
    let repo_url = match (present(stamp.server), repo) {
        (Some(server), Some(repo)) => Some(format!("{}/{repo}", server.trim_end_matches('/'))),
        _ => None,
    };
    let (built_unix, built) = match present(stamp.built) {
        None => (None, DEVELOPMENT.to_string()),
        Some(text) => match text.parse::<i64>() {
            Ok(unix) if (0..=253_402_300_799).contains(&unix) => (Some(unix), text.to_string()),
            _ => (None, format!("unreadable build time stamp: {text}")),
        },
    };
    json!({
        "repo": repo.unwrap_or(DEVELOPMENT),
        "repo_url": repo_url,
        "version": present(stamp.version).unwrap_or(DEVELOPMENT),
        "built_unix": built_unix,
        "built": built,
        "executable": executable(exe),
        "checkout": checkout(repos, repo),
    })
}

pub fn info() -> Value {
    facts(BUILD, std::env::current_exe(), Path::new(REPOS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> io::Result<PathBuf> {
        Ok(PathBuf::from(
            r"C:\Users\n\AppData\Local\shows\versions\v0.1.9\shows-desktop.exe",
        ))
    }

    #[test]
    fn a_ci_build_names_its_repo_link_release_and_time() {
        let repos = std::env::temp_dir().join(format!("shows-info-{}", std::process::id()));
        std::fs::create_dir_all(repos.join("shows").join(".git")).unwrap();
        let stamp = Stamp {
            repo: Some("nelsong6/shows"),
            server: Some("https://forgejo.romaine.life/"),
            version: Some("v0.1.9"),
            built: Some("1763300000"),
        };
        let facts = facts(stamp, exe(), &repos);
        std::fs::remove_dir_all(&repos).unwrap();
        assert_eq!(facts["repo"], "nelsong6/shows");
        assert_eq!(
            facts["repo_url"],
            "https://forgejo.romaine.life/nelsong6/shows"
        );
        assert_eq!(facts["version"], "v0.1.9");
        assert_eq!(facts["built_unix"], 1763300000);
        assert_eq!(facts["executable"], exe().unwrap().display().to_string());
        assert_eq!(facts["checkout"], repos.join("shows").display().to_string());
    }

    #[test]
    fn a_local_build_says_so_instead_of_inventing_values() {
        let stamp = Stamp {
            repo: None,
            server: None,
            version: Some(" "),
            built: None,
        };
        let missing = Path::new(r"Z:\no-such-repos");
        let facts = facts(stamp, Err(io::Error::other("gone")), missing);
        assert_eq!(facts["repo"], DEVELOPMENT);
        assert_eq!(facts["repo_url"], Value::Null);
        assert_eq!(facts["version"], DEVELOPMENT);
        assert_eq!(facts["built_unix"], Value::Null);
        assert_eq!(facts["built"], DEVELOPMENT);
        assert_eq!(
            facts["executable"],
            "could not read the running executable's path: gone"
        );
        assert_eq!(facts["checkout"], NO_CHECKOUT);
    }

    #[test]
    fn an_unreadable_build_time_is_named() {
        let stamp = Stamp {
            repo: None,
            server: None,
            version: None,
            built: Some("yesterday"),
        };
        let facts = facts(stamp, exe(), Path::new(r"Z:\no-such-repos"));
        assert_eq!(facts["built_unix"], Value::Null);
        assert_eq!(facts["built"], "unreadable build time stamp: yesterday");
    }
}
