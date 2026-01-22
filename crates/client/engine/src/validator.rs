//! Implements custom engine validator that is optimized for validating canonical blocks

use std::{fmt::Debug, sync::Arc};

use alloy_evm::EvmFactory;
use base_flashblocks::{FlashblocksAPI, FlashblocksState};
use op_alloy_consensus::OpReceipt;
use op_alloy_rpc_types_engine::OpExecutionData;
use op_revm::OpHaltReason;
use reth_chainspec::EthChainSpec;
use reth_consensus::FullConsensus;
use reth_engine_primitives::{ConfigureEngineEvm, InvalidBlockHook, PayloadValidator};
use reth_engine_tree::tree::{
    BaseEngineValidator, CachedExecutionProvider, EngineValidator,
    error::InsertPayloadError,
    payload_validator::{BlockOrPayload, TreeCtx, ValidationOutcome},
};
use reth_evm::{ConfigureEvm, block::BlockExecutorFactory};
use reth_node_api::{
    AddOnsContext, BlockTy, FullNodeComponents, FullNodeTypes, InvalidPayloadAttributesError,
    NodeTypes, PayloadTypes, TreeConfig,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_payload_primitives::{BuiltPayload, NewPayloadError};
use reth_primitives_traits::{NodePrimitives, SealedBlock};
use reth_provider::{
    BlockNumReader, BlockReader, ChangeSetReader, DatabaseProviderFactory, HashedPostStateProvider,
    PruneCheckpointReader, StageCheckpointReader, StateProviderFactory, StateReader, TrieReader,
};
use revm::context::result::{ExecutionResult, HaltReason, ResultAndState, SuccessReason};
use revm_primitives::B256;
use tracing::instrument;

#[derive(Debug, Clone)]
pub struct FlashblocksCachedExecutionProvider<P> {
    flashblocks_state: Option<Arc<FlashblocksState>>,

    provider: P,
}

impl<P> FlashblocksCachedExecutionProvider<P> {
    pub fn new(provider: P, flashblocks_state: Option<Arc<FlashblocksState>>) -> Self {
        Self { provider, flashblocks_state }
    }
}

impl<P, Receipt> CachedExecutionProvider<Receipt, OpHaltReason>
    for FlashblocksCachedExecutionProvider<P>
where
    P: BlockNumReader,
{
    fn get_cached_execution_for_tx<'a>(
        &self,
        parent_block_hash: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<ResultAndState<OpHaltReason>> {
        let Some(flashblocks_state) = self.flashblocks_state.as_ref() else {
            return None;
        };
        let Some(pending_blocks) = flashblocks_state.get_pending_blocks().clone() else {
            return None;
        };

        let Ok(Some(parent_block_number)) = self.provider.block_number(*parent_block_hash) else {
            return None;
        };

        let this_block_number = parent_block_number.saturating_add(1);

        let tracked_txns = pending_blocks.get_transactions_for_block(this_block_number);

        if let Some(prev_cached_hash) = prev_cached_hash {
            // all previous transactions from start of block to prev_cached_hash are cached, so only check if the previous transaction is cached
            if !tracked_txns.iter().map(|tx| tx.inner.inner.tx_hash()).any(|hash| &hash == prev_cached_hash) {
                warn!("Not using cached results - previous transaction not cached: {:?}", prev_cached_hash);
                return None;
            }
        } else {
            // must be the first tx in the block
            if tracked_txns.get(0).map(|tx| tx.inner.inner.tx_hash()) != Some(*tx_hash) {
                warn!("Not using cached results - first transaction not cached: {:?}", tx_hash);
                return None;
            }
        }

        let receipt_and_state = pending_blocks
            .get_transaction_result(*tx_hash)
            .zip(pending_blocks.get_transaction_state(tx_hash));

        // info!("Using cached results - receipt and state found for tx: {:?}", tx_hash);
        let (result, state) = receipt_and_state?;

        Some(ResultAndState::new(result, state))
    }
}
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
        Evm: ConfigureEngineEvm<OpExecutionData>
                 + ConfigureEvm<
            BlockExecutorFactory: BlockExecutorFactory<
                EvmFactory: EvmFactory<HaltReason = OpHaltReason>,
            >,
        >,
    >,
    <<Node as FullNodeTypes>::Types as NodeTypes>::Payload:
        PayloadTypes<ExecutionData = OpExecutionData>,
    EV: PayloadValidatorBuilder<Node>,
    EV::Validator: reth_engine_primitives::PayloadValidator<
            <Node::Types as NodeTypes>::Payload,
            Block = BlockTy<Node::Types>,
        >,
{
    type EngineValidator = BaseEngineValidator<
        Node::Provider,
        Node::Evm,
        EV::Validator,
        FlashblocksCachedExecutionProvider<Node::Provider>,
    >;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, Node>,
        tree_config: TreeConfig,
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
            FlashblocksCachedExecutionProvider::new(
                ctx.node.provider().clone(),
                self.flashblocks_state.clone(),
            ),
        ))
    }
}
