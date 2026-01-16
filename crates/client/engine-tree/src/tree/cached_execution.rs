use reth_errors::BlockExecutionError;
use reth_evm::{
    ConfigureEvm, Evm,
    block::{BlockExecutor, BlockExecutorFor, ExecutableTx},
    execute::ExecutableTxFor,
};
use reth_provider::ExecutionOutcome;
use revm::context::result::ResultAndState;
use revm_primitives::B256;

use crate::tree::error::InsertBlockErrorKind;

pub trait CachedExecutionProvider<Receipt> {
    // TODO: what do we need to check to ensure the tx execution is valid?
    fn get_cached_execution_for_tx(
        &self,
        start_state_root: &B256,
        prev_tx_hashes: &[B256],
        tx_hash: &B256,
    ) -> Option<Result<ExecutionOutcome<Receipt>, InsertBlockErrorKind>>;
}

#[derive(Debug, Clone, Default)]
pub struct NoopCachedExecutionProvider;

impl<Receipt> CachedExecutionProvider<Receipt> for NoopCachedExecutionProvider {
    fn get_cached_execution_for_tx(
        &self,
        start_state_root: &B256,
        prev_tx_hashes: &[B256],
        tx_hash: &B256,
    ) -> Option<Result<ExecutionOutcome<Receipt>, InsertBlockErrorKind>> {
        None
    }
}

pub struct CachedExecutor<E, C> {
    executor: E,
    cached_execution_provider: C,
    txs: Vec<B256>,
}

impl<E, C> CachedExecutor<E, C> {
    pub fn new(executor: E, cached_execution_provider: C, txs: Vec<B256>) -> Self {
        Self { executor, cached_execution_provider, txs }
    }
}

impl<E, C> BlockExecutor for CachedExecutor<E, C>
where
    E: BlockExecutor,
    C: CachedExecutionProvider<E::Receipt>,
{
    type Transaction = E::Transaction;
    type Receipt = E::Receipt;
    type Evm = E::Evm;

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<ResultAndState<<Self::Evm as Evm>::HaltReason>, BlockExecutionError> {
        self.executor.execute_transaction_without_commit(tx)
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
