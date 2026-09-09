use std::sync::Arc;

use base_builder_core::{BuilderConfig, FlashblocksServiceBuilder, NodeBounds, PoolBounds};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{ResourceMeteringConfig, config::BaseBuilderConfig};
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
    /// Whether to run only the basic payload builder.
    pub basic_only: bool,
}

impl MultiplexingServiceBuilder {
    /// Creates a new multiplexing service builder.
    pub fn new(builder_config: BuilderConfig) -> Self {
        Self { builder_config, routing_config: RoutingConfig::default(), basic_only: false }
    }

    /// Configures multiplexer runtime config.
    pub const fn with_routing_config(mut self, routing_config: RoutingConfig) -> Self {
        self.routing_config = routing_config;
        self
    }

    /// Enables or disables the Denim payload-builder cutover.
    pub const fn with_cutover_enabled(mut self, cutover_enabled: bool) -> Self {
        self.routing_config.cutover_enabled = cutover_enabled;
        self
    }

    /// Configures the service to run only the basic payload builder.
    pub const fn with_basic_only(mut self, basic_only: bool) -> Self {
        self.basic_only = basic_only;
        self
    }

    /// Native payload settings copied from the shared builder config.
    ///
    /// Cutover and basic-only modes spawn `BasePayloadBuilder`. The rejection
    /// cache and metering provider must be the same objects Flashblocks uses,
    /// otherwise permanently rejected hashes and `meterBundle` data diverge
    /// after cutover.
    fn native_payload_config(&self) -> BaseBuilderConfig {
        BaseBuilderConfig {
            da_config: self.builder_config.da_config.clone(),
            gas_limit_config: self.builder_config.gas_limit_config.clone(),
            manifest_precheck_enabled: self.builder_config.manifest_precheck_enabled,
            predicate_eval_hard_cutoff: self.builder_config.predicate_eval_hard_cutoff,
            resource_metering: ResourceMeteringConfig {
                provider: Arc::clone(&self.builder_config.metering_provider),
                ..ResourceMeteringConfig::default()
            },
            rejection_cache: self.builder_config.rejection_cache.clone(),
        }
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
        eyre::ensure!(
            !(self.routing_config.cutover_enabled && self.basic_only),
            "payload builder cutover and basic-only modes are mutually exclusive"
        );

        if !self.routing_config.cutover_enabled && !self.basic_only {
            return FlashblocksServiceBuilder::new(self.builder_config)
                .spawn_payload_builder_service(ctx, pool, evm_config)
                .await;
        }

        let payload_builder =
            base_execution_payload_builder::BasePayloadBuilder::with_builder_config(
                pool.clone(),
                ctx.provider().clone(),
                evm_config.clone(),
                self.native_payload_config(),
            );

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

        if self.basic_only {
            ctx.task_executor().spawn_critical_task(
                "basic payload builder service",
                Box::pin(basic_payload_service),
            );
            info!("basic payload builder service started");
            return Ok(basic_handle);
        }

        let flashblocks_handle = FlashblocksServiceBuilder::new(self.builder_config)
            .spawn_payload_builder_service(ctx, pool, evm_config)
            .await?;

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
            ctx.chain_spec(),
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

#[cfg(test)]
mod tests {
    use alloy_primitives::TxHash;

    use super::*;

    #[test]
    fn native_payload_config_preserves_metering_provider_and_rejection_cache() {
        let builder_config = BuilderConfig::default();
        let hash = TxHash::repeat_byte(0x11);
        builder_config.rejection_cache.insert(hash);
        let provider = Arc::clone(&builder_config.metering_provider);

        let native = MultiplexingServiceBuilder::new(builder_config).native_payload_config();
        assert!(native.rejection_cache.contains_key(&hash));
        assert!(Arc::ptr_eq(&native.resource_metering.provider, &provider));
    }
}
