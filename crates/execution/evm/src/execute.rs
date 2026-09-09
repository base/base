//! Traits for execution.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use alloy_eip7928::{BlockAccessList, compute_block_access_list_hash};
use alloy_eips::eip2718::WithEncoded;
use alloy_primitives::{Address, B256};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope, BlockHeader};
pub use base_evm_handler::{
    BlockExecutionError, BlockExecutor, BlockExecutorFactory, BlockValidationError, GasOutput,
    InternalBlockExecutionError,
};
use base_evm_handler::{
    CommitChanges, Evm, EvmEnv, EvmFactory, ExecutableTxParts, RecoveredTx, ToTxEnv,
};
use base_evm_handler::{
    database::{BundleRetention, BundleState, State},
    state::bal::Bal,
};
use reth_execution_types::BlockExecutionResult;
pub use reth_execution_types::{BlockExecutionOutput, ExecutionOutcome};
use reth_primitives_traits::{Recovered, RecoveredBlock, SealedHeader};
use reth_storage_api::StateProvider;
pub use reth_storage_errors::provider::ProviderError;
use reth_trie_common::{HashedPostState, updates::TrieUpdates};

use crate::{Database, OnStateHook, TxEnvFor};

/// A type that knows how to execute a block. It is assumed to operate on a
/// [`crate::Evm`] internally and use [`State`] as database.
pub trait Executor<DB: Database>: Sized {
    /// The error type returned by the executor.
    type Error;

    /// Executes a single block and returns [`BlockExecutionResult`], without the state changes.
    fn execute_one(
        &mut self,
        block: &RecoveredBlock,
    ) -> Result<BlockExecutionResult<BaseReceipt>, Self::Error>;

    /// Executes the EVM with the given input and accepts a state hook closure that is invoked with
    /// the EVM state after execution.
    fn execute_one_with_state_hook<F>(
        &mut self,
        block: &RecoveredBlock,
        state_hook: F,
    ) -> Result<BlockExecutionResult<BaseReceipt>, Self::Error>
    where
        F: OnStateHook + 'static;

    /// Consumes the type and executes the block.
    ///
    /// # Note
    /// Execution happens without any validation of the output.
    ///
    /// # Returns
    /// The output of the block execution.
    fn execute(mut self, block: &RecoveredBlock) -> Result<BlockExecutionOutput, Self::Error> {
        let result = self.execute_one(block)?;
        let mut state = self.into_state();
        Ok(BlockExecutionOutput { state: state.take_bundle(), result })
    }

    /// Executes multiple inputs in the batch, and returns an aggregated [`ExecutionOutcome`].
    fn execute_batch<'a, I>(mut self, blocks: I) -> Result<ExecutionOutcome, Self::Error>
    where
        I: IntoIterator<Item = &'a RecoveredBlock>,
    {
        let blocks_iter = blocks.into_iter();
        let capacity = blocks_iter.size_hint().0;
        let mut results = Vec::with_capacity(capacity);
        let mut first_block = None;
        for block in blocks_iter {
            if first_block.is_none() {
                first_block = Some(block.header().number());
            }
            results.push(self.execute_one(block)?);
        }

        Ok(ExecutionOutcome::from_blocks(
            first_block.unwrap_or_default(),
            self.into_state().take_bundle(),
            results,
        ))
    }

    /// Executes the EVM with the given input and accepts a state closure that is invoked with
    /// the EVM state after execution.
    fn execute_with_state_closure<F>(
        mut self,
        block: &RecoveredBlock,
        mut f: F,
    ) -> Result<BlockExecutionOutput, Self::Error>
    where
        F: FnMut(&State<DB>),
    {
        let result = self.execute_one(block)?;
        let mut state = self.into_state();
        f(&state);
        Ok(BlockExecutionOutput { state: state.take_bundle(), result })
    }

