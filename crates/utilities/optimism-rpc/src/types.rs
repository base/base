//! RPC response types for Optimism rollup nodes.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// L1 block reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L1BlockRef {
    /// Block hash.
    pub hash: B256,
    /// Block number.
    pub number: u64,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Block timestamp.
    pub timestamp: u64,
}

/// L2 block reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2BlockRef {
    /// Block hash.
    pub hash: B256,
    /// Block number.
    pub number: u64,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Block timestamp.
    pub timestamp: u64,
    /// L1 origin reference (only hash and number are provided by the RPC).
    pub l1origin: L1BlockId,
    /// Sequence number within the epoch.
    pub sequence_number: u64,
}

/// Minimal L1 block identifier containing only hash and number.
///
/// Used for `L2BlockRef`.l1origin and genesis config where the full `L1BlockRef` fields
/// (parentHash, timestamp) are not provided by the RPC response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L1BlockId {
    /// Block hash.
    pub hash: B256,
    /// Block number.
    pub number: u64,
}

/// Minimal L2 block reference for genesis config.
///
/// Unlike `L2BlockRef`, this only contains hash and number as returned by `optimism_rollupConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenesisL2BlockRef {
    /// Block hash.
    pub hash: B256,
    /// Block number.
    pub number: u64,
}

/// Output root at a specific L2 block, from `optimism_outputAtBlock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputAtBlock {
    /// The output root at this block.
    pub output_root: B256,
    /// The L2 block reference.
    pub block_ref: L2BlockRef,
}

/// Sync status from `optimism_syncStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SyncStatus {
    /// Current L1 block reference.
    pub current_l1: L1BlockRef,
    /// Current L1 finalized block.
    #[serde(default)]
    pub current_l1_finalized: Option<L1BlockRef>,
    /// Head L1 block reference.
    pub head_l1: L1BlockRef,
    /// Safe L1 block reference.
    pub safe_l1: L1BlockRef,
    /// Finalized L1 block reference.
    pub finalized_l1: L1BlockRef,
    /// Unsafe L2 block reference.
    pub unsafe_l2: L2BlockRef,
    /// Safe L2 block reference.
    pub safe_l2: L2BlockRef,
    /// Finalized L2 block reference.
    pub finalized_l2: L2BlockRef,
    /// Pending safe L2 block reference.
    #[serde(default)]
    pub pending_safe_l2: Option<L2BlockRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_status_deserialize() {
        let json = r#"{
            "current_l1": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                "number": 100,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "timestamp": 1234567890
            },
            "head_l1": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000002",
                "number": 101,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                "timestamp": 1234567902
            },
            "safe_l1": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                "number": 100,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "timestamp": 1234567890
            },
            "finalized_l1": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                "number": 100,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "timestamp": 1234567890
            },
            "unsafe_l2": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000003",
                "number": 200,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000002",
                "timestamp": 1234567900,
                "l1origin": {
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                    "number": 100
                },
                "sequenceNumber": 0
            },
            "safe_l2": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000004",
                "number": 199,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000003",
                "timestamp": 1234567898,
                "l1origin": {
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                    "number": 100
                },
                "sequenceNumber": 1
            },
            "finalized_l2": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000005",
                "number": 198,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000004",
                "timestamp": 1234567896,
                "l1origin": {
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                    "number": 100
                },
                "sequenceNumber": 2
            }
        }"#;

        let status: SyncStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.current_l1.number, 100);
        assert_eq!(status.unsafe_l2.l1origin.number, 100);
        assert!(status.current_l1_finalized.is_none());
    }
}
