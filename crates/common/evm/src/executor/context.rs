//! Contains the context for base block execution.

use alloy_primitives::{Address, B256, Bytes};

/// Context for base block execution.
#[derive(Debug, Default, Clone)]
pub struct BaseBlockExecutionCtx {
    /// Parent block hash.
    pub parent_hash: B256,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// The block's extra data.
    pub extra_data: Bytes,
    /// Activation registry admin address for this block.
    pub activation_admin_address: Option<Address>,
}
