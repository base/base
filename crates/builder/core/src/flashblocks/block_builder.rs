use std::sync::Arc;

use alloy_evm::{Database, Evm};
use alloy_primitives::B256;
use base_common_flashblocks::{FlashblockId, FlashblocksPayloadV1};
use base_execution_payload_builder::BaseBuiltPayload;
use parking_lot::Mutex;
use reth_evm::{ConfigureEvm, OnStateHook, execute::BlockBuilder};
use reth_node_api::PayloadBuilderError;
use reth_provider::{
    HashedPostStateProvider, ProviderError, StateRootProvider, StorageRootProvider,
};
use reth_revm::State;
use reth_trie::updates::TrieUpdates;
use reth_trie_parallel::state_root_task::{PayloadStateRootHandle, StateRootUpdateHook};
use revm::state::EvmState;
use tracing::{debug, warn};

use crate::{
    ExecutionInfo, PayloadTxsBounds, ResourceLimits,
    flashblocks::{FlashblockDiagnostics, context::BasePayloadBuilderCtx, payload::build_block},
};

/// Forwards state updates from disposable EVM builders without ending the sparse-trie stream.
///
/// Dropping this forwarder does not finish the stream; only dropping the inner
/// [`StateRootUpdateHook`] does.
#[derive(Clone)]
pub(crate) struct SharedStateRootHook {
    inner: Arc<Mutex<Option<StateRootUpdateHook>>>,
}

impl OnStateHook for SharedStateRootHook {
    fn on_state(&mut self, state: EvmState) {
        if let Some(hook) = self.inner.lock().as_mut() {
            hook.on_state(state);
        }
    }
}

/// Flashblocks-specific block builder preserving a state-root stream across flashblock batches.
pub(crate) struct FlashblocksBlockBuilder<'a, DB> {
    state: &'a mut State<DB>,
    info: ExecutionInfo,
    state_root_handle: Option<PayloadStateRootHandle>,
    state_root_hook: Option<Arc<Mutex<Option<StateRootUpdateHook>>>>,
}

impl<'a, DB> FlashblocksBlockBuilder<'a, DB> {
    /// Creates a builder that accumulates all execution state for one payload job.
    pub(crate) fn new(
        state: &'a mut State<DB>,
        mut state_root_handle: Option<PayloadStateRootHandle>,
    ) -> Self {
        let state_root_hook = state_root_handle
            .as_mut()
            .map(|handle| Arc::new(Mutex::new(Some(handle.take_state_hook()))));

        Self { state, info: ExecutionInfo::default(), state_root_handle, state_root_hook }
    }

    /// Returns the accumulated execution information.
    pub(crate) const fn info(&self) -> &ExecutionInfo {
        &self.info
    }

    /// Returns mutable accumulated execution information.
    pub(crate) const fn info_mut(&mut self) -> &mut ExecutionInfo {
        &mut self.info
    }

    /// Returns the underlying mutable execution state.
    pub(crate) const fn state_mut(&mut self) -> &mut State<DB> {
        self.state
    }

    /// Returns whether this build streams state updates to a sparse-trie task.
    pub(crate) const fn has_sparse_state_root(&self) -> bool {
        self.state_root_hook.is_some()
    }

    /// Applies pre-execution changes and executes payload-attribute transactions.
    pub(crate) fn execute_pre_steps(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
    ) -> Result<(), PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + std::fmt::Debug + revm::Database,
    {
        self.apply_pre_execution_changes(ctx)?;
        let state_root_hook = self.shared_state_root_hook();
        self.info = ctx.execute_sequencer_transactions(self.state, state_root_hook.as_ref())?;
        Ok(())
    }

    fn apply_pre_execution_changes(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
    ) -> Result<(), PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + std::fmt::Debug + revm::Database,
    {
        let state_root_hook = self.shared_state_root_hook();
        let mut builder = ctx
            .evm_config
            .builder_for_next_block(self.state, ctx.parent(), ctx.block_env_attributes.clone())
            .map_err(PayloadBuilderError::other)?;

        if let Some(state_root_hook) = state_root_hook {
            builder.evm_mut().db_mut().set_state_hook(Some(Box::new(state_root_hook)));
        }
        builder.apply_pre_execution_changes()?;
        builder.evm_mut().db_mut().set_state_hook(None);
        Ok(())
    }

    /// Executes a single flashblock batch of best transactions.
    pub(crate) fn execute_best_transactions(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
        best_txs: &mut impl PayloadTxsBounds,
        limits: &ResourceLimits,
    ) -> Result<FlashblockDiagnostics, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let state_root_hook = self.shared_state_root_hook();
        ctx.execute_best_transactions(
            &mut self.info,
            self.state,
            best_txs,
            limits,
            state_root_hook.as_ref(),
        )
    }

    /// Builds a publishable payload and flashblock from the current execution state.
    pub(crate) fn build_payload<P>(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
        calculate_state_root: bool,
        prev_flashblock_id: FlashblockId,
    ) -> Result<
        (BaseBuiltPayload, FlashblocksPayloadV1, Vec<base_execution_txpool::AccountStateDiff>),
        PayloadBuilderError,
    >
    where
        DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        build_block(self.state, ctx, &mut self.info, prev_flashblock_id, calculate_state_root, None)
    }

    /// Builds a final payload, consuming the sparse-trie root when available.
    pub(crate) fn build_payload_with_state_root<P>(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
        prev_flashblock_id: FlashblockId,
    ) -> Result<
        (BaseBuiltPayload, FlashblocksPayloadV1, Vec<base_execution_txpool::AccountStateDiff>),
        PayloadBuilderError,
    >
    where
        DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        let state_root_precomputed = self.precomputed_state_root(ctx);
        build_block(
            self.state,
            ctx,
            &mut self.info,
            prev_flashblock_id,
            true,
            state_root_precomputed,
        )
    }

    /// Finalizes the payload and flushes block-scoped rejected transactions.
    pub(crate) fn finish<P>(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
    ) -> Result<BaseBuiltPayload, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        let (payload, _, _) = self.build_payload_with_state_root(ctx, FlashblockId::default())?;
        ctx.flush_rejected_txs(self.info_mut());
        Ok(payload)
    }

    fn shared_state_root_hook(&self) -> Option<SharedStateRootHook> {
        self.state_root_hook.as_ref().map(|inner| SharedStateRootHook { inner: Arc::clone(inner) })
    }

    fn precomputed_state_root(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
    ) -> Option<(B256, TrieUpdates)> {
        let mut state_root_handle = self.state_root_handle.take()?;
        let state_root_hook = self.state_root_hook.take()?;
        drop(state_root_hook.lock().take());

        match state_root_handle.state_root() {
            Ok(outcome) => {
                debug!(
                    target: "payload_builder",
                    block_number = ctx.block_number(),
                    state_root = ?outcome.state_root,
                    "received state root from sparse trie"
                );
                Some((outcome.state_root, Arc::unwrap_or_clone(outcome.trie_updates)))
            }
            Err(error) => {
                warn!(
                    target: "payload_builder",
                    block_number = ctx.block_number(),
                    error = %error,
                    "sparse trie state root failed; falling back to synchronous state root"
                );
                None
            }
        }
    }
}
