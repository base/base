//! Startup repair for the `ExEx` write-ahead log directory.

use std::{fs, path::PathBuf};

use eyre::{Context, Result};
use tracing::{info, warn};

/// Extension reth gives to `ExEx` WAL notification files.
const WAL_FILE_EXTENSION: &str = "wal";

/// Closes holes in the `ExEx` WAL file-id sequence before reth loads it.
///
/// reth derives the notifications to load from the dense range `min_id..=max_id` over the
/// `<id>.wal` filenames and treats any absent id inside that range as fatal, so one missing file
/// wedges the node into a boot loop that nothing but wiping the WAL clears. Holes arise in normal
/// operation: WAL finalization selects files by block height rather than by id, deletes them in an
/// unordered batch, ignores individual unlink failures, and drops its in-memory index before the
/// unlink happens.
///
/// Renaming the survivors down into a contiguous run preserves every notification. reth rebuilds
/// its block index from file contents and re-derives the next id from the highest filename, so the
/// ids carry no state of their own and are safe to renumber as long as their order is kept.
#[derive(Debug, Clone)]
pub struct ExExWalRepair {
    directory: PathBuf,
}

impl ExExWalRepair {
    /// Targets the `ExEx` WAL directory at the given path.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into() }
    }

    /// Renumbers the WAL notification files into a gap-free sequence.
    ///
    /// Does nothing when the directory is absent or the sequence is already contiguous.
    pub fn run(&self) -> Result<()> {
        let Some(mut ids) = self.notification_ids()? else {
            return Ok(());
        };
        ids.sort_unstable();

        if ids.windows(2).all(|pair| pair[1] == pair[0] + 1) {
            return Ok(());
        }

        let lowest = ids[0];
        warn!(
            target: "base-runner",
            directory = %self.directory.display(),
            notifications = ids.len(),
            lowest,
            highest = ids[ids.len() - 1],
            "ExEx WAL notification ids are not contiguous; renumbering so reth can load the WAL"
        );

        // Targets only ever move downwards, so an ascending pass never overwrites a file that has
        // yet to be renamed.
        let mut renumbered = 0usize;
        for (target, id) in (lowest..).zip(ids) {
            if target == id {
                continue;
            }

            fs::rename(self.file_path(id), self.file_path(target)).wrap_err_with(|| {
                format!("failed to renumber ExEx WAL notification {id} to {target}")
            })?;
            renumbered += 1;
        }

        info!(
            target: "base-runner",
            directory = %self.directory.display(),
            renumbered,
            "ExEx WAL notification ids renumbered"
        );

        Ok(())
    }

    /// Reads the ids of every `<id>.wal` file in the directory.
    ///
    /// Returns `None` when the directory does not exist yet. Files whose name reth cannot parse
    /// are left alone so that reth reports them itself.
    fn notification_ids(&self) -> Result<Option<Vec<u32>>> {
        if !self.directory.exists() {
            return Ok(None);
        }

        let entries = fs::read_dir(&self.directory).wrap_err_with(|| {
            format!("failed to read ExEx WAL directory {}", self.directory.display())
        })?;

        let mut ids = Vec::new();
        for entry in entries {
            let path = entry
                .wrap_err_with(|| {
                    format!("failed to read ExEx WAL directory {}", self.directory.display())
                })?
                .path();

            if path.extension().is_some_and(|extension| extension == WAL_FILE_EXTENSION)
                && let Some(id) = path.file_stem().and_then(|stem| stem.to_str()?.parse().ok())
            {
                ids.push(id);
            }
        }

        Ok(Some(ids))
    }

    fn file_path(&self, id: u32) -> PathBuf {
        self.directory.join(format!("{id}.{WAL_FILE_EXTENSION}"))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn wal_dir(ids: &[u32]) -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for id in ids {
            fs::write(dir.path().join(format!("{id}.wal")), id.to_string()).expect("write");
        }
        dir
    }

    fn contents(dir: &TempDir) -> Vec<(u32, u32)> {
        let mut found = fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|entry| {
                let path = entry.expect("entry").path();
                let id = path.file_stem().unwrap().to_str().unwrap().parse().unwrap();
                let origin = fs::read_to_string(&path).expect("read").parse().unwrap();
                (id, origin)
            })
            .collect::<Vec<(u32, u32)>>();
        found.sort_unstable();
        found
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        ExExWalRepair::new(dir.path().join("exex/wal")).run().expect("run");
    }

    #[test]
    fn empty_directory_is_left_alone() {
        let dir = wal_dir(&[]);
        ExExWalRepair::new(dir.path()).run().expect("run");
        assert!(contents(&dir).is_empty());
    }

    #[test]
    fn contiguous_sequence_is_left_alone() {
        let dir = wal_dir(&[7, 8, 9]);
        ExExWalRepair::new(dir.path()).run().expect("run");
        assert_eq!(contents(&dir), vec![(7, 7), (8, 8), (9, 9)]);
    }

    #[test]
    fn single_hole_is_closed_preserving_order() {
        let dir = wal_dir(&[10, 12, 13]);
        ExExWalRepair::new(dir.path()).run().expect("run");
        assert_eq!(contents(&dir), vec![(10, 10), (11, 12), (12, 13)]);
    }

    #[test]
    fn multiple_holes_are_closed_preserving_order() {
        let dir = wal_dir(&[4, 9, 10, 40]);
        ExExWalRepair::new(dir.path()).run().expect("run");
        assert_eq!(contents(&dir), vec![(4, 4), (5, 9), (6, 10), (7, 40)]);
    }

    #[test]
    fn repair_is_idempotent() {
        let dir = wal_dir(&[2, 5, 6]);
        let repair = ExExWalRepair::new(dir.path());
        repair.run().expect("first run");
        let after_first = contents(&dir);
        repair.run().expect("second run");
        assert_eq!(contents(&dir), after_first);
    }

    /// A crash part way through a repair leaves a still-holey but otherwise valid set, which the
    /// next startup finishes closing.
    #[test]
    fn interrupted_repair_is_finished_on_the_next_run() {
        let dir = wal_dir(&[3, 5, 8]);
        // Simulates the first rename of `[3, 5, 8]` landing before the process died.
        fs::rename(dir.path().join("5.wal"), dir.path().join("4.wal")).expect("rename");

        ExExWalRepair::new(dir.path()).run().expect("run");
        assert_eq!(contents(&dir), vec![(3, 3), (4, 5), (5, 8)]);
    }

    #[test]
    fn unrelated_files_are_ignored() {
        let dir = wal_dir(&[1, 3]);
        fs::write(dir.path().join("scratch.tmp"), "1").expect("write");
        fs::write(dir.path().join("notanid.wal"), "1").expect("write");

        ExExWalRepair::new(dir.path()).run().expect("run");

        assert!(dir.path().join("scratch.tmp").exists());
        assert!(dir.path().join("notanid.wal").exists());
        assert!(dir.path().join("1.wal").exists());
        assert!(dir.path().join("2.wal").exists());
        assert!(!dir.path().join("3.wal").exists());
    }
}
