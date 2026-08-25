//! Startup repair for the `ExEx` write-ahead log directory.

use std::{
    fs::{self, File},
    path::PathBuf,
};

use eyre::{Context, Result, eyre};
use tracing::{info, warn};

/// Extension reth gives to `ExEx` WAL notification files.
const WAL_FILE_EXTENSION: &str = "wal";

/// Closes holes in the `ExEx` WAL file-id sequence before reth loads it.
///
/// reth derives the notifications to load from the dense range `min_id..=max_id` over the
/// `<id>.wal` filenames and treats any absent id inside that range as fatal, so one missing file
/// wedges the node into a boot loop that nothing but wiping the WAL clears.
///
/// Two mechanisms open those holes, and both recur in normal operation:
///
/// - Finalization selects files by the highest block each notification touches, which is not
///   monotonic in the file id. A reorg notification carries the reverted tip as its height, so a
///   later commit at a lower height finalizes ahead of it and leaves the earlier file behind.
/// - A failed unlink is swallowed and only logged, and the in-memory index entry is dropped before
///   the unlink is attempted. The file is orphaned for good, and later finalizations delete around
///   it.
///
/// Renaming the survivors down into a contiguous run preserves every notification. reth rebuilds
/// its block index from file contents and re-derives the next id from the highest filename, so the
/// ids carry no state of their own and are safe to renumber as long as their order is kept.
///
/// Verified against reth `v2.5.1`. A reth bump should re-check three things this relies on: the
/// `<id>.wal` naming, the `u32` id width, and the WAL being opened after the
/// `on_component_initialized` hook has run.
#[derive(Debug, Clone)]
pub struct ExExWalRepair {
    directory: PathBuf,
}

impl ExExWalRepair {
    /// Targets the `ExEx` WAL directory at the given path.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into() }
    }

    /// Renumbers the WAL notification files into a gap-free sequence of canonically named files.
    ///
    /// Does nothing when the directory is absent or reth can already load it as-is.
    pub fn run(&self) -> Result<()> {
        let notifications = self.notifications()?;
        let (Some(lowest), Some(highest)) =
            (notifications.first().map(|(id, _)| *id), notifications.last().map(|(id, _)| *id))
        else {
            return Ok(());
        };

        // A file whose name parses to an in-range id but is not spelled `<id>.wal` is as fatal as a
        // hole, because reth resolves the id it just listed back into that exact filename.
        let loadable = (lowest..)
            .zip(&notifications)
            .all(|(target, (id, path))| target == *id && *path == self.file_path(*id));
        if loadable {
            return Ok(());
        }

        warn!(
            target: "base-runner",
            directory = %self.directory.display(),
            notifications = notifications.len(),
            lowest,
            highest,
            "ExEx WAL notification ids are not a contiguous run; renumbering so reth can load them"
        );

        // Targets only ever move downwards, so an ascending pass never overwrites a file that has
        // yet to be renamed.
        let mut renumbered = 0usize;
        for (target, (id, path)) in (lowest..).zip(&notifications) {
            if target == *id && *path == self.file_path(*id) {
                continue;
            }

            fs::rename(path, self.file_path(target)).wrap_err_with(|| {
                format!("failed to renumber ExEx WAL notification {id} to {target}")
            })?;
            // Each rename is atomic, so a crash mid-pass leaves a shorter but still-valid run that
            // the next startup finishes. Flushing per rename is what keeps the directory entries
            // from landing out of order, which is the one way a crash could drop a notification.
            self.sync_directory()?;
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

    /// Reads every `<id>.wal` file in the directory, ordered by id.
    ///
    /// Returns an empty list when the directory does not exist yet. Paths are carried through as
    /// read rather than rebuilt from the id, so two spellings of one id cannot rename over each
    /// other.
    fn notifications(&self) -> Result<Vec<(u32, PathBuf)>> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.directory).wrap_err_with(|| {
            format!("failed to read ExEx WAL directory {}", self.directory.display())
        })?;

        let mut notifications = Vec::new();
        for entry in entries {
            let path = entry
                .wrap_err_with(|| {
                    format!("failed to read ExEx WAL directory {}", self.directory.display())
                })?
                .path();

            if path.extension().is_none_or(|extension| extension != WAL_FILE_EXTENSION) {
                continue;
            }

            match path.file_stem().and_then(|stem| stem.to_str()?.parse().ok()) {
                Some(id) => notifications.push((id, path)),
                // reth rejects the whole directory over one of these, so name the file here rather
                // than leave the operator with a boot loop that points at nothing.
                None => warn!(
                    target: "base-runner",
                    path = %path.display(),
                    "ExEx WAL file name is not a notification id; reth will refuse to load the WAL"
                ),
            }
        }

        notifications.sort_unstable_by_key(|(id, _)| *id);

        if let Some(pair) = notifications.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(eyre!(
                "ExEx WAL notification id {} is claimed by both {} and {}",
                pair[0].0,
                pair[0].1.display(),
                pair[1].1.display()
            ));
        }

        Ok(notifications)
    }

    /// Flushes the directory entries so that a crash cannot reorder the renames.
    fn sync_directory(&self) -> Result<()> {
        File::open(&self.directory).and_then(|directory| directory.sync_all()).wrap_err_with(|| {
            format!("failed to flush ExEx WAL directory {}", self.directory.display())
        })
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

    /// reth parses `007.wal` as id 7 and then tries to open `7.wal`, so a non-canonical name is as
    /// fatal as a hole even when the ids themselves are contiguous.
    #[test]
    fn non_canonical_name_is_renamed_to_its_id() {
        let dir = wal_dir(&[1]);
        fs::write(dir.path().join("002.wal"), "2").expect("write");

        ExExWalRepair::new(dir.path()).run().expect("run");

        assert_eq!(contents(&dir), vec![(1, 1), (2, 2)]);
    }

    /// Two names for one id would otherwise rename over each other and destroy a notification.
    #[test]
    fn duplicate_notification_id_is_rejected() {
        let dir = wal_dir(&[1, 7]);
        fs::write(dir.path().join("007.wal"), "7").expect("write");

        let error = ExExWalRepair::new(dir.path()).run().expect_err("duplicate id");

        assert!(error.to_string().contains("claimed by both"), "{error}");
        assert!(dir.path().join("7.wal").exists());
        assert!(dir.path().join("007.wal").exists());
    }

    /// Non-notification files are left in place; an unparsable `.wal` name is only warned about,
    /// since renaming a file whose contents reth may not understand is worse than reporting it.
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
