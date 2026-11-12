//! Deferred parent-commitment adapter with private, ready-only trie publication.
//!
//! This adapter is not installed on ordinary nodes. Experimental launch admission must reject
//! incompatible sync, witness and service configurations before selecting it.

use std::sync::{Arc, Mutex};
use alloy_consensus::Header;
use alloy_primitives::B256;
use base_common_consensus::{BaseBlock, BasePrimitives};
use base_common_rpc_types_engine::ExecutionData;
use base_execution_payload_builder::{BaseBuiltPayload, PostStateRootData, PostStateRootKey, PostStateRootManager, PostStateRootService, PostStateRootSource};
use reth_chain_state::ExecutedBlock;
use reth_engine_tree::tree::{CacheWaitDurations, EngineApiTreeState, EngineValidator, ValidationOutcome, WaitForCaches, error::InsertBlockError, payload_validator::TreeCtx, state_root_strategy::{DefaultStateRootStrategy, LazyHashedPostState, PayloadStateRootJobContext, PreparedStateRootJob, StateRootJob, StateRootJobContext, StateRootJobOutcome, StateRootStrategy}};
use reth_evm::{ConfigureEvm, EvmEnv};
use reth_network_p2p::full_block::SealedBlockWithAccessList;
use reth_payload_builder::PayloadBuilderResources;
use reth_payload_primitives::{BuiltPayloadExecutedBlock, InvalidPayloadAttributesError, NewPayloadError, PayloadTypes};
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use reth_provider::{BlockExecutionOutput, BlockNumReader, ChangeSetReader, DatabaseProviderFactory, DatabaseProviderROFactory, PruneCheckpointReader, StageCheckpointReader, StateRootProvider, StorageChangeSetReader, StorageSettingsCache};
use reth_storage_api::errors::ProviderResult;
use reth_storage_overlay::{OverlayManager, OverlayStateProviderFactory};
use reth_trie_common::HashedPostState;
use reth_trie_parallel::state_root_task::PayloadStateRootHandle;

/// One privately completed validator result, not yet certified for tree insertion.
#[derive(Debug)]
pub struct DeferredStateRootStaged {
    /// Exact sealed identity.
    pub key: PostStateRootKey,
    /// Actual current-state data.
    pub data: PostStateRootData,
}

/// Shared lazy state for one validator and its root strategy.
#[derive(Debug, Default)]
pub struct DeferredStateRootState {
    /// Shared lazy worker and configuration for independent builder/validator execution.
    pub service: Arc<PostStateRootService>,
    /// Only a successful inner validation may consume and publish this data.
    pub staged: Mutex<Option<DeferredStateRootStaged>>,
}

/// Strategy capturing the same public provider and overlay manager as BasicEngineValidator.
#[derive(Debug)]
pub struct DeferredStateRootStrategy<P> {
    /// Stable database provider factory.
    pub provider: P,
    /// Branch-specific overlay ownership shared with the engine.
    pub overlay: OverlayManager<BasePrimitives>,
    /// Lazy worker and private completion slot.
    pub state: Arc<DeferredStateRootState>,
    /// Untouched ordinary stock options and sharing behavior.
    pub stock: DefaultStateRootStrategy,
}

