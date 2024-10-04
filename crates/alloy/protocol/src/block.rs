//! Block Types for Optimism.

use crate::block_info::{DecodeError, L1BlockInfoTx};
use alloy_eips::{eip2718::Eip2718Error, BlockNumHash};
use alloy_primitives::B256;
use op_alloy_consensus::{OpBlock, OpTxEnvelope, OpTxType};
use op_alloy_genesis::ChainGenesis;

/// Block Header Info
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Default)]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BlockInfo {
    /// The block hash
    pub hash: B256,
    /// The block number
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub number: u64,
    /// The parent block hash
    pub parent_hash: B256,
    /// The block timestamp
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub timestamp: u64,
}

impl BlockInfo {
    /// Instantiates a new [BlockInfo].
    pub const fn new(hash: B256, number: u64, parent_hash: B256, timestamp: u64) -> Self {
        Self { hash, number, parent_hash, timestamp }
    }

    /// Returns the block ID.
    pub const fn id(&self) -> BlockNumHash {
        BlockNumHash { hash: self.hash, number: self.number }
    }
}

impl From<OpBlock> for BlockInfo {
    fn from(block: OpBlock) -> Self {
        Self::from(&block)
    }
}

impl From<&OpBlock> for BlockInfo {
    fn from(block: &OpBlock) -> Self {
        Self {
            hash: block.header.hash_slow(),
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        }
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
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct L2BlockInfo {
    /// The base [BlockInfo]
    pub block_info: BlockInfo,
    /// The L1 origin [BlockNumHash]
    pub l1_origin: BlockNumHash,
    /// The sequence number of the L2 block
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub seq_num: u64,
}

#[cfg(any(test, feature = "arbitrary"))]
impl arbitrary::Arbitrary<'_> for L2BlockInfo {
    fn arbitrary(g: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(Self {
            block_info: g.arbitrary()?,
            l1_origin: BlockNumHash { number: g.arbitrary()?, hash: g.arbitrary()? },
            seq_num: g.arbitrary()?,
        })
    }
}

/// An error that can occur when converting an [OpBlock] to an [L2BlockInfo].
#[derive(Debug, derive_more::Display)]
pub enum FromBlockError {
    /// The genesis block hash does not match the expected value.
    #[display("Invalid genesis hash")]
    InvalidGenesisHash,
    /// The L2 block is missing the L1 info deposit transaction.
    #[display("L2 block is missing L1 info deposit transaction ({_0})")]
    MissingL1InfoDeposit(B256),
    /// The first payload transaction has an unexpected type.
    #[display("First payload transaction has unexpected type: {_0}")]
    UnexpectedTxType(u8),
    /// Failed to decode the first transaction into an [OpTxEnvelope].
    #[display("Failed to decode the first transaction into an OpTxEnvelope: {_0}")]
    TxEnvelopeDecodeError(Eip2718Error),
    /// The first payload transaction is not a deposit transaction.
    #[display("First payload transaction is not a deposit transaction, type: {_0}")]
    FirstTxNonDeposit(u8),
    /// Failed to decode the [L1BlockInfoTx] from the deposit transaction.
    #[display("Failed to decode the L1BlockInfoTx from the deposit transaction: {_0}")]
    BlockInfoDecodeError(DecodeError),
}

// Since `Eip2718Error` uses an msrv prior to rust `1.81`, the `core::error::Error` type
// is not stabalized and `Eip2718Error` only implements `std::error::Error` and not
// `core::error::Error`. So we need to implement `std::error::Error` to provide the `Eip2718Error`
// as a source when the `std` feature is enabled.
#[cfg(feature = "std")]
impl std::error::Error for FromBlockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TxEnvelopeDecodeError(err) => Some(err),
            Self::BlockInfoDecodeError(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(not(feature = "std"))]
impl core::error::Error for FromBlockError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::BlockInfoDecodeError(err) => Some(err),
            _ => None,
        }
    }
}

impl L2BlockInfo {
    /// Instantiates a new [L2BlockInfo].
    pub const fn new(block_info: BlockInfo, l1_origin: BlockNumHash, seq_num: u64) -> Self {
        Self { block_info, l1_origin, seq_num }
    }

    /// Constructs an [L2BlockInfo] from a given [OpBlock] and [ChainGenesis].
    pub fn from_block_and_genesis(
        block: &OpBlock,
        genesis: &ChainGenesis,
    ) -> Result<Self, FromBlockError> {
        let block_info = BlockInfo::from(block);

        let (l1_origin, sequence_number) = if block_info.number == genesis.l2.number {
            if block_info.hash != genesis.l2.hash {
                return Err(FromBlockError::InvalidGenesisHash);
            }
            (genesis.l1, 0)
        } else {
            if block.body.transactions.is_empty() {
                return Err(FromBlockError::MissingL1InfoDeposit(block_info.hash));
            }

            let tx = &block.body.transactions[0];
            if tx.tx_type() != OpTxType::Deposit {
                return Err(FromBlockError::UnexpectedTxType(tx.tx_type().into()));
            }

            let OpTxEnvelope::Deposit(tx) = tx else {
                return Err(FromBlockError::FirstTxNonDeposit(tx.tx_type().into()));
            };

            let l1_info = L1BlockInfoTx::decode_calldata(tx.input.as_ref())
                .map_err(FromBlockError::BlockInfoDecodeError)?;
            (l1_info.id(), l1_info.sequence_number())
        };

        Ok(Self { block_info, l1_origin, seq_num: sequence_number })
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;
    use arbitrary::Arbitrary;
    use rand::Rng;

    #[test]
    fn test_arbitrary_block_info() {
        let mut bytes = [0u8; 1024];
        rand::thread_rng().fill(bytes.as_mut_slice());
        BlockInfo::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    fn test_arbitrary_l2_block_info() {
        let mut bytes = [0u8; 1024];
        rand::thread_rng().fill(bytes.as_mut_slice());
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
    fn test_deserialize_block_info_with_hex() {
        let block_info = BlockInfo {
            hash: B256::from([1; 32]),
            number: 1,
            parent_hash: B256::from([2; 32]),
            timestamp: 1,
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
                number: 1,
                parent_hash: B256::from([2; 32]),
                timestamp: 1,
            },
            l1_origin: BlockNumHash { hash: B256::from([3; 32]), number: 2 },
            seq_num: 3,
        };

        let json = r#"{
            "blockInfo": {
                "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
                "number": 1,
                "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
                "timestamp": 1
            },
            "l1Origin": {
                "hash": "0x0303030303030303030303030303030303030303030303030303030303030303",
                "number": 2
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
                number: 1,
                parent_hash: B256::from([2; 32]),
                timestamp: 1,
            },
            l1_origin: BlockNumHash { hash: B256::from([3; 32]), number: 2 },
            seq_num: 3,
        };

        let json = r#"{
            "blockInfo": {
                "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
                "number": "0x1",
                "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
                "timestamp": "0x1"
            },
            "l1Origin": {
                "hash": "0x0303030303030303030303030303030303030303030303030303030303030303",
                "number": 2
            },
            "seqNum": "0x3"
        }"#;

        let deserialized: L2BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, l2_block_info);
    }
}
