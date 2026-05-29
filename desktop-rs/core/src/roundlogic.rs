//! Pure round helpers — no GUI, mpv, or network, so they unit-test on any
//! platform. The runner delegates the parts that must be exactly right here.

/// Parse the `SHOWS_PLAYLISTS` value (comma-separated) into a playlist list,
/// trimming blanks. Falls back to `default` when nothing is set — mirrors the
/// server's `parsePlaylists` so a single entry behaves exactly like the
/// single-playlist path. (Callers pass `""` for an unset env var.)
pub fn parse_playlists(raw: &str, default: &[&str]) -> Vec<String> {
    let out: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if out.is_empty() {
        default.iter().map(|s| s.to_string()).collect()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_when_empty() {
        assert_eq!(parse_playlists("", &["nelson"]), ["nelson"]);
        assert_eq!(parse_playlists("   ", &["nelson"]), ["nelson"]);
    }

    #[test]
    fn trims_and_drops_blanks() {
        assert_eq!(parse_playlists(" a , b ,, c ", &["x"]), ["a", "b", "c"]);
    }

    #[test]
    fn single() {
        assert_eq!(parse_playlists("couple", &["nelson"]), ["couple"]);
    }
}
