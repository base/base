//! This module contains the [SingleBatch] type.

use crate::{starts_with_2718_deposit, BatchValidity, BlockInfo, L2BlockInfo};
use alloc::vec::Vec;
use alloy_eips::BlockNumHash;
use alloy_primitives::{BlockHash, Bytes};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use op_alloy_genesis::RollupConfig;

/// Represents a single batch: a single encoded L2 block
#[derive(Debug, Default, RlpDecodable, RlpEncodable, Clone, PartialEq, Eq)]
pub struct SingleBatch {
    /// Block hash of the previous L2 block. `B256::ZERO` if it has not been set by the Batch
    /// Queue.
    pub parent_hash: BlockHash,
    /// The batch epoch number. Same as the first L1 block number in the epoch.
    pub epoch_num: u64,
    /// The block hash of the first L1 block in the epoch
    pub epoch_hash: BlockHash,
    /// The L2 block timestamp of this batch
    pub timestamp: u64,
    /// The L2 block transactions in this batch
    pub transactions: Vec<Bytes>,
}

impl SingleBatch {
    /// If any transactions are empty or deposited transaction types.
    pub fn has_invalid_transactions(&self) -> bool {
        self.transactions.iter().any(|tx| tx.0.is_empty() || tx.0[0] == 0x7E)
    }

    /// Returns the [BlockNumHash] of the batch.
    pub const fn epoch(&self) -> BlockNumHash {
        BlockNumHash { number: self.epoch_num, hash: self.epoch_hash }
    }

    /// Validate the batch timestamp.
    pub fn check_batch_timestamp(
        &self,
        cfg: &RollupConfig,
        l2_safe_head: L2BlockInfo,
        inclusion_block: &BlockInfo,
    ) -> BatchValidity {
        let next_timestamp = l2_safe_head.block_info.timestamp + cfg.block_time;
        if self.timestamp > next_timestamp {
            if cfg.is_holocene_active(inclusion_block.timestamp) {
                return BatchValidity::Drop;
            }
            return BatchValidity::Future;
        }
        if self.timestamp < next_timestamp {
            if cfg.is_holocene_active(inclusion_block.timestamp) {
                return BatchValidity::Past;
            }
            return BatchValidity::Drop;
        }
        BatchValidity::Accept
    }

