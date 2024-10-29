//! The Span Batch Type

use crate::{
    BlockInfo, L2BlockInfo, SingleBatch, SpanBatchBits, SpanBatchElement, SpanBatchError,
    SpanBatchTransactions,
};
use alloc::vec::Vec;
use alloy_primitives::FixedBytes;

/// The span batch contains the input to build a span of L2 blocks in derived form.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpanBatch {
    /// First 20 bytes of the first block's parent hash
    pub parent_check: FixedBytes<20>,
    /// First 20 bytes of the last block's L1 origin hash
    pub l1_origin_check: FixedBytes<20>,
    /// Genesis block timestamp
    pub genesis_timestamp: u64,
    /// Chain ID
    pub chain_id: u64,
    /// List of block input in derived form
    pub batches: Vec<SpanBatchElement>,
    /// Caching - origin bits
    pub origin_bits: SpanBatchBits,
    /// Caching - block tx counts
    pub block_tx_counts: Vec<u64>,
    /// Caching - span batch txs
    pub txs: SpanBatchTransactions,
}

impl SpanBatch {
    /// Returns the starting timestamp for the first batch in the span.
    ///
    /// ## Safety
    /// Panics if [Self::batches] is empty.
    pub fn starting_timestamp(&self) -> u64 {
        self.batches[0].timestamp
    }

    /// Returns the final timestamp for the last batch in the span.
    ///
    /// ## Safety
    /// Panics if [Self::batches] is empty.
    pub fn final_timestamp(&self) -> u64 {
        self.batches[self.batches.len() - 1].timestamp
    }

    /// Returns the epoch number for the first batch in the span.
    ///
    /// ## Safety
    /// Panics if [Self::batches] is empty.
    pub fn starting_epoch_num(&self) -> u64 {
        self.batches[0].epoch_num
    }

    /// Checks if the first 20 bytes of the given hash match the L1 origin check.
    pub fn check_origin_hash(&self, hash: FixedBytes<32>) -> bool {
        self.l1_origin_check == hash[..20]
    }

    /// Checks if the first 20 bytes of the given hash match the parent check.
    pub fn check_parent_hash(&self, hash: FixedBytes<32>) -> bool {
        self.parent_check == hash[..20]
    }

    /// Peek at the `n`th-to-last last element in the batch.
    fn peek(&self, n: usize) -> &SpanBatchElement {
        &self.batches[self.batches.len() - 1 - n]
    }

    /// Converts all [SpanBatchElement]s after the L2 safe head to [SingleBatch]es. The resulting
    /// [SingleBatch]es do not contain a parent hash, as it is populated by the Batch Queue
    /// stage.
    pub fn get_singular_batches(
        &self,
        l1_origins: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
    ) -> Result<Vec<SingleBatch>, SpanBatchError> {
        let mut single_batches = Vec::new();
        let mut origin_index = 0;
        for batch in &self.batches {
            if batch.timestamp <= l2_safe_head.block_info.timestamp {
                continue;
            }
            let origin_epoch_hash = l1_origins[origin_index..l1_origins.len()]
                .iter()
                .enumerate()
                .find(|(_, origin)| origin.number == batch.epoch_num)
                .map(|(i, origin)| {
                    origin_index = i;
                    origin.hash
                })
                .ok_or(SpanBatchError::MissingL1Origin)?;
            let single_batch = SingleBatch {
                epoch_num: batch.epoch_num,
                epoch_hash: origin_epoch_hash,
                timestamp: batch.timestamp,
                transactions: batch.transactions.clone(),
                ..Default::default()
            };
            single_batches.push(single_batch);
        }
        Ok(single_batches)
    }

    /// Append a [SingleBatch] to the [SpanBatch]. Updates the L1 origin check if need be.
    pub fn append_singular_batch(
        &mut self,
        singular_batch: SingleBatch,
        seq_num: u64,
    ) -> Result<(), SpanBatchError> {
        // If the new element is not ordered with respect to the last element, panic.
        if !self.batches.is_empty() && self.peek(0).timestamp > singular_batch.timestamp {
            panic!("Batch is not ordered");
        }

        let SingleBatch { epoch_hash, parent_hash, .. } = singular_batch;

        // Always append the new batch and set the L1 origin check.
        self.batches.push(singular_batch.into());
        // Always update the L1 origin check.
        self.l1_origin_check = epoch_hash[..20].try_into().expect("Sub-slice cannot fail");

        let epoch_bit = if self.batches.len() == 1 {
            // If there is only one batch, initialize the parent check and set the epoch bit based
            // on the sequence number.
            self.parent_check = parent_hash[..20].try_into().expect("Sub-slice cannot fail");
            seq_num == 0
        } else {
            // If there is more than one batch, set the epoch bit based on the last two batches.
            self.peek(1).epoch_num < self.peek(0).epoch_num
        };

        // Set the respective bit in the origin bits.
        self.origin_bits.set_bit(self.batches.len() - 1, epoch_bit);

        let new_txs = self.peek(0).transactions.clone();

        // Update the block tx counts cache with the latest batch's transaction count.
        self.block_tx_counts.push(new_txs.len() as u64);

        // Add the new transactions to the transaction cache.
        self.txs.add_txs(new_txs, self.chain_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloy_primitives::b256;

    #[test]
    fn test_timestamp() {
        let timestamp = 10;
        let first_element = SpanBatchElement { timestamp, ..Default::default() };
        let batch =
            SpanBatch { batches: vec![first_element, Default::default()], ..Default::default() };
        assert_eq!(batch.starting_timestamp(), timestamp);
    }

    #[test]
    fn test_starting_timestamp() {
        let timestamp = 10;
        let first_element = SpanBatchElement { timestamp, ..Default::default() };
        let batch =
            SpanBatch { batches: vec![first_element, Default::default()], ..Default::default() };
        assert_eq!(batch.starting_timestamp(), timestamp);
    }

    #[test]
    fn test_final_timestamp() {
        let timestamp = 10;
        let last_element = SpanBatchElement { timestamp, ..Default::default() };
        let batch =
            SpanBatch { batches: vec![Default::default(), last_element], ..Default::default() };
        assert_eq!(batch.final_timestamp(), timestamp);
    }

    #[test]
    fn test_starting_epoch_num() {
        let epoch_num = 10;
        let first_element = SpanBatchElement { epoch_num, ..Default::default() };
        let batch =
            SpanBatch { batches: vec![first_element, Default::default()], ..Default::default() };
        assert_eq!(batch.starting_epoch_num(), epoch_num);
    }

    #[test]
    fn test_check_origin_hash() {
        let l1_origin_check = FixedBytes::from([17u8; 20]);
        let hash = b256!("1111111111111111111111111111111111111111000000000000000000000000");
        let batch = SpanBatch { l1_origin_check, ..Default::default() };
        assert!(batch.check_origin_hash(hash));
        // This hash has 19 matching bytes, the other 13 are zeros.
        let invalid = b256!("1111111111111111111111111111111111111100000000000000000000000000");
        assert!(!batch.check_origin_hash(invalid));
    }

    #[test]
    fn test_check_parent_hash() {
        let parent_check = FixedBytes::from([17u8; 20]);
        let hash = b256!("1111111111111111111111111111111111111111000000000000000000000000");
        let batch = SpanBatch { parent_check, ..Default::default() };
        assert!(batch.check_parent_hash(hash));
        // This hash has 19 matching bytes, the other 13 are zeros.
        let invalid = b256!("1111111111111111111111111111111111111100000000000000000000000000");
        assert!(!batch.check_parent_hash(invalid));
    }
}
