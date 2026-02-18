/// Contains flashblocks execution info.
use base_access_lists::FlashblockAccessListBuilder;

/// Execution information specific to flashblocks.
///
/// Tracks the last consumed flashblock index and manages the
/// flashblock-level access list builder for progressive block construction.
#[derive(Debug, Default, Clone)]
pub struct FlashblocksExecutionInfo {
    /// Index of the last consumed flashblock
    pub last_flashblock_index: usize,

    /// Flashblock-level access list builder
    pub access_list_builder: FlashblockAccessListBuilder,
}
