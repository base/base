//! L1 and L2 block references shared by rollup protocols and clients.

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use derive_more::Display;

use crate::Block;

/// Block Header Info
#[derive(Debug, Clone, Display, Copy, Eq, Hash, PartialEq, Default)]
#[display(
    "BlockInfo {{ hash: {hash}, number: {number}, parent_hash: {parent_hash}, timestamp: {timestamp} }}"
)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BlockInfo {
    /// The block hash
    pub hash: B256,
    /// The block number
    pub number: u64,
    /// The parent block hash
    pub parent_hash: B256,
    /// The block timestamp
    pub timestamp: u64,
}

impl BlockInfo {
    /// Instantiates a new [`BlockInfo`].
    pub const fn new(hash: B256, number: u64, parent_hash: B256, timestamp: u64) -> Self {
        Self { hash, number, parent_hash, timestamp }
    }

    /// Returns the block ID.
    pub const fn id(&self) -> BlockNumHash {
        BlockNumHash { hash: self.hash, number: self.number }
    }

    /// Returns `true` if this [`BlockInfo`] is the direct parent of the given block.
    pub fn is_parent_of(&self, block: &Self) -> bool {
        self.number + 1 == block.number && self.hash == block.parent_hash
    }
}

impl<T> From<Block<T>> for BlockInfo {
    fn from(block: Block<T>) -> Self {
        Self::from(&block)
    }
}

impl<T> From<&Block<T>> for BlockInfo {
    fn from(block: &Block<T>) -> Self {
        Self {
            hash: block.header.hash_slow(),
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        }
    }
}

/// L2 Block Header Info
#[derive(Debug, Display, Clone, Copy, Hash, Eq, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[display(
    "L2BlockInfo {{ block_info: {block_info}, l1_origin: {l1_origin:?}, seq_num: {seq_num} }}"
)]
pub struct L2BlockInfo {
    /// The base [`BlockInfo`]
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub block_info: BlockInfo,
    /// The L1 origin [`BlockNumHash`]
    #[cfg_attr(feature = "serde", serde(rename = "l1origin", alias = "l1Origin"))]
    pub l1_origin: BlockNumHash,
    /// The sequence number of the L2 block
    #[cfg_attr(feature = "serde", serde(rename = "sequenceNumber", alias = "seqNum"))]
    pub seq_num: u64,
}

impl L2BlockInfo {
    /// Returns the block hash.
    pub const fn hash(&self) -> B256 {
        self.block_info.hash
    }
}

#[cfg(feature = "arbitrary")]
impl arbitrary::Arbitrary<'_> for L2BlockInfo {
    fn arbitrary(g: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(Self {
            block_info: g.arbitrary()?,
            l1_origin: BlockNumHash { number: g.arbitrary()?, hash: g.arbitrary()? },
            seq_num: g.arbitrary()?,
        })
    }
}

impl L2BlockInfo {
    /// Instantiates a new [`L2BlockInfo`].
    pub const fn new(block_info: BlockInfo, l1_origin: BlockNumHash, seq_num: u64) -> Self {
        Self { block_info, l1_origin, seq_num }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use alloy_primitives::b256;

    use super::*;
    use crate::{Header, TxEnvelope};

    #[test]
    fn test_from_block() {
        let block: Block<TxEnvelope, Header> = Block {
            header: Header {
                number: 1,
                parent_hash: B256::from([2; 32]),
                timestamp: 1,
                ..Default::default()
            },
            body: Default::default(),
        };
        let block_info = BlockInfo::from(&block);
        assert_eq!(
            block_info,
            BlockInfo {
                hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
                number: block.header.number,
                parent_hash: block.header.parent_hash,
                timestamp: block.header.timestamp,
            }
        );
    }

    #[test]
    fn test_block_info_display() {
        let hash = B256::from([1; 32]);
        let parent_hash = B256::from([2; 32]);
        let block_info = BlockInfo::new(hash, 1, parent_hash, 1);
        assert_eq!(
            block_info.to_string(),
            "BlockInfo { hash: 0x0101010101010101010101010101010101010101010101010101010101010101, number: 1, parent_hash: 0x0202020202020202020202020202020202020202020202020202020202020202, timestamp: 1 }"
        );
    }

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_block_info() {
        use arbitrary::Arbitrary;
        use rand::Rng;
        let mut bytes = [0u8; 1024];
        rand::rng().fill(bytes.as_mut_slice());
        BlockInfo::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_l2_block_info() {
        use arbitrary::Arbitrary;
        use rand::Rng;
        let mut bytes = [0u8; 1024];
        rand::rng().fill(bytes.as_mut_slice());
        L2BlockInfo::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    fn test_block_id_bounds() {
        let block_info = BlockInfo {
            hash: B256::from([1; 32]),
            number: 0,
            parent_hash: B256::from([2; 32]),
            timestamp: 1,
        };
        let expected = BlockNumHash { hash: B256::from([1; 32]), number: 0 };
        assert_eq!(block_info.id(), expected);

        let block_info = BlockInfo {
            hash: B256::from([1; 32]),
            number: u64::MAX,
            parent_hash: B256::from([2; 32]),
            timestamp: 1,
        };
        let expected = BlockNumHash { hash: B256::from([1; 32]), number: u64::MAX };
        assert_eq!(block_info.id(), expected);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_block_info() {
        let block_info = BlockInfo {
            hash: B256::from([1; 32]),
            number: 1,
            parent_hash: B256::from([2; 32]),
            timestamp: 1,
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
    #[cfg(feature = "serde")]
    fn test_deserialize_l2_block_info() {
        let l2_block_info = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::from([1; 32]),
                number: 1,
                parent_hash: B256::from([2; 32]),
                timestamp: 1,
            },
            l1_origin: BlockNumHash { hash: B256::from([3; 32]), number: 2 },
            seq_num: 3,
        };

        let json = r#"{
            "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
            "number": 1,
            "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
            "timestamp": 1,
            "l1origin": {
                "hash": "0x0303030303030303030303030303030303030303030303030303030303030303",
                "number": 2
            },
            "sequenceNumber": 3
        }"#;

        let deserialized: L2BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, l2_block_info);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_l2_block_info_hex() {
        let l2_block_info = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::from([1; 32]),
                number: 1,
                parent_hash: B256::from([2; 32]),
                timestamp: 1,
            },
            l1_origin: BlockNumHash { hash: B256::from([3; 32]), number: 2 },
            seq_num: 3,
        };

        let json = r#"{
            "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
            "number": 1,
            "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
            "timestamp": 1,
            "l1origin": {
                "hash": "0x0303030303030303030303030303030303030303030303030303030303030303",
                "number": 2
            },
            "sequenceNumber": 3
        }"#;

        let deserialized: L2BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, l2_block_info);
    }

    #[test]
    fn test_is_parent_of() {
        let parent = BlockInfo {
            hash: B256::from([1u8; 32]),
            number: 10,
            parent_hash: B256::from([0u8; 32]),
            timestamp: 1000,
        };
        let child = BlockInfo {
            hash: B256::from([2u8; 32]),
            number: 11,
            parent_hash: parent.hash,
            timestamp: 1010,
        };
        let unrelated = BlockInfo {
            hash: B256::from([3u8; 32]),
            number: 12,
            parent_hash: B256::from([9u8; 32]),
            timestamp: 1020,
        };

        assert!(parent.is_parent_of(&child));
        assert!(!child.is_parent_of(&parent));
        assert!(!parent.is_parent_of(&unrelated));
    }
}
