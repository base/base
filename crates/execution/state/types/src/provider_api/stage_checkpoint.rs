use alloc::{string::String, vec::Vec};

use crate::{ProviderResult, StageCheckpoint, StageId};

/// The trait for fetching stage checkpoint related data.
#[auto_impl::auto_impl(&, Arc)]
pub trait StageCheckpointReader: Send {
    /// Fetch the checkpoint for the given stage.
    fn get_stage_checkpoint(&self, id: StageId) -> ProviderResult<Option<StageCheckpoint>>;

    /// Get stage checkpoint progress.
    fn get_stage_checkpoint_progress(&self, id: StageId) -> ProviderResult<Option<Vec<u8>>>;

    /// Reads all stage checkpoints and returns a list with the name of the stage and the checkpoint
    /// data.
    fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>>;
}
