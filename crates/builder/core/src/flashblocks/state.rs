//! The in-progress block a flashblocks job builds across its flashblocks.

use core::mem;

use base_common_evm2::{BaseBlockExecutionCtx, BaseBlockExecutor, BaseBlockState, BaseEvmTypes};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, BaseEvmEnv, UpgradeActivations};
use evm2::{
    Evm, PendingState,
    evm::{Cache, Database},
};
use reth_evm::{BundleState, ConfigureEvm, execute::map_handler_error};
use reth_execution_types::{BlockState, StateUpdate};
use reth_node_api::PayloadBuilderError;

/// The block a flashblocks job builds: the parent state, the state the block's committed changes
/// produce on top of it, and those changes in commit order.
///
/// Each flashblock seals the block built so far without ending it, so the committed changes are
/// kept and folded into a [`BundleState`] on demand rather than consumed. The block holds no EVM
/// between execution steps, because the build loop awaits between flashblocks and an EVM is not
/// `Send`; [`Self::execute`] opens one over the block's state for each step.
#[derive(Debug)]
pub struct FlashblocksBlock<DB> {
    db: DB,
    cache: Cache,
    changes: CommittedChanges,
}

impl<DB> FlashblocksBlock<DB> {
    /// Returns the state changes committed to the block.
    pub const fn changes(&self) -> &CommittedChanges {
        &self.changes
    }
}

impl<DB: Database> FlashblocksBlock<DB> {
    /// Starts a block on `db`, which must read the parent state.
    pub fn new(db: DB) -> Self {
        Self { db, cache: Cache::default(), changes: CommittedChanges::default() }
    }

    /// Runs `f` on an EVM configured with `evm_env` over the block's state; the changes `f`
    /// commits extend the block.
    pub fn execute<R>(
        &mut self,
        evm_config: &BaseEvmConfig,
        evm_env: BaseEvmEnv,
        f: impl FnOnce(&mut FlashblockState<'_>) -> R,
    ) -> R {
        let mut evm = evm_config.evm_with_env(&mut self.db, evm_env);
        evm.overlay_db_mut().cache = mem::take(&mut self.cache);
        let mut state = FlashblockState { evm, changes: &mut self.changes };
        let output = f(&mut state);
        self.cache = mem::take(&mut state.evm.overlay_db_mut().cache);
        output
    }

    /// Applies the block's pre-execution system calls and Base upgrade transition changes, as
    /// block execution applies them before the first transaction.
    pub fn apply_pre_execution_changes(
        &mut self,
        evm_config: &BaseEvmConfig,
        evm_env: BaseEvmEnv,
        ctx: BaseBlockExecutionCtx,
        chain_spec: &BaseChainSpec,
    ) -> Result<(), PayloadBuilderError> {
        let mut evm = evm_config.evm_with_env(&mut self.db, evm_env);
        evm.overlay_db_mut().cache = mem::take(&mut self.cache);
        let mut executor = BaseBlockExecutor::with_block_state(evm, ctx, &mut self.changes);
        let applied = executor
            .apply_pre_execution()
            .and_then(|()| executor.apply_transition_hooks(&UpgradeActivations(chain_spec)));
        let (mut evm, _, _) = executor.finish();
        self.cache = mem::take(&mut evm.overlay_db_mut().cache);
        applied.map_err(|error| map_handler_error(error).into())
    }
}

/// A [`FlashblocksBlock`] opened on an EVM for one execution step.
#[derive(Debug)]
pub struct FlashblockState<'a> {
    /// The EVM, reading the parent state through every change committed to the block.
    pub evm: Evm<'a, BaseEvmTypes>,
    changes: &'a mut CommittedChanges,
}

impl FlashblockState<'_> {
    /// Commits one transaction's state changes to the block.
    pub fn commit(&mut self, state: PendingState) {
        self.changes.commit(&mut self.evm, state);
    }
}

/// The state changes committed to a block, one [`PendingState`] per commit.
#[derive(Debug, Default)]
pub struct CommittedChanges(Vec<PendingState>);

