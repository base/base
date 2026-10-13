//! Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].

use alloy_primitives::FixedBytes;
use base_common_consensus::BaseBlock;

/// Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].
#[derive(Debug, Clone)]
pub enum L2BlockEvent {
    /// A new unsafe L2 block arrived.
    Block(Box<BaseBlock>),
    /// A block with detached authorization for its canonical BLS transfer intents.
    WitnessedBlock {
        /// The canonical block, without individual signatures.
        block: Box<BaseBlock>,
        /// Aggregate signature over exactly the block's BLS transfers.
        bls_aggregate: Option<FixedBytes<48>>,
    },
    /// An L2 reorg was detected.
    Reorg,
}
