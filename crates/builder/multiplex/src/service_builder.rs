use base_builder_core::{BuilderConfig, FlashblocksServiceBuilder, NodeBounds, PoolBounds};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::config::BaseBuilderConfig;
use base_node_core::{
    BaseConsensusBuilder, BaseEngineTypes, BaseExecutorBuilder, BaseNetworkBuilder,
    node::BasePoolBuilder,
};
use base_node_runner::{
    BaseNode, BaseNodeTypes, PayloadServiceBuilder as BasePayloadServiceBuilder,
};
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_node_api::NodeTypes;
use reth_node_builder::{
    BuilderContext,
    components::{ComponentsBuilder, PayloadServiceBuilder},
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_provider::CanonStateSubscriptions;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::{HealthState, MultiplexRouter, RoutingConfig};

/// Spawns flashblocks + basic payload services and returns one routing handle.
#[derive(Debug, Clone)]
pub struct MultiplexingServiceBuilder {
    /// Flashblocks/shared builder config.
    pub builder_config: BuilderConfig,
    /// Multiplexer settings.
    pub routing_config: RoutingConfig,
    /// Whether to compute pending block in basic builder.
    pub compute_pending_block: bool,
}

impl MultiplexingServiceBuilder {
    /// Creates a new multiplexing service builder.
    pub fn new(builder_config: BuilderConfig) -> Self {
        Self {
            builder_config,
            routing_config: RoutingConfig::default(),
            compute_pending_block: false,
        }
    }

    /// Configures pending-block computation for the basic payload builder.
    pub const fn with_compute_pending_block(mut self, compute_pending_block: bool) -> Self {
        self.compute_pending_block = compute_pending_block;
        self
    }

    /// Configures multiplexer runtime config.
    pub const fn with_routing_config(mut self, routing_config: RoutingConfig) -> Self {
        self.routing_config = routing_config;
        self
    }

    /// Enables or disables dual payload builders mode.
    pub const fn with_dual_builders_enabled(mut self, dual_builders_enabled: bool) -> Self {
        self.routing_config.dual_builders_enabled = dual_builders_enabled;
        self
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool, BaseEvmConfig> for MultiplexingServiceBuilder
where
    Node: NodeBounds,
    Pool: PoolBounds + Clone,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        if !self.routing_config.dual_builders_enabled {
            return FlashblocksServiceBuilder::new(self.builder_config)
                .spawn_payload_builder_service(ctx, pool, evm_config)
                .await;
        }

        let flashblocks_handle = FlashblocksServiceBuilder::new(self.builder_config.clone())
            .spawn_payload_builder_service(ctx, pool.clone(), BaseEvmConfig::base(ctx.chain_spec()))
            .await?;

        let payload_builder =
            base_execution_payload_builder::BasePayloadBuilder::with_builder_config(
                pool,
                ctx.provider().clone(),
                evm_config,
                BaseBuilderConfig {
                    da_config: self.builder_config.da_config.clone(),
                    gas_limit_config: self.builder_config.gas_limit_config.clone(),
                    manifest_precheck_enabled: self.builder_config.manifest_precheck_enabled,
                },
            )
            .set_compute_pending_block(self.compute_pending_block);

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

        let (basic_payload_service, basic_handle) =
            PayloadBuilderService::<_, _, BaseEngineTypes>::new(
                payload_generator,
                ctx.provider().canonical_state_stream(),
            );

        let flashblocks_health = HealthState::new();
        let basic_health = HealthState::new();

        let basic_health_for_task = basic_health.clone();
        ctx.task_executor().spawn_critical_task(
            "multiplex basic payload builder service",
            Box::pin(async move {
                basic_payload_service.await;
                basic_health_for_task.mark_unavailable();
                MultiplexRouter::set_service_health_metric("basic", false);
                error!(
                    builder = "basic",
                    selected = false,
                    result = "err",
                    "basic payload service exited"
                );
            }),
        );

        MultiplexRouter::set_service_health_metric("flashblocks", true);
        MultiplexRouter::set_service_health_metric("basic", true);

        let router = MultiplexRouter::new(
            flashblocks_handle,
            basic_handle,
            flashblocks_health,
            basic_health,
            self.routing_config,
        );

        let (router_tx, router_rx) = mpsc::unbounded_channel();
        ctx.task_executor()
            .spawn_critical_task("payload multiplex router", Box::pin(router.run(router_rx)));

        info!("payload builder multiplex service started");
        Ok(PayloadBuilderHandle::new(router_tx))
    }
}

impl BasePayloadServiceBuilder for MultiplexingServiceBuilder {
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