    /// Executes the EVM with the given input and accepts a state closure that is always invoked
    /// with the EVM state after execution, even after failure.
    fn execute_with_state_closure_always<F>(
        mut self,
        block: &RecoveredBlock,
        mut f: F,
    ) -> Result<BlockExecutionOutput, Self::Error>
    where
        F: FnMut(&State<DB>),
    {
        let result = self.execute_one(block);
        let mut state = self.into_state();
        f(&state);

        Ok(BlockExecutionOutput { state: state.take_bundle(), result: result? })
    }

    /// Executes the EVM with the given input and accepts a state hook closure that is invoked with
    /// the EVM state after execution.
    fn execute_with_state_hook<F>(
        mut self,
        block: &RecoveredBlock,
        state_hook: F,
    ) -> Result<BlockExecutionOutput, Self::Error>
    where
        F: OnStateHook + 'static,
    {
        let result = self.execute_one_with_state_hook(block, state_hook)?;
        let mut state = self.into_state();
        Ok(BlockExecutionOutput { state: state.take_bundle(), result })
    }

    /// Consumes the executor and returns the [`State`] containing all state changes.
    fn into_state(self) -> State<DB>;

    /// The size hint of the batch's tracked state size.
    ///
    /// This is used to optimize DB commits depending on the size of the state.
    fn size_hint(&self) -> usize;

    /// Takes built [`BlockAccessList`] from executor.
    fn take_bal(&mut self) -> Option<BlockAccessList>;
}

/// Input for block building. Consumed by [`crate::BaseBlockAssembler`].
///
/// This struct contains all the data needed by the [`crate::BaseBlockAssembler`] to create
/// a complete block after transaction execution.
///
/// # Fields Overview
///
/// - `evm_env`: The EVM configuration used during execution (spec ID, block env, etc.)
/// - `execution_ctx`: Additional context like withdrawals and ommers
/// - `parent`: The parent block header this block builds on
/// - `transactions`: All transactions that were successfully executed
/// - `output`: Execution results including receipts and gas used
/// - `bundle_state`: Accumulated state changes from all transactions
/// - `state_provider`: Access to the current state for additional lookups
/// - `state_root`: The calculated state root after all changes
/// - `block_access_list_hash`: Block access list hash (EIP-7928, Amsterdam)
///
/// # Usage
///
/// This is typically created internally by [`BlockBuilder::finish`] after all
/// transactions have been executed:
///
/// ```rust,ignore
/// let input = BlockAssemblerInput {
///     evm_env: builder.evm_env(),
///     execution_ctx: builder.context(),
///     parent: &parent_header,
///     transactions: executed_transactions,
///     output: &execution_result,
///     bundle_state: &state_changes,
///     state_provider: &state,
///     state_root: calculated_root,
///     block_access_list_hash: Some(calculated_bal_hash),
/// };
///
/// let block = assembler.assemble_block(input)?;
/// ```
#[derive(derive_more::Debug)]
#[non_exhaustive]
pub struct BlockAssemblerInput<'a, 'b> {
    /// Configuration of EVM used when executing the block.
    ///
    /// Contains context relevant to EVM such as [`base_evm_context::BlockEnv`].
    pub evm_env: EvmEnv<base_common_evm::BaseSpecId>,
    /// [`BlockExecutorFactory::ExecutionCtx`] used to execute the block.
    pub execution_ctx: base_common_evm::BaseBlockExecutionCtx,
    /// Parent block header.
    pub parent: &'a SealedHeader,
    /// Transactions that were executed in this block.
    pub transactions: Vec<BaseTxEnvelope>,
    /// Output of block execution.
    pub output: &'b BlockExecutionResult<BaseReceipt>,
    /// [`BundleState`] after the block execution.
    pub bundle_state: &'a BundleState,
    /// Provider with access to state.
    #[debug(skip)]
    pub state_provider: &'b dyn StateProvider,
    /// State root for this block.
    pub state_root: B256,
    /// Block access list hash (EIP-7928, Amsterdam).
    pub block_access_list_hash: Option<B256>,
}

