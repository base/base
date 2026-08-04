use std::sync::Arc;

use base_builder_publish::WebSocketPublisher;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::BasePooledTransaction;
use base_node_core::{
    BaseConsensusBuilder, BaseExecutorBuilder, BaseNetworkBuilder, node::BasePoolBuilder,
};
use base_node_runner::{
    BaseNode, BaseNodeTypes, PayloadServiceBuilder as BasePayloadServiceBuilder,
};
use derive_more::Debug;
use reth_node_api::NodeTypes;
use reth_node_builder::{
    BuilderContext,
    components::{ComponentsBuilder, PayloadServiceBuilder},
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_provider::CanonStateSubscriptions;
use tracing::info;

use super::{
    PayloadHandler,
    generator::BlockPayloadJobGenerator,
    payload::{BasePayloadBuilder, BuilderOutputs},
};
use crate::{
    BuilderConfig, CandidateSource, DefaultCandidateSource, RejectedTxForwarder,
    traits::{NodeBounds, PoolBounds},
};

/// Builder for the flashblocks payload service.
///
/// Holds a [`BuilderConfig`] and a [`CandidateSource`], and implements
/// [`BasePayloadServiceBuilder`] to spawn the flashblocks payload builder service, which produces
/// sub-block chunks (flashblocks) at sub-second intervals during block construction.
///
/// The candidate source defaults to [`DefaultCandidateSource`] (the pool's best transactions,
/// unchanged); use [`FlashblocksServiceBuilder::with_candidate_source`] to supply an alternative.
#[derive(Debug)]
pub struct FlashblocksServiceBuilder<S = DefaultCandidateSource> {
    config: BuilderConfig,
    candidate_source: S,
}

impl FlashblocksServiceBuilder {
    /// Create a service builder that uses the default candidate source
    /// (the pool's priority-ordered best transactions, unchanged).
    pub const fn new(config: BuilderConfig) -> Self {
        Self { config, candidate_source: DefaultCandidateSource }
    }
}

impl<S> FlashblocksServiceBuilder<S> {
    /// Replace the candidate transaction source used by the flashblocks build loop.
    #[must_use]
    pub fn with_candidate_source<S2>(self, candidate_source: S2) -> FlashblocksServiceBuilder<S2> {
        FlashblocksServiceBuilder { config: self.config, candidate_source }
    }

    fn spawn_payload_builder_service<Node, Pool>(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>>
    where
        Node: NodeBounds,
        Pool: PoolBounds,
        S: CandidateSource<Pool::Transaction> + Clone + Unpin + 'static,
    {
        let (built_payload_tx, built_payload_rx) = tokio::sync::mpsc::channel(16);

        let rejected_tx_sender = if let Some(ref url) = self.config.audit_archiver_url {
            let (tx, rx) = tokio::sync::mpsc::channel(self.config.rejected_tx_channel_size);
            let forwarder = RejectedTxForwarder::new(url, rx)
                .map_err(|e| eyre::eyre!("Failed to create rejected tx forwarder: {e}"))?;
            ctx.task_executor().spawn_task(Box::pin(forwarder.run()));
            info!(audit_archiver_url = %url, "Rejected transaction forwarder started");
            Some(tx)
        } else {
            None
        };

        let ws_pub: Arc<WebSocketPublisher> =
            WebSocketPublisher::new(self.config.flashblocks_ws_addr)?.into();
        let payload_builder = BasePayloadBuilder::new(
            BaseEvmConfig::base(ctx.chain_spec()),
            pool,
            ctx.provider().clone(),
            self.config.clone(),
            BuilderOutputs { payload_tx: built_payload_tx, ws_pub, rejected_tx_sender },
            self.candidate_source.clone(),
        );
        let payload_generator = BlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_builder,
            true,
            self.config.block_time_leeway,
        );

        let (payload_service, payload_builder_handle) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        let payload_handler =
            PayloadHandler::new(built_payload_rx, payload_service.payload_events_handle());

        ctx.task_executor()
            .spawn_critical_task("custom payload builder service", Box::pin(payload_service));
        ctx.task_executor()
            .spawn_critical_task("flashblocks payload handler", Box::pin(payload_handler.run()));

        info!("Flashblocks payload builder service started");
        Ok(payload_builder_handle)
    }
}

impl<Node, Pool, S> PayloadServiceBuilder<Node, Pool, BaseEvmConfig>
    for FlashblocksServiceBuilder<S>
where
    Node: NodeBounds,
    Pool: PoolBounds,
    S: CandidateSource<Pool::Transaction> + Clone + Unpin + 'static,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        _: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        self.spawn_payload_builder_service(ctx, pool)
    }
}

impl<S> BasePayloadServiceBuilder for FlashblocksServiceBuilder<S>
where
    S: CandidateSource<BasePooledTransaction> + Clone + Unpin + 'static,
{
    type ComponentsBuilder = ComponentsBuilder<
        BaseNodeTypes,
        BasePoolBuilder<BasePooledTransaction>,
        Self,
        BaseNetworkBuilder,
        BaseExecutorBuilder,
        BaseConsensusBuilder,
    >;

    fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder {
        base_node.components::<BaseNodeTypes>().payload(self)
    }
}