    /// Checks if the batch is valid.
    ///
    /// The batch format type is defined in the [OP Stack Specs][specs].
    ///
    /// [specs]: https://specs.optimism.io/protocol/derivation.html#batch-format
    pub fn check_batch(
        &self,
        cfg: &RollupConfig,
        l1_blocks: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
        inclusion_block: &BlockInfo,
    ) -> BatchValidity {
        // Cannot have empty l1_blocks for batch validation.
        if l1_blocks.is_empty() {
            return BatchValidity::Undecided;
        }

        let epoch = l1_blocks[0];

        // If the batch is not accepted by the timestamp check, return the result.
        let timestamp_check = self.check_batch_timestamp(cfg, l2_safe_head, inclusion_block);
        if !timestamp_check.is_accept() {
            return timestamp_check;
        }

        // Dependent on the above timestamp check.
        // If the timestamp is correct, then it must build on top of the safe head.
        if self.parent_hash != l2_safe_head.block_info.hash {
            return BatchValidity::Drop;
        }

        // Filter out batches that were included too late.
        if self.epoch_num + cfg.seq_window_size < inclusion_block.number {
            return BatchValidity::Drop;
        }

        // Check the L1 origin of the batch
        let mut batch_origin = epoch;
        if self.epoch_num < epoch.number {
            return BatchValidity::Drop;
        } else if self.epoch_num == epoch.number {
            // Batch is sticking to the current epoch, continue.
        } else if self.epoch_num == epoch.number + 1 {
            // With only 1 l1Block we cannot look at the next L1 Origin.
            // Note: This means that we are unable to determine validity of a batch
            // without more information. In this case we should bail out until we have
            // more information otherwise the eager algorithm may diverge from a non-eager
            // algorithm.
            if l1_blocks.len() < 2 {
                return BatchValidity::Undecided;
            }
            batch_origin = l1_blocks[1];
        } else {
            return BatchValidity::Drop;
        }

        // Validate the batch epoch hash
        if self.epoch_hash != batch_origin.hash {
            return BatchValidity::Drop;
        }

        if self.timestamp < batch_origin.timestamp {
            return BatchValidity::Drop;
        }

        // Check if we ran out of sequencer time drift
        let max_drift = cfg.max_sequencer_drift(batch_origin.timestamp);
        let max = if let Some(max) = batch_origin.timestamp.checked_add(max_drift) {
            max
        } else {
            return BatchValidity::Drop;
        };

        let no_txs = self.transactions.is_empty();
        if self.timestamp > max && !no_txs {
            // If the sequencer is ignoring the time drift rule, then drop the batch and force an
            // empty batch instead, as the sequencer is not allowed to include anything
            // past this point without moving to the next epoch.
            return BatchValidity::Drop;
        }
        if self.timestamp > max && no_txs {
            // If the sequencer is co-operating by producing an empty batch,
            // allow the batch if it was the right thing to do to maintain the L2 time >= L1 time
            // invariant. Only check batches that do not advance the epoch, to ensure
            // epoch advancement regardless of time drift is allowed.
            if epoch.number == batch_origin.number {
                if l1_blocks.len() < 2 {
                    return BatchValidity::Undecided;
                }
                let next_origin = l1_blocks[1];
                // Check if the next L1 Origin could have been adopted
                if self.timestamp >= next_origin.timestamp {
                    return BatchValidity::Drop;
                }
            }
        }

        // We can do this check earlier, but it's intensive so we do it last for the sad-path.
        for tx in self.transactions.iter() {
            if tx.is_empty() {
                return BatchValidity::Drop;
            }
            if starts_with_2718_deposit(tx) {
                return BatchValidity::Drop;
            }
        }

        BatchValidity::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_batch_timestamp_holocene_inactive_future() {
        let cfg = RollupConfig::default();
        let l2_safe_head = L2BlockInfo {
            block_info: BlockInfo { timestamp: 1, ..Default::default() },
            ..Default::default()
        };
        let inclusion_block = BlockInfo { timestamp: 1, ..Default::default() };
        let batch = SingleBatch { epoch_num: 1, timestamp: 2, ..Default::default() };
        assert_eq!(
            batch.check_batch_timestamp(&cfg, l2_safe_head, &inclusion_block),
            BatchValidity::Future
        );
    }

    #[test]
    fn test_check_batch_timestamp_holocene_active_drop() {
        let cfg = RollupConfig { holocene_time: Some(0), ..Default::default() };
        let l2_safe_head = L2BlockInfo {
            block_info: BlockInfo { timestamp: 1, ..Default::default() },
            ..Default::default()
        };
        let inclusion_block = BlockInfo { timestamp: 1, ..Default::default() };
        let batch = SingleBatch { epoch_num: 1, timestamp: 2, ..Default::default() };
        assert_eq!(
            batch.check_batch_timestamp(&cfg, l2_safe_head, &inclusion_block),
            BatchValidity::Drop
        );
    }

    #[test]
    fn test_check_batch_timestamp_holocene_active_past() {
        let cfg = RollupConfig { holocene_time: Some(0), ..Default::default() };
        let l2_safe_head = L2BlockInfo {
            block_info: BlockInfo { timestamp: 2, ..Default::default() },
            ..Default::default()
        };
        let inclusion_block = BlockInfo { timestamp: 1, ..Default::default() };
        let batch = SingleBatch { epoch_num: 1, timestamp: 1, ..Default::default() };
        assert_eq!(
            batch.check_batch_timestamp(&cfg, l2_safe_head, &inclusion_block),
            BatchValidity::Past
        );
    }

    #[test]
    fn test_check_batch_timestamp_holocene_inactive_drop() {
        let cfg = RollupConfig::default();
        let l2_safe_head = L2BlockInfo {
            block_info: BlockInfo { timestamp: 2, ..Default::default() },
            ..Default::default()
        };
        let inclusion_block = BlockInfo { timestamp: 1, ..Default::default() };
        let batch = SingleBatch { epoch_num: 1, timestamp: 1, ..Default::default() };
        assert_eq!(
            batch.check_batch_timestamp(&cfg, l2_safe_head, &inclusion_block),
            BatchValidity::Drop
        );
    }

    #[test]
    fn test_check_batch_timestamp_accept() {
        let cfg = RollupConfig::default();
        let l2_safe_head = L2BlockInfo {
            block_info: BlockInfo { timestamp: 2, ..Default::default() },
            ..Default::default()
        };
        let inclusion_block = BlockInfo::default();
        let batch = SingleBatch { timestamp: 2, ..Default::default() };
        assert_eq!(
            batch.check_batch_timestamp(&cfg, l2_safe_head, &inclusion_block),
            BatchValidity::Accept
        );
    }
}
