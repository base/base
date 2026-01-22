use alloy_consensus::transaction::TxHashRef;
use reth_errors::BlockExecutionError;
use reth_evm::{
    ConfigureEvm, Evm,
    block::{BlockExecutor, BlockExecutorFor, ExecutableTx},
    execute::ExecutableTxFor,
};
use reth_provider::ExecutionOutcome;
use reth_revm::State;
use revm::{Database, context::result::ResultAndState};
use revm_primitives::B256;

use crate::tree::error::InsertBlockErrorKind;

pub trait CachedExecutionProvider<Receipt, HaltReason> {
    // TODO: what do we need to check to ensure the tx execution is valid?
    fn get_cached_execution_for_tx<'a>(
        &self,
        start_state_root: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<ResultAndState<HaltReason>>;
}

#[derive(Debug, Clone, Default)]
pub struct NoopCachedExecutionProvider;

impl<Receipt, HaltReason> CachedExecutionProvider<Receipt, HaltReason>
    for NoopCachedExecutionProvider
{
    fn get_cached_execution_for_tx<'a>(
        &self,
        start_state_root: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<ResultAndState<HaltReason>> {
        None
    }
}

pub struct CachedExecutor<E, C> {
    executor: E,
    cached_execution_provider: C,
    txs: Vec<B256>,
    block_state_root: B256,
    all_txs_cached: bool,
}

impl<E, C> CachedExecutor<E, C> {
    pub fn new(
        executor: E,
        cached_execution_provider: C,
        txs: Vec<B256>,
        block_state_root: B256,
    ) -> Self {
        Self { executor, cached_execution_provider, txs, block_state_root, all_txs_cached: true }
    }
}

impl<'a, E, C, DB> BlockExecutor for CachedExecutor<E, C>
where
    DB: Database + 'a,
    E: BlockExecutor<Transaction: TxHashRef, Evm: Evm<DB = &'a mut State<DB>>>,
    C: CachedExecutionProvider<E::Receipt, <E::Evm as Evm>::HaltReason>,
{
    type Transaction = E::Transaction;
    type Receipt = E::Receipt;
    type Evm = E::Evm;

    fn execute_transaction_without_commit(
        &mut self,
        executing_tx: impl ExecutableTx<Self>,
    ) -> Result<ResultAndState<<Self::Evm as Evm>::HaltReason>, BlockExecutionError> {

        if !self.all_txs_cached {
            return self.executor.execute_transaction_without_commit(executing_tx);
        }

        // find tx just before this one
        let prev_tx_hash = self.txs.iter().take_while(|tx| *tx != executing_tx.tx().tx_hash()).last();
        
        let cached_execution = self.cached_execution_provider.get_cached_execution_for_tx(
            &self.block_state_root,
            prev_tx_hash,
            &executing_tx.tx().tx_hash(),
        );
        if let Some(cached_execution) = cached_execution {
            // load accounts into cache
            for (address, _) in cached_execution.state.iter() {
                let _ = self.executor.evm_mut().db_mut().load_cache_account(*address);
            }
            return Ok(cached_execution);
        }
        self.all_txs_cached = false;
        self.executor.execute_transaction_without_commit(executing_tx)
    }

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.executor.apply_pre_execution_changes()
    }

    fn commit_transaction(
        &mut self,
        output: ResultAndState<<Self::Evm as Evm>::HaltReason>,
        tx: impl ExecutableTx<Self>,
    ) -> Result<u64, BlockExecutionError> {
        self.executor.commit_transaction(output, tx)
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
