//! Deferred parent-commitment adapter with private, ready-only trie publication.
//!
//! This adapter is not installed on ordinary nodes. Experimental launch admission must reject
//! incompatible sync, witness and service configurations before selecting it.

use std::sync::{Arc, Mutex, OnceLock};
use alloy_consensus::Header;
use alloy_primitives::B256;
use base_common_consensus::{BaseBlock, BasePrimitives};
use base_common_rpc_types_engine::ExecutionData;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{BaseBuiltPayload, PostStateRootData, PostStateRootKey, PostStateRootLimits, PostStateRootManager, PostStateRootSource};
use reth_chain_state::ExecutedBlock;
use reth_engine_tree::tree::{CacheWaitDurations, EngineApiTreeState, EngineValidator, ValidationOutcome, WaitForCaches, error::InsertBlockError, payload_validator::TreeCtx, state_root_strategy::{DefaultStateRootStrategy, LazyHashedPostState, PayloadStateRootJobContext, PreparedStateRootJob, StateRootJob, StateRootJobContext, StateRootJobOutcome, StateRootStrategy}};
use reth_evm::EvmEnv;
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
    /// Never means ordinary stock delegation at every height.
    pub activation: Option<u64>,
    /// Positive bounded root-worker limits.
    pub limits: PostStateRootLimits,
    /// No manager is constructed on Never or pre-activation calls.
    pub manager: OnceLock<Arc<PostStateRootManager>>,
    /// Only a successful inner validation may consume and publish this data.
    pub staged: Mutex<Option<DeferredStateRootStaged>>,
}

impl DeferredStateRootState {
    /// Resolves the branch-independent timestamp gate, excluding genesis.
    pub fn active(&self, timestamp: u64, number: u64) -> bool { number != 0 && self.activation.is_some_and(|at| timestamp >= at) }

    /// Initializes the worker only for an active operation. Initialization failures stay fatal.
    pub fn manager(&self) -> ProviderResult<Arc<PostStateRootManager>> {
        if let Some(manager) = self.manager.get() { return Ok(manager.clone()); }
        let manager = Arc::new(PostStateRootManager::new(self.limits)?);
        if self.manager.set(manager.clone()).is_err() {
            manager.drain()?;
            return self.manager.get().cloned().ok_or_else(|| PostStateRootManager::error("missing initialized root manager"));
        }
        Ok(manager)
    }
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

impl<P> StateRootStrategy<BasePrimitives, P, BaseEvmConfig> for DeferredStateRootStrategy<P>
where
    P: DatabaseProviderFactory + Clone + Send + Sync + 'static,
    P::Provider: BlockNumReader + PruneCheckpointReader + StageCheckpointReader + ChangeSetReader + StorageChangeSetReader + StorageSettingsCache + 'static,
{
    fn prepare(&self, ctx: StateRootJobContext<'_, BasePrimitives, P, BaseEvmConfig>) -> ProviderResult<PreparedStateRootJob<BasePrimitives>> {
        let timestamp = ctx.env().evm_env.block_env().timestamp.saturating_to();
        if !self.state.active(timestamp, ctx.parent_header().number + 1) {
            return self.stock.prepare(ctx);
        }
        let parent = ctx.parent_header();
        let key = PostStateRootKey { hash: parent.hash(), parent_hash: parent.parent_hash, number: parent.number };
        let factory = OverlayStateProviderFactory::new(self.provider.clone(), self.overlay.overlay_builder(parent.hash()));
        Ok(PreparedStateRootJob::new(Box::new(DeferredStateRootJob { factory, parent: key, state: self.state.clone() }), None))
    }

    fn prepare_payload_builder(&self, ctx: PayloadStateRootJobContext<'_, BasePrimitives, P>) -> ProviderResult<Option<PayloadStateRootHandle>> {
        if self.state.active(ctx.timestamp(), ctx.parent_header().number + 1) { Ok(None) } else { <DefaultStateRootStrategy as StateRootStrategy<BasePrimitives, P, BaseEvmConfig>>::prepare_payload_builder(&self.stock, ctx) }
    }
}

/// Active serial job without stock sparse sharing or header-derived post-state fallbacks.
#[derive(Debug)]
pub struct DeferredStateRootJob<P> {
    /// Pinned parent overlay factory, opened on the provider worker.
    pub factory: OverlayStateProviderFactory<P, BasePrimitives>,
    /// Exact parent identity, independent of height collisions.
    pub parent: PostStateRootKey,
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
        let manager = self.state.manager()?;
        let parent_root = if let Some(parent) = manager.get(self.parent, PostStateRootSource::Validator)? { parent.root } else {
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
        if !self.state.active(block.timestamp, block.number) { return Ok(output); }
        let error = |message: &str| InsertBlockError::new(output.executed_block.sealed_block().clone(), PostStateRootManager::error(message).into());
        let staged = self.state.staged.lock().map_err(|_| error("root staging mutex poisoned"))?.take().ok_or_else(|| error("missing ready current root"))?;
        if staged.key.hash != block.hash() || staged.key.parent_hash != block.parent_hash || staged.key.number != block.number {
            return Err(error("ready root identity mismatch").into());
        }
        self.state.manager().and_then(|manager| manager.publish(staged.key, PostStateRootSource::Validator, staged.data.clone()))
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
        if self.state.active(block.recovered_block.timestamp, block.recovered_block.number) { return Err(PostStateRootManager::error("trusted insertion is unsupported for deferred state roots")); }
        self.inner.on_inserted_executed_block(block)
    }
    fn on_canonical_head_changed(&self, hash: B256, state: &EngineApiTreeState<BasePrimitives>) { self.inner.on_canonical_head_changed(hash, state); }
    fn payload_builder_resources(&self, parent: B256, header: &Header, timestamp: u64, state: &mut EngineApiTreeState<BasePrimitives>) -> PayloadBuilderResources { self.inner.payload_builder_resources(parent, header, timestamp, state) }
}

impl<V: WaitForCaches> WaitForCaches for DeferredEngineValidator<V> {
    fn wait_for_caches(&self) -> CacheWaitDurations { self.inner.wait_for_caches() }
}
