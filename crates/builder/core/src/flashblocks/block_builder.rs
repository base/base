use std::sync::Arc;

use alloy_evm::{
    Database,
    block::{BlockExecutor, StateChangeSource},
};
use alloy_primitives::B256;
use base_execution_payload_builder::BaseBuiltPayload;
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_node_api::PayloadBuilderError;
use reth_provider::{
    HashedPostStateProvider, ProviderError, StateRootProvider, StorageRootProvider,
};
use reth_revm::State;
use reth_trie::updates::TrieUpdates;
use reth_trie_parallel::state_root_task::{StateRootHandle, StateRootMessage};
use revm::state::EvmState;
use tracing::{debug, warn};

use crate::{
    ExecutionInfo, PayloadTxsBounds, ResourceLimits,
    flashblocks::{
        FlashblockDiagnostics,
        context::BasePayloadBuilderCtx,
        payload::{BuildBlockOutput, build_block},
    },
};

/// Flashblocks-specific block builder that mirrors reth's `BlockBuilder` lifecycle while
/// preserving Base builder behavior that is layered around transaction execution.
///
/// This is intentionally not an implementation of reth's `BlockBuilder` trait. Reth's builder
/// owns one continuous executor and finishes exactly once. Base flashblocks need to publish a
/// fallback payload and then repeated intermediate payloads before finalization, while FBAL wraps
/// the underlying `State` separately around sequencer and pool transaction execution. Keep this
/// type close to reth's lifecycle shape, but document any divergence here so future reth ports can
/// decide whether a difference is still required.
pub(crate) struct FlashblocksBlockBuilder<'a, DB> {
    state: &'a mut State<DB>,
    /// Base-specific execution accumulator.
    ///
    /// Reth's `BlockBuilder` tracks executed transactions and receipts in its executor/result
    /// types. Flashblocks also need per-batch FBAL metadata, DA footprint accounting, metering
    /// diagnostics, and rejected transaction audit state, so that state remains in `ExecutionInfo`.
    info: ExecutionInfo,
    /// Sparse-trie handoff from reth's payload job.
    ///
    /// Reth's current payload builder can compute the state root in parallel from streamed state
    /// updates. Flashblocks keep that optimization, but the stream crosses multiple local
    /// execution phases and is finalized only when we build the final payload or the skip path.
    trie_handle: Option<StateRootHandle>,
    state_root_updates: Option<crossbeam_channel::Sender<StateRootMessage>>,
}

impl<'a, DB> FlashblocksBlockBuilder<'a, DB> {
    /// Creates a new flashblocks block builder.
    pub(crate) fn new(state: &'a mut State<DB>, trie_handle: Option<StateRootHandle>) -> Self {
        let state_root_updates = trie_handle.as_ref().map(|handle| handle.updates_tx().clone());
        Self { state, info: ExecutionInfo::default(), trie_handle, state_root_updates }
    }

    /// Returns immutable access to the accumulated execution info.
    pub(crate) const fn info(&self) -> &ExecutionInfo {
        &self.info
    }

    /// Returns mutable access to the accumulated execution info.
    pub(crate) const fn info_mut(&mut self) -> &mut ExecutionInfo {
        &mut self.info
    }

    /// Returns mutable access to the underlying state.
    pub(crate) fn state_mut(&mut self) -> &mut State<DB> {
        self.state
    }

    /// Returns true when a sparse-trie state-root task is attached.
    pub(crate) const fn has_sparse_state_root(&self) -> bool {
        self.state_root_updates.is_some()
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
        // Divergence from reth: payload-attribute transactions are not executed through the
        // temporary `BlockBuilder` above. The Base helper enforces Base-specific sequencer rules
        // such as strict invalid-tx propagation for `no_tx_pool`, deposit receipt handling, DA
        // accounting, and FBAL indexing. Keep future reth transaction-execution ports compatible
        // with those rules before moving this closer to upstream.
        self.info =
            ctx.execute_sequencer_transactions(self.state, self.state_root_updates.as_ref())?;
        Ok(())
    }

