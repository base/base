//! Contains the [`Metadata`] type used in Flashblocks.

use alloy_primitives::{B256, map::HashMap};
use base_common_rpc_types::BaseTransactionReceipt;
use serde::{Deserialize, Serialize};

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    /// Block number this flashblock belongs to.
    pub block_number: u64,
    /// Optional pre-computed execution receipts from the sequencer builder.
    ///
    /// When present, consumers use these directly instead of re-executing
    /// transactions, eliminating the divergence described in #3274 where
    /// re-execution produces different gas/logs than the builder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipts: Option<HashMap<B256, BaseTransactionReceipt>>,
}
