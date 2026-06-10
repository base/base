use std::cmp;

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use reth_provider::BlockHashReader;
use tokio::time::Instant;
use tracing::{error, info, trace};

use crate::{
    BaseProofsStorage, BaseProofsStore,
    prune::{
        error::{BaseProofStoragePrunerResult, PrunerError, PrunerOutput},
        metrics::Metrics,
    },
};

/// Prunes the proof storage by calling `prune_earliest_state` on the storage provider.
#[derive(Debug)]
pub struct BaseProofStoragePruner<P, H> {
    /// Database provider for the prune
    provider: BaseProofsStorage<P>,
    /// Reader to fetch block hash by block number
    block_hash_reader: H,
    /// Keep at least these many recent blocks.
    retention_blocks: u64,
    /// Maximum number of blocks to prune in one database transaction
    prune_batch_size: u64,
}

impl<P, H> BaseProofStoragePruner<P, H> {
    /// Create a new pruner.
    pub fn new(
        provider: BaseProofsStorage<P>,
        block_hash_reader: H,
        retention_blocks: u64,
        prune_batch_size: u64,
    ) -> Result<Self, &'static str> {
        if prune_batch_size == 0 {
            return Err("prune batch size must be greater than zero");
        }
        Ok(Self { provider, block_hash_reader, retention_blocks, prune_batch_size })
    }

    const fn target_earliest_block(&self, latest_block: u64) -> u64 {
        latest_block.saturating_sub(self.retention_blocks)
    }
}

impl<P, H> BaseProofStoragePruner<P, H>
where
    P: BaseProofsStore,
    H: BlockHashReader,
{
    /// Executes the pruning logic and returns the pruner output.
    pub fn run_inner(&self) -> BaseProofStoragePrunerResult {
        let latest_block_opt = self.provider.get_latest_block_number()?;
        if latest_block_opt.is_none() {
            trace!(target: "trie::pruner", "No latest blocks in the proof storage");
            return Ok(PrunerOutput::default());
        }

        let earliest_block_opt = self.provider.get_earliest_block_number()?;
        if earliest_block_opt.is_none() {
            trace!(target: "trie::pruner", "No earliest blocks in the proof storage");
            return Ok(PrunerOutput::default());
        }

        let latest_block = latest_block_opt.unwrap().0;
        let earliest_block = earliest_block_opt.unwrap().0;

        let target_earliest_block = self.target_earliest_block(latest_block);
        info!(
            target: "trie::pruner",
            earliest_block,
            latest_block,
            target_earliest_block,
            retention_blocks = self.retention_blocks,
            prune_batch_size = self.prune_batch_size,
            "Calculated proof storage pruning target",
        );
        if earliest_block >= target_earliest_block {
            trace!(target: "trie::pruner", "Nothing to prune");
            return Ok(PrunerOutput::default());
        }

        info!(
            target: "trie::pruner",
            from_block = earliest_block,
            to_block = target_earliest_block,
            latest_block,
            retention_blocks = self.retention_blocks,
           "Starting pruning proof storage",
        );

        let mut current_earliest_block = earliest_block;
        let mut prune_output = PrunerOutput {
            start_block: earliest_block,
            end_block: target_earliest_block,
            ..Default::default()
        };

        // Prune in batches
        while current_earliest_block < target_earliest_block {
            // Calculate the end of this batch
            let batch_end_block = cmp::min(
                current_earliest_block.saturating_add(self.prune_batch_size),
                target_earliest_block,
            );
            info!(
                target: "trie::pruner",
                start_block = current_earliest_block,
                end_block = batch_end_block,
                target_earliest_block,
                batch_size = batch_end_block.saturating_sub(current_earliest_block),
                "Starting proof storage prune batch",
            );

            let batch_output = self.prune_batch(current_earliest_block, batch_end_block)?;

            prune_output.extend_ref(batch_output);

            // Update loop state
            current_earliest_block = batch_end_block;
        }

        Ok(prune_output)
    }

    /// Prunes a single batch of blocks.
    fn prune_batch(&self, start_block: u64, end_block: u64) -> Result<PrunerOutput, PrunerError> {
        let batch_start_time = Instant::now();
        info!(
            target: "trie::pruner",
            start_block,
            end_block,
            "Resolving proof storage prune batch block hashes",
        );
        if end_block == 0 {
            trace!(
                target: "trie::pruner",
                start_block,
                end_block,
                "Skipping proof storage prune batch at genesis block",
            );
            return Ok(PrunerOutput {
                duration: batch_start_time.elapsed(),
                start_block,
                end_block,
                ..Default::default()
            });
        }

        // Fetch the block hash for the new earliest block of this batch.
        //
        // The parent hash is intentionally not fetched: `prune_earliest_state` only reads
        // `block.number` and `block.hash` from the supplied `BlockWithParent`. Fetching the
        // parent hash here was wasted I/O proportional to the number of batches.
        let new_earliest_block_hash = self
            .block_hash_reader
            .block_hash(end_block)
            .inspect_err(|err| {
                error!(
                    target: "trie::pruner",
                    block = end_block,
                    ?err,
                    "Failed to fetch block hash for new earliest block during pruning"
                )
            })?
            .ok_or(PrunerError::BlockNotFound(end_block))?;

        let block_with_parent = BlockWithParent {
            parent: Default::default(),
            block: BlockNumHash { number: end_block, hash: new_earliest_block_hash },
        };

        info!(
            target: "trie::pruner",
            start_block,
            end_block,
            block_hash = ?new_earliest_block_hash,
            "Resolved proof storage prune batch block hashes",
        );

        info!(
            target: "trie::pruner",
            start_block,
            end_block,
            "Applying proof storage prune batch",
        );
        let write_counts = self.provider.prune_earliest_state(block_with_parent)?;

        let duration = batch_start_time.elapsed();
        let batch_output = PrunerOutput { duration, start_block, end_block, write_counts };

        Metrics::record_prune_result(batch_output.clone());

        info!(
            target: "trie::pruner",
            ?batch_output,
            "Finished pruning batch of proof storage",
        );
        Ok(batch_output)
    }

    /// Run the pruner
    pub fn run(&self) {
        let res = self.run_inner();
        if let Err(e) = res {
            error!(target: "trie::pruner", err=%e, "Pruner failed");
            return;
        }
        info!(target: "trie::pruner", result = %res.unwrap(), "Finished pruning proof storage");
    }
}