impl<P, Evm> StateRootStrategy<BasePrimitives, P, Evm> for DeferredStateRootStrategy<P>
where
    Evm: ConfigureEvm<Primitives = BasePrimitives> + 'static,
    P: DatabaseProviderFactory + Clone + Send + Sync + 'static,
    P::Provider: BlockNumReader + PruneCheckpointReader + StageCheckpointReader + ChangeSetReader + StorageChangeSetReader + StorageSettingsCache + 'static,
{
    fn prepare(&self, ctx: StateRootJobContext<'_, BasePrimitives, P, Evm>) -> ProviderResult<PreparedStateRootJob<BasePrimitives>> {
        let timestamp = ctx.env().evm_env.block_env().timestamp.saturating_to();
        if !self.state.service.controlled(timestamp, ctx.parent_header().number + 1) {
            return self.stock.prepare(ctx);
        }
        let parent = ctx.parent_header();
        let key = PostStateRootKey { hash: parent.hash(), parent_hash: parent.parent_hash, number: parent.number };
        let factory = OverlayStateProviderFactory::new(self.provider.clone(), self.overlay.overlay_builder(parent.hash()));
        Ok(PreparedStateRootJob::new(Box::new(DeferredStateRootJob { factory, parent: key, state: self.state.clone(), deferred: self.state.service.active(timestamp, ctx.parent_header().number + 1) }), None))
    }

    fn prepare_payload_builder(&self, ctx: PayloadStateRootJobContext<'_, BasePrimitives, P>) -> ProviderResult<Option<PayloadStateRootHandle>> {
        if self.state.service.controlled(ctx.timestamp(), ctx.parent_header().number + 1) { Ok(None) } else { <DefaultStateRootStrategy as StateRootStrategy<BasePrimitives, P, Evm>>::prepare_payload_builder(&self.stock, ctx) }
    }
}

/// Active serial job without stock sparse sharing or header-derived post-state fallbacks.
#[derive(Debug)]
pub struct DeferredStateRootJob<P> {
    /// Pinned parent overlay factory, opened on the provider worker.
    pub factory: OverlayStateProviderFactory<P, BasePrimitives>,
    /// Exact parent identity, independent of height collisions.
    pub parent: PostStateRootKey,
    /// D checks the parent commitment; S/R check the actual current root.
    pub deferred: bool,
    /// Private publication and worker state.
    pub state: Arc<DeferredStateRootState>,
}

impl<P> StateRootJob<BasePrimitives> for DeferredStateRootJob<P>
where
    P: DatabaseProviderFactory + Clone + Send + Sync + 'static,
    P::Provider: BlockNumReader + PruneCheckpointReader + StageCheckpointReader + ChangeSetReader + StorageChangeSetReader + StorageSettingsCache + 'static,
{
    fn name(&self) -> &'static str { "base-deferred-parent-commitment" }

    fn finish(&mut self, block: &RecoveredBlock<BaseBlock>, _output: Arc<BlockExecutionOutput<<BasePrimitives as reth_primitives_traits::NodePrimitives>::Receipt>>, hashed: &LazyHashedPostState) -> ProviderResult<StateRootJobOutcome> {
        let manager = self.state.service.manager()?;
        let parent_root = if !self.deferred { block.state_root } else if let Some(parent) = manager.get(self.parent, PostStateRootSource::Validator)? { parent.root } else {
            let factory = self.factory.clone();
            // Persisted/evicted parent recovery reads actual MPT plus ordered overlays. It never
            // reads delayed parent.header.state_root as an actual root.
            manager.enqueue(Box::new(move || {
                let provider = factory.database_provider_ro()?;
                let root = provider.state_root(HashedPostState::default())?;
                Ok(PostStateRootData::new(root, &HashedPostState::default(), Default::default()))
            }))?.wait()?.root
        };
        if parent_root != block.state_root {
            // The stock equality check must see the computed parent on mismatch, never an echo.
            return Ok(StateRootJobOutcome::new(parent_root, Default::default()));
        }
        let hashed = hashed.get().clone(); let factory = self.factory.clone();
        let data = manager.enqueue(Box::new(move || {
            let provider = factory.database_provider_ro()?;
            let (root, updates) = provider.state_root_with_updates((*hashed).clone())?;
            Ok(PostStateRootData::new(root, &hashed, updates))
        }))?.wait()?;
        if !self.deferred && data.root != block.state_root { return Ok(StateRootJobOutcome::new(data.root, data.updates)); }
        let updates = data.updates.clone();
        let key = PostStateRootKey { hash: block.hash(), parent_hash: block.parent_hash, number: block.number };
        *self.state.staged.lock().map_err(PostStateRootManager::error)? = Some(DeferredStateRootStaged { key, data });
        // This is a commitment-height adapter, not the actual current root. The only successful
        // path here follows independent parent equality and complete current-root materialization.
        Ok(StateRootJobOutcome::new(parent_root, updates))
    }
}