impl<'a, 'b> BlockAssemblerInput<'a, 'b> {
    /// Creates a new [`BlockAssemblerInput`].
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        evm_env: EvmEnv<base_common_evm::BaseSpecId>,
        execution_ctx: base_common_evm::BaseBlockExecutionCtx,
        parent: &'a SealedHeader,
        transactions: Vec<BaseTxEnvelope>,
        output: &'b BlockExecutionResult<BaseReceipt>,
        bundle_state: &'a BundleState,
        state_provider: &'b dyn StateProvider,
        state_root: B256,
        block_access_list_hash: Option<B256>,
    ) -> Self {
        Self {
            evm_env,
            execution_ctx,
            parent,
            transactions,
            output,
            bundle_state,
            state_provider,
            state_root,
            block_access_list_hash,
        }
    }
}

/// Output of block building.
#[derive(Debug, Clone)]
pub struct BlockBuilderOutcome {
    /// Result of block execution.
    pub execution_result: BlockExecutionResult<BaseReceipt>,
    /// Hashed state after execution.
    pub hashed_state: HashedPostState,
    /// Trie updates collected during state root calculation.
    pub trie_updates: TrieUpdates,
    /// The built block.
    pub block: RecoveredBlock,
    /// Block access list built during execution (EIP-7928, Amsterdam).
    pub block_access_list: Option<BlockAccessList>,
}

/// A type that knows how to execute and build a block.
///
/// It wraps an inner [`BlockExecutor`] and provides a way to execute transactions and
/// construct a block.
///
/// This is a helper to erase `BasicBlockBuilder` type.
pub trait BlockBuilder {
    /// Inner [`BlockExecutor`].
    type Executor: BlockExecutor<Transaction = BaseTxEnvelope, Receipt = BaseReceipt>;

    /// Invokes [`BlockExecutor::apply_pre_execution_changes`].
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError>;

    /// Invokes [`BlockExecutor::execute_transaction_with_commit_condition`] and saves the
    /// transaction in internal state only if the transaction was committed.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(&<Self::Executor as BlockExecutor>::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError>;

    /// Invokes [`BlockExecutor::execute_transaction_with_result_closure`] and saves the
    /// transaction in internal state.
    fn execute_transaction_with_result_closure(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(&<Self::Executor as BlockExecutor>::Result),
    ) -> Result<GasOutput, BlockExecutionError> {
        self.execute_transaction_with_commit_condition(tx, |res| {
            f(res);
            CommitChanges::Yes
        })
        .map(Option::unwrap_or_default)
    }

    /// Invokes [`BlockExecutor::execute_transaction`] and saves the transaction in
    /// internal state.
    fn execute_transaction(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
    ) -> Result<GasOutput, BlockExecutionError> {
        self.execute_transaction_with_result_closure(tx, |_| ())
    }

    /// Completes the block building process and returns the [`BlockBuilderOutcome`].
    ///
    /// When `state_root_precomputed` is `None`, the state root is computed internally via
    /// `state_root_with_updates()`. When `Some`, the provided root and trie updates are used
    /// directly, skipping the expensive computation (e.g. when using the sparse trie pipeline).
    fn finish(
        self,
        state_provider: impl StateProvider,
        state_root_precomputed: Option<(B256, TrieUpdates)>,
    ) -> Result<BlockBuilderOutcome, BlockExecutionError>;

    /// Provides mutable access to the inner [`BlockExecutor`].
    fn executor_mut(&mut self) -> &mut Self::Executor;

    /// Provides access to the inner [`BlockExecutor`].
    fn executor(&self) -> &Self::Executor;

    /// Helper to access inner [`BlockExecutor::Evm`] mutably.
    fn evm_mut(&mut self) -> &mut <Self::Executor as BlockExecutor>::Evm {
        self.executor_mut().evm_mut()
    }

    /// Helper to access inner [`BlockExecutor::Evm`].
    fn evm(&self) -> &<Self::Executor as BlockExecutor>::Evm {
        self.executor().evm()
    }

    /// Consumes the type and returns the underlying [`BlockExecutor`].
    fn into_executor(self) -> Self::Executor;
}

