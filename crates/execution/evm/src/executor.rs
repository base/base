use alloc::{boxed::Box, sync::Arc};

use alloy_consensus::transaction::TxHashRef;
use alloy_eip7928::BlockAccessList;
use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use base_common_chains::{BaseUpgrade, Upgrades};
use base_common_consensus::{BaseReceipt, BaseTransactionSigned};
use base_common_evm2::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseBlockState, BaseEvmTypes, BaseExecutedTx,
    BaseForkActivations, BlockGasLimitExceeded, PreExecutionError,
};
use evm2::{
    Evm, PendingState,
    evm::{Bal, BlockAccessIndex},
    registry::HandlerError,
};
use reth_evm::{
    BlockExecutionError, BlockExecutionOutput, BlockExecutor, BlockValidationError, ExecutorTx,
    GasOutput, InternalBlockExecutionError, RecoveredTx, execute::map_handler_error,
};
use reth_execution_types::{BlockExecutionResult, BlockState, StateUpdate};

use crate::BaseInvalidTxError;

/// Executes Base blocks for Reth on top of the shared [`BaseBlockExecutor`].
///
/// The shared executor owns the Base execution rules; this adapter maps its errors onto Reth's
/// block-execution errors, keeps the block's state as Reth's [`BlockState`], streams per-commit
/// state updates to the installed state hook, and maintains the EIP-7928 block access list index.
#[derive(Debug)]
pub struct BaseRethBlockExecutor<'a, ChainSpec> {
    inner: BaseBlockExecutor<'a, BaseRethBlockState>,
    chain_spec: &'a Arc<ChainSpec>,
}

impl<'a, ChainSpec: Upgrades> BaseRethBlockExecutor<'a, ChainSpec> {
    /// Creates an executor for the block configured in `evm`.
    pub fn new(
        evm: Evm<'a, BaseEvmTypes>,
        ctx: BaseBlockExecutionCtx,
        chain_spec: &'a Arc<ChainSpec>,
    ) -> Self {
        let inner = BaseBlockExecutor::with_block_state(evm, ctx, BaseRethBlockState::default());
        Self { inner, chain_spec }
    }

    /// Points the block access list builder at the next transaction, when it is enabled.
    fn set_transaction_block_access_index(&mut self) {
        let index = self.inner.receipts().len() as u64 + 1;
        let state = self.inner.evm_mut().state_mut();
        if state.bal_builder().is_some() {
            state.set_bal_index(BlockAccessIndex::new(index));
        }
    }
}

impl<'a, ChainSpec: Upgrades> BlockExecutor for BaseRethBlockExecutor<'a, ChainSpec> {
    type Transaction = BaseTransactionSigned;
    type Receipt = BaseReceipt;
    type Evm = Evm<'a, BaseEvmTypes>;
    type TransactionResultWithState = BaseExecutedTx;
    type BlockAccessList = Bal;

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn set_state_hook(&mut self, hook: impl FnMut(StateUpdate) + Send + 'static) -> bool {
        self.inner.block_state_mut().hook = Some(Box::new(hook));
        true
    }

    fn convert_block_access_list(
        block_access_list: &BlockAccessList,
    ) -> Result<Self::BlockAccessList, BlockExecutionError> {
        Bal::try_from(block_access_list.as_slice()).map_err(BlockExecutionError::other)
    }

    fn set_block_access_list(&mut self, block_access_list: Arc<Self::BlockAccessList>) {
        self.inner.evm_mut().state_mut().set_bal(block_access_list);
    }

    fn set_block_access_index(&mut self, index: BlockAccessIndex) {
        self.inner.evm_mut().state_mut().set_bal_index(index);
    }

    fn enable_block_access_list_builder(&mut self) {
        let state = self.inner.evm_mut().state_mut();
        state.enable_bal_builder();
        state.reset_bal_index();
    }

