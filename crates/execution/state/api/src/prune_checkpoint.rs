use alloc::vec::Vec;

use base_execution_state_types::ProviderResult;
use base_execution_state_types::{PruneCheckpoint, PruneSegment};

/// The trait for fetching prune checkpoint related data.
#[auto_impl::auto_impl(&, Arc)]
pub trait PruneCheckpointReader: Send {
    /// Fetch the prune checkpoint for the given segment.
    fn get_prune_checkpoint(
        &self,
        segment: PruneSegment,
    ) -> ProviderResult<Option<PruneCheckpoint>>;

    /// Fetch all the prune checkpoints.
    fn get_prune_checkpoints(&self) -> ProviderResult<Vec<(PruneSegment, PruneCheckpoint)>>;
}

/// The trait for updating prune checkpoint related data.
#[auto_impl::auto_impl(&, Arc)]
pub trait PruneCheckpointWriter {
    /// Save prune checkpoint.
    fn save_prune_checkpoint(
        &self,
        segment: PruneSegment,
        checkpoint: PruneCheckpoint,
    ) -> ProviderResult<()>;
}
