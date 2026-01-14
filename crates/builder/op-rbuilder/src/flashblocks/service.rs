use std::sync::Arc;

use derive_more::Debug;
use eyre::WrapErr as _;
use reth_basic_payload_builder::BasicPayloadJobGeneratorConfig;
use reth_node_api::NodeTypes;
use reth_node_builder::{BuilderContext, components::PayloadServiceBuilder};
use reth_optimism_evm::OpEvmConfig;
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_provider::CanonStateSubscriptions;

use super::{
    BuilderConfig, FlashblocksBuilderTx, generator::BlockPayloadJobGenerator,
    payload::OpPayloadBuilder, payload_handler::PayloadHandler, wspub::WebSocketPublisher,
};
use crate::{
    metrics::OpRBuilderMetrics,
    traits::{NodeBounds, PoolBounds},
};

#[derive(Debug)]
pub struct FlashblocksServiceBuilder(pub BuilderConfig);

impl FlashblocksServiceBuilder {
    fn spawn_payload_builder_service<Node, Pool>(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        builder_tx: FlashblocksBuilderTx,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>>
    where
        Node: NodeBounds,
        Pool: PoolBounds,
    {
        // TODO: is there a different global token?
        // this is effectively unused right now due to the usage of reth's `task_executor`.
        let cancel = tokio_util::sync::CancellationToken::new();

        let metrics = Arc::new(OpRBuilderMetrics::default());
        let (built_payload_tx, built_payload_rx) = tokio::sync::mpsc::channel(16);

        let ws_pub: Arc<WebSocketPublisher> =
            WebSocketPublisher::new(self.0.flashblocks.ws_addr, metrics.clone())
                .wrap_err("failed to create ws publisher")?
                .into();
        let payload_builder = OpPayloadBuilder::new(
            OpEvmConfig::optimism(ctx.chain_spec()),
            pool,
            ctx.provider().clone(),
            self.0.clone(),
            builder_tx,
            built_payload_tx,
            ws_pub,
            metrics.clone(),
        );
        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = BlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
            true,
            self.0.block_time_leeway,
        );

        let (payload_service, payload_builder_handle) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        let syncer_ctx = super::ctx::OpPayloadSyncerCtx::new(
            &ctx.provider().clone(),
            self.0,
            OpEvmConfig::optimism(ctx.chain_spec()),
            metrics,
        )
        .wrap_err("failed to create flashblocks payload builder context")?;

        let payload_handler = PayloadHandler::new(
            built_payload_rx,
            payload_service.payload_events_handle(),
            syncer_ctx,
            ctx.provider().clone(),
            cancel,
        );

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
        let builder_tx = FlashblocksBuilderTx::new(
            self.0.builder_signer,
            self.0.flashblocks.flashblocks_number_contract_address,
        );
        self.spawn_payload_builder_service(ctx, pool, builder_tx)
    }
}
