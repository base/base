//! Live trie collector for external proofs storage.

use std::{sync::Arc, time::Instant};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use derive_more::Constructor;
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_primitives_traits::{AlloyBlockHeader, BlockTy, NodePrimitives, RecoveredBlock};
use reth_provider::{
    DatabaseProviderFactory, HashedPostStateProvider, StateProviderFactory, StateReader,
    StateRootProvider,
};
use reth_revm::database::StateProviderDatabase;
use reth_trie_common::{HashedPostStateSorted, updates::TrieUpdatesSorted};
use tracing::{info, warn};

use crate::{
    BaseProofsStorage, BaseProofsStorageError, BaseProofsStore, BlockStateDiff,
    api::{BaseProofsBatchSession, BaseProofsBatchStore, OperationDurations, WriteCounts},
    batch_provider::BaseProofsBatchStateProviderRef,
    metrics::BlockMetrics,
    provider::BaseProofsStateProviderRef,
};

/// Live trie collector for external proofs storage.
#[derive(Debug, Constructor)]
pub struct LiveTrieCollector<'tx, Evm, Provider, PreimageStore>
where
    Evm: ConfigureEvm,
    Provider: StateReader + DatabaseProviderFactory + StateProviderFactory,
{
    evm_config: Evm,
    provider: Provider,
    storage: &'tx BaseProofsStorage<PreimageStore>,
}

