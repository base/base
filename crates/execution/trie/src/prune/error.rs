use std::{
    fmt,
    fmt::{Display, Formatter},
    time::Duration,
};

use reth_provider::ProviderError;
use thiserror::Error;

use crate::{BaseProofsStorageError, api::WriteCounts};

/// Result of a single [`BaseProofStoragePruner`](crate::BaseProofStoragePruner) run.
pub type BaseProofStoragePrunerResult = Result<PrunerOutput, PrunerError>;

/// Successful prune summary.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct PrunerOutput {
    /// Total elapsed wall time for this run (fetch + apply).
    pub duration: Duration,
    /// Earliest block at the start of the run.
    pub start_block: u64,
    /// New earliest block at the end of the run.
    pub end_block: u64,
    /// Number of entries updated/removed per table.
    pub write_counts: WriteCounts,
}

impl Display for PrunerOutput {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let blocks = self.end_block.saturating_sub(self.start_block);
        let total_entries = self.write_counts.hashed_accounts_written_total
            + self.write_counts.hashed_storages_written_total
            + self.write_counts.account_trie_updates_written_total
            + self.write_counts.storage_trie_updates_written_total;
        write!(
            f,
            "Pruned {}→{} ({} blocks), entries={}, elapsed={:.3}s",
            self.start_block,
            self.end_block,
            blocks,
            total_entries,
            self.duration.as_secs_f64(),
        )
    }
}

impl PrunerOutput {
    /// extend the current [`PrunerOutput`] with another [`PrunerOutput`]
    pub fn extend_ref(&mut self, other: Self) {
        self.duration += other.duration;
        self.start_block = self.start_block.min(other.start_block);
        self.end_block = self.end_block.max(other.end_block);
        self.write_counts += other.write_counts;
    }
}

/// Error returned by the pruner.
#[derive(Debug, Error)]
pub enum PrunerError {
    /// Wrapped error from the underlying `BaseProofStorage` layer.
    #[error(transparent)]
    Storage(#[from] BaseProofsStorageError),

    /// Wrapped error from the reth db provider.
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Block not found in the underlying reth storage provider.
    #[error("block {0} not found in reth storage")]
    BlockNotFound(u64),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{PrunerError, PrunerOutput};
    use crate::api::WriteCounts;

    #[test]
    fn test_pruner_output_display() {
        let pruner_output = PrunerOutput {
            duration: Duration::from_secs(10),
            start_block: 1,
            end_block: 2,
            write_counts: WriteCounts::new(1, 2, 3, 4),
        };
        let formatted_pruner_output = format!("{pruner_output}");

        assert_eq!(formatted_pruner_output, "Pruned 1→2 (1 blocks), entries=10, elapsed=10.000s");
    }

    #[test]
    fn test_pruner_error_display_keeps_cause() {
        assert_eq!(
            PrunerError::BlockNotFound(42).to_string(),
            "block 42 not found in reth storage"
        );
    }
}
