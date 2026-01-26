//! Implements custom engine validator that is optimized for validating canonical blocks

use std::{fmt::Debug, sync::Arc};

use base_flashblocks::FlashblocksState;
use reth_chainspec::EthChainSpec;
use reth_consensus::FullConsensus;
use reth_engine_primitives::{ConfigureEngineEvm, InvalidBlockHook, PayloadValidator};
use reth_engine_tree::tree::{
    BasicEngineValidator, EngineValidator,
    error::InsertPayloadError,
    payload_validator::{BlockOrPayload, TreeCtx, ValidationOutcome},
};
use reth_evm::ConfigureEvm;
use reth_node_api::{
    AddOnsContext, BlockTy, FullNodeComponents, InvalidPayloadAttributesError, NodeTypes,
    PayloadTypes, TreeConfig,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{ChangesetCache, EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_payload_primitives::{BuiltPayload, NewPayloadError};
use reth_primitives_traits::{NodePrimitives, SealedBlock};
use reth_provider::{
    BlockNumReader, BlockReader, ChangeSetReader, DatabaseProviderFactory, HashedPostStateProvider,
    PruneCheckpointReader, StageCheckpointReader, StateProviderFactory, StateReader,
};
use tracing::instrument;

/// Basic implementation of [`EngineValidatorBuilder`].
///
/// This builder creates a [`BasicEngineValidator`] using the provided payload validator builder.
#[derive(Debug, Clone)]
pub struct BaseEngineValidatorBuilder<EV> {
    /// The payload validator builder used to create the engine validator.
    payload_validator_builder: EV,

    /// The flashblocks state used to create the engine validator.
    flashblocks_state: Option<Arc<FlashblocksState>>,
}

impl<EV> BaseEngineValidatorBuilder<EV> {
    /// Creates a new instance with the given payload validator builder.
    pub const fn new(payload_validator_builder: EV) -> Self {
        Self { payload_validator_builder, flashblocks_state: None }
    }

    /// Sets the flashblocks state used to create the engine validator.
    pub fn with_flashblocks_state(mut self, flashblocks_state: Arc<FlashblocksState>) -> Self {
        self.flashblocks_state = Some(flashblocks_state);
        self
    }
}

impl<EV> Default for BaseEngineValidatorBuilder<EV>
where
    EV: Default,
{
    fn default() -> Self {
        Self::new(EV::default())
    }
}

impl<Node, EV> EngineValidatorBuilder<Node> for BaseEngineValidatorBuilder<EV>
where
    Node: FullNodeComponents<
        Evm: ConfigureEngineEvm<
            <<Node::Types as NodeTypes>::Payload as PayloadTypes>::ExecutionData,
        >,
    >,
    EV: PayloadValidatorBuilder<Node>,
    EV::Validator: reth_engine_primitives::PayloadValidator<
            <Node::Types as NodeTypes>::Payload,
            Block = BlockTy<Node::Types>,
        >,
{
    type EngineValidator = BaseEngineValidator<Node::Provider, Node::Evm, EV::Validator>;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, Node>,
        tree_config: TreeConfig,
        changeset_cache: ChangesetCache,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = self.payload_validator_builder.build(ctx).await?;
        let data_dir = ctx.config.datadir.clone().resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = ctx.create_invalid_block_hook(&data_dir).await?;
        Ok(BaseEngineValidator::new(
            ctx.node.provider().clone(),
            std::sync::Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            invalid_block_hook,
            changeset_cache,
        ))
    }
}

/// A helper type that provides reusable payload validation logic for network-specific validators.
///
/// This type satisfies [`EngineValidator`] and is responsible for executing blocks/payloads.
///
/// This type contains common validation, execution, and state root computation logic that can be
/// used by network-specific payload validators (e.g., Ethereum, Optimism). It is not meant to be
/// used as a standalone component, but rather as a building block for concrete implementations.
#[derive(derive_more::Debug)]
pub struct BaseEngineValidator<P, Evm, V>
where
    Evm: ConfigureEvm,
{
    inner: BasicEngineValidator<P, Evm, V>,
}

impl<N, P, Evm, V> BaseEngineValidator<P, Evm, V>
where
    N: NodePrimitives,
    P: DatabaseProviderFactory<
            Provider: BlockReader
                          + StageCheckpointReader
                          + PruneCheckpointReader
                          + ChangeSetReader
                          + BlockNumReader,
        > + BlockReader<Header = N::BlockHeader>
        + ChangeSetReader
        + BlockNumReader
        + StateProviderFactory
        + StateReader
        + HashedPostStateProvider
        + Clone
        + 'static,
    Evm: ConfigureEvm<Primitives = N> + 'static,
{
    /// Creates a new `TreePayloadValidator`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: P,
        consensus: Arc<dyn FullConsensus<N>>,
        evm_config: Evm,
        validator: V,
        config: TreeConfig,
        invalid_block_hook: Box<dyn InvalidBlockHook<N>>,
        changeset_cache: ChangesetCache,
    ) -> Self {
        Self {
            inner: BasicEngineValidator::new(
                provider,
                consensus,
                evm_config,
                validator,
                config,
                invalid_block_hook,
                changeset_cache,
            ),
        }
    }

    /// Validates a block that has already been converted from a payload.
    ///
    /// This method performs:
    /// - Consensus validation
    /// - Block execution
    /// - State root computation
    /// - Fork detection
    #[instrument(
        level = "debug",
        target = "engine::tree::payload_validator",
        skip_all,
        fields(
            parent = ?input.parent_hash(),
            type_name = ?input.type_name(),
        )
    )]
    pub fn validate_block_with_state<T: PayloadTypes<BuiltPayload: BuiltPayload<Primitives = N>>>(
        &mut self,
        input: BlockOrPayload<T>,
        ctx: TreeCtx<'_, N>,
    ) -> ValidationOutcome<N, InsertPayloadError<N::Block>>
    where
        V: PayloadValidator<T, Block = N::Block>,
        Evm: ConfigureEngineEvm<T::ExecutionData, Primitives = N>,
    {
        self.inner.validate_block_with_state(input, ctx)
    }
}

