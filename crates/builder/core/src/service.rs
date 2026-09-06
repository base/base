//! Full-block payload service wiring.

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

use crate::{BuilderConfig, NodeBounds, PoolBounds};

/// Spawns the full-block payload service.
#[derive(Debug, Clone)]
pub struct BlockServiceBuilder {
    /// Payload builder configuration.
    pub builder_config: BuilderConfig,
}

impl BlockServiceBuilder {
    /// Creates a full-block payload service builder.
    pub const fn new(builder_config: BuilderConfig) -> Self {
        Self { builder_config }
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool, BaseEvmConfig> for BlockServiceBuilder
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
        let payload_builder =
            base_execution_payload_builder::BasePayloadBuilder::with_builder_config(
                pool,
                ctx.provider().clone(),
                evm_config,
                BaseBuilderConfig {
                    da_config: self.builder_config.da_config.clone(),
                    gas_limit_config: self.builder_config.gas_limit_config.clone(),
                    manifest_precheck_enabled: self.builder_config.manifest_precheck_enabled,
                    predicate_eval_hard_cutoff: self.builder_config.predicate_eval_hard_cutoff,
                    max_gas_per_txn: self.builder_config.max_gas_per_txn,
                    max_uncompressed_block_size: self.builder_config.max_uncompressed_block_size,
                },
            );

        let payload_config = ctx.config().builder.clone();
        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(payload_config.interval)
            .deadline(
                self.builder_config
                    .block_time
                    .saturating_add(self.builder_config.block_time_leeway),
            )
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

        ctx.task_executor()
            .spawn_critical_task("payload builder service", Box::pin(basic_payload_service));
        Ok(basic_handle)
    }
}

impl BasePayloadServiceBuilder for BlockServiceBuilder {
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
