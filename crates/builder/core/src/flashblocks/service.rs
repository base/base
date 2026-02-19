use std::sync::Arc;

use base_builder_publish::WebSocketPublisher;
use base_client_node::{BaseNode, OpNodeTypes, PayloadServiceBuilder as BasePayloadServiceBuilder};
use derive_more::Debug;
use reth_basic_payload_builder::BasicPayloadJobGeneratorConfig;
use reth_node_api::NodeTypes;
use reth_node_builder::{
    BuilderContext,
    components::{ComponentsBuilder, PayloadServiceBuilder},
};
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::{
    OpConsensusBuilder, OpExecutorBuilder, OpNetworkBuilder, node::OpPoolBuilder,
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_provider::CanonStateSubscriptions;

use crate::{
    BlockPayloadJobGenerator, BuilderConfig, BuilderMetrics, NodeBounds, OpPayloadBuilder,
    PayloadHandler, PoolBounds,
};

/// Builder for the flashblocks payload service.
///
/// Wraps [`BuilderConfig`] and implements [`BasePayloadServiceBuilder`] to spawn
/// the flashblocks payload builder service, which produces sub-block chunks
/// (flashblocks) at sub-second intervals during block construction.
#[derive(Debug)]
pub struct FlashblocksServiceBuilder(pub BuilderConfig);

impl FlashblocksServiceBuilder {
    fn spawn_payload_builder_service<Node, Pool>(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>>
    where
        Node: NodeBounds,
        Pool: PoolBounds,
    {
        let metrics = Arc::new(BuilderMetrics::default());
        let (built_payload_tx, built_payload_rx) = tokio::sync::mpsc::channel(16);

        let ws_pub: Arc<WebSocketPublisher> =
            WebSocketPublisher::new(self.0.flashblocks.ws_addr)?.into();
        let payload_builder = OpPayloadBuilder::new(
            OpEvmConfig::optimism(ctx.chain_spec()),
            pool,
            ctx.provider().clone(),
            self.0.clone(),
            built_payload_tx,
            ws_pub,
            metrics,
        );
        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = BlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
            true,
            self.0.block_time_leeway,
            self.0.flashblocks.compute_state_root_on_finalize,
        );

        let (payload_service, payload_builder_handle) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        let payload_handler =
            PayloadHandler::new(built_payload_rx, payload_service.payload_events_handle());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));
        ctx.task_executor()
            .spawn_critical("flashblocks payload handler", Box::pin(payload_handler.run()));

        tracing::info!("Flashblocks payload builder service started");
        Ok(payload_builder_handle)
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool, OpEvmConfig> for FlashblocksServiceBuilder
where
    Node: NodeBounds,
    Pool: PoolBounds,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        _: OpEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        self.spawn_payload_builder_service(ctx, pool)
    }
}

impl BasePayloadServiceBuilder for FlashblocksServiceBuilder {
    type ComponentsBuilder = ComponentsBuilder<
        OpNodeTypes,
        OpPoolBuilder,
        Self,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >;

    fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder {
        base_node.components::<OpNodeTypes>().payload(self)
    }
}