/// A type that constructs a block from transactions and execution results.
#[derive(Debug)]
pub struct BasicBlockBuilder<'a, Executor> {
    /// The block executor used to execute transactions.
    pub executor: Executor,
    /// The transactions executed in this block.
    pub transactions: Vec<Recovered<BaseTxEnvelope>>,
    /// The parent block execution context.
    pub ctx: base_common_evm::BaseBlockExecutionCtx,
    /// The sealed parent block header.
    pub parent: &'a SealedHeader,
    /// The assembler used to build the block.
    pub assembler: &'a crate::BaseBlockAssembler,
}

/// Conversions for executable transactions.
pub trait ExecutorTx<Executor: BlockExecutor> {
    /// Converts the transaction into a tuple of [`TxEnvFor`] and [`Recovered`].
    fn into_parts(self) -> (<Executor::Evm as Evm>::Tx, Recovered<Executor::Transaction>);
}

impl<Executor: BlockExecutor> ExecutorTx<Executor>
    for WithEncoded<Recovered<Executor::Transaction>>
{
    fn into_parts(self) -> (<Executor::Evm as Evm>::Tx, Recovered<Executor::Transaction>) {
        (self.to_tx_env(), self.1)
    }
}

impl<Executor: BlockExecutor> ExecutorTx<Executor> for Recovered<Executor::Transaction> {
    fn into_parts(self) -> (<Executor::Evm as Evm>::Tx, Self) {
        (self.to_tx_env(), self)
    }
}

impl<Executor: BlockExecutor> ExecutorTx<Executor>
    for (<Executor::Evm as Evm>::Tx, Recovered<Executor::Transaction>)
{
    fn into_parts(self) -> (<Executor::Evm as Evm>::Tx, Recovered<Executor::Transaction>) {
        self
    }
}

impl<Executor> ExecutorTx<Executor>
    for WithTxEnv<<Executor::Evm as Evm>::Tx, Recovered<Executor::Transaction>>
where
    Executor: BlockExecutor<Transaction: Clone>,
{
    fn into_parts(self) -> (<Executor::Evm as Evm>::Tx, Recovered<Executor::Transaction>) {
        (self.tx_env, Arc::unwrap_or_clone(self.tx))
    }
}

impl<'a, DB, Executor> BlockBuilder for BasicBlockBuilder<'a, Executor>
where
    Executor: BlockExecutor<
            Evm: Evm<
                Spec = <<crate::BaseExecutorFactory as BlockExecutorFactory>::EvmFactory as EvmFactory>::Spec,
                HaltReason = <<crate::BaseExecutorFactory as BlockExecutorFactory>::EvmFactory as EvmFactory>::HaltReason,
                BlockEnv = <<crate::BaseExecutorFactory as BlockExecutorFactory>::EvmFactory as EvmFactory>::BlockEnv,
                DB = &'a mut State<DB>,
            >,
            Transaction = BaseTxEnvelope,
            Receipt = BaseReceipt,
        >,
    DB: Database + 'a,
{
    type Executor = Executor;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.executor.apply_pre_execution_changes()?;
        self.executor.evm_mut().db_mut().bump_bal_index();

        Ok(())
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(&<Self::Executor as BlockExecutor>::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let (tx_env, tx) = tx.into_parts();
        if let Some(gas_used) =
            self.executor.execute_transaction_with_commit_condition((tx_env, &tx), f)?
        {
            self.transactions.push(tx);
            self.executor.evm_mut().db_mut().bump_bal_index();
            Ok(Some(gas_used))
        } else {
            Ok(None)
        }
    }

    fn finish(
        self,
        state: impl StateProvider,
        state_root_precomputed: Option<(B256, TrieUpdates)>,
    ) -> Result<BlockBuilderOutcome, BlockExecutionError> {
        let (evm, result) = self.executor.finish()?;
        let (db, evm_env) = evm.finish();

        // merge all transitions into bundle state
        db.merge_transitions(BundleRetention::Reverts);

        let block_access_list = db.take_built_alloy_bal();
        let block_access_list_hash =
            block_access_list.as_ref().map(|bal| compute_block_access_list_hash(bal.as_slice()));

        let hashed_state =
            state.hashed_post_state(&db.bundle_state).map_err(BlockExecutionError::other)?;
        let (state_root, trie_updates) = match state_root_precomputed {
            Some(precomputed) => precomputed,
            None => state
                .state_root_with_updates(hashed_state.clone())
                .map_err(BlockExecutionError::other)?,
        };

        let (transactions, senders) =
            self.transactions.into_iter().map(|tx| tx.into_parts()).unzip();

        let block = self.assembler.assemble_block(BlockAssemblerInput {
            evm_env,
            execution_ctx: self.ctx,
            parent: self.parent,
            transactions,
            output: &result,
            bundle_state: &db.bundle_state,
            state_provider: &state,
            state_root,
            block_access_list_hash,
        })?;

        let block = RecoveredBlock::new_unhashed(block, senders);

        Ok(BlockBuilderOutcome {
            execution_result: result,
            hashed_state,
            trie_updates,
            block,
            block_access_list,
        })
    }

    fn executor_mut(&mut self) -> &mut Self::Executor {
        &mut self.executor
    }

    fn executor(&self) -> &Self::Executor {
        &self.executor
    }

    fn into_executor(self) -> Self::Executor {
        self.executor
    }
}

