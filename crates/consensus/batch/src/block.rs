//! Block Types for Base.

use alloc::vec::Vec;

use alloy_eips::{eip2718::Eip2718Error, eip7685::EMPTY_REQUESTS_HASH};
use alloy_primitives::B256;
use base_common_chain_config::ChainGenesis;
use base_common_types_chain::{
    BaseBlock, BaseTxEnvelope, Block, BlockInfo, L2BlockInfo, Transaction,
};
use base_common_types_payload::{
    BaseExecutionPayload, BaseExecutionPayloadSidecar, BasePayloadError, CancunPayloadFields,
    PraguePayloadFields,
};

use crate::{DecodeError, L1BlockInfoTx};

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

/// Decodes the L1 origin and sequence number from a Base block's deposit transaction.
#[derive(Debug)]
pub struct L2BlockInfoDecoder;

impl L2BlockInfoDecoder {
    /// Constructs an [`L2BlockInfo`] from a given Base [`Block`] and [`ChainGenesis`].
    pub fn from_block_and_genesis<T: AsRef<BaseTxEnvelope>>(
        block: &Block<T>,
        genesis: &ChainGenesis,
    ) -> Result<L2BlockInfo, FromBlockError> {
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

            let tx = block.body.transactions[0].as_ref();
            let Some(tx) = tx.as_deposit() else {
                return Err(FromBlockError::FirstTxNonDeposit(tx.tx_type() as u8));
            };

            let l1_info = L1BlockInfoTx::decode_calldata(tx.input().as_ref())
                .map_err(FromBlockError::BlockInfoDecodeError)?;
            (l1_info.id(), l1_info.sequence_number())
        };

        Ok(L2BlockInfo { block_info, l1_origin, seq_num: sequence_number })
    }

    /// Constructs an [`L2BlockInfo`] From a given [`BaseExecutionPayload`] and [`ChainGenesis`].
    pub fn from_payload_and_genesis(
        payload: BaseExecutionPayload,
        parent_beacon_block_root: Option<B256>,
        genesis: &ChainGenesis,
    ) -> Result<L2BlockInfo, FromBlockError> {
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

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_eips::BlockNumHash;
    use alloy_primitives::b256;
    use base_common_types_chain::{BaseBlock, Header};

    use super::*;
    use crate::test_utils::RAW_BEDROCK_INFO_TX;

    #[test]
    fn test_from_block_and_genesis() {
        let genesis = ChainGenesis {
            l1: BlockNumHash { hash: B256::from([4; 32]), number: 2 },
            l2: BlockNumHash { hash: B256::from([5; 32]), number: 1 },
            ..Default::default()
        };
        let tx_env = base_common_types_rpc::Transaction {
            inner: base_common_types_chain::transaction::Recovered::new_unchecked(
                base_common_types_chain::BaseTxEnvelope::Deposit(alloy_primitives::Sealed::new(
                    base_common_types_chain::TxDeposit {
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
        let block: base_common_types_rpc::Block<base_common_types_rpc::BaseTransaction> =
            base_common_types_rpc::Block {
                header: base_common_types_rpc::Header {
                    hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
                    inner: base_common_types_chain::Header {
                        number: 3,
                        parent_hash: b256!(
                            "0202020202020202020202020202020202020202020202020202020202020202"
                        ),
                        timestamp: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                transactions: base_common_types_rpc::BlockTransactions::Full(vec![
                    base_common_types_rpc::BaseTransaction {
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
        let derived = crate::L2BlockInfoDecoder::from_block_and_genesis(&block, &genesis).unwrap();
        assert_eq!(derived, expected);
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
        let err =
            crate::L2BlockInfoDecoder::from_block_and_genesis(&base_block, &genesis).unwrap_err();
        assert_eq!(err, FromBlockError::InvalidGenesisHash);
    }
}
