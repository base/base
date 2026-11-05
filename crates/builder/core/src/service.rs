//! Payload builder service for the Base builder.

use base_common_consensus::{BasePrimitives, BaseTransactionSigned};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{BasePayloadBuilder, config::BaseBuilderConfig};
use base_execution_txpool::{BasePooledTx, ParkableTransactionPool};
use base_node_core::{
    BaseConsensusBuilder, BaseEngineTypes, BaseExecutorBuilder, BaseNetworkBuilder,
    node::BasePoolBuilder,
};
use base_node_runner::{
    BaseNode, BaseNodeTypes, PayloadServiceBuilder as BasePayloadServiceBuilder,
};
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{
    BuilderContext,
    components::{ComponentsBuilder, PayloadServiceBuilder},
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_provider::CanonStateSubscriptions;
use tracing::info;

use crate::BuilderMetrics;

/// Spawns the native Base payload builder service for the builder node.
///
/// Jobs are driven by Reth's basic payload job generator, so payload rebuild interval and deadline
/// follow Reth's `--builder.interval` and `--builder.deadline` settings.
#[derive(Debug, Clone)]
pub struct BuilderServiceBuilder {
    /// Payload builder settings.
    pub config: BaseBuilderConfig,
}

impl BuilderServiceBuilder {
    /// Creates a payload service builder with the given payload builder settings.
    pub const fn new(config: BaseBuilderConfig) -> Self {
        Self { config }
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool, BaseEvmConfig> for BuilderServiceBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Payload = BaseEngineTypes,
            ChainSpec = BaseChainSpec,
            Primitives = BasePrimitives,
        >,
    >,
    Pool: ParkableTransactionPool<Transaction: BasePooledTx<Consensus = BaseTransactionSigned>>
        + Unpin
        + 'static,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        // Recorded here rather than at config time so the node's Prometheus recorder is installed.
        if self.config.resource_metering.enabled
            && self.config.resource_metering.schedule.is_empty()
        {
            BuilderMetrics::resource_metering_schedule_missing_total().increment(1);
        }

        // Reth's own flag drives build-loop state-read timing, so the builder and the engine's
        // validation path are instrumented by the same switch.
        let config =
            self.config.with_state_provider_metrics(ctx.config().engine.state_provider_metrics);
        let payload_builder = BasePayloadBuilder::with_builder_config(
            pool,
            ctx.provider().clone(),
            evm_config,
            config,
        )
        .with_task_executor(ctx.task_executor().clone());

        let payload_config = ctx.config().builder.clone();
        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(payload_config.interval)
            .deadline(payload_config.deadline)
            .max_payload_tasks(payload_config.max_payload_tasks)
            .pre_cache_state(true);

        let payload_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
        );

        let (payload_service, payload_builder_handle) =
            PayloadBuilderService::<_, _, BaseEngineTypes>::new(
                payload_generator,
                ctx.provider().canonical_state_stream(),
            );

        ctx.task_executor()
            .spawn_critical_task("payload builder service", Box::pin(payload_service));
        info!("payload builder service started");
        Ok(payload_builder_handle)
    }
}

impl BasePayloadServiceBuilder for BuilderServiceBuilder {
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
