//! Custom RPC response types for Optimism rollup nodes.

use alloy_primitives::{B256, Bytes};
use serde::{Deserialize, Serialize};

/// L1 block reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// L1 origin reference.
    pub l1origin: L1BlockRef,
    /// Sequence number within the epoch.
    pub sequence_number: u64,
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

/// Genesis configuration for rollup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenesisConfig {
    /// L1 genesis block reference.
    pub l1: L1BlockRef,
    /// L2 genesis block reference.
    pub l2: L2BlockRef,
    /// L2 genesis timestamp.
    pub l2_time: u64,
    /// System config.
    #[serde(default)]
    pub system_config: Option<serde_json::Value>,
}

/// Rollup configuration from `optimism_rollupConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollupConfig {
    /// Genesis configuration.
    pub genesis: GenesisConfig,
    /// Block time in seconds.
    pub block_time: u64,
    /// Maximum sequencer drift.
    pub max_sequencer_drift: u64,
    /// Sequencer window size.
    pub seq_window_size: u64,
    /// Channel timeout.
    pub channel_timeout: u64,
    /// L1 chain ID.
    pub l1_chain_id: u64,
    /// L2 chain ID.
    pub l2_chain_id: u64,
    /// Batch inbox address (hex string).
    #[serde(default)]
    pub batch_inbox_address: Option<String>,
    /// Deposit contract address (hex string).
    #[serde(default)]
    pub deposit_contract_address: Option<String>,
    /// L1 system config address (hex string).
    #[serde(default)]
    pub l1_system_config_address: Option<String>,
}

/// Reth-specific execution witness format.
///
/// Reth returns arrays instead of maps for codes and state.
/// This needs to be converted to the standard [`ExecutionWitness`] format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RethExecutionWitness {
    /// State trie node preimages.
    #[serde(default)]
    pub state: Vec<Bytes>,
    /// Contract bytecodes.
    #[serde(default)]
    pub codes: Vec<Bytes>,
    /// MPT keys/paths.
    #[serde(default)]
    pub keys: Vec<Bytes>,
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
                    "number": 100,
                    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "timestamp": 1234567890
                },
                "sequenceNumber": 0
            },
            "safe_l2": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000003",
                "number": 200,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000002",
                "timestamp": 1234567900,
                "l1origin": {
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                    "number": 100,
                    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "timestamp": 1234567890
                },
                "sequenceNumber": 0
            },
            "finalized_l2": {
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000003",
                "number": 200,
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000002",
                "timestamp": 1234567900,
                "l1origin": {
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                    "number": 100,
                    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "timestamp": 1234567890
                },
                "sequenceNumber": 0
            }
        }"#;

        let status: SyncStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.current_l1.number, 100);
        assert_eq!(status.head_l1.number, 101);
        assert_eq!(status.safe_l2.number, 200);
    }

    #[test]
    fn test_reth_witness_deserialize() {
        let json = r#"{
            "state": ["0x1234", "0x5678"],
            "codes": ["0xabcd"],
            "keys": []
        }"#;

        let witness: RethExecutionWitness = serde_json::from_str(json).unwrap();
        assert_eq!(witness.state.len(), 2);
        assert_eq!(witness.codes.len(), 1);
        assert!(witness.keys.is_empty());
    }
}
