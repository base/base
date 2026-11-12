//! Payload service wiring for the Base block builder.

use base_common_rpc_types_engine::PayloadReady;
use base_execution_evm::BaseEvmConfig;
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
use tokio::sync::broadcast;
use tracing::info;

use crate::{BuilderConfig, NodeBounds, PoolBounds};

/// Spawns the native Base payload builder behind reth's basic payload job generator.
///
/// Each payload job builds one full block from the transaction pool and resolves it on
/// `getPayload`, using reth's `--builder.interval` and `--builder.deadline` settings for job
/// pacing.
#[derive(Debug, Clone)]
pub struct NativePayloadServiceBuilder {
    /// Builder configuration shared with the RPC extensions.
    pub builder_config: BuilderConfig,
    /// Optional notifications for frozen sequencer payloads.
    pub payload_ready: Option<broadcast::Sender<PayloadReady>>,
}

impl NativePayloadServiceBuilder {
    /// Creates a payload service builder from the given builder configuration.
    pub const fn new(builder_config: BuilderConfig) -> Self {
        Self { builder_config, payload_ready: None }
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool, BaseEvmConfig> for NativePayloadServiceBuilder
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
        self.builder_config.record_startup_metrics();

        // Reth's own flag drives build-loop state-read timing, so the builder and the engine's
        // validation path are instrumented by the same switch.
        let mut builder_config = self.builder_config;
        builder_config.state_provider_metrics = ctx.config().engine.state_provider_metrics;

        let payload_builder =
            base_execution_payload_builder::BasePayloadBuilder::with_builder_config(
                pool,
                ctx.provider().clone(),
                evm_config,
                builder_config.payload_builder_config(),
            )
            .with_task_executor(ctx.task_executor().clone())
            .with_payload_ready(self.payload_ready);

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

        let (payload_service, payload_handle) = PayloadBuilderService::<_, _, BaseEngineTypes>::new(
            payload_generator,
            ctx.provider().canonical_state_stream(),
        );

        ctx.task_executor()
            .spawn_critical_task("native payload builder service", Box::pin(payload_service));
        info!("native payload builder service started");
        Ok(payload_handle)
    }
}

impl BasePayloadServiceBuilder for NativePayloadServiceBuilder {
    type ComponentsBuilder = ComponentsBuilder<
        BaseNodeTypes,
        BasePoolBuilder,
        Self,
        BaseNetworkBuilder,
        BaseExecutorBuilder,
        BaseConsensusBuilder,
    >;

    fn build_components(mut self, base_node: &BaseNode) -> Self::ComponentsBuilder {
        self.payload_ready = base_node.payload_ready.clone();
        base_node.components::<BaseNodeTypes>().payload(self)
    }
}
