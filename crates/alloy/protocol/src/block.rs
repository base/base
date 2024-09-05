//! Block Types for Optimism.

use alloy_primitives::{B256, U64};
use superchain_primitives::BlockID;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Block Header Info
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Default)]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BlockInfo {
    /// The block hash
    pub hash: B256,
    /// The block number
    pub number: U64,
    /// The parent block hash
    pub parent_hash: B256,
    /// The block timestamp
    pub timestamp: U64,
}

impl BlockInfo {
    /// Instantiates a new [BlockInfo].
    pub const fn new(hash: B256, number: U64, parent_hash: B256, timestamp: U64) -> Self {
        Self { hash, number, parent_hash, timestamp }
    }

    /// Returns the block ID.
    pub fn id(&self) -> BlockID {
        BlockID { hash: self.hash, number: self.number.try_into().expect("U64 conversion to u64") }
    }
}

impl core::fmt::Display for BlockInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "BlockInfo {{ hash: {}, number: {}, parent_hash: {}, timestamp: {} }}",
            self.hash, self.number, self.parent_hash, self.timestamp
        )
    }
}

/// L2 Block Header Info
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Default)]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct L2BlockInfo {
    /// The base [BlockInfo]
    pub block_info: BlockInfo,
    /// The L1 origin [BlockID]
    pub l1_origin: BlockID,
    /// The sequence number of the L2 block
    pub seq_num: U64,
}

impl L2BlockInfo {
    /// Instantiates a new [L2BlockInfo].
    pub const fn new(block_info: BlockInfo, l1_origin: BlockID, seq_num: U64) -> Self {
        Self { block_info, l1_origin, seq_num }
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;

    #[test]
    fn test_block_id_bounds() {
        let block_info = BlockInfo {
            hash: B256::from([1; 32]),
            number: U64::from(0),
            parent_hash: B256::from([2; 32]),
            timestamp: U64::from(1),
        };
        let expected = BlockID { hash: B256::from([1; 32]), number: 0 };
        assert_eq!(block_info.id(), expected);

        let block_info = BlockInfo {
            hash: B256::from([1; 32]),
            number: U64::MAX,
            parent_hash: B256::from([2; 32]),
            timestamp: U64::from(1),
        };
        let expected = BlockID { hash: B256::from([1; 32]), number: u64::MAX };
        assert_eq!(block_info.id(), expected);
    }

    #[test]
    fn test_deserialize_block_info() {
        let block_info = BlockInfo {
            hash: B256::from([1; 32]),
            number: U64::from(1),
            parent_hash: B256::from([2; 32]),
            timestamp: U64::from(1),
        };

        let json = r#"{
            "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
            "number": 1,
            "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
            "timestamp": 1
        }"#;

        let deserialized: BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, block_info);
    }

    #[test]
    fn test_deserialize_block_info_with_hex() {
        let block_info = BlockInfo {
            hash: B256::from([1; 32]),
            number: U64::from(1),
            parent_hash: B256::from([2; 32]),
            timestamp: U64::from(1),
        };

        let json = r#"{
            "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
            "number": "0x1",
            "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
            "timestamp": "0x1"
        }"#;

        let deserialized: BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, block_info);
    }

    #[test]
    fn test_deserialize_l2_block_info() {
        let l2_block_info = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::from([1; 32]),
                number: U64::from(1),
                parent_hash: B256::from([2; 32]),
                timestamp: U64::from(1),
            },
            l1_origin: BlockID { hash: B256::from([3; 32]), number: 2 },
            seq_num: U64::from(3),
        };

        let json = r#"{
            "blockInfo": {
                "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
                "number": 1,
                "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
                "timestamp": 1
            },
            "l1Origin": {
                "Hash": "0x0303030303030303030303030303030303030303030303030303030303030303",
                "Number": 2
            },
            "seqNum": 3
        }"#;

        let deserialized: L2BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, l2_block_info);
    }

    #[test]
    fn test_deserialize_l2_block_info_hex() {
        let l2_block_info = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::from([1; 32]),
                number: U64::from(1),
                parent_hash: B256::from([2; 32]),
                timestamp: U64::from(1),
            },
            l1_origin: BlockID { hash: B256::from([3; 32]), number: 2 },
            seq_num: U64::from(3),
        };

        let json = r#"{
            "blockInfo": {
                "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
                "number": "0x1",
                "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
                "timestamp": "0x1"
            },
            "l1Origin": {
                "Hash": "0x0303030303030303030303030303030303030303030303030303030303030303",
                "Number": 2
            },
            "seqNum": "0x3"
        }"#;

        let deserialized: L2BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, l2_block_info);
    }
}
