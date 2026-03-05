//! Cached execution provider and executor.

use std::{collections::HashMap, fmt::Debug, sync::Arc};

use base_alloy_consensus::{OpReceipt, OpTxEnvelope, OpTxType};
use base_alloy_evm::{OpBlockExecutor, OpTxResult};
use base_execution_chainspec::OpChainSpec;
use base_execution_evm::OpRethReceiptBuilder;
use base_flashblocks::{FlashblocksAPI, FlashblocksState};
use base_revm::{OpHaltReason, OpTransaction};
use metrics::counter;
use reth_errors::BlockExecutionError;
use reth_evm::{
    Evm, RecoveredTx,
    block::{BlockExecutor, ExecutableTx, InternalBlockExecutionError, TxResult},
};
use reth_primitives_traits::Recovered;
use reth_provider::BlockNumReader;
use reth_revm::State;
use revm::{Database, context::TxEnv};
use revm_primitives::B256;
use tracing::{instrument, trace, warn};

/// Provider that fetches cached execution results for transactions.
#[derive(Debug, Clone)]
pub struct FlashblocksCachedExecutionProvider<P> {
    flashblocks_state: Option<Arc<FlashblocksState>>,

    provider: P,
}

impl<P> FlashblocksCachedExecutionProvider<P> {
    /// Creates a new [`FlashblocksCachedExecutionProvider`].
    pub const fn new(provider: P, flashblocks_state: Option<Arc<FlashblocksState>>) -> Self {
        Self { provider, flashblocks_state }
    }
}

const CACHE_MISALIGNMENT_TOTAL: &str = "reth_base_engine_tree_cache_misalignment_total";

#[inline]
fn cache_alignment_miss_reason(
    this_block_number: u64,
    tx_block_number: u64,
    tx_index: u64,
    prev_position: Option<(u64, u64)>,
) -> Option<&'static str> {
    if tx_block_number != this_block_number {
        return Some("tx_block_number_mismatch");
    }

    match prev_position {
        Some((prev_block_number, prev_index)) => {
            if prev_block_number != this_block_number {
                Some("prev_block_number_mismatch")
            } else if prev_index.checked_add(1) != Some(tx_index) {
                Some("non_adjacent_prev_tx")
            } else {
                None
            }
        }
        None => {
            if tx_index != 0 {
                Some("non_zero_first_tx_index")
            } else {
                None
            }
        }
    }
}

impl<P> CachedExecutionProvider<OpTxResult<OpHaltReason, OpTxType>>
    for FlashblocksCachedExecutionProvider<P>
