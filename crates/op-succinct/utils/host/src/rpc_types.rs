//! RPC Types for Optimism Rollup
//!
//! These types are copied from kona-rpc to avoid bringing in rollup-boost dependencies
//! which cause alloy version conflicts with hokulea v1.1.4.

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use kona_protocol::{L2BlockInfo, SyncStatus};
use serde::{Deserialize, Serialize};

/// An [output response][or] for Optimism Rollup.
///
/// [or]: https://github.com/ethereum-optimism/optimism/blob/f20b92d3eb379355c876502c4f28e72a91ab902f/op-service/eth/output.go#L10-L17
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputResponse {
    /// The output version.
    pub version: B256,
    /// The output root hash.
    pub output_root: B256,
    /// A reference to the L2 block.
    pub block_ref: L2BlockInfo,
    /// The withdrawal storage root.
    pub withdrawal_storage_root: B256,
    /// The state root.
    pub state_root: B256,
    /// The status of the node sync.
    pub sync_status: SyncStatus,
}

/// The safe head response.
///
/// <https://github.com/ethereum-optimism/optimism/blob/77c91d09eaa44d2c53bec60eb89c5c55737bc325/op-service/eth/output.go#L19-L22>
/// Note: the optimism "eth.BlockID" type is number,hash <https://github.com/ethereum-optimism/optimism/blob/77c91d09eaa44d2c53bec60eb89c5c55737bc325/op-service/eth/id.go#L10-L13>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeHeadResponse {
    /// The L1 block.
    pub l1_block: BlockNumHash,
    /// The safe head.
    pub safe_head: BlockNumHash,
}
