//! Op types related to sync.

use alloy_primitives::{BlockNumber, B256};
use alloy_rpc_types_eth::BlockId;
use serde::{Deserialize, Serialize};

/// The block reference for an L2 block.
///
/// See: <https://github.com/ethereum-optimism/optimism/blob/develop/op-service/eth/id.go#L33>
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2BlockRef {
    /// The block hash.
    pub hash: B256,
    /// The block number.
    pub number: BlockNumber,
    /// The parent hash.
    pub parent_hash: B256,
    /// The timestamp.
    pub timestamp: u64,
    /// The L1 origin.
    #[serde(rename = "l1Origin")]
    pub l1_origin: BlockId,
    /// The sequence number.
    pub sequence_number: u64,
}

/// The block reference for an L1 block.
///
/// See: <https://github.com/ethereum-optimism/optimism/blob/develop/op-service/eth/id.go#L52>
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L1BlockRef {
    /// The block hash.
    pub hash: B256,
    /// The block number.
    pub number: BlockNumber,
    /// The parent hash.
    pub parent_hash: B256,
    /// The timestamp.
    pub timestamp: u64,
}

/// The [`SyncStatus`][ss] of an Optimism Rollup Node.
///
/// [ss]: https://github.com/ethereum-optimism/optimism/blob/develop/op-service/eth/sync_status.go#L5
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    /// The current L1 block.
    pub current_l1: L1BlockRef,
    /// The current L1 finalized block.
    pub current_l1_finalized: L1BlockRef,
    /// The L1 head block ref.
    pub head_l1: L1BlockRef,
    /// The L1 safe head block ref.
    pub safe_l1: L1BlockRef,
    /// The finalized L1 block ref.
    pub finalized_l1: L1BlockRef,
    /// The unsafe L2 block ref.
    pub unsafe_l2: L2BlockRef,
    /// The safe L2 block ref.
    pub safe_l2: L2BlockRef,
    /// The finalized L2 block ref.
    pub finalized_l2: L2BlockRef,
    /// The pending safe L2 block ref.
    pub pending_safe_l2: L2BlockRef,
}
