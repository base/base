//! Contains the context for base block execution.

use alloy_primitives::{B256, Bytes};

use crate::BlsBlockAuthorization;

/// Context for base block execution.
#[derive(Debug, Default, Clone)]
pub struct BaseBlockExecutionCtx {
    /// Parent block hash.
    pub parent_hash: B256,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// The block's extra data.
    pub extra_data: Bytes,
    /// How the block's native BLS transfers are authorized. Defaults to verifying with no
    /// aggregate, which fails closed for any block that includes a transfer.
    pub bls_authorization: BlsBlockAuthorization,
}
