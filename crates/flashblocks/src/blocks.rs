//! Contains the [`Flashblock`] and [`Metadata`] types used in Flashblocks.

use alloy_primitives::{Address, B256, U256, map::foldhash::HashMap};
use alloy_rpc_types_engine::PayloadId;
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use serde::{Deserialize, Serialize};

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Metadata {
    /// Transaction receipts indexed by hash.
    pub receipts: HashMap<B256, OpReceipt>,
    /// Updated account balances.
    pub new_account_balances: HashMap<Address, U256>,
    /// Block number this flashblock belongs to.
    pub block_number: u64,
}

/// A flashblock containing partial block data.
#[derive(Debug, Clone)]
pub struct Flashblock {
    /// Unique payload identifier.
    pub payload_id: PayloadId,
    /// Index of this flashblock within the block.
    pub index: u64,
    /// Base payload data (only present on first flashblock).
    pub base: Option<ExecutionPayloadBaseV1>,
    /// Delta containing transactions and state changes.
    pub diff: ExecutionPayloadFlashblockDeltaV1,
    /// Associated metadata.
    pub metadata: Metadata,
}
