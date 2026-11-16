//! Scan a show's directory for episode files.
//!
//! The desktop is the only thing that can see the media (the AKS server has no
//! filesystem), so adding a show and detecting new episodes both happen here,
//! then sync up. Relative paths use backslashes to match the ordering hash —
//! see [`crate::ordering`] and `docs/feature-contracts/round-and-advance.md`.

use std::path::Path;

const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "m4v", "webm", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts", "ogv",
];

/// Folders holding a show's bonus material rather than its episodes: Jellyfin's
/// extras folder names, plus `specials`, so behind-the-scenes clips never play
/// between episodes.
const EXTRAS_DIRS: &[&str] = &[
    "behind the scenes",
    "clips",
    "deleted scenes",
    "extras",
    "featurettes",
    "interviews",
    "other",
    "samples",
    "scenes",
    "shorts",
    "specials",
    "trailers",
];

fn is_extras_dir(name: &str) -> bool {
    EXTRAS_DIRS.contains(&name.to_lowercase().as_str())
}

/// Whether a backslash-joined relative path lies in an extras folder.
pub fn is_extra(relative_path: &str) -> bool {
    let mut folders = relative_path.split('\\');
    folders.next_back();
    folders.any(is_extras_dir)
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Token {
    Text(String),
    Num(u64),
}

/// Natural-sort key: digit runs compared as integers, other runs as lowercased
/// text — so `S01E02` sorts before `S01E10`. Mirrors the Python `_natural_key`
/// (`re.split(r"(\d+)", s)`), including the empty segments at digit boundaries,
/// so the alternating text/number positions line up between any two keys.
fn natural_key(s: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut text = String::new();
    let mut num = String::new();
    let mut in_digits = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                tokens.push(Token::Text(std::mem::take(&mut text).to_lowercase()));
            }
            num.push(ch);
            in_digits = true;
        } else {
            if in_digits {
                tokens.push(Token::Num(num.parse().unwrap_or(u64::MAX)));
                num.clear();
            }
            text.push(ch);
            in_digits = false;
        }
    }
    if in_digits {
        tokens.push(Token::Num(num.parse().unwrap_or(u64::MAX)));
        tokens.push(Token::Text(String::new()));
    } else {
        tokens.push(Token::Text(text.to_lowercase()));
    }
    tokens
}

/// Backslash-joined relative paths of video files under `root_path`,
/// recursively, natural-sorted. Non-video files and extras folders are ignored.
pub fn scan_episodes(root_path: &str) -> Vec<String> {
    let root = Path::new(root_path);
    let mut found: Vec<String> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // unreadable dir: skip, like os.walk's onerror default
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let extras = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_extras_dir);
                if !extras {
                    stack.push(path);
                }
            } else if is_video(&path) {
                if let Ok(rel) = path.strip_prefix(root) {
                    found.push(rel.to_string_lossy().replace('/', "\\"));
                }
            }
        }
    }
    let mut keyed: Vec<(Vec<Token>, String)> =
        found.into_iter().map(|p| (natural_key(&p), p)).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn extras_folders_are_not_episodes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let season = root.join("Show S01");
        fs::create_dir_all(season.join("Featurettes").join("Extra Footage")).unwrap();
        fs::create_dir_all(season.join("Deleted Scenes")).unwrap();
        fs::write(season.join("Show - S01E01.mkv"), "x").unwrap();
        fs::write(season.join("Featurettes").join("Adapting.mkv"), "x").unwrap();
        fs::write(season.join("Featurettes").join("Extra Footage").join("Clip.mkv"), "x").unwrap();
        fs::write(season.join("Deleted Scenes").join("Cut.mkv"), "x").unwrap();
        assert_eq!(scan_episodes(root.to_str().unwrap()), ["Show S01\\Show - S01E01.mkv"]);
    }

    #[test]
    fn is_extra_reads_folders_not_file_names() {
        assert!(is_extra("Show S01\\FEATURETTES\\Adapting.mkv"));
        assert!(is_extra("Specials\\S00E01.mkv"));
        assert!(!is_extra("Show S01\\Show - S01E01 - Featurettes.mkv"));
        assert!(!is_extra("Season 1\\Extras Are People Too.mkv"));
    }

    #[test]
    fn finds_videos_natural_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("S01")).unwrap();
        for name in ["S01E02.mkv", "S01E10.mkv", "S01E01.mkv", "readme.txt", "poster.jpg"] {
            fs::write(root.join("S01").join(name), "x").unwrap();
        }
        fs::write(root.join("extra.avi"), "x").unwrap();
        let out = scan_episodes(root.to_str().unwrap());
        // video files only; natural order (E02 before E10); backslash relative paths
        assert_eq!(
            out,
            ["extra.avi", "S01\\S01E01.mkv", "S01\\S01E02.mkv", "S01\\S01E10.mkv"]
        );
    }

    #[test]
    fn ignores_non_video_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::write(dir.path().join("b.nfo"), "x").unwrap();
        assert!(scan_episodes(dir.path().to_str().unwrap()).is_empty());
    }
}
