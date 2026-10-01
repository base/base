use std::sync::Arc;

use base_builder_publish::WebSocketPublisher;
use base_execution_evm::BaseEvmConfig;
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
    BuilderConfig,
    traits::{NodeBounds, PoolBounds},
};

/// Builder for the flashblocks payload service.
///
/// Holds a [`BuilderConfig`] and implements [`BasePayloadServiceBuilder`] to spawn the flashblocks
/// payload builder service, which produces sub-block chunks at sub-second intervals.
#[derive(Debug)]
pub struct FlashblocksServiceBuilder {
    config: BuilderConfig,
}

impl FlashblocksServiceBuilder {
    /// Creates a flashblocks payload service builder.
    pub const fn new(config: BuilderConfig) -> Self {
        Self { config }
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool, BaseEvmConfig> for FlashblocksServiceBuilder
where
    Node: NodeBounds,
    Pool: PoolBounds,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        _: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        let (built_payload_tx, built_payload_rx) = tokio::sync::mpsc::channel(16);

        let ws_pub: Arc<WebSocketPublisher> =
            WebSocketPublisher::new(self.config.flashblocks_ws_addr)?.into();
        let payload_builder = BasePayloadBuilder::new(
            BaseEvmConfig::base(ctx.chain_spec()),
            pool,
            ctx.provider().clone(),
            self.config.clone(),
            BuilderOutputs { payload_tx: built_payload_tx, ws_pub },
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

impl BasePayloadServiceBuilder for FlashblocksServiceBuilder {
    type ComponentsBuilder = ComponentsBuilder<
        BaseNodeTypes,
        BasePoolBuilder,
        Self,
        BaseNetworkBuilder,
        BaseExecutorBuilder,
        BaseConsensusBuilder,
    >;

    fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder {
        base_node.components::<BaseNodeTypes>().payload(self)
    }
}
