use super::{payload::OpPayloadBuilder, FlashblocksConfig};
use crate::{
    builders::{
        builder_tx::StandardBuilderTx, generator::BlockPayloadJobGenerator, BuilderConfig,
        BuilderTx,
    },
    flashtestations::service::spawn_flashtestations_service,
    traits::{NodeBounds, PoolBounds},
};
use reth_basic_payload_builder::BasicPayloadJobGeneratorConfig;
use reth_node_api::NodeTypes;
use reth_node_builder::{components::PayloadServiceBuilder, BuilderContext};
use reth_optimism_evm::OpEvmConfig;
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_provider::CanonStateSubscriptions;

pub struct FlashblocksServiceBuilder(pub BuilderConfig<FlashblocksConfig>);

impl FlashblocksServiceBuilder {
    fn spawn_payload_builder_service<Node, Pool, BT>(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        builder_tx: BT,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>>
    where
        Node: NodeBounds,
        Pool: PoolBounds,
        BT: BuilderTx + Unpin + Clone + Send + Sync + 'static,
    {
        let payload_builder = OpPayloadBuilder::new(
            OpEvmConfig::optimism(ctx.chain_spec()),
            pool,
            ctx.provider().clone(),
            self.0.clone(),
            builder_tx,
        )?;

        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = BlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
            true,
            self.0.block_time_leeway,
        );

        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));

        tracing::info!("Flashblocks payload builder service started");

        Ok(payload_builder)
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
        tracing::debug!("Spawning flashblocks payload builder service");
        let signer = self.0.builder_signer;
        if self.0.flashtestations_config.flashtestations_enabled {
            let flashtestations_service = match spawn_flashtestations_service(
                self.0.flashtestations_config.clone(),
                ctx,
            )
            .await
            {
                Ok(service) => service,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to spawn flashtestations service, falling back to standard builder tx");
                    return self.spawn_payload_builder_service(
                        ctx,
                        pool,
                        StandardBuilderTx { signer },
                    );
                }
            };

            if self.0.flashtestations_config.enable_block_proofs {
                return self.spawn_payload_builder_service(ctx, pool, flashtestations_service);
            }
        }
        self.spawn_payload_builder_service(ctx, pool, StandardBuilderTx { signer })
    }
}
