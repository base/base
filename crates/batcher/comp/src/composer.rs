//! Contains the [`BatchComposer`] type.

use alloy_eips::eip2718::Encodable2718;
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_protocol::{L1BlockInfoTx, SingleBatch};

/// Errors returned by [`BatchComposer::block_to_single_batch`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BatchComposeError {
    /// Block has no transactions.
    #[error("block has no transactions")]
    EmptyBlock,
    /// The first transaction is not a deposit.
    #[error("first transaction is not a deposit")]
    NotDepositTx,
    /// Failed to decode the L1 info deposit calldata.
    #[error("failed to decode L1 info deposit calldata")]
    L1InfoDecode,
}

/// Converts L2 blocks into [`SingleBatch`]es.
///
/// This is the Rust equivalent of `BlockToSingularBatch` from op-batcher's
/// `channel_out.go`.
pub struct BatchComposer;

impl BatchComposer {
    /// Convert an L2 [`OpBlock`] into a [`SingleBatch`] and the decoded
    /// [`L1BlockInfoTx`].
    ///
    /// Mirrors op-batcher's `BlockToSingularBatch`:
    /// 1. The first transaction must be a deposit carrying L1 block info calldata.
    /// 2. All deposit transactions are filtered out; remaining user transactions
    ///    are EIP-2718-encoded.
    /// 3. [`SingleBatch`] fields are populated from the block header and decoded
    ///    L1 block info.
    ///
    /// The [`L1BlockInfoTx`] is returned alongside the batch so callers can
    /// access the sequence number for span batch construction without
    /// re-decoding the deposit.
    pub fn block_to_single_batch(
        block: &OpBlock,
    ) -> Result<(SingleBatch, L1BlockInfoTx), BatchComposeError> {
        if block.body.transactions.is_empty() {
            return Err(BatchComposeError::EmptyBlock);
        }

        let deposit =
            block.body.transactions[0].as_deposit().ok_or(BatchComposeError::NotDepositTx)?;

        let l1_info = L1BlockInfoTx::decode_calldata(&deposit.input)
            .map_err(|_| BatchComposeError::L1InfoDecode)?;

        let epoch = l1_info.id();

        let transactions = block
            .body
            .transactions
            .iter()
            .filter(|tx| !matches!(tx, OpTxEnvelope::Deposit(_)))
            .map(|tx| tx.encoded_2718().into())
            .collect();

        Ok((
            SingleBatch {
                parent_hash: block.header.parent_hash,
                epoch_num: epoch.number,
                epoch_hash: epoch.hash,
                timestamp: block.header.timestamp,
                transactions,
            },
            l1_info,
        ))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{B256, Bytes, Sealed, Signature};
    use base_alloy_consensus::{OpBlock, OpTxEnvelope, TxDeposit};
    use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx};
    use rstest::rstest;

    use super::{BatchComposeError, BatchComposer};

    fn make_block(transactions: Vec<OpTxEnvelope>) -> OpBlock {
        OpBlock {
            header: Header::default(),
            body: BlockBody { transactions, ..Default::default() },
        }
    }

    fn deposit_tx(calldata: Bytes) -> OpTxEnvelope {
        OpTxEnvelope::Deposit(Sealed::new(TxDeposit { input: calldata, ..Default::default() }))
    }

    fn valid_deposit_tx() -> OpTxEnvelope {
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
        deposit_tx(calldata)
    }

    fn non_deposit_tx() -> OpTxEnvelope {
        let signed = TxLegacy::default().into_signed(Signature::test_signature());
        OpTxEnvelope::Legacy(signed)
    }

    #[rstest]
    #[case::empty_block(make_block(vec![]), BatchComposeError::EmptyBlock)]
    #[case::not_deposit(make_block(vec![non_deposit_tx()]), BatchComposeError::NotDepositTx)]
    #[case::bad_calldata(make_block(vec![deposit_tx(Bytes::new())]), BatchComposeError::L1InfoDecode)]
    fn test_errors(#[case] block: OpBlock, #[case] expected: BatchComposeError) {
        assert_eq!(BatchComposer::block_to_single_batch(&block).unwrap_err(), expected);
    }

    #[test]
    fn test_deposits_filtered() {
        // The L1 info deposit plus an extra deposit — both must be stripped.
        let block = make_block(vec![valid_deposit_tx(), deposit_tx(Bytes::new())]);
        let (batch, _) = BatchComposer::block_to_single_batch(&block).unwrap();
        assert!(batch.transactions.is_empty());
    }

    #[test]
    fn test_user_txs_encoded() {
        let user_tx = non_deposit_tx();
        let expected: Bytes = user_tx.encoded_2718().into();
        let block = make_block(vec![valid_deposit_tx(), user_tx]);
        let (batch, _) = BatchComposer::block_to_single_batch(&block).unwrap();
        assert_eq!(batch.transactions, vec![expected]);
    }

    #[test]
    fn test_fields_match_block() {
        let parent_hash = B256::from([0xAB; 32]);
        let timestamp = 1_234_567_u64;
        let mut block = make_block(vec![valid_deposit_tx()]);
        block.header.parent_hash = parent_hash;
        block.header.timestamp = timestamp;

        let info = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default());
        let (batch, _) = BatchComposer::block_to_single_batch(&block).unwrap();

        assert_eq!(batch.parent_hash, parent_hash);
        assert_eq!(batch.timestamp, timestamp);
        assert_eq!(batch.epoch_num, info.id().number);
        assert_eq!(batch.epoch_hash, info.id().hash);
    }

    #[test]
    fn test_l1_info_returned() {
        let block = make_block(vec![valid_deposit_tx()]);
        let (_, l1_info) = BatchComposer::block_to_single_batch(&block).unwrap();
        let expected = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default());
        assert_eq!(l1_info.sequence_number(), expected.sequence_number());
        assert_eq!(l1_info.id(), expected.id());
    }
}
