use alloy_primitives::BlockNumber;
use base_execution_state_types::ProviderResult;
use reth_primitives_traits::{BlockHeader, SealedHeader};

/// Provider for getting the local tip header for sync gap calculation.
pub trait HeaderSyncGapProvider: Send {
    /// The header type.
    type Header: BlockHeader;

    /// Returns the local tip header for the given highest uninterrupted block.
    fn local_tip_header(
        &self,
        highest_uninterrupted_block: BlockNumber,
    ) -> ProviderResult<SealedHeader>;
}