where
    P: BlockNumReader,
{
    #[instrument(level = "debug", skip_all, fields(tx_hash = ?tx_hash))]
    fn get_cached_execution_for_tx(
        &self,
        parent_block_hash: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<OpTxResult<OpHaltReason, OpTxType>> {
        let flashblocks_state = self.flashblocks_state.as_ref()?;

        // if block_number is not found, we can't use cached execution
        let parent_block_number = self.provider.block_number(*parent_block_hash).ok().flatten()?;

        let this_block_number = parent_block_number.checked_add(1).unwrap();

        let pending_blocks = flashblocks_state.get_pending_blocks().clone()?;

        let Some(tx) = pending_blocks.get_transaction_by_hash(*tx_hash) else {
            warn!(tx_hash = ?tx_hash, "Not using cached results - transaction not cached");
            return None;
        };

        let Some(tx_block_number) = tx.inner.block_number else {
            warn!(tx_hash = ?tx_hash, "Not using cached results - cached tx missing block number");
            return None;
        };

        let Some(tx_index) = tx.inner.transaction_index else {
            warn!(tx_hash = ?tx_hash, "Not using cached results - cached tx missing transaction index");
            return None;
        };

        let prev_position = if let Some(prev_cached_hash) = prev_cached_hash {
            // Enforce strict adjacency in the same block to avoid cross-block / out-of-order cache hits.
            let Some(prev_tx) = pending_blocks.get_transaction_by_hash(*prev_cached_hash) else {
                warn!(
                    prev_cached_hash = ?prev_cached_hash,
                    "Not using cached results - previous transaction not cached",
                );
                return None;
            };

            let Some(prev_block_number) = prev_tx.inner.block_number else {
                warn!(
                    prev_cached_hash = ?prev_cached_hash,
                    "Not using cached results - previous cached tx missing block number",
                );
                return None;
            };

            let Some(prev_index) = prev_tx.inner.transaction_index else {
                warn!(
                    prev_cached_hash = ?prev_cached_hash,
                    "Not using cached results - previous cached tx missing transaction index",
                );
                return None;
            };
            Some((prev_block_number, prev_index))
        } else {
            None
        };

        if let Some(reason) =
            cache_alignment_miss_reason(this_block_number, tx_block_number, tx_index, prev_position)
        {
            counter!(CACHE_MISALIGNMENT_TOTAL, "reason" => reason).increment(1);
            warn!(
                tx_hash = ?tx_hash,
                tx_block_number,
                tx_index,
                ?prev_position,
                this_block_number,
                reason,
                "Not using cached results - tx not aligned with expected block/position",
            );
            return None;
        }

        trace!(tx_hash = ?tx_hash, "cache hit for transaction");
        pending_blocks.get_op_tx_result(tx_hash)
    }
}

/// Trait for providers that fetch cached execution results for transactions.
pub trait CachedExecutionProvider<TxResult> {
    /// Gets the cached execution result for a transaction. This method is expected to be called in the order of the transactions in the block.
    /// This allows only checking if the previous transaction matches the expected hash.
    fn get_cached_execution_for_tx(
        &self,
        parent_block_hash: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<TxResult>;
}

/// Default implementation of [`CachedExecutionProvider`] that does not provide any cached execution.
#[derive(Debug, Clone, Default)]
pub struct NoopCachedExecutionProvider;

impl<TxResult> CachedExecutionProvider<TxResult> for NoopCachedExecutionProvider {
    fn get_cached_execution_for_tx(
        &self,
        _parent_block_hash: &B256,
        _prev_cached_hash: Option<&B256>,
        _tx_hash: &B256,
    ) -> Option<TxResult> {
        None
    }
}

/// Executor that fetches cached execution results for transactions.
#[derive(Debug)]
pub struct CachedExecutor<E, C> {
    executor: OpBlockExecutor<E, OpRethReceiptBuilder, Arc<OpChainSpec>>,
    cached_execution_provider: C,
    txs: Vec<B256>,
    position_by_hash: HashMap<B256, usize>,
    parent_block_hash: B256,
    all_txs_cached: bool,
}

impl<E, C> CachedExecutor<E, C> {
    /// Creates a new [`CachedExecutor`].
    pub fn new(
        executor: OpBlockExecutor<E, OpRethReceiptBuilder, Arc<OpChainSpec>>,
        cached_execution_provider: C,
        txs: Vec<B256>,
        parent_block_hash: B256,
    ) -> Self {
        let position_by_hash =
            txs.iter().enumerate().map(|(i, tx)| (*tx, i)).collect::<HashMap<_, _>>();
        Self {
            executor,
            cached_execution_provider,
            txs,
            position_by_hash,
            parent_block_hash,
            all_txs_cached: true,
        }
    }
}

impl<'a, DB, E, C> BlockExecutor for CachedExecutor<E, C>
where
    DB: Database + alloy_evm::Database + 'a,
    E: Evm<DB = &'a mut State<DB>, Tx = OpTransaction<TxEnv>>,
    C: CachedExecutionProvider<OpTxResult<E::HaltReason, OpTxType>>,
{
    type Transaction = OpTxEnvelope;
    type Receipt = OpReceipt;
    type Evm = E;
    type Result = OpTxResult<E::HaltReason, OpTxType>;

    fn receipts(&self) -> &[Self::Receipt] {
        self.executor.receipts()
    }

    #[instrument(level = "debug", skip_all)]
    fn execute_transaction_without_commit(
        &mut self,
        executing_tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        if !self.all_txs_cached {
            return self.executor.execute_transaction_without_commit(executing_tx);
        }

        let executing_tx_recovered = executing_tx.into_parts().1;
        let tx_hash = executing_tx_recovered.tx().tx_hash();

        // find tx just before this one
        let tx_position = self.position_by_hash.get(&tx_hash);

        // not found, we need to execute the transaction
        let Some(tx_position) = tx_position else {
            self.all_txs_cached = false;
            return self.executor.execute_transaction_without_commit(Recovered::new_unchecked(
                executing_tx_recovered.tx(),
                *executing_tx_recovered.signer(),
            ));
        };

        let prev_tx_hash = tx_position.checked_sub(1).and_then(|pos| self.txs.get(pos));

        let cached_execution = self.cached_execution_provider.get_cached_execution_for_tx(
            &self.parent_block_hash,
            prev_tx_hash,
            &tx_hash,
        );
        if let Some(cached_execution) = cached_execution {
            // load accounts into cache
            for address in cached_execution.result().state.keys() {
                // ignore the result since we don't care if the account exists or not
                self.executor.evm_mut().db_mut().load_cache_account(*address).map_err(|err| {
                    BlockExecutionError::Internal(InternalBlockExecutionError::Other(Box::new(err)))
                })?;
            }
            return Ok(cached_execution);
        }
        self.all_txs_cached = false;
        self.executor.execute_transaction_without_commit(Recovered::new_unchecked(
            executing_tx_recovered.tx(),
            *executing_tx_recovered.signer(),
        ))
    }

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.executor.apply_pre_execution_changes()
    }

    fn commit_transaction(&mut self, output: Self::Result) -> Result<u64, BlockExecutionError> {
        self.executor.commit_transaction(output)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, reth_provider::BlockExecutionResult<Self::Receipt>), BlockExecutionError>
    {
        self.executor.finish()
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn reth_evm::OnStateHook>>) {
        self.executor.set_state_hook(hook)
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.executor.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.executor.evm()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_alignment_requires_same_block() {
        assert_eq!(
            cache_alignment_miss_reason(100, 101, 0, None),
            Some("tx_block_number_mismatch")
        );
        assert_eq!(cache_alignment_miss_reason(100, 99, 0, None), Some("tx_block_number_mismatch"));
    }

    #[test]
    fn cache_alignment_requires_zero_index_for_first_tx() {
        assert_eq!(cache_alignment_miss_reason(100, 100, 0, None), None);
        assert_eq!(cache_alignment_miss_reason(100, 100, 1, None), Some("non_zero_first_tx_index"));
    }

    #[test]
    fn cache_alignment_requires_adjacent_previous_tx() {
        assert_eq!(cache_alignment_miss_reason(100, 100, 1, Some((100, 0))), None);
        assert_eq!(
            cache_alignment_miss_reason(100, 100, 2, Some((100, 0))),
            Some("non_adjacent_prev_tx")
        );
        assert_eq!(
            cache_alignment_miss_reason(100, 100, 1, Some((101, 0))),
            Some("prev_block_number_mismatch")
        );
        assert_eq!(
            cache_alignment_miss_reason(100, 100, 0, Some((100, 0))),
            Some("non_adjacent_prev_tx")
        );
    }
}
