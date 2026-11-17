//! The playlist this computer plays, chosen in the overlay or from the remote.
//!
//! The choice belongs to the computer, not the library, so it lives in a file
//! beside the pending-watch list rather than in the shared database: the TV and
//! the desk can play different playlists at once. A computer that has never
//! chosen plays the first playlist by name.

use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::replica::{DbResult, Replica};

pub struct PlaylistChoice {
    path: Option<PathBuf>,
    chosen: Mutex<Option<String>>,
}

impl PlaylistChoice {
    /// The choice saved at `path`; none when the file is missing or empty.
    pub fn open(path: PathBuf) -> PlaylistChoice {
        let chosen = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text.trim().to_string()).filter(|name| !name.is_empty()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                log::warn!("playlist choice at {} unreadable: {e}", path.display());
                None
            }
        };
        PlaylistChoice {
            path: Some(path),
            chosen: Mutex::new(chosen),
        }
    }

    /// A choice that is never written anywhere, for tests.
    pub fn in_memory(chosen: Option<&str>) -> PlaylistChoice {
        PlaylistChoice {
            path: None,
            chosen: Mutex::new(chosen.map(str::to_string)),
        }
    }

    pub fn get(&self) -> Option<String> {
        self.chosen.lock().unwrap().clone()
    }

    /// The playlist to play: the one chosen, or the first by name in `replica`
    /// when none has been; none when the library has no shows.
    pub fn resolve(&self, replica: &Replica) -> DbResult<Option<String>> {
        match self.get() {
            Some(chosen) => Ok(Some(chosen)),
            None => Ok(replica.playlists()?.into_iter().next()),
        }
    }

    /// Save `name` as this computer's playlist. The file is replaced whole, so
    /// a crash leaves the old choice or the new one, never half of either.
    pub fn save(&self, name: &str) -> io::Result<()> {
        if let Some(path) = &self.path {
            let partial = path.with_extension("partial");
            std::fs::write(&partial, name)?;
            std::fs::rename(&partial, path)?;
        }
        *self.chosen.lock().unwrap() = Some(name.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_choice_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("playlist.txt");
        assert_eq!(PlaylistChoice::open(path.clone()).get(), None);
        PlaylistChoice::open(path.clone())
            .save("background")
            .unwrap();
        assert_eq!(
            PlaylistChoice::open(path).get().as_deref(),
            Some("background")
        );
    }

    #[test]
    fn a_failed_save_keeps_the_old_choice() {
        let dir = tempfile::tempdir().unwrap();
        let choice = PlaylistChoice::open(dir.path().join("missing").join("playlist.txt"));
        assert!(choice.save("background").is_err());
        assert_eq!(choice.get(), None);
    }
}
