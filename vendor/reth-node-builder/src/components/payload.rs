//! Payload service component for the node builder.

use std::future::Future;

use base_common_consensus::BaseTxEnvelope;
use reth_basic_payload_builder::{
    BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig, PayloadBuilder,
};
use reth_chain_state::CanonStateSubscriptions;
use reth_evm::BaseEvmConfig;
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService, PayloadServiceCommand};
use reth_payload_primitives::{BaseBuiltPayload, BasePayloadBuilderAttributes};
use reth_transaction_pool::TransactionPool;
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

use crate::{BuilderContext, FullNodeTypes};

/// A type that knows how to spawn the payload service.
pub trait PayloadServiceBuilder<Node: FullNodeTypes, Pool: TransactionPool>: Send + Sized {
    /// Spawns the [`PayloadBuilderService`] and returns the handle to it for use by the engine.
    ///
    /// We provide default implementation via [`BasicPayloadJobGenerator`] but it can be overridden
    /// for custom job orchestration logic,
    fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: BaseEvmConfig,
    ) -> impl Future<Output = eyre::Result<PayloadBuilderHandle>> + Send;
}

impl<Node, F, Fut, Pool> PayloadServiceBuilder<Node, Pool> for F
where
    Node: FullNodeTypes,
    Pool: TransactionPool,
    F: Fn(&BuilderContext<Node>, Pool, BaseEvmConfig) -> Fut + Send,
    Fut: Future<Output = eyre::Result<PayloadBuilderHandle>> + Send,
{
    fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: BaseEvmConfig,
    ) -> impl Future<Output = eyre::Result<PayloadBuilderHandle>> {
        self(ctx, pool, evm_config)
    }
}

/// A type that knows how to build a payload builder to plug into [`BasicPayloadServiceBuilder`].
pub trait PayloadBuilderBuilder<Node: FullNodeTypes, Pool: TransactionPool>: Send + Sized {
    /// Payload builder implementation.
    type PayloadBuilder: PayloadBuilder<
            Attributes = BasePayloadBuilderAttributes<BaseTxEnvelope>,
            BuiltPayload = BaseBuiltPayload,
        > + Unpin
        + 'static;

    /// Spawns the payload service and returns the handle to it.
    ///
    /// The [`BuilderContext`] is provided to allow access to the node's configuration.
    fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: BaseEvmConfig,
    ) -> impl Future<Output = eyre::Result<Self::PayloadBuilder>> + Send;
}

/// Basic payload service builder that spawns a [`BasicPayloadJobGenerator`]
#[derive(Debug, Clone)]
pub struct BasicPayloadServiceBuilder<PB> {
    /// Builds the payload builder used by generated payload jobs.
    payload_builder_builder: PB,
    /// Whether to pre-cache changed state from canonical state notifications.
    pre_cache_state: bool,
}

impl<PB> BasicPayloadServiceBuilder<PB> {
    /// Create a new [`BasicPayloadServiceBuilder`].
    pub const fn new(payload_builder_builder: PB) -> Self {
        Self { payload_builder_builder, pre_cache_state: true }
    }

    /// Sets whether to pre-cache changed state from canonical state notifications.
    pub const fn with_pre_cache_state(mut self, pre_cache_state: bool) -> Self {
        self.pre_cache_state = pre_cache_state;
        self
    }
}

impl<PB> Default for BasicPayloadServiceBuilder<PB>
where
    PB: Default,
{
    fn default() -> Self {
        Self::new(PB::default())
    }
}

impl<Node, Pool, PB> PayloadServiceBuilder<Node, Pool> for BasicPayloadServiceBuilder<PB>
where
    Node: FullNodeTypes,
    Pool: TransactionPool,
    PB: PayloadBuilderBuilder<Node, Pool>,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle> {
        let Self { payload_builder_builder, pre_cache_state } = self;
        let payload_builder =
            payload_builder_builder.build_payload_builder(ctx, pool, evm_config).await?;

        let conf = ctx.config().builder.clone();

        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(conf.interval)
            .deadline(conf.deadline)
            .max_payload_tasks(conf.max_payload_tasks)
            .pre_cache_state(pre_cache_state);

        let payload_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
        );
        let (payload_service, payload_service_handle) = PayloadBuilderService::<_, _>::new(
            payload_generator,
            ctx.provider().canonical_state_stream(),
        );

        ctx.task_executor().spawn_critical_os_thread(
            "payload-service",
            "payload builder service",
            payload_service,
        );

        Ok(payload_service_handle)
    }
}

/// A `NoopPayloadServiceBuilder` useful for node implementations that are not implementing
/// validating/sequencing logic.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct NoopPayloadServiceBuilder;

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for NoopPayloadServiceBuilder
where
    Node: FullNodeTypes,
    Pool: TransactionPool,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        _pool: Pool,
        _evm_config: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle> {
        let (tx, mut rx) = mpsc::unbounded_channel();

        ctx.task_executor().spawn_critical_os_thread(
            "payload-service",
            "payload builder service",
            async move {
                #[expect(clippy::collection_is_never_read)]
                let mut subscriptions = Vec::new();

                while let Some(message) = rx.recv().await {
                    match message {
                        PayloadServiceCommand::Subscribe(tx) => {
                            let (events_tx, events_rx) = broadcast::channel(100);
                            // Retain senders to make sure that channels are not getting closed
                            subscriptions.push(events_tx);
                            let _ = tx.send(events_rx);
                        }
                        message => warn!(?message, "Noop payload service received a message"),
                    }
                }
            },
        );

        Ok(PayloadBuilderHandle::new(tx))
    }
}