impl BaseBlockState for &mut CommittedChanges {
    fn commit(&mut self, evm: &mut Evm<'_, BaseEvmTypes>, state: PendingState) {
        self.0.push(state.clone());
        evm.state_mut().set_pending_state(state);
        evm.state_mut().commit_transaction();
    }
}

impl CommittedChanges {
    /// Folds the committed changes into the block's [`BundleState`], with reverts.
    pub fn bundle(&self) -> BundleState {
        let mut block = BlockState::new();
        for state in &self.0 {
            for changes in state.account_changes() {
                block.commit_account_changes(changes, None);
            }
        }
        block.into_bundle()
    }

    /// Returns each commit's state update, in commit order.
    pub fn state_updates(&self) -> Vec<StateUpdate> {
        let mut block = BlockState::new();
        self.0
            .iter()
            .map(|state| {
                let mut update = StateUpdate::default();
                for changes in state.account_changes() {
                    block.commit_account_changes(changes, Some(&mut update));
                }
                update
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Header, transaction::Recovered};
    use alloy_primitives::{Address, B256, TxKind, U256};
    use base_common_consensus::TxDeposit;
    use base_common_evm2::BaseTxEnvelope;
    use evm2::evm::InMemoryDB;
    use reth_evm::Evm as _;
    use reth_primitives_traits::SealedHeader;

    use super::{FlashblockState, FlashblocksBlock};
    use crate::{BasePayloadBuilderCtx, test_utils::chain_spec};

    const RECIPIENT: Address = Address::repeat_byte(0x22);

    fn builder_context() -> BasePayloadBuilderCtx {
        let parent = Header { gas_limit: 30_000_000, ..Default::default() };
        BasePayloadBuilderCtx::for_test(chain_spec(), Arc::new(SealedHeader::seal_slow(parent)))
    }

    /// Executes and commits a deposit minting `mint` wei to [`RECIPIENT`].
    fn commit_mint(state: &mut FlashblockState<'_>, mint: u128) {
        let deposit = TxDeposit {
            source_hash: B256::with_last_byte(mint as u8),
            from: RECIPIENT,
            to: TxKind::Call(RECIPIENT),
            mint,
            gas_limit: 100_000,
            ..Default::default()
        };
        let tx = Recovered::new_unchecked(BaseTxEnvelope::Deposit(deposit), RECIPIENT);
        let executed = state.evm.transact(&tx).expect("deposit executes").detach();
        state.commit(executed.pending_state);
    }

    fn balance(state: &mut FlashblockState<'_>) -> U256 {
        state
            .evm
            .state_db_mut()
            .get_account(&RECIPIENT)
            .expect("in-memory read")
            .map_or(U256::ZERO, |info| info.balance)
    }

    #[test]
    fn changes_carry_across_execution_steps_into_one_block_bundle() {
        let ctx = builder_context();
        let mut block = FlashblocksBlock::new(InMemoryDB::default());

        block.execute(&ctx.evm_config, ctx.evm_env.clone(), |state| commit_mint(state, 10));
        let first_bundle = block.changes().bundle();
        let balance_seen_by_next_step =
            block.execute(&ctx.evm_config, ctx.evm_env.clone(), |state| {
                let balance = balance(state);
                commit_mint(state, 5);
                balance
            });
        let bundle = block.changes().bundle();

        assert_eq!(balance_seen_by_next_step, U256::from(10));
        assert_eq!(
            first_bundle
                .account(&RECIPIENT)
                .and_then(|account| account.info.as_ref())
                .map(|info| info.balance),
            Some(U256::from(10))
        );
        assert_eq!(
            bundle
                .account(&RECIPIENT)
                .and_then(|account| account.info.as_ref())
                .map(|info| info.balance),
            Some(U256::from(15))
        );
        assert_eq!(bundle.reverts.len(), 1, "every commit belongs to one block");
        assert_eq!(block.changes().bundle(), bundle, "folding does not consume the changes");
    }
}
