//! Block-to-batch composition.

use alloy_eips::eip2718::Encodable2718;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
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
#[derive(Debug)]
pub struct BatchComposer;

impl BatchComposer {
    /// Convert an L2 [`BaseBlock`] into a [`SingleBatch`].
    ///
    /// 1. The first transaction must be a deposit carrying L1 block info calldata.
    /// 2. All deposit transactions are filtered out; remaining user transactions
    ///    are EIP-2718-encoded.
    /// 3. [`SingleBatch`] fields are populated from the block header and decoded
    ///    L1 block info.
    pub fn block_to_single_batch(block: &BaseBlock) -> Result<SingleBatch, BatchComposeError> {
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
            .filter(|tx| !matches!(tx, BaseTxEnvelope::Deposit(_)))
            .map(|tx| tx.encoded_2718().into())
            .collect();

        Ok(SingleBatch {
            parent_hash: block.header.parent_hash,
            epoch_num: epoch.number,
            epoch_hash: epoch.hash,
            timestamp: block.header.timestamp,
            transactions,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::vec;

    use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{B256, Bytes, Sealed, Signature};
    use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
    use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx};
    use rstest::rstest;

    use super::{BatchComposeError, BatchComposer};

    fn make_block(transactions: Vec<BaseTxEnvelope>) -> BaseBlock {
        BaseBlock {
            header: Header::default(),
            body: BlockBody { transactions, ..Default::default() },
        }
    }

    fn deposit_tx(calldata: Bytes) -> BaseTxEnvelope {
        BaseTxEnvelope::Deposit(Sealed::new(TxDeposit { input: calldata, ..Default::default() }))
    }

    fn valid_deposit_tx() -> BaseTxEnvelope {
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
        deposit_tx(calldata)
    }

    fn non_deposit_tx() -> BaseTxEnvelope {
        let signed = TxLegacy::default().into_signed(Signature::test_signature());
        BaseTxEnvelope::Legacy(signed)
    }

    #[rstest]
    #[case::empty_block(make_block(vec![]), BatchComposeError::EmptyBlock)]
    #[case::not_deposit(make_block(vec![non_deposit_tx()]), BatchComposeError::NotDepositTx)]
    #[case::bad_calldata(make_block(vec![deposit_tx(Bytes::new())]), BatchComposeError::L1InfoDecode)]
    fn test_errors(#[case] block: BaseBlock, #[case] expected: BatchComposeError) {
        assert_eq!(BatchComposer::block_to_single_batch(&block).unwrap_err(), expected);
    }

    #[test]
    fn test_deposits_filtered() {
        let block = make_block(vec![valid_deposit_tx(), deposit_tx(Bytes::new())]);
        let batch = BatchComposer::block_to_single_batch(&block).unwrap();
        assert!(batch.transactions.is_empty());
    }

    #[test]
    fn test_user_txs_encoded() {
        let user_tx = non_deposit_tx();
        let expected: Bytes = user_tx.encoded_2718().into();
        let block = make_block(vec![valid_deposit_tx(), user_tx]);
        let batch = BatchComposer::block_to_single_batch(&block).unwrap();
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
        let batch = BatchComposer::block_to_single_batch(&block).unwrap();

        assert_eq!(batch.parent_hash, parent_hash);
        assert_eq!(batch.timestamp, timestamp);
        assert_eq!(batch.epoch_num, info.id().number);
        assert_eq!(batch.epoch_hash, info.id().hash);
    }
}