/// A generic block executor that uses a [`BlockExecutor`] to
/// execute blocks.
#[expect(missing_debug_implementations)]
pub struct BasicBlockExecutor<DB> {
    /// Block execution strategy.
    pub(crate) strategy_factory: crate::BaseEvmConfig,
    /// Database.
    pub(crate) db: State<DB>,
}

impl<DB: Database> BasicBlockExecutor<DB> {
    /// Creates a new `BasicBlockExecutor` with the given strategy.
    pub fn new(strategy_factory: crate::BaseEvmConfig, db: DB) -> Self {
        let db = State::builder().with_database(db).with_bundle_update().build();
        Self { strategy_factory, db }
    }
}

impl<DB> Executor<DB> for BasicBlockExecutor<DB>
where
    DB: Database,
{
    type Error = BlockExecutionError;

    fn execute_one(
        &mut self,
        block: &RecoveredBlock,
    ) -> Result<BlockExecutionResult<BaseReceipt>, Self::Error> {
        let mut executor = self
            .strategy_factory
            .executor_for_block(&mut self.db, block)
            .map_err(BlockExecutionError::other)?;

        let has_bal = block.header().block_access_list_hash().is_some();

        if has_bal {
            executor.evm_mut().db_mut().bal_state.bal_builder = Some(Bal::new());
        } else {
            executor.evm_mut().db_mut().bal_state.bal_builder = None;
        }

        executor.apply_pre_execution_changes()?;

        if has_bal {
            executor.evm_mut().db_mut().bump_bal_index();
        }

        for tx in block.transactions_recovered() {
            executor.execute_transaction(tx)?;
            if has_bal {
                executor.evm_mut().db_mut().bump_bal_index();
            }
        }

        let result = executor.apply_post_execution_changes()?;

        self.db.merge_transitions(BundleRetention::Reverts);

        Ok(result)
    }

    fn execute_one_with_state_hook<H>(
        &mut self,
        block: &RecoveredBlock,
        state_hook: H,
    ) -> Result<BlockExecutionResult<BaseReceipt>, Self::Error>
    where
        H: OnStateHook + 'static,
    {
        let mut executor = self
            .strategy_factory
            .executor_for_block(&mut self.db, block)
            .map_err(BlockExecutionError::other)?;

        executor.evm_mut().db_mut().set_state_hook(Some(Box::new(state_hook)));

        let result = executor.execute_block(block.transactions_recovered());

        self.db.set_state_hook(None);
        self.db.merge_transitions(BundleRetention::Reverts);

        result
    }

    fn into_state(self) -> State<DB> {
        self.db
    }

    fn size_hint(&self) -> usize {
        self.db.bundle_state.size_hint()
    }

    fn take_bal(&mut self) -> Option<BlockAccessList> {
        self.db.take_built_alloy_bal()
    }
}

