//! Block Types

use alloy_consensus::Block;
use alloy_eips::{BlockNumHash, eip2718::Eip2718Error};
use alloy_primitives::B256;
use alloy_rpc_types_eth::Block as RpcBlock;
use derive_more::Display;
use op_alloy_rpc_types_engine::OpPayloadError;

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

impl<T> From<RpcBlock<T>> for BlockInfo {
    fn from(block: RpcBlock<T>) -> Self {
        Self {
            hash: block.header.hash_slow(),
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        }
    }
}

impl<T> From<&RpcBlock<T>> for BlockInfo {
    fn from(block: &RpcBlock<T>) -> Self {
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

/// An error that can occur when converting an OP [`Block`] to [`L2BlockInfo`].
#[derive(Debug, thiserror::Error)]
pub enum FromBlockError {
    /// The genesis block hash does not match the expected value.
    #[error("invalid genesis hash")]
    InvalidGenesisHash,
    /// The L2 block is missing the L1 info deposit transaction.
    #[error("L2 block is missing L1 info deposit transaction ({0})")]
    MissingL1InfoDeposit(B256),
    /// The first payload transaction has an unexpected type.
    #[error("first payload transaction has unexpected type: {0}")]
    UnexpectedTxType(u8),
    /// Failed to decode the first transaction into an OP transaction.
    #[error("failed to decode the first transaction into an OP transaction: {0}")]
    TxEnvelopeDecodeError(Eip2718Error),
    /// The first payload transaction is not a deposit transaction.
    #[error("first payload transaction is not a deposit transaction, type: {0}")]
    FirstTxNonDeposit(u8),
    /// Failed to convert [`OpExecutionPayload`] to [`OpBlock`].
    #[error(transparent)]
    OpPayload(#[from] OpPayloadError),
}

impl PartialEq<Self> for FromBlockError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::MissingL1InfoDeposit(a), Self::MissingL1InfoDeposit(b)) => a == b,
            (Self::UnexpectedTxType(a), Self::UnexpectedTxType(b))
            | (Self::FirstTxNonDeposit(a), Self::FirstTxNonDeposit(b)) => a == b,
            (Self::InvalidGenesisHash, Self::InvalidGenesisHash)
            | (Self::TxEnvelopeDecodeError(_), Self::TxEnvelopeDecodeError(_))
            | (Self::OpPayload(_), Self::OpPayload(_)) => true,
            _ => false,
        }
    }
}

impl From<Eip2718Error> for FromBlockError {
    fn from(value: Eip2718Error) -> Self {
        Self::TxEnvelopeDecodeError(value)
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
    use alloy_consensus::{Header, TxEnvelope};
    use alloy_primitives::b256;
    use op_alloy_consensus::OpTxEnvelope;

    use super::*;

    #[test]
    fn test_rpc_block_into_info() {
        let block: alloy_rpc_types_eth::Block<OpTxEnvelope> = alloy_rpc_types_eth::Block {
            header: alloy_rpc_types_eth::Header {
                hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
                inner: alloy_consensus::Header {
                    number: 1,
                    parent_hash: b256!(
                        "0202020202020202020202020202020202020202020202020202020202020202"
                    ),
                    timestamp: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let expected = BlockInfo {
            hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
            number: 1,
            parent_hash: b256!("0202020202020202020202020202020202020202020202020202020202020202"),
            timestamp: 1,
        };
        let block = block.into_consensus();
        assert_eq!(BlockInfo::from(block), expected);
    }

    #[test]
    fn test_from_block_error_partial_eq() {
        assert_eq!(FromBlockError::InvalidGenesisHash, FromBlockError::InvalidGenesisHash);
        assert_eq!(
            FromBlockError::MissingL1InfoDeposit(b256!(
                "04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"
            )),
            FromBlockError::MissingL1InfoDeposit(b256!(
                "04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"
            )),
        );
        assert_eq!(FromBlockError::UnexpectedTxType(1), FromBlockError::UnexpectedTxType(1));
        assert_eq!(
            FromBlockError::TxEnvelopeDecodeError(Eip2718Error::UnexpectedType(1)),
            FromBlockError::TxEnvelopeDecodeError(Eip2718Error::UnexpectedType(1))
        );
        assert_eq!(FromBlockError::FirstTxNonDeposit(1), FromBlockError::FirstTxNonDeposit(1));
    }

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
    fn test_deserialize_block_info_with_different_values() {
        let block_info = BlockInfo {
            hash: B256::from([0xaa; 32]),
            number: 999,
            parent_hash: B256::from([0xbb; 32]),
            timestamp: 1_700_000_000,
        };

        let json = r#"{
            "hash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "number": 999,
            "parentHash": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "timestamp": 1700000000
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
