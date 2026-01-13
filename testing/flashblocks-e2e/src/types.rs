//! RPC types for flashblocks testing.

use alloy_primitives::U256;
// Re-export metering types from base-bundles
pub use base_bundles::{Bundle, MeterBundleResponse, TransactionResult};
// Re-export flashblock types from base-flashtypes
pub use base_flashtypes::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, FlashblockDecodeError,
    FlashblocksPayloadV1, Metadata as FlashblockMetadata,
};
use serde::{Deserialize, Serialize};

/// Block type for Optimism network.
pub type OpBlock = alloy_rpc_types_eth::Block<op_alloy_rpc_types::Transaction>;

/// Response from `base_meteredPriorityFeePerGas` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteredPriorityFeeResponse {
    /// Number of blocks sampled.
    pub blocks_sampled: u64,
    /// Suggested priority fee.
    pub priority_fee: U256,
}
