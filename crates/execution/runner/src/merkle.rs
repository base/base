//! Base defaults for the merkle stage section of `reth.toml`.

use std::path::Path;

use eyre::Result;
use reth_config::{Config, config::MerkleConfig};
use tracing::info;

/// Merkle stage settings tuned for Base's 200ms blocks.
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
    /// stays below the 500,000-block hashing clean threshold, past which pruned nodes skip the
    /// changesets an incremental run reads.
    pub const REBUILD_THRESHOLD: u64 = 450_000;

    /// Replaces reth's default `rebuild_threshold` in the `reth.toml` at `path` with
    /// [`Self::REBUILD_THRESHOLD`], keeping any other value an operator chose.
    ///
    /// The default is also kept when lowered hashing clean thresholds would prune the changesets
    /// that incremental runs up to [`Self::REBUILD_THRESHOLD`] blocks read. A missing file is
    /// created with reth's defaults first, as node startup does.
    pub fn apply(path: &Path) -> Result<()> {
        let mut config = Config::from_path(path)?;
        if config.stages.merkle.rebuild_threshold != MerkleConfig::default().rebuild_threshold
            || config.stages.execution_external_clean_threshold() < Self::REBUILD_THRESHOLD
        {
            return Ok(());
        }
        config.stages.merkle.rebuild_threshold = Self::REBUILD_THRESHOLD;
        config.save(path)?;
        info!(
            target: "base-runner",
            path = %path.display(),
            rebuild_threshold = Self::REBUILD_THRESHOLD,
            "raised merkle stage rebuild threshold"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rebuild_threshold(path: &Path) -> u64 {
        Config::from_path(path).unwrap().stages.merkle.rebuild_threshold
    }

    #[test]
    fn raises_reth_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reth.toml");
        Config::default().save(&path).unwrap();

        MerkleStageDefaults::apply(&path).unwrap();

        assert_eq!(rebuild_threshold(&path), MerkleStageDefaults::REBUILD_THRESHOLD);
    }

    #[test]
    fn creates_missing_config_with_base_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reth.toml");

        MerkleStageDefaults::apply(&path).unwrap();

        assert_eq!(rebuild_threshold(&path), MerkleStageDefaults::REBUILD_THRESHOLD);
    }

    #[test]
    fn keeps_operator_threshold_and_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reth.toml");
        let mut config = Config::default();
        config.stages.merkle.rebuild_threshold = 250_000;
        config.stages.merkle.incremental_threshold = 1_000;
        config.save(&path).unwrap();

        MerkleStageDefaults::apply(&path).unwrap();

        let loaded = Config::from_path(&path).unwrap();
        assert_eq!(loaded.stages.merkle.rebuild_threshold, 250_000);
        assert_eq!(loaded.stages.merkle.incremental_threshold, 1_000);
    }

    #[test]
    fn keeps_default_when_changesets_are_pruned_sooner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reth.toml");
        let mut config = Config::default();
        config.stages.account_hashing.clean_threshold = 200_000;
        config.stages.storage_hashing.clean_threshold = 200_000;
        config.save(&path).unwrap();

        MerkleStageDefaults::apply(&path).unwrap();

        assert_eq!(rebuild_threshold(&path), MerkleConfig::default().rebuild_threshold);
    }
}