/// Fully delegating validator wrapper that replaces pending trie data before tree admission.
#[derive(Debug)]
pub struct DeferredEngineValidator<V> {
    /// Stock validator with all consensus/layout/execution checks and hooks preserved.
    pub inner: V,
    /// Lazy active-only strategy state.
    pub state: Arc<DeferredStateRootState>,
}

impl<V> DeferredEngineValidator<V> {
    /// Publishes private ready data only after every inner validation check succeeded.
    pub fn publish(&self, outcome: ValidationOutcome<BasePrimitives>) -> ValidationOutcome<BasePrimitives> {
        let mut output = match outcome {
            Ok(output) => output,
            Err(error) => { self.state.staged.lock().map(|mut slot| slot.take()).ok(); return Err(error); }
        };
        let block = output.executed_block.recovered_block();
        if !self.state.service.controlled(block.timestamp, block.number) { return Ok(output); }
        let error = |message: &str| InsertBlockError::new(output.executed_block.sealed_block().clone(), PostStateRootManager::error(message).into());
        let staged = self.state.staged.lock().map_err(|_| error("root staging mutex poisoned"))?.take().ok_or_else(|| error("missing ready current root"))?;
        if staged.key.hash != block.hash() || staged.key.parent_hash != block.parent_hash || staged.key.number != block.number {
            return Err(error("ready root identity mismatch").into());
        }
        self.state.service.manager().and_then(|manager| manager.wait_builder(staged.key).and_then(|()| manager.publish(staged.key, PostStateRootSource::Validator, staged.data.clone())))
            .map_err(|e| InsertBlockError::new(output.executed_block.sealed_block().clone(), e.into()))?;
        // Do not read the stock pending handle. The runtime still owns its redundant sorting task;
        // replacing this handle does not cancel it or remove its cost from measurements.
        output.executed_block = ExecutedBlock::new(output.executed_block.recovered_block.clone(), output.executed_block.execution_output.clone(), staged.data.sorted)
            .with_bal(output.executed_block.bal.clone());
        Ok(output)
    }
}