impl<N, Types, P, Evm, V> EngineValidator<Types> for BaseEngineValidator<P, Evm, V>
where
    P: DatabaseProviderFactory<
            Provider: BlockReader
                          + StageCheckpointReader
                          + PruneCheckpointReader
                          + ChangeSetReader
                          + BlockNumReader,
        > + BlockReader<Header = N::BlockHeader>
        + StateProviderFactory
        + StateReader
        + ChangeSetReader
        + BlockNumReader
        + HashedPostStateProvider
        + Clone
        + 'static,
    N: NodePrimitives,
    V: PayloadValidator<Types, Block = N::Block>,
    Evm: ConfigureEngineEvm<Types::ExecutionData, Primitives = N> + 'static,
    Types: PayloadTypes<BuiltPayload: BuiltPayload<Primitives = N>>,
{
    fn validate_payload_attributes_against_header(
        &self,
        attr: &Types::PayloadAttributes,
        header: &N::BlockHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        self.inner.validate_payload_attributes_against_header(attr, header)
    }

    fn convert_payload_to_block(
            &self,
            payload: <Types as PayloadTypes>::ExecutionData,
    ) -> Result<reth_primitives_traits::SealedBlock<<<<Types as PayloadTypes>::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block>, NewPayloadError>{
        self.inner.convert_payload_to_block(payload)
    }

    fn validate_payload(
        &mut self,
        payload: Types::ExecutionData,
        ctx: TreeCtx<'_, N>,
    ) -> ValidationOutcome<N> {
        self.validate_block_with_state(BlockOrPayload::Payload(payload), ctx)
    }

    fn validate_block(
        &mut self,
        block: SealedBlock<N::Block>,
        ctx: TreeCtx<'_, N>,
    ) -> ValidationOutcome<N> {
        self.validate_block_with_state(BlockOrPayload::Block(block), ctx)
    }

    fn on_inserted_executed_block(
        &self,
        block: reth_chain_state::ExecutedBlock<
            <<Types as PayloadTypes>::BuiltPayload as BuiltPayload>::Primitives,
        >,
    ) {
        self.inner.on_inserted_executed_block(block)
    }
}
