//! An L2 block buffered by the encoder with its estimated DA size.

use alloy_eips::eip2718::Encodable2718;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_flz::tx_estimated_size_fjord_bytes;

/// An L2 block queued for encoding, with the DA bytes it is estimated to cost once compressed.
///
/// The estimate is the Fjord `FastLZ` size of each non-deposit transaction plus a fixed batch
/// overhead, the same unit the block builders apply DA limits in. It is computed once on entry
/// because the throttle reads the backlog on every driver iteration.
#[derive(Debug, Clone)]
pub struct BufferedBlock {
    block: BaseBlock,
    estimated_da_bytes: u64,
}

impl BufferedBlock {
    /// Estimated DA bytes of the batch metadata that wraps a block's transactions.
    pub const BATCH_OVERHEAD_BYTES: u64 = 70;

    /// Buffers `block` and estimates its DA size.
    pub fn new(block: BaseBlock) -> Self {
        let transactions: u64 = block
            .body
            .transactions
            .iter()
            .filter(|tx| !matches!(tx, BaseTxEnvelope::Deposit(_)))
            .map(|tx| tx_estimated_size_fjord_bytes(&tx.encoded_2718()))
            .sum();
        Self { block, estimated_da_bytes: Self::BATCH_OVERHEAD_BYTES + transactions }
    }

    /// Returns the buffered block.
    pub const fn block(&self) -> &BaseBlock {
        &self.block
    }

    /// Returns the estimated DA bytes of the block once compressed.
    pub const fn estimated_da_bytes(&self) -> u64 {
        self.estimated_da_bytes
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, SignableTransaction, TxLegacy};
    use alloy_primitives::{Bytes, Sealed, Signature};
    use base_common_consensus::TxDeposit;

    use super::*;

    fn user_tx(input: Vec<u8>) -> BaseTxEnvelope {
        BaseTxEnvelope::Legacy(
            TxLegacy { input: Bytes::from(input), ..Default::default() }
                .into_signed(Signature::test_signature()),
        )
    }

    fn block(transactions: Vec<BaseTxEnvelope>) -> BaseBlock {
        BaseBlock {
            header: Default::default(),
            body: BlockBody { transactions, ..Default::default() },
        }
    }

    #[test]
    fn estimate_is_the_fjord_size_of_user_transactions_plus_batch_overhead() {
        let deposit = BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
            input: Bytes::from(vec![0xab; 4_096]),
            ..Default::default()
        }));
        let user = user_tx((0..=u8::MAX).cycle().take(4_096).collect());
        let user_estimate = tx_estimated_size_fjord_bytes(&user.encoded_2718());

        let buffered = BufferedBlock::new(block(vec![deposit, user]));

        assert_eq!(
            buffered.estimated_da_bytes(),
            BufferedBlock::BATCH_OVERHEAD_BYTES + user_estimate
        );
    }

    #[test]
    fn compressible_transactions_count_their_compressed_size_not_their_raw_size() {
        let transactions: Vec<_> = (0..20).map(|_| user_tx(vec![0; 100_000])).collect();
        let raw_bytes: u64 = transactions.iter().map(|tx| tx.encode_2718_len() as u64).sum();

        let buffered = BufferedBlock::new(block(transactions));

        assert!(
            buffered.estimated_da_bytes() < raw_bytes / 50,
            "estimated {} DA bytes for {raw_bytes} raw bytes",
            buffered.estimated_da_bytes()
        );
    }
}
