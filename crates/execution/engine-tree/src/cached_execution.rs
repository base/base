//! Cached execution provider and executor.

use std::{collections::HashMap, fmt::Debug, sync::Arc};

use base_common_consensus::{BaseReceipt, BaseTxEnvelope, OpTxType};
use base_common_evm::{BaseBlockExecutor, BaseTxResult, OpHaltReason, OpTransaction};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::OpRethReceiptBuilder;
use base_flashblocks::{FlashblocksAPI, FlashblocksState};
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

impl<P> CachedExecutionProvider<BaseTxResult<OpHaltReason, OpTxType>>
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
    ) -> Option<BaseTxResult<OpHaltReason, OpTxType>> {
        let flashblocks_state = self.flashblocks_state.as_ref()?;

        // if block_number is not found, we can't use cached execution
        let parent_block_number = self.provider.block_number(*parent_block_hash).ok().flatten()?;

        let this_block_number = parent_block_number.checked_add(1).unwrap();

        let pending_blocks = flashblocks_state.get_pending_blocks().clone()?;

        if let Some(prev_cached_hash) = prev_cached_hash {
            // all previous transactions from start of block to prev_cached_hash are cached, so only check if the previous transaction is cached
            if !pending_blocks.has_transaction_hash(prev_cached_hash) {
                warn!(
                    prev_cached_hash = ?prev_cached_hash,
                    "Not using cached results - previous transaction not cached",
                );
                return None;
            }
        } else {
            // must be the first tx in the block
            if pending_blocks
                .get_transactions_for_block(this_block_number)
                .next()
                .map(|tx| tx.inner.inner.tx_hash())
                != Some(*tx_hash)
            {
                warn!(tx_hash = ?tx_hash, "Not using cached results - first transaction not cached");
                return None;
            }
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
    executor: BaseBlockExecutor<E, OpRethReceiptBuilder, Arc<BaseChainSpec>>,
    cached_execution_provider: C,
    txs: Vec<B256>,
    position_by_hash: HashMap<B256, usize>,
    parent_block_hash: B256,
    all_txs_cached: bool,
}

impl<E, C> CachedExecutor<E, C> {
    /// Creates a new [`CachedExecutor`].
    pub fn new(
        executor: BaseBlockExecutor<E, OpRethReceiptBuilder, Arc<BaseChainSpec>>,
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
    C: CachedExecutionProvider<BaseTxResult<E::HaltReason, OpTxType>>,
{
    type Transaction = BaseTxEnvelope;
    type Receipt = BaseReceipt;
    type Evm = E;
    type Result = BaseTxResult<E::HaltReason, OpTxType>;

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
