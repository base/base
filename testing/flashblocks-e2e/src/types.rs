//! RPC types for flashblocks testing.

// Re-export metering types from base-bundles
pub use base_bundles::{Bundle, MeterBundleResponse, TransactionResult};
// Re-export flashblock types from base-flashtypes
pub use base_flashtypes::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, FlashblockDecodeError,
    FlashblocksPayloadV1, Metadata as FlashblockMetadata,
};

/// Block type for Optimism network.
pub type OpBlock = alloy_rpc_types_eth::Block<op_alloy_rpc_types::Transaction>;
