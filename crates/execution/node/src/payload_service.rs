//! Payload service construction for Base nodes.

use std::time::Duration;

use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig, PayloadBuilderHandle,
    PayloadBuilderService, builder::BasePayloadTransactions, config::BaseBuilderConfig,
};
use reth_chain_state::CanonStateSubscriptions;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_node_builder::BuilderContext;

use crate::BasePayloadBuilder;

/// Scheduling used for Base payload construction.
#[derive(Debug, Clone, Copy)]
pub enum BasePayloadServiceMode {
    /// Standard node service, isolated on a dedicated OS thread.
    DedicatedThread,
    /// Sequencer service, scheduled as a critical task with its block deadline.
    Task {
        /// Maximum duration of a payload job.
        deadline: Duration,
    },
}

/// Builds the standard Base payload service with a configurable transaction source.
#[derive(Debug, Clone)]
pub struct BasePayloadServiceBuilder<Payload = BasePayloadBuilder> {
    /// Payload construction settings and transaction source.
    pub payload_builder: Payload,
    /// Whether canonical state notifications populate the payload cache.
    pub pre_cache_state: bool,
    /// Full-block construction settings, when supplied by the sequencer.
    pub builder_config: Option<BaseBuilderConfig>,
    /// Scheduling and deadline configuration.
    pub mode: BasePayloadServiceMode,
}

impl<Payload> BasePayloadServiceBuilder<Payload> {
    /// Creates a service using the configured transaction source.
    pub const fn new(payload_builder: Payload) -> Self {
        Self {
            payload_builder,
            pre_cache_state: true,
            builder_config: None,
            mode: BasePayloadServiceMode::DedicatedThread,
        }
    }

    /// Controls caching of changed state from canonical notifications.
    pub const fn with_pre_cache_state(mut self, pre_cache_state: bool) -> Self {
        self.pre_cache_state = pre_cache_state;
        self
    }
}

impl<Payload: Default> Default for BasePayloadServiceBuilder<Payload> {
    fn default() -> Self {
        Self::new(Payload::default())
    }
}

impl BasePayloadServiceBuilder {
    /// Configures the full-block service used by the sequencer.
    pub fn full_block(builder_config: BaseBuilderConfig, deadline: Duration) -> Self {
        Self {
            payload_builder: BasePayloadBuilder::default(),
            pre_cache_state: true,
            builder_config: Some(builder_config),
            mode: BasePayloadServiceMode::Task { deadline },
        }
    }
}

impl<Txs> BasePayloadServiceBuilder<BasePayloadBuilder<Txs>> {
    /// Starts the Base payload service with the selected scheduling mode.
    pub async fn spawn_payload_builder_service<DB>(
        self,
        ctx: &BuilderContext<DB>,
        pool: base_node_context::BaseNodePool<reth_provider::providers::BlockchainProvider<DB>>,
        evm_config: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle>
    where
        DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
        Txs: BasePayloadTransactions<
            base_node_context::BaseNodePool<reth_provider::providers::BlockchainProvider<DB>>,
        >,
    {
        let payload_builder =
            base_execution_payload_builder::BasePayloadBuilder::with_builder_config(
                pool,
                ctx.provider().clone(),
                evm_config,
                self.builder_config.unwrap_or(BaseBuilderConfig {
                    da_config: self.payload_builder.da_config,
                    gas_limit_config: self.payload_builder.gas_limit_config,
                    manifest_precheck_enabled: self.payload_builder.manifest_precheck_enabled,
                    predicate_eval_hard_cutoff: self.payload_builder.predicate_eval_hard_cutoff,
                    ..Default::default()
                }),
            )
            .with_transactions(self.payload_builder.best_transactions);
        let config = &ctx.config().builder;
        let job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(config.interval)
            .deadline(match self.mode {
                BasePayloadServiceMode::DedicatedThread => config.deadline,
                BasePayloadServiceMode::Task { deadline } => deadline,
            })
            .max_payload_tasks(config.max_payload_tasks)
            .pre_cache_state(self.pre_cache_state);
        let generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            job_config,
            payload_builder,
        );
        let (service, handle) =
            PayloadBuilderService::<_, _>::new(generator, ctx.provider().canonical_state_stream());
        match self.mode {
            BasePayloadServiceMode::DedicatedThread => {
                ctx.task_executor().spawn_critical_os_thread(
                    "payload-service",
                    "payload builder service",
                    service,
                );
            }
            BasePayloadServiceMode::Task { .. } => {
                ctx.task_executor()
                    .spawn_critical_task("payload builder service", Box::pin(service));
            }
        }
        Ok(handle)
    }
}
