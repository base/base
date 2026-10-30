//! Contains the context for base block execution.

use alloy_primitives::{B256, Bytes};

/// Context for base block execution.
#[derive(Debug, Default, Clone)]
pub struct BaseBlockExecutionCtx {
    /// Parent block hash.
    pub parent_hash: B256,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// The block's extra data.
    pub extra_data: Bytes,
    /// The block's Amsterdam slot number (EIP-7843), when known.
    ///
    /// Base carries the L2 block number in this field.
    pub slot_number: Option<u64>,
}