    fn apply_pre_execution_changes(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
    ) -> Result<(), PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + std::fmt::Debug + revm::Database,
    {
        // This is the closest point to upstream reth's `apply_pre_execution_changes`: use the
        // real reth/Base EVM block builder for system calls and hardfork pre-block work.
        let mut builder = ctx
            .evm_config
            .builder_for_next_block(self.state, ctx.parent(), ctx.block_env_attributes.clone())
            .map_err(PayloadBuilderError::other)?;
        if let Some(sender) = &self.state_root_updates {
            // Install a plain streaming hook instead of `StateRootHandle::state_hook()`. This
            // temporary reth builder is dropped after pre-execution work, but the sparse-trie stream
            // must stay open for the block's actual transactions: typically flashblock batches, plus
            // any transactions supplied by the FCU.
            let sender = sender.clone();
            builder.executor_mut().set_state_hook(Some(Box::new(
                move |source: StateChangeSource, state: &EvmState| {
                    let _ =
                        sender.send(StateRootMessage::StateUpdate(source.into(), state.clone()));
                },
            )));
        }
        builder.apply_pre_execution_changes()?;
        builder.executor_mut().set_state_hook(None);
        drop(builder);

        Ok(())
    }

    /// Executes best transactions for a single flashblock batch.
    pub(crate) fn execute_best_transactions(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
        best_txs: &mut impl PayloadTxsBounds,
        limits: &ResourceLimits,
    ) -> Result<FlashblockDiagnostics, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        // Divergence from reth: flashblocks execute the pool in timer-bounded batches and wrap
        // the DB with `FBALBuilderDb` for each batch. The context helper owns Base policy
        // decisions around bundles, metering, DA limits, rejected-tx audit records, and FBAL
        // merging. This builder owns the lifecycle and state-root stream around that helper.
        ctx.execute_best_transactions(
            &mut self.info,
            self.state,
            best_txs,
            limits,
            self.state_root_updates.as_ref(),
        )
    }

    /// Builds a publishable payload/flashblock pair from the current execution state.
    pub(crate) fn build_payload<P>(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
        calculate_state_root: bool,
    ) -> Result<BuildBlockOutput, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        // Reth normally calls `finish` once and computes the final block root. Flashblocks call
        // this repeatedly to publish fallback/intermediate payloads. Those calls must preserve
        // transition state for future batches and must not consume the sparse-trie root handle.
        build_block(self.state, ctx, &mut self.info, calculate_state_root, None)
    }

    /// Builds a payload that needs a final state root, consuming the sparse-trie root if available.
    pub(crate) fn build_payload_with_state_root<P>(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
    ) -> Result<BuildBlockOutput, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        // Used for `no_tx_pool`, zero-flashblock, and finalization paths. These are the only paths
        // that should consume the sparse-trie root, because the handle represents all streamed
        // state updates for this payload build.
        let state_root_precomputed = self.precomputed_state_root(ctx);
        build_block(self.state, ctx, &mut self.info, true, state_root_precomputed)
    }

    /// Finalizes the block and returns the final built payload.
    pub(crate) fn finish<P>(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
    ) -> Result<BaseBuiltPayload, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        let (payload, _) = self.build_payload_with_state_root(ctx)?;
        // Rejected transactions are emitted once per final block. Intermediate flashblock payloads
        // may still be superseded by later batches, so flushing earlier would duplicate or leak
        // audit records for a not-yet-final execution view.
        ctx.flush_rejected_txs(&mut self.info);
        Ok(payload)
    }

    fn precomputed_state_root(
        &mut self,
        ctx: &BasePayloadBuilderCtx,
    ) -> Option<(B256, TrieUpdates)> {
        let mut handle = self.trie_handle.take()?;
        if let Some(sender) = &self.state_root_updates {
            // The sparse-trie worker only knows it has a complete block after this signal. All
            // Base-specific execution paths above must have sent their `StateUpdate`s before this.
            let _ = sender.send(StateRootMessage::FinishedStateUpdates);
        }

        match handle.state_root() {
            Ok(outcome) => {
                debug!(
                    target: "payload_builder",
                    block_number = ctx.block_number(),
                    state_root = ?outcome.state_root,
                    "Received state root from sparse trie"
                );
                Some((outcome.state_root, Arc::unwrap_or_clone(outcome.trie_updates)))
            }
            Err(err) => {
                warn!(
                    target: "payload_builder",
                    block_number = ctx.block_number(),
                    %err,
                    "Sparse trie state root failed; falling back to synchronous state root"
                );
                None
            }
        }
    }
}
