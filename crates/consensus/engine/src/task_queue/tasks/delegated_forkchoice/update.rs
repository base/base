//! Input types for delegated forkchoice updates.

use base_protocol::L2BlockInfo;

/// Delegated forkchoice labels from a remote follow source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegatedForkchoiceUpdate {
    /// The delegated safe L2 block.
    pub safe_l2: L2BlockInfo,
    /// The delegated finalized L2 block number, if available.
    pub finalized_l2_number: Option<u64>,
}
