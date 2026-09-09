//! Payload service construction for Base nodes.

use std::time::Duration;

use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig, PayloadBuilderHandle,
    PayloadBuilderService, config::BaseBuilderConfig,
};
use reth_chain_state::CanonStateSubscriptions;

use crate::BuilderContext;

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

/// Base payload construction settings shared by the node and sequencer.
#[derive(Debug, Clone)]
pub struct BasePayloadServiceConfig {
    /// Execution and block admission settings.
    pub config: BaseBuilderConfig,
    /// Whether canonical state notifications populate the payload cache.
    pub pre_cache_state: bool,
    /// Scheduling and deadline used for payload jobs.
    pub mode: BasePayloadServiceMode,
}

impl Default for BasePayloadServiceConfig {
    fn default() -> Self {
        Self {
            config: BaseBuilderConfig::default(),
            pre_cache_state: true,
            mode: BasePayloadServiceMode::DedicatedThread,
        }
    }
}

impl BasePayloadServiceConfig {
    /// Configures full-block construction for the sequencer.
    pub fn full_block(config: BaseBuilderConfig, deadline: Duration) -> Self {
        Self { config, pre_cache_state: true, mode: BasePayloadServiceMode::Task { deadline } }
    }

    /// Starts the standard Base payload implementation.
    pub async fn start(
        self,
        ctx: &BuilderContext,
        pool: base_node_context::BaseNodePool<reth_provider::providers::BlockchainProvider>,
        evm_config: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle> {
        let payload_builder =
            base_execution_payload_builder::BasePayloadBuilder::with_builder_config(
                pool,
                ctx.provider().clone(),
                evm_config,
                self.config,
            );
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
            PayloadBuilderService::new(generator, ctx.provider().canonical_state_stream());
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