impl<'tx, Evm, Provider, Store> LiveTrieCollector<'tx, Evm, Provider, Store>
where
    Evm: ConfigureEvm,
    Provider: StateReader + DatabaseProviderFactory + StateProviderFactory,
    Store: 'tx + BaseProofsStore + Clone + 'static,
{
    fn record_storage_metrics(
        operation_durations: &OperationDurations,
        write_counts: Option<&WriteCounts>,
    ) {
        BlockMetrics::record_operation_durations(operation_durations);
        if let Some(write_counts) = write_counts {
            BlockMetrics::increment_write_counts(write_counts);
        }
    }

    /// Execute a block and store the updates in the storage.
    pub fn execute_and_store_block_updates(
        &self,
        block: &RecoveredBlock<BlockTy<Evm::Primitives>>,
    ) -> Result<(), BaseProofsStorageError> {
        let mut operation_durations = OperationDurations::default();

        let start = Instant::now();
        // ensure that we have the state of the parent block
        let (Some((earliest, _)), Some((latest, _))) =
            (self.storage.get_earliest_block_number()?, self.storage.get_latest_block_number()?)
        else {
            return Err(BaseProofsStorageError::NoBlocksFound);
        };

        let parent_block_number = block.number() - 1;
        if parent_block_number < earliest {
            return Err(BaseProofsStorageError::UnknownParent);
        }

        if parent_block_number > latest {
            return Err(BaseProofsStorageError::MissingParentBlock {
                block_number: block.number(),
                parent_block_number,
                latest_block_number: latest,
            });
        }

        let block_ref =
            BlockWithParent::new(block.parent_hash(), NumHash::new(block.number(), block.hash()));

        // TODO: should we check block hash here?

        let state_provider = BaseProofsStateProviderRef::new(
            self.provider.state_by_block_hash(block.parent_hash())?,
            self.storage,
            parent_block_number,
        );

        let db = StateProviderDatabase::new(&state_provider);
        let block_executor = self.evm_config.batch_executor(db);

        let execution_result = block_executor.execute(&(*block).clone())?;

        operation_durations.execution_duration_seconds = start.elapsed();

        let hashed_state = state_provider.hashed_post_state(&execution_result.state);
        let (state_root, trie_updates) =
            state_provider.state_root_with_updates(hashed_state.clone())?;

        operation_durations.state_root_duration_seconds =
            start.elapsed() - operation_durations.execution_duration_seconds;

        if state_root != block.state_root() {
            return Err(BaseProofsStorageError::StateRootMismatch {
                block_number: block.number(),
                current_state_hash: state_root,
                expected_state_hash: block.state_root(),
            });
        }

        let update_result = self.storage.store_trie_updates(
            block_ref,
            BlockStateDiff {
                sorted_trie_updates: trie_updates.into_sorted(),
                sorted_post_state: hashed_state.into_sorted(),
            },
        )?;

        operation_durations.total_duration_seconds = start.elapsed();
        operation_durations.write_duration_seconds = operation_durations.total_duration_seconds
            - operation_durations.state_root_duration_seconds
            - operation_durations.execution_duration_seconds;

        Self::record_storage_metrics(&operation_durations, Some(&update_result));

        info!(
            block_number = block.number(),
            ?operation_durations,
            ?update_result,
            "Block executed and trie updates stored successfully",
        );

        Ok(())
    }

    /// Store trie updates for a given block.
    pub fn store_block_updates(
        &self,
        block: BlockWithParent,
        sorted_trie_updates: TrieUpdatesSorted,
        sorted_post_state: HashedPostStateSorted,
    ) -> Result<(), BaseProofsStorageError> {
        let start = Instant::now();
        let mut operation_durations = OperationDurations::default();

        let storage_result = match self
            .storage
            .store_trie_updates(block, BlockStateDiff { sorted_trie_updates, sorted_post_state })
        {
            Ok(res) => res,
            Err(BaseProofsStorageError::OutOfOrder {
                block_number,
                latest_block_hash,
                parent_block_hash,
            }) => {
                warn!(
                    block_number,
                    ?latest_block_hash,
                    ?parent_block_hash,
                    "Skipping out of order block updates"
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        let write_duration = start.elapsed();
        operation_durations.total_duration_seconds = write_duration;
        operation_durations.write_duration_seconds = write_duration;

        Self::record_storage_metrics(&operation_durations, Some(&storage_result));

        info!(
            block_number = block.block.number,
            ?operation_durations,
            ?storage_result,
            "Trie updates stored successfully",
        );

        Ok(())
    }

    /// Handles chain reorganizations by replacing block updates after a common ancestor.
    ///
    /// This method removes all block updates after the latest common ancestor (the block before
    /// the first block in `new_blocks`) and replaces them with the updates from the provided new
    /// chain.
    ///
    /// # Arguments
    ///
    /// * `new_blocks` - A vector of references to `RecoveredBlock` instances representing the new
    ///   blocks to be added to the trie storage.
    pub fn unwind_and_store_block_updates(
        &self,
        block_updates: Vec<(BlockWithParent, Arc<TrieUpdatesSorted>, Arc<HashedPostStateSorted>)>,
    ) -> Result<(), BaseProofsStorageError> {
        if block_updates.is_empty() {
            return Ok(());
        }

        let start = Instant::now();
        let mut operation_durations = OperationDurations::default();
        let first = &block_updates[0].0;
        let latest_common_block =
            BlockNumHash::new(first.block.number.saturating_sub(1), first.parent);
        let mut block_trie_updates: Vec<(BlockWithParent, BlockStateDiff)> =
            Vec::with_capacity(block_updates.len());

        for (block, trie_updates, hashed_state) in &block_updates {
            block_trie_updates.push((
                *block,
                BlockStateDiff {
                    sorted_trie_updates: (**trie_updates).clone(),
                    sorted_post_state: (**hashed_state).clone(),
                },
            ));
        }

        self.storage.replace_updates(latest_common_block, block_trie_updates)?;
        let write_duration = start.elapsed();
        operation_durations.total_duration_seconds = write_duration;
        operation_durations.write_duration_seconds = write_duration;

        Self::record_storage_metrics(&operation_durations, None);

        info!(
            start_block_number = block_updates.first().map(|(b, _, _)| b.block.number),
            end_block_number = block_updates.last().map(|(b, _, _)| b.block.number),
            ?operation_durations,
            "Trie updates rewound and stored successfully",
        );
        Ok(())
    }

    /// Remove account, storage and trie updates from historical storage for all blocks from
    /// the specified block (inclusive).
    pub fn unwind_history(&self, to: BlockWithParent) -> Result<(), BaseProofsStorageError> {
        self.storage.unwind_history(to)
    }
}

/// One block to process inside a batch session: either pre-computed cached trie data (fast
/// path) or a fully recovered block that needs full execution against the session's
/// transaction-local state (cold catch-up path).
#[derive(Debug)]
pub enum BatchBlock<P: NodePrimitives> {
    /// Pre-computed cached trie data; only writes happen.
    Cached {
        /// Block reference being written.
        block_with_parent: BlockWithParent,
        /// Pre-computed trie updates from cached notification data.
        sorted_trie_updates: Arc<TrieUpdatesSorted>,
        /// Pre-computed hashed post-state from cached notification data.
        sorted_post_state: Arc<HashedPostStateSorted>,
    },
    /// Full block requiring execution against session-local state.
    Execute(Box<RecoveredBlock<BlockTy<P>>>),
}

impl<'tx, Evm, Provider, Store> LiveTrieCollector<'tx, Evm, Provider, Store>
where
    Evm: ConfigureEvm,
    Provider: StateReader + DatabaseProviderFactory + StateProviderFactory,
    Store: 'tx + BaseProofsBatchStore + Clone + 'static,
{
    /// Execute and write a batch of blocks inside a single underlying transaction.
    /// Reads during execution of block N observe writes from blocks earlier in the batch,
    /// enabling cold catch-up where parent state is staged but not yet committed. The
    /// entire batch commits atomically on success and aborts on the first error.
    pub fn execute_and_store_batch(
        &self,
        blocks: Vec<BatchBlock<Evm::Primitives>>,
    ) -> Result<(), BaseProofsStorageError> {
        if blocks.is_empty() {
            return Ok(());
        }

        let start = Instant::now();
        let mut total_writes = WriteCounts::default();
        let mut block_count: u64 = 0;
        let mut last_block_number: u64 = 0;

        self.storage.with_batch_session(|session| {
            let (Some((earliest, _)), Some((_, _))) =
                (session.get_earliest_block_number()?, session.get_latest_block_number()?)
            else {
                return Err(BaseProofsStorageError::NoBlocksFound);
            };

            for entry in blocks {
                match entry {
                    BatchBlock::Cached {
                        block_with_parent,
                        sorted_trie_updates,
                        sorted_post_state,
                    } => {
                        let counts = session.store_trie_updates(
                            block_with_parent,
                            BlockStateDiff {
                                sorted_trie_updates: (*sorted_trie_updates).clone(),
                                sorted_post_state: (*sorted_post_state).clone(),
                            },
                        )?;
                        total_writes += counts;
                        block_count += 1;
                        last_block_number = block_with_parent.block.number;
                    }
                    BatchBlock::Execute(block) => {
                        let counts = self.execute_one_in_session(session, &block, earliest)?;
                        total_writes += counts;
                        block_count += 1;
                        last_block_number = block.number();
                    }
                }
            }

            Ok(())
        })?;

        let total = start.elapsed();
        BlockMetrics::record_operation_durations(&OperationDurations {
            total_duration_seconds: total,
            ..Default::default()
        });
        BlockMetrics::increment_write_counts(&total_writes);
        BlockMetrics::latest_number().set(last_block_number as f64);

        info!(
            block_count,
            last_block_number,
            ?total_writes,
            duration = ?total,
            "Batch executed and committed",
        );

        Ok(())
    }

    fn execute_one_in_session<S>(
        &self,
        session: &mut S,
        block: &RecoveredBlock<BlockTy<Evm::Primitives>>,
        earliest: u64,
    ) -> Result<WriteCounts, BaseProofsStorageError>
    where
        S: BaseProofsBatchSession,
    {
        let latest_in_session =
            session.get_latest_block_number()?.ok_or(BaseProofsStorageError::NoBlocksFound)?.0;

        let parent_block_number = block.number() - 1;
        if parent_block_number < earliest {
            return Err(BaseProofsStorageError::UnknownParent);
        }
        if parent_block_number > latest_in_session {
            return Err(BaseProofsStorageError::MissingParentBlock {
                block_number: block.number(),
                parent_block_number,
                latest_block_number: latest_in_session,
            });
        }

        let block_ref =
            BlockWithParent::new(block.parent_hash(), NumHash::new(block.number(), block.hash()));

        let state_provider = BaseProofsBatchStateProviderRef::new(
            self.provider.state_by_block_hash(block.parent_hash())?,
            session,
            parent_block_number,
        );

        let db = StateProviderDatabase::new(&state_provider);
        let block_executor = self.evm_config.batch_executor(db);

        let execution_result = block_executor.execute(&(*block).clone())?;

        let hashed_state = state_provider.hashed_post_state(&execution_result.state);
        let (state_root, trie_updates) =
            state_provider.state_root_with_updates(hashed_state.clone())?;

        if state_root != block.state_root() {
            return Err(BaseProofsStorageError::StateRootMismatch {
                block_number: block.number(),
                current_state_hash: state_root,
                expected_state_hash: block.state_root(),
            });
        }

        drop(state_provider);

        session.store_trie_updates(
            block_ref,
            BlockStateDiff {
                sorted_trie_updates: trie_updates.into_sorted(),
                sorted_post_state: hashed_state.into_sorted(),
            },
        )
    }
}