impl<T, V> EngineValidator<T> for DeferredEngineValidator<V>
where
    T: PayloadTypes<BuiltPayload = BaseBuiltPayload, ExecutionData = ExecutionData>,
    V: EngineValidator<T>,
{
    fn validate_payload_attributes_against_header(&self, attributes: &T::PayloadAttributes, header: &Header) -> Result<(), InvalidPayloadAttributesError> { self.inner.validate_payload_attributes_against_header(attributes, header) }
    fn convert_payload_to_block(&self, payload: ExecutionData) -> Result<SealedBlock<BaseBlock>, NewPayloadError> { self.inner.convert_payload_to_block(payload) }
    fn validate_payload(&mut self, payload: ExecutionData, ctx: TreeCtx<'_, BasePrimitives>) -> ValidationOutcome<BasePrimitives> { let outcome = self.inner.validate_payload(payload, ctx); self.publish(outcome) }
    fn validate_block(&mut self, block: SealedBlockWithAccessList<BaseBlock>, ctx: TreeCtx<'_, BasePrimitives>) -> ValidationOutcome<BasePrimitives> { let outcome = self.inner.validate_block(block, ctx); self.publish(outcome) }
    fn on_inserted_executed_block(&self, block: BuiltPayloadExecutedBlock<BasePrimitives>) -> ProviderResult<ExecutedBlock<BasePrimitives>> {
        if self.state.service.reexecution(block.recovered_block.timestamp, block.recovered_block.number) { return Err(PostStateRootManager::error("trusted insertion is unsupported for deferred state roots")); }
        self.inner.on_inserted_executed_block(block)
    }
    fn on_canonical_head_changed(&self, hash: B256, state: &EngineApiTreeState<BasePrimitives>) { self.inner.on_canonical_head_changed(hash, state); }
    fn payload_builder_resources(&self, parent: B256, header: &Header, timestamp: u64, state: &mut EngineApiTreeState<BasePrimitives>) -> PayloadBuilderResources { self.inner.payload_builder_resources(parent, header, timestamp, state) }
}

impl<V: WaitForCaches> WaitForCaches for DeferredEngineValidator<V> {
    fn wait_for_caches(&self) -> CacheWaitDurations { self.inner.wait_for_caches() }
}

/// Base's engine builder preserving stock hook and prewarming installation.
#[derive(Debug, Clone, Default)]
pub struct BaseDeferredEngineValidatorBuilder<EV> {
    /// Existing payload layout and post-execution validator builder.
    pub payload_validator_builder: EV,
    /// Shared custom-chain service. None delegates root work to the untouched stock strategy.
    pub service: Option<Arc<PostStateRootService>>,
}

impl<Node, EV> reth_node_builder::rpc::EngineValidatorBuilder<Node> for BaseDeferredEngineValidatorBuilder<EV>
where
    Node: reth_node_api::FullNodeComponents<
        Types: crate::BaseNodeTypes,
        Evm: reth_node_api::ConfigureEngineEvm<ExecutionData, Primitives = BasePrimitives>,
    >,
    EV: reth_node_builder::rpc::PayloadValidatorBuilder<Node>,
    EV::Validator: reth_node_api::PayloadValidator<crate::BaseEngineTypes, Block = BaseBlock> + Clone,
{
    type EngineValidator = DeferredEngineValidator<reth_engine_tree::tree::BasicEngineValidator<Node::Provider, Node::Evm, EV::Validator>>;

    async fn build_tree_validator(
        self,
        ctx: &reth_node_api::AddOnsContext<'_, Node>,
        tree_config: reth_engine_tree::tree::TreeConfig,
        overlay_manager: OverlayManager<BasePrimitives>,
    ) -> eyre::Result<Self::EngineValidator> {
        if self.service.as_ref().is_some_and(|service| service.activation.is_some() || service.cohort != base_execution_payload_builder::PostStateRootCohort::Ordinary) {
            if ctx.config.debug.skip_state_root { eyre::bail!("DeferredStateRoot is incompatible with debug.skip-state-root"); }
            if ctx.config.debug.invalid_block_hook.as_ref().is_some_and(|hooks| !hooks.is_empty()) {
                eyre::bail!("DeferredStateRoot is incompatible with configured invalid-block diagnostic hooks, including witness");
            }
        }
        // Delegate construction itself, not a copy of its setup. This preserves the exact
        // stock invalid-block hook, txpool prewarming source and every future stock option.
        let mut inner = reth_node_builder::rpc::BasicEngineValidatorBuilder::new(self.payload_validator_builder)
            .build_tree_validator(ctx, tree_config, overlay_manager.clone()).await?;
        let state = Arc::new(DeferredStateRootState { service: self.service.unwrap_or_default(), ..Default::default() });
        if state.service.activation.is_some() || state.service.cohort != base_execution_payload_builder::PostStateRootCohort::Ordinary {
            inner = inner.with_state_root_strategy(Arc::new(DeferredStateRootStrategy {
                provider: ctx.node.provider().clone(), overlay: overlay_manager,
                state: state.clone(), stock: Default::default(),
            }));
        }
        if state.service.activation.is_some() || state.service.cohort != base_execution_payload_builder::PostStateRootCohort::Ordinary {
            let service = state.service.clone();
            ctx.node.task_executor().spawn_critical_with_graceful_shutdown_signal("deferred-state-root-health", async move |shutdown| {
                tokio::pin!(shutdown);
                loop {
                    tokio::select! {
                        guard = &mut shutdown => {
                            if let Some(manager) = service.manager.get().cloned() {
                                tokio::task::spawn_blocking(move || manager.drain()).await
                                    .expect("root worker drain task must not panic")
                                    .expect("root worker must drain within its configured deadline");
                            }
                            drop(guard); break;
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                            if service.manager.get().is_some_and(|manager| manager.failed.load(std::sync::atomic::Ordering::Acquire)) {
                                panic!("deferred state root operational failure; node advancement stopped");
                            }
                        }
                    }
                }
            });
        }
        Ok(DeferredEngineValidator { inner, state })
    }
}