/// A helper trait marking a 'static type that can be converted into an [`ExecutableTxParts`] for
/// block executor.
pub trait ExecutableTxFor:
    ExecutableTxParts<TxEnvFor, BaseTxEnvelope> + RecoveredTx<BaseTxEnvelope>
{
}

impl<T> ExecutableTxFor for T where
    T: ExecutableTxParts<TxEnvFor, BaseTxEnvelope> + RecoveredTx<BaseTxEnvelope>
{
}

/// A transaction stored together with its `TxEnv`.
///
/// See also [`ExecutableTxParts`] for types that can be split into a transaction environment and
/// recovered transaction.
#[derive(Debug)]
pub struct WithTxEnv<TxEnv, T> {
    /// The transaction environment for EVM.
    pub tx_env: TxEnv,
    /// The recovered transaction.
    pub tx: Arc<T>,
}

impl<TxEnv, T> WithTxEnv<TxEnv, T> {
    /// Creates a transaction/environment pair from a type that can be split with
    /// [`ExecutableTxParts::into_parts`].
    pub fn new<Tx, InnerTx>(tx: Tx) -> Self
    where
        Tx: ExecutableTxParts<TxEnv, InnerTx, Recovered = T>,
    {
        let (tx_env, tx) = tx.into_parts();
        Self { tx_env, tx: Arc::new(tx) }
    }
}

impl<TxEnv: Clone, T> Clone for WithTxEnv<TxEnv, T> {
    fn clone(&self) -> Self {
        Self { tx_env: self.tx_env.clone(), tx: self.tx.clone() }
    }
}

impl<TxEnv, Tx, T: RecoveredTx<Tx>> RecoveredTx<Tx> for WithTxEnv<TxEnv, T> {
    fn tx(&self) -> &Tx {
        self.tx.tx()
    }

    fn signer(&self) -> &Address {
        self.tx.signer()
    }
}

impl<TxEnv, T: RecoveredTx<Tx>, Tx> ExecutableTxParts<TxEnv, Tx> for WithTxEnv<TxEnv, T> {
    type Recovered = Arc<T>;

    fn into_parts(self) -> (TxEnv, Self::Recovered) {
        (self.tx_env, self.tx)
    }
}

#[cfg(test)]
mod tests {
    use core::marker::PhantomData;

    use base_common_consensus::BaseReceipt;
    use base_evm_handler::database::{CacheDB, EmptyDB};

    use super::*;

    #[derive(Clone, Debug, Default)]
    struct TestExecutorProvider;

    impl TestExecutorProvider {
        fn executor<DB>(&self, _db: DB) -> TestExecutor<DB>
        where
            DB: Database,
        {
            TestExecutor(PhantomData)
        }
    }

    struct TestExecutor<DB>(PhantomData<DB>);

    impl<DB: Database> Executor<DB> for TestExecutor<DB> {
        type Error = BlockExecutionError;

        fn execute_one(
            &mut self,
            _block: &RecoveredBlock,
        ) -> Result<BlockExecutionResult<BaseReceipt>, Self::Error> {
            Err(BlockExecutionError::msg("execution unavailable for tests"))
        }

        fn execute_one_with_state_hook<F>(
            &mut self,
            _block: &RecoveredBlock,
            _state_hook: F,
        ) -> Result<BlockExecutionResult<BaseReceipt>, Self::Error>
        where
            F: OnStateHook + 'static,
        {
            Err(BlockExecutionError::msg("execution unavailable for tests"))
        }

        fn into_state(self) -> State<DB> {
            unreachable!()
        }

        fn size_hint(&self) -> usize {
            0
        }

        fn take_bal(&mut self) -> Option<BlockAccessList> {
            None
        }
    }

    #[test]
    fn test_provider() {
        let provider = TestExecutorProvider;
        let db = CacheDB::<EmptyDB>::default();
        let executor = provider.executor(db);
        let _ = executor.execute(&Default::default());
    }
}
