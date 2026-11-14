//! Block Types for Base.

use alloc::vec::Vec;

use alloy_consensus::{Block, Header, Transaction, Typed2718};
use alloy_eips::{
    BlockNumHash,
    eip2718::{Decodable2718, Eip2718Error},
    eip7685::EMPTY_REQUESTS_HASH,
};
use alloy_primitives::{B256, Bytes, Sealed};
use alloy_rpc_types_engine::{CancunPayloadFields, PraguePayloadFields};
use alloy_rpc_types_eth::Block as RpcBlock;
use base_common_consensus::{BaseBlock, BaseTxEnvelope, DEPOSIT_TX_TYPE_ID, TxDeposit};
use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig};
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadSidecar, BasePayloadError,
};
use derive_more::Display;

use crate::{BaseBlockConversionError, DecodeError, L1BlockInfoTx, to_system_config};

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

    /// Returns the [`BlockInfo`] of the L2 genesis block described by `genesis`, whose parent
    /// hash is zero.
    pub const fn from_l2_genesis(genesis: &ChainGenesis) -> Self {
        Self::new(genesis.l2.hash, genesis.l2.number, B256::ZERO, genesis.l2_time)
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

/// An error that can occur when converting a [`Block`] to [`L2BlockInfo`].
#[derive(Debug, thiserror::Error)]
pub enum FromBlockError {
    /// The genesis block hash does not match the expected value.
    #[error("Invalid genesis hash")]
    InvalidGenesisHash,
    /// The L2 block is missing the L1 info deposit transaction.
    #[error("L2 block is missing L1 info deposit transaction ({0})")]
    MissingL1InfoDeposit(B256),
    /// The first payload transaction has an unexpected type.
    #[error("First payload transaction has unexpected type: {0}")]
    UnexpectedTxType(u8),
    /// Failed to decode the first transaction into a Base transaction.
    #[error("Failed to decode the first transaction into a Base transaction: {0}")]
    TxEnvelopeDecodeError(Eip2718Error),
    /// The first payload transaction is not a deposit transaction.
    #[error("First payload transaction is not a deposit transaction, type: {0}")]
    FirstTxNonDeposit(u8),
    /// Failed to decode the [`L1BlockInfoTx`] from the deposit transaction.
    #[error("Failed to decode the L1BlockInfoTx from the deposit transaction: {0}")]
    BlockInfoDecodeError(#[from] DecodeError),
    /// Failed to convert [`BaseExecutionPayload`] to [`BaseBlock`].
    #[error(transparent)]
    BasePayload(#[from] BasePayloadError),
}

impl PartialEq<Self> for FromBlockError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InvalidGenesisHash, Self::InvalidGenesisHash)
            | (Self::TxEnvelopeDecodeError(_), Self::TxEnvelopeDecodeError(_)) => true,
            (Self::MissingL1InfoDeposit(a), Self::MissingL1InfoDeposit(b)) => a == b,
            (Self::UnexpectedTxType(a), Self::UnexpectedTxType(b))
            | (Self::FirstTxNonDeposit(a), Self::FirstTxNonDeposit(b)) => *a == *b,
            (Self::BlockInfoDecodeError(a), Self::BlockInfoDecodeError(b)) => a.eq(b),
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

    /// Returns the [`L2BlockInfo`] of the L2 genesis block described by `genesis`, with the
    /// genesis L1 block as L1 origin and sequence number zero.
    pub const fn from_l2_genesis(genesis: &ChainGenesis) -> Self {
        Self::new(BlockInfo::from_l2_genesis(genesis), genesis.l1, 0)
    }

    /// Constructs an [`L2BlockInfo`] from a given Base [`Block`] and [`ChainGenesis`].
    pub fn from_block_and_genesis<T: AsRef<BaseTxEnvelope>>(
        block: &Block<T>,
        genesis: &ChainGenesis,
    ) -> Result<Self, FromBlockError> {
        let first_tx = block.body.transactions.first().map(|tx| {
            let tx = tx.as_ref();
            tx.as_deposit().map(Sealed::inner).ok_or_else(|| tx.tx_type() as u8)
        });
        Self::from_block_info_and_first_tx(BlockInfo::from(block), first_tx, genesis)
    }

    /// Constructs an [`L2BlockInfo`] from a sealed [`Header`] and the raw EIP-2718 encoded first
    /// transaction of the block.
    ///
    /// Equivalent to [`Self::from_block_and_genesis`], but only the L1-info deposit is decoded
    /// and the block hash is taken from the seal instead of being recomputed.
    ///
    /// The caller must ensure that `first_tx` is the first transaction committed to by
    /// `header.transactions_root`, and that the seal is the header's true hash.
    pub fn from_header_and_first_tx(
        header: &Sealed<Header>,
        first_tx: Option<&Bytes>,
        genesis: &ChainGenesis,
    ) -> Result<Self, FromBlockError> {
        debug_assert_eq!(header.hash(), header.hash_slow(), "seal must be the header's true hash");
        let block_info =
            BlockInfo::new(header.hash(), header.number, header.parent_hash, header.timestamp);
        // The genesis block carries no L1-info deposit, so its first transaction is not inspected.
        // Deposits decode as a bare `TxDeposit`: sealing them in a `BaseTxEnvelope` would hash
        // the transaction on every block for nothing.
        let first_tx = match first_tx {
            Some(tx) if header.number != genesis.l2.number => {
                Some(if tx.first() == Some(&DEPOSIT_TX_TYPE_ID) {
                    Ok(TxDeposit::decode_2718_exact(tx.as_ref())?)
                } else {
                    Err(BaseTxEnvelope::decode_2718_exact(tx.as_ref())?.tx_type() as u8)
                })
            }
            _ => None,
        };
        Self::from_block_info_and_first_tx(
            block_info,
            first_tx.as_ref().map(|tx| tx.as_ref().map_err(|&ty| ty)),
            genesis,
        )
    }

    /// `first_tx` is the block's first transaction: its deposit, or its type if not a deposit.
    fn from_block_info_and_first_tx(
        block_info: BlockInfo,
        first_tx: Option<Result<&TxDeposit, u8>>,
        genesis: &ChainGenesis,
    ) -> Result<Self, FromBlockError> {
        let (l1_origin, sequence_number) = if block_info.number == genesis.l2.number {
            if block_info.hash != genesis.l2.hash {
                return Err(FromBlockError::InvalidGenesisHash);
            }
            (genesis.l1, 0)
        } else {
            let Some(tx) = first_tx else {
                return Err(FromBlockError::MissingL1InfoDeposit(block_info.hash));
            };
            let tx = tx.map_err(FromBlockError::FirstTxNonDeposit)?;

            let l1_info = L1BlockInfoTx::decode_calldata(tx.input.as_ref())
                .map_err(FromBlockError::BlockInfoDecodeError)?;
            (l1_info.id(), l1_info.sequence_number())
        };

        Ok(Self { block_info, l1_origin, seq_num: sequence_number })
    }

    /// Constructs an [`L2BlockInfo`] From a given [`BaseExecutionPayload`] and [`ChainGenesis`].
    pub fn from_payload_and_genesis(
        payload: BaseExecutionPayload,
        parent_beacon_block_root: Option<B256>,
        genesis: &ChainGenesis,
    ) -> Result<Self, FromBlockError> {
        let block: BaseBlock = match payload {
            BaseExecutionPayload::V4(_) => {
                let sidecar = BaseExecutionPayloadSidecar::v4(
                    CancunPayloadFields::new(
                        parent_beacon_block_root.unwrap_or_default(),
                        Vec::new(),
                    ),
                    PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
                );
                payload.try_into_block_with_sidecar(&sidecar)?
            }
            BaseExecutionPayload::V3(_) => {
                let sidecar = BaseExecutionPayloadSidecar::v3(CancunPayloadFields::new(
                    parent_beacon_block_root.unwrap_or_default(),
                    Vec::new(),
                ));
                payload.try_into_block_with_sidecar(&sidecar)?
            }
            _ => payload.try_into_block()?,
        };
        Self::from_block_and_genesis(&block, genesis)
    }
}

/// Metadata decoded from an L2 block's L1-info deposit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L2BlockMetadata {
    /// L2 block identity, L1 origin, and sequence number.
    pub l2_block_info: L2BlockInfo,
    /// L1-info deposit decoded from transaction zero.
    pub l1_info: L1BlockInfoTx,
    /// Effective system configuration at the block.
    pub system_config: SystemConfig,
}

