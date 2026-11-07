//! Watched marks on their way to the NAS database.
//!
//! Every mark lands in this local file before anything else happens, then is
//! applied to the database, so a mark survives an outage or a restart and there
//! is one path whether the database is reachable or not. Applying a mark twice
//! changes nothing (contract I3), which is what makes replaying this list safe
//! after a crash and across computers. This list is not a copy of the database:
//! it holds only writes that haven't landed, and it empties once they do.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWatch {
    pub show_id: String,
    pub episode_id: String,
    pub playlist: String,
    pub watched_at: String,
}

pub struct PendingWatches {
    path: PathBuf,
    file: Mutex<()>,
    flushing: Mutex<()>,
    count: AtomicUsize,
}

impl PendingWatches {
    pub fn open(path: PathBuf) -> PendingWatches {
        let count = read_entries(&path).len();
        PendingWatches {
            path,
            file: Mutex::new(()),
            flushing: Mutex::new(()),
            count: AtomicUsize::new(count),
        }
    }

    /// The list belonging to one database, so a preview pointed at another
    /// database can't apply this one's marks there.
    pub fn for_database(dir: &Path, database_path: &str) -> PendingWatches {
        let digest = Sha256::digest(database_path.as_bytes());
        let name: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        PendingWatches::open(dir.join(format!("pending-watches-{name}.jsonl")))
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Durably append one mark; on `Ok` it survives a crash.
    pub fn add(&self, watch: &PendingWatch) -> io::Result<()> {
        let _file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(watch).map_err(io::Error::other)?;
        file.write_all(format!("{line}\n").as_bytes())?;
        file.sync_all()?;
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Apply waiting marks oldest first, stopping at the first failure; what
    /// wasn't applied stays for next time. Marks added meanwhile are kept. A
    /// crash before the list is rewritten replays marks that already landed,
    /// which changes nothing.
    pub fn flush<E>(
        &self,
        mut apply: impl FnMut(&PendingWatch) -> Result<(), E>,
    ) -> Result<usize, E> {
        let _one_at_a_time = self.flushing.lock().unwrap_or_else(PoisonError::into_inner);
        if self.count() == 0 {
            return Ok(0);
        }
        let waiting = {
            let _file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
            read_entries(&self.path)
        };
        let mut applied = 0;
        let mut failure = None;
        for watch in &waiting {
            match apply(watch) {
                Ok(()) => applied += 1,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        {
            let _file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
            let remaining = read_entries(&self.path).split_off(applied);
            if let Err(e) = write_entries(&self.path, &remaining) {
                log::error!("could not update {}: {e}", self.path.display());
            }
            self.count.store(remaining.len(), Ordering::SeqCst);
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(applied),
        }
    }
}

fn read_entries(path: &Path) -> Vec<PendingWatch> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            log::error!("could not read {}: {e}", path.display());
            return Vec::new();
        }
    };
    text.lines()
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(watch) => Some(watch),
            // A crash mid-append can only tear the last line, and that mark's
            // control never reported success.
            Err(e) => {
                log::warn!("dropping unreadable pending watch: {e}");
                None
            }
        })
        .collect()
}

fn write_entries(path: &Path, entries: &[PendingWatch]) -> io::Result<()> {
    if entries.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let mut text = String::new();
    for entry in entries {
        text.push_str(&serde_json::to_string(entry).map_err(io::Error::other)?);
        text.push('\n');
    }
    let temporary = path.with_extension("jsonl.tmp");
    let mut file = fs::File::create(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(episode_id: &str) -> PendingWatch {
        PendingWatch {
            show_id: "s1".into(),
            episode_id: episode_id.into(),
            playlist: "nelson".into(),
            watched_at: "2026-11-07T00:00:00Z".into(),
        }
    }

    #[test]
    fn marks_survive_a_restart_until_they_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.jsonl");
        let first = PendingWatches::open(path.clone());
        first.add(&watch("e1")).unwrap();
        first.add(&watch("e2")).unwrap();

        let restarted = PendingWatches::open(path.clone());
        assert_eq!(restarted.count(), 2);
        let mut seen = Vec::new();
        let applied = restarted.flush(|w| {
            seen.push(w.episode_id.clone());
            Ok::<(), ()>(())
        });
        assert_eq!(applied, Ok(2));
        assert_eq!(seen, ["e1", "e2"]);
        assert_eq!(restarted.count(), 0);
        assert!(!path.exists(), "an empty list leaves nothing behind");
    }

    #[test]
    fn a_failed_apply_keeps_that_mark_and_everything_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.jsonl");
        let list = PendingWatches::open(path.clone());
        for id in ["e1", "e2", "e3"] {
            list.add(&watch(id)).unwrap();
        }

        let outcome = list.flush(|w| if w.episode_id == "e2" { Err("down") } else { Ok(()) });
        assert_eq!(outcome, Err("down"));
        assert_eq!(list.count(), 2);

        let mut seen = Vec::new();
        list.flush(|w| {
            seen.push(w.episode_id.clone());
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!(seen, ["e2", "e3"]);
    }

    #[test]
    fn a_mark_added_during_a_flush_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let list = PendingWatches::open(dir.path().join("pending.jsonl"));
        list.add(&watch("e1")).unwrap();

        list.flush(|_| list.add(&watch("e2")).map_err(|_| ())).unwrap();

        assert_eq!(list.count(), 1);
        let mut seen = Vec::new();
        list.flush(|w| {
            seen.push(w.episode_id.clone());
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!(seen, ["e2"]);
    }

    #[test]
    fn a_torn_last_line_does_not_cost_the_marks_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.jsonl");
        let list = PendingWatches::open(path.clone());
        list.add(&watch("e1")).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"show_id\":\"s")
            .unwrap();

        assert_eq!(PendingWatches::open(path).count(), 1);
    }

    #[test]
    fn each_database_has_its_own_list() {
        let dir = tempfile::tempdir().unwrap();
        let real = PendingWatches::for_database(dir.path(), "S:\\shows.db");
        let preview = PendingWatches::for_database(dir.path(), "D:\\preview.db");
        real.add(&watch("e1")).unwrap();
        assert_eq!(preview.count(), 0);
        assert_eq!(PendingWatches::for_database(dir.path(), "S:\\shows.db").count(), 1);
    }
}
