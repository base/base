//! Implements custom engine validator that is optimized for validating canonical blocks

use std::{fmt::Debug, sync::Arc};

use base_alloy_consensus::OpTxType;
use base_alloy_evm::{OpBlockExecutorFactory, OpTxResult};
use base_alloy_rpc_types_engine::OpExecutionData;
use base_engine_tree::{BaseEngineValidator, CachedExecutionProvider};
use base_execution_chainspec::OpChainSpec;
use base_execution_primitives::OpPrimitives;
use base_flashblocks::{FlashblocksAPI, FlashblocksState};
use base_node_core::{OpEngineTypes, OpRethReceiptBuilder};
use base_revm::OpHaltReason;
use reth_chainspec::EthChainSpec;
use reth_engine_primitives::ConfigureEngineEvm;
use reth_evm::{ConfigureEvm, eth::EthTxResult};
use reth_node_api::{
    AddOnsContext, BlockTy, FullNodeComponents, FullNodeTypes, NodeTypes, PayloadTypes, TreeConfig,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{ChangesetCache, EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_provider::BlockNumReader;
use revm::context::result::ExecResultAndState;
use revm_primitives::B256;
use tracing::warn;

/// Provider that fetches cached execution results for transactions.
#[derive(Debug, Clone)]
pub struct FlashblocksCachedExecutionProvider<P> {
    flashblocks_state: Option<Arc<FlashblocksState>>,

    provider: P,
}

impl<P> FlashblocksCachedExecutionProvider<P> {
    /// Creates a new [`FlashblocksCachedExecutionProvider`].
    pub const fn new(provider: P, flashblocks_state: Option<Arc<FlashblocksState>>) -> Self {
        Self { provider, flashblocks_state }
    }
}

impl<P> CachedExecutionProvider<OpTxResult<OpHaltReason, OpTxType>>
    for FlashblocksCachedExecutionProvider<P>
where
    P: BlockNumReader,
{
    fn get_cached_execution_for_tx<'a>(
        &self,
        parent_block_hash: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<OpTxResult<OpHaltReason, OpTxType>> {
        let flashblocks_state = self.flashblocks_state.as_ref()?;

        let parent_block_number = self.provider.block_number(*parent_block_hash).ok().flatten()?;

        let this_block_number = parent_block_number.saturating_add(1);

        let pending_blocks = flashblocks_state.get_pending_blocks().clone()?;

        if let Some(prev_cached_hash) = prev_cached_hash {
            // all previous transactions from start of block to prev_cached_hash are cached, so only check if the previous transaction is cached
            if !pending_blocks.has_transaction_hash(prev_cached_hash) {
                warn!(
                    "Not using cached results - previous transaction not cached: {:?}",
                    prev_cached_hash
                );
                return None;
            }
        } else {
            // must be the first tx in the block
            if pending_blocks
                .get_transactions_for_block(this_block_number)
                .next()
                .map(|tx| tx.inner.inner.tx_hash())
                != Some(*tx_hash)
            {
                warn!("Not using cached results - first transaction not cached: {:?}", tx_hash);
                return None;
            }
        }

        let receipt_and_state = pending_blocks
            .get_transaction_result(tx_hash)
            .zip(pending_blocks.get_transaction_state(tx_hash))
            .zip(pending_blocks.get_transaction_by_hash(*tx_hash))
            .zip(pending_blocks.get_transaction_sender(tx_hash));

        let (((result, state), tx), sender) = receipt_and_state?;

        let eth_tx_result = EthTxResult {
            result: ExecResultAndState::new(result.clone(), state),
            blob_gas_used: 0,
            tx_type: tx.inner.inner.tx_type(),
        };

        let op_tx_result =
            OpTxResult { inner: eth_tx_result, is_deposit: tx.inner.inner.is_deposit(), sender };

        Some(op_tx_result)
    }
}
/// Basic implementation of [`EngineValidatorBuilder`].
///
/// This builder creates a [`BaseEngineValidator`] using the provided payload validator builder.
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
            Types: NodeTypes<
                Payload = OpEngineTypes,
                ChainSpec = OpChainSpec,
                Primitives = OpPrimitives,
            >,
            Evm: ConfigureEngineEvm<OpExecutionData>
                     + ConfigureEvm<
                BlockExecutorFactory = OpBlockExecutorFactory<
                    OpRethReceiptBuilder,
                    Arc<OpChainSpec>,
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
            FlashblocksCachedExecutionProvider::new(
                ctx.node.provider().clone(),
                self.flashblocks_state.clone(),
            ),
            changeset_cache,
            ctx.node.task_executor().clone(),
        ))
    }
}
