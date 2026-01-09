//! Node Builder Extensions
//!
//! Builder extensions for the node nicely modularizes parts
//! of the node building process.

// Re-export extension traits and types from base-primitives
pub use base_primitives::{
    BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksCell, FlashblocksConfig,
    OpBuilder, OpProvider, TracingConfig,
};

// Re-export extension implementations from domain crates
pub use base_reth_flashblocks::{FlashblocksCanonConfig, FlashblocksCanonExtension};
pub use base_reth_rpc::{BaseRpcConfig, BaseRpcExtension};
pub use base_txpool::{TransactionTracingConfig, TransactionTracingExtension};
