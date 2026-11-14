//! Base defaults for the merkle stage section of `reth.toml`.

use std::path::Path;

use eyre::Result;
use reth_config::Config;
use tracing::info;

/// Merkle stage settings tuned for Base's 200ms blocks, written when a `reth.toml` is created.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MerkleStageDefaults;

impl MerkleStageDefaults {
    /// Pipeline ranges longer than this many blocks rebuild the whole trie instead of applying
    /// the range's changes.
    ///
    /// A mainnet rebuild takes 7-8 hours whatever the range. Reth's default of 100,000 blocks is
    /// about 5.5 hours of 200ms blocks, so each rebuild ends more than 100,000 blocks behind and
    /// the next pipeline round rebuilds again: a node restored from a snapshot older than that
    /// never reaches the tip. 450,000 blocks (25 hours) keeps day-old snapshots incremental and
    /// stays below reth's default 500,000-block hashing clean threshold, past which pruned nodes
    /// skip the changesets an incremental run reads.
    pub const REBUILD_THRESHOLD: u64 = 450_000;

    /// Creates the `reth.toml` at `path` with reth's defaults and [`Self::REBUILD_THRESHOLD`].
    ///
    /// An existing file is left as it is.
    pub fn create(path: &Path) -> Result<()> {
        if path.exists() {
            return Ok(());
        }
        Self::apply(path)
    }

    /// Sets [`Self::REBUILD_THRESHOLD`] in the `reth.toml` at `path`, creating it with reth's
    /// defaults when missing. Call only on a file that was just generated.
    pub fn apply(path: &Path) -> Result<()> {
        let mut config = Config::from_path(path)?;
        config.stages.merkle.rebuild_threshold = Self::REBUILD_THRESHOLD;
        config.save(path)?;
        info!(
            target: "base-runner",
            path = %path.display(),
            rebuild_threshold = Self::REBUILD_THRESHOLD,
            "wrote merkle stage rebuild threshold"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use reth_config::config::MerkleConfig;

    use super::*;

    fn rebuild_threshold(path: &Path) -> u64 {
        Config::from_path(path).unwrap().stages.merkle.rebuild_threshold
    }

    #[test]
    fn create_writes_base_threshold_to_missing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reth.toml");

        MerkleStageDefaults::create(&path).unwrap();

        assert_eq!(rebuild_threshold(&path), MerkleStageDefaults::REBUILD_THRESHOLD);
    }

    #[test]
    fn create_leaves_existing_config_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reth.toml");
        Config::default().save(&path).unwrap();

        MerkleStageDefaults::create(&path).unwrap();

        assert_eq!(rebuild_threshold(&path), MerkleConfig::default().rebuild_threshold);
    }

    #[test]
    fn apply_sets_base_threshold_and_keeps_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reth.toml");
        let mut config = Config::default();
        config.stages.merkle.incremental_threshold = 1_000;
        config.save(&path).unwrap();

        MerkleStageDefaults::apply(&path).unwrap();

        let loaded = Config::from_path(&path).unwrap();
        assert_eq!(loaded.stages.merkle.rebuild_threshold, MerkleStageDefaults::REBUILD_THRESHOLD);
        assert_eq!(loaded.stages.merkle.incremental_threshold, 1_000);
    }
}