    fn take_block_access_list(&mut self) -> Option<BlockAccessList> {
        self.inner.evm_mut().state_mut().take_bal_builder().map(Into::into)
    }

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution().map_err(pre_execution_error)?;
        self.inner
            .apply_transition_hooks(&UpgradeActivations(self.chain_spec.as_ref()))
            .map_err(map_handler_error)
    }

    fn into_state(self) -> reth_evm::BundleState {
        let (_, _, block_state) = self.inner.finish();
        block_state.state.into_bundle()
    }

    /// Checks a gas limit against the block's remaining gas. Block execution additionally reserves
    /// an EIP-8130 transaction's payer-authentication gas and exempts pre-Regolith deposits.
    fn validate_transaction_gas_limit(
        &mut self,
        gas_limit: u64,
    ) -> Result<(), BlockExecutionError> {
        let block_gas_limit = self.inner.evm().block().gas_limit.saturating_to::<u64>();
        let block_available_gas = block_gas_limit.saturating_sub(self.inner.gas_used());
        if gas_limit > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: gas_limit,
                block_available_gas,
            }
            .into());
        }
        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        transaction: impl ExecutorTx<Self>,
    ) -> Result<Self::TransactionResultWithState, BlockExecutionError> {
        let (tx_env, tx) = transaction.into_parts();
        self.set_transaction_block_access_index();
        self.inner
            .execute_transaction_without_commit(&tx_env)
            .map_err(|error| transaction_error(error, *TxHashRef::tx_hash(tx.tx())))
    }

    fn commit_transaction(
        &mut self,
        output: Self::TransactionResultWithState,
    ) -> Result<GasOutput, BlockExecutionError> {
        self.set_transaction_block_access_index();
        let tx_gas_used =
            self.inner.commit_transaction(output).map_err(BlockExecutionError::other)?;
        Ok(GasOutput::new(tx_gas_used, 0))
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }

    fn finish_with_block_access_list(
        self,
    ) -> Result<(BlockExecutionOutput<Self::Receipt>, Option<BlockAccessList>), BlockExecutionError>
    {
        let (output, bal) = self.finish_with_prepared_block_access_list()?;
        Ok((output, bal.map(Into::into)))
    }

    fn finish_with_prepared_block_access_list(
        mut self,
    ) -> Result<(BlockExecutionOutput<Self::Receipt>, Option<Bal>), BlockExecutionError> {
        let block_access_list = self.inner.evm_mut().state_mut().take_bal_builder();
        let (_, result, block_state) = self.inner.finish();
        let output = BlockExecutionOutput::new(
            BlockExecutionResult {
                receipts: result.receipts,
                requests: Requests::default(),
                gas_used: result.gas_used,
                blob_gas_used: result.blob_gas_used,
            },
            block_state.state.into_bundle(),
        );
        Ok((output, block_access_list))
    }
}

/// The block state of a [`BaseRethBlockExecutor`]: Reth's [`BlockState`], plus the hook that
/// receives each commit's state update.
#[derive(Default)]
pub struct BaseRethBlockState {
    /// The block's account transitions.
    pub state: BlockState,
    /// Receives the state update of every commit, when installed.
    pub hook: Option<Box<dyn FnMut(StateUpdate) + Send>>,
}

impl core::fmt::Debug for BaseRethBlockState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BaseRethBlockState")
            .field("state", &self.state)
            .field("hook", &self.hook.is_some())
            .finish()
    }
}

impl BaseBlockState for BaseRethBlockState {
    fn commit(&mut self, evm: &mut Evm<'_, BaseEvmTypes>, state: PendingState) {
        let mut update = self.hook.is_some().then(StateUpdate::default);
        for changes in state.account_changes() {
            self.state.commit_account_changes(changes, update.as_mut());
        }
        if let (Some(hook), Some(update)) = (self.hook.as_mut(), update)
            && !update.is_empty()
        {
            hook(update);
        }
        evm.state_mut().set_pending_state(state);
        evm.state_mut().commit_transaction();
    }
}

/// A chain spec's Base upgrade schedule, as the transition hooks read it.
#[derive(Debug)]
pub struct UpgradeActivations<'a, ChainSpec>(pub &'a ChainSpec);

impl<ChainSpec: Upgrades> BaseForkActivations for UpgradeActivations<'_, ChainSpec> {
    fn is_active_at_timestamp(&self, upgrade: BaseUpgrade, timestamp: u64) -> bool {
        self.0.fork_condition(upgrade).active_at_timestamp(timestamp)
    }
}

/// Maps a pre-execution error onto the matching Reth block-validation error.
fn pre_execution_error(error: HandlerError) -> BlockExecutionError {
    match error.external_ref::<PreExecutionError>() {
        Some(PreExecutionError::MissingParentBeaconBlockRoot) => {
            BlockValidationError::MissingParentBeaconBlockRoot.into()
        }
        Some(&PreExecutionError::CancunGenesisParentBeaconBlockRootNotZero {
            parent_beacon_block_root,
        }) => BlockValidationError::CancunGenesisParentBeaconBlockRootNotZero {
            parent_beacon_block_root,
        }
        .into(),
        None => map_handler_error(error),
    }
}

/// Maps a transaction's execution error onto Reth's block-execution errors, keeping invalid
/// transactions classifiable through [`reth_evm::InvalidTxError`].
fn transaction_error(error: HandlerError, hash: B256) -> BlockExecutionError {
    if let Some(&BlockGasLimitExceeded { transaction_gas_limit, block_available_gas }) =
        error.external_ref()
    {
        return BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
            transaction_gas_limit,
            block_available_gas,
        }
        .into();
    }
    let error = match BaseInvalidTxError::try_from(error) {
        Ok(error) => {
            return BlockValidationError::InvalidTx { hash, error: Box::new(error) }.into();
        }
        Err(error) => error,
    };
    if let HandlerError::Database(database_error) = &error
        && !database_error.is_fatal()
    {
        BlockValidationError::EVM { hash, error: Box::new(error) }.into()
    } else {
        InternalBlockExecutionError::EVM { hash, error: Box::new(error) }.into()
    }
}