impl L2BlockMetadata {
    /// Decodes metadata from a block whose body starts with its L1-info deposit.
    ///
    /// Unlike [`L2BlockInfo::from_block_and_genesis`], this never substitutes genesis metadata,
    /// so the genesis block and blocks with missing bodies are rejected.
    pub fn from_block(
        block: &BaseBlock,
        rollup_config: &RollupConfig,
    ) -> Result<Self, BaseBlockConversionError> {
        let block_info = BlockInfo::from(block);
        let first_tx = block
            .body
            .transactions
            .first()
            .ok_or(BaseBlockConversionError::EmptyTransactions(block_info.hash))?;
        let deposit = first_tx
            .as_deposit()
            .ok_or_else(|| BaseBlockConversionError::InvalidTxType(first_tx.ty()))?;
        let l1_info = L1BlockInfoTx::decode_calldata(deposit.input().as_ref())?;
        let system_config = to_system_config(block, rollup_config)?;
        Ok(Self {
            l2_block_info: L2BlockInfo::new(block_info, l1_info.id(), l1_info.sequence_number()),
            l1_info,
            system_config,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use alloy_consensus::{BlockBody, Signed, TxEip1559, TxEnvelope, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Signature, U256, address, b256, bytes};
    use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};
    use rstest::rstest;

    use super::*;
    use crate::{BaseTimeUpdateTx, L1BlockInfoJovian, test_utils::RAW_BEDROCK_INFO_TX};

    #[test]
    fn test_rpc_block_into_info() {
        let block: alloy_rpc_types_eth::Block<BaseTxEnvelope> = alloy_rpc_types_eth::Block {
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
    fn test_from_block_and_genesis() {
        let genesis = ChainGenesis {
            l1: BlockNumHash { hash: B256::from([4; 32]), number: 2 },
            l2: BlockNumHash { hash: B256::from([5; 32]), number: 1 },
            ..Default::default()
        };
        let tx_env = alloy_rpc_types_eth::Transaction {
            inner: alloy_consensus::transaction::Recovered::new_unchecked(
                base_common_consensus::BaseTxEnvelope::Deposit(alloy_primitives::Sealed::new(
                    base_common_consensus::TxDeposit {
                        input: alloy_primitives::Bytes::from(&RAW_BEDROCK_INFO_TX),
                        ..Default::default()
                    },
                )),
                Default::default(),
            ),
            block_hash: None,
            block_number: Some(1),
            block_timestamp: None,
            effective_gas_price: Some(1),
            transaction_index: Some(0),
        };
        let block: alloy_rpc_types_eth::Block<base_common_rpc_types::Transaction> =
            alloy_rpc_types_eth::Block {
                header: alloy_rpc_types_eth::Header {
                    hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
                    inner: alloy_consensus::Header {
                        number: 3,
                        parent_hash: b256!(
                            "0202020202020202020202020202020202020202020202020202020202020202"
                        ),
                        timestamp: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                transactions: alloy_rpc_types_eth::BlockTransactions::Full(vec![
                    base_common_rpc_types::Transaction {
                        inner: tx_env,
                        block_timestamp_ms: None,
                        deposit_nonce: None,
                        deposit_receipt_version: None,
                    },
                ]),
                ..Default::default()
            };
        let expected = L2BlockInfo {
            block_info: BlockInfo {
                hash: b256!("e65ecd961cee8e4d2d6e1d424116f6fe9a794df0244578b6d5860a3d2dfcd97e"),
                number: 3,
                parent_hash: b256!(
                    "0202020202020202020202020202020202020202020202020202020202020202"
                ),
                timestamp: 1,
            },
            l1_origin: BlockNumHash {
                hash: b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc"),
                number: 18334955,
            },
            seq_num: 4,
        };
        let block = block.into_consensus();
        let derived = L2BlockInfo::from_block_and_genesis(&block, &genesis).unwrap();
        assert_eq!(derived, expected);
    }

    fn test_genesis() -> ChainGenesis {
        ChainGenesis {
            l1: BlockNumHash { hash: B256::from([4; 32]), number: 2 },
            l2: BlockNumHash { hash: B256::from([5; 32]), number: 1 },
            ..Default::default()
        }
    }

    fn test_header(number: u64) -> Header {
        Header { number, parent_hash: B256::from([2; 32]), timestamp: 1, ..Default::default() }
    }

    fn l1_info_deposit() -> BaseTxEnvelope {
        BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
            input: Bytes::from(&RAW_BEDROCK_INFO_TX),
            ..Default::default()
        }))
    }

    fn test_signature() -> Signature {
        Signature::new(U256::from(1), U256::from(1), false)
    }

    fn legacy_tx() -> BaseTxEnvelope {
        BaseTxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy::default(),
            test_signature(),
            B256::ZERO,
        ))
    }

    fn eip1559_tx() -> BaseTxEnvelope {
        BaseTxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559::default(),
            test_signature(),
            B256::ZERO,
        ))
    }

    #[rstest]
    #[case::l1_info_deposit(vec![l1_info_deposit()])]
    #[case::l1_info_deposit_then_user_tx(vec![l1_info_deposit(), legacy_tx()])]
    #[case::no_transactions(vec![])]
    #[case::legacy_first(vec![legacy_tx()])]
    #[case::typed_non_deposit_first(vec![eip1559_tx()])]
    #[case::invalid_l1_info_calldata(vec![BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
        input: Bytes::from_static(&[0xde, 0xad]),
        ..Default::default()
    }))])]
    fn test_from_header_and_first_tx_matches_from_block(#[case] transactions: Vec<BaseTxEnvelope>) {
        let genesis = test_genesis();
        let header = test_header(3);
        let raw = transactions.first().map(|tx| Bytes::from(tx.encoded_2718()));
        let block = BaseBlock {
            header: header.clone(),
            body: BlockBody { transactions, ..Default::default() },
        };

        assert_eq!(
            L2BlockInfo::from_header_and_first_tx(&Sealed::new(header), raw.as_ref(), &genesis),
            L2BlockInfo::from_block_and_genesis(&block, &genesis),
        );
    }

    #[test]
    fn test_from_header_and_first_tx_genesis() {
        let sealed = Sealed::new(test_header(1));
        let genesis =
            ChainGenesis { l2: BlockNumHash { hash: sealed.hash(), number: 1 }, ..test_genesis() };
        let expected = L2BlockInfo::new(
            BlockInfo::new(sealed.hash(), 1, B256::from([2; 32]), 1),
            genesis.l1,
            0,
        );

        assert_eq!(L2BlockInfo::from_header_and_first_tx(&sealed, None, &genesis), Ok(expected));
        // The first transaction is not inspected at genesis.
        let malformed = Bytes::from_static(&[0x7e, 0x01, 0x02]);
        assert_eq!(
            L2BlockInfo::from_header_and_first_tx(&sealed, Some(&malformed), &genesis),
            Ok(expected)
        );
        assert_eq!(
            L2BlockInfo::from_header_and_first_tx(&sealed, None, &test_genesis()),
            Err(FromBlockError::InvalidGenesisHash)
        );
    }

    #[rstest]
    #[case::empty(Bytes::new())]
    #[case::malformed_legacy(Bytes::from_static(&[0xc5, 0x01, 0x02, 0x03, 0x04, 0x05]))]
    #[case::rlp_string_prefix(Bytes::from_static(&[0x85, 0x01, 0x02, 0x03, 0x04, 0x05]))]
    #[case::malformed_deposit(Bytes::from_static(&[0x7e, 0x01, 0x02]))]
    #[case::trailing_bytes({
        let mut raw = l1_info_deposit().encoded_2718();
        raw.push(0);
        Bytes::from(raw)
    })]
    fn test_from_header_and_first_tx_decode_error(#[case] raw: Bytes) {
        let err = L2BlockInfo::from_header_and_first_tx(
            &Sealed::new(test_header(3)),
            Some(&raw),
            &test_genesis(),
        )
        .unwrap_err();
        assert!(matches!(err, FromBlockError::TxEnvelopeDecodeError(_)), "{err:?}");
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
        assert_eq!(
            FromBlockError::BlockInfoDecodeError(DecodeError::InvalidSelector),
            FromBlockError::BlockInfoDecodeError(DecodeError::InvalidSelector)
        );
    }

    #[test]
    fn test_l2_block_info_invalid_genesis_hash() {
        let genesis = ChainGenesis {
            l1: BlockNumHash { hash: B256::from([4; 32]), number: 2 },
            l2: BlockNumHash { hash: B256::from([5; 32]), number: 1 },
            ..Default::default()
        };
        let base_block = BaseBlock {
            header: Header {
                number: 1,
                parent_hash: B256::from([2; 32]),
                timestamp: 1,
                ..Default::default()
            },
            body: Default::default(),
        };
        let err = L2BlockInfo::from_block_and_genesis(&base_block, &genesis).unwrap_err();
        assert_eq!(err, FromBlockError::InvalidGenesisHash);
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

    #[rstest]
    #[case::pre_denim(None)]
    #[case::post_denim(Some(0))]
    fn test_l2_block_metadata_from_block(#[case] denim: Option<u64>) {
        let l1_info = L1BlockInfoJovian::new(
            18,
            20,
            3,
            B256::from([4; 32]),
            7,
            address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            5,
            6,
            8,
            9,
            10,
            11,
        );
        let mut transactions = vec![BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
            input: l1_info.encode_calldata(),
            ..Default::default()
        }))];
        if denim.is_some() {
            transactions.push(BaseTxEnvelope::Deposit(
                BaseTimeUpdateTx::new(200).unwrap().into_deposit_tx(12),
            ));
        }
        let block = BaseBlock {
            header: Header {
                number: 12,
                timestamp: 24,
                extra_data: bytes!("010000beef0000babe0000000000000123"),
                ..Default::default()
            },
            body: BlockBody { transactions, ..Default::default() },
        };
        let rollup_config = RollupConfig {
            upgrades: UpgradeConfig {
                isthmus_time: Some(0),
                jovian_time: Some(0),
                base: BaseUpgradeConfig { denim, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };

        let metadata = L2BlockMetadata::from_block(&block, &rollup_config).unwrap();

        assert_eq!(
            metadata.l2_block_info,
            L2BlockInfo::new(
                BlockInfo::from(&block),
                BlockNumHash { number: 18, hash: B256::from([4; 32]) },
                7,
            )
        );
        assert_eq!(metadata.l1_info, L1BlockInfoTx::Jovian(l1_info));
        assert_eq!(
            metadata.system_config.batcher_address,
            address!("6887246668a3b87f54deb3b94ba47a6f63f32985")
        );
        assert_eq!(metadata.system_config.min_base_fee, Some(0x123));
        assert_eq!(metadata.system_config.da_footprint_gas_scalar, Some(11));
    }

    #[rstest]
    #[case::genesis(0)]
    #[case::pruned(12)]
    fn test_l2_block_metadata_requires_l1_info_deposit(#[case] number: u64) {
        let block =
            BaseBlock { header: Header { number, ..Default::default() }, body: Default::default() };
        let hash = block.header.hash_slow();
        let rollup_config = RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { number: 0, hash },
                system_config: Some(SystemConfig::default()),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            L2BlockMetadata::from_block(&block, &rollup_config).unwrap_err(),
            BaseBlockConversionError::EmptyTransactions(hash)
        );
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
