use alloc::sync::Arc;

use alloy_primitives::BlockNumber;
use base_common_types_chain::{BlockHeader, RecoveredBlock, SealedBlock};

use crate::{
    BlockExecutionOutput, BlockExecutionResult, ComputedTrieData, HashedPostStateSorted,
    LazyTrieData, updates::TrieUpdatesSorted,
};

/// Represents an executed block stored in-memory.
#[derive(Clone, Debug)]
pub struct ExecutedBlock {
    /// Recovered Block
    pub recovered_block: Arc<RecoveredBlock>,
    /// Block's execution outcome.
    pub execution_output: Arc<BlockExecutionOutput>,
    /// Deferred trie data produced by execution.
    ///
    /// This allows deferring the computation of the trie data which can be expensive.
    /// The data can be populated asynchronously after the block was validated.
    pub trie_data: LazyTrieData,
}

impl Default for ExecutedBlock {
    fn default() -> Self {
        Self {
            recovered_block: Default::default(),
            execution_output: Arc::new(BlockExecutionOutput {
                result: BlockExecutionResult {
                    receipts: Default::default(),
                    requests: Default::default(),
                    gas_used: 0,
                    blob_gas_used: 0,
                },
                state: Default::default(),
            }),
            trie_data: LazyTrieData::ready(ComputedTrieData::default()),
        }
    }
}

impl PartialEq for ExecutedBlock {
    fn eq(&self, other: &Self) -> bool {
        // Trie data is computed asynchronously and doesn't define block identity.
        self.recovered_block == other.recovered_block
            && self.execution_output == other.execution_output
    }
}

impl ExecutedBlock {
    /// Create a new [`ExecutedBlock`] with already-computed trie data.
    ///
    /// Use this constructor when trie data is available immediately (e.g., sequencers,
    /// payload builders). This is the safe default path.
    pub fn new(
        recovered_block: Arc<RecoveredBlock>,
        execution_output: Arc<BlockExecutionOutput>,
        trie_data: ComputedTrieData,
    ) -> Self {
        Self { recovered_block, execution_output, trie_data: LazyTrieData::ready(trie_data) }
    }

    /// Create a new [`ExecutedBlock`] with deferred trie data.
    ///
    /// This is useful if the trie data is populated somewhere else, e.g. asynchronously
    /// after the block was validated.
    ///
    /// The [`LazyTrieData`] handle allows expensive trie operations (sorting hashed state and
    /// trie updates) to be performed outside the critical validation path by a background task.
    /// This can improve latency for time-sensitive operations like block validation.
    ///
    /// If the data hasn't been populated when [`Self::trie_data()`] is called, the caller waits
    /// for the background task to publish it.
    ///
    /// Use [`Self::new()`] instead when trie data is already computed and available immediately.
    pub const fn with_deferred_trie_data(
        recovered_block: Arc<RecoveredBlock>,
        execution_output: Arc<BlockExecutionOutput>,
        trie_data: LazyTrieData,
    ) -> Self {
        Self { recovered_block, execution_output, trie_data }
    }

    /// Returns a reference to an inner [`SealedBlock`]
    #[inline]
    pub fn sealed_block(&self) -> &SealedBlock {
        self.recovered_block.sealed_block()
    }

    /// Returns a reference to [`RecoveredBlock`]
    #[inline]
    pub fn recovered_block(&self) -> &RecoveredBlock {
        &self.recovered_block
    }

    /// Returns a reference to the block's execution outcome
    #[inline]
    pub fn execution_outcome(&self) -> &BlockExecutionOutput {
        &self.execution_output
    }

    /// Returns the trie data, waiting for the background task if not already cached.
    ///
    /// Uses `OnceLock::get_or_init` internally:
    /// - If already computed: returns cached result immediately
    /// - If not computed: first caller waits for the publishing task, others wait for that result
    #[inline]
    #[tracing::instrument(level = "debug", target = "engine::tree", name = "trie_data", skip_all)]
    pub fn trie_data(&self) -> ComputedTrieData {
        self.trie_data.get().clone()
    }

    /// Returns a clone of the deferred trie data handle.
    ///
    /// A handle is a lightweight reference that can be passed to descendants without
    /// forcing trie data to be observed immediately. The actual work runs in the background task.
    #[inline]
    pub fn trie_data_handle(&self) -> LazyTrieData {
        self.trie_data.clone()
    }

    /// Returns the hashed state result of the execution outcome.
    ///
    /// May wait for trie data if the deferred task hasn't completed.
    #[inline]
    pub fn hashed_state(&self) -> Arc<HashedPostStateSorted> {
        self.trie_data().sorted.hashed_state
    }

    /// Returns the trie updates resulting from the execution outcome.
    ///
    /// May wait for trie data if the deferred task hasn't completed.
    #[inline]
    pub fn trie_updates(&self) -> Arc<TrieUpdatesSorted> {
        self.trie_data().sorted.trie_updates
    }

    /// Returns a [`BlockNumber`] of the block.
    #[inline]
    pub fn block_number(&self) -> BlockNumber {
        self.recovered_block.header().number()
    }
}
