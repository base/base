//! Cached execution provider and executor.

use std::{fmt::Debug, sync::Arc};

use alloy_op_evm::{OpBlockExecutor, block::OpTxResult};
use op_alloy_consensus::{OpTxEnvelope, OpTxType};
use op_revm::OpTransaction;
use reth_errors::BlockExecutionError;
use reth_evm::{
    Evm, RecoveredTx,
    block::{BlockExecutor, ExecutableTx, TxResult},
};
use reth_op::{OpReceipt, chainspec::OpChainSpec};
use reth_optimism_evm::OpRethReceiptBuilder;
use reth_primitives_traits::Recovered;
use reth_revm::State;
use revm::{Database, context::TxEnv};
use revm_primitives::B256;

/// Trait for providers that fetch cached execution results for transactions.
pub trait CachedExecutionProvider<Result> {
    /// Gets the cached execution result for a transaction. This method is expected to be called in the order of the transactions in the block.
    /// This allows only checking if the previous transaction matches the expected hash.
    fn get_cached_execution_for_tx(
        &self,
        parent_block_hash: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<Result>;
}

/// Default implementation of [`CachedExecutionProvider`] that does not provide any cached execution.
#[derive(Debug, Clone, Default)]
pub struct NoopCachedExecutionProvider;

impl<Result> CachedExecutionProvider<Result> for NoopCachedExecutionProvider {
    fn get_cached_execution_for_tx(
        &self,
        _parent_block_hash: &B256,
        _prev_cached_hash: Option<&B256>,
        _tx_hash: &B256,
    ) -> Option<Result> {
        None
    }
}

/// Executor that fetches cached execution results for transactions.
#[derive(Debug)]
pub struct CachedExecutor<E, C> {
    executor: OpBlockExecutor<E, OpRethReceiptBuilder, Arc<OpChainSpec>>,
    cached_execution_provider: C,
    txs: Vec<B256>,
    parent_block_hash: B256,
    all_txs_cached: bool,
}

impl<E, C> CachedExecutor<E, C> {
    /// Creates a new [`CachedExecutor`].
    pub const fn new(
        executor: OpBlockExecutor<E, OpRethReceiptBuilder, Arc<OpChainSpec>>,
        cached_execution_provider: C,
        txs: Vec<B256>,
        parent_block_hash: B256,
    ) -> Self {
        Self { executor, cached_execution_provider, txs, parent_block_hash, all_txs_cached: true }
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

    fn execute_transaction_without_commit(
        &mut self,
        executing_tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        if !self.all_txs_cached {
            return self.executor.execute_transaction_without_commit(executing_tx);
        }

        let executing_tx_recovered = executing_tx.into_parts().1;

        // ensure the executing tx is in the list of txs
        if !self.txs.contains(&executing_tx_recovered.tx().tx_hash()) {
            self.all_txs_cached = false;
            return self.executor.execute_transaction_without_commit(Recovered::new_unchecked(
                executing_tx_recovered.tx(),
                *executing_tx_recovered.signer(),
            ));
        }

        // find tx just before this one
        let prev_tx_hash =
            self.txs.iter().take_while(|tx| **tx != executing_tx_recovered.tx().tx_hash()).last();

        let cached_execution = self.cached_execution_provider.get_cached_execution_for_tx(
            &self.parent_block_hash,
            prev_tx_hash,
            &executing_tx_recovered.tx().tx_hash(),
        );
        if let Some(cached_execution) = cached_execution {
            // load accounts into cache
            for address in cached_execution.result().state.keys() {
                let _ = self.executor.evm_mut().db_mut().load_cache_account(*address);
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
