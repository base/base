//! Payload service component for the node builder.

use std::future::Future;

use reth_evm::BaseEvmConfig;
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService, PayloadServiceCommand};
use reth_transaction_pool::TransactionPool;
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

use crate::{BuilderContext, FullNodeTypes};

/// A type that knows how to spawn the payload service.
pub trait PayloadServiceBuilder<Node: FullNodeTypes, Pool: TransactionPool>: Send + Sized {
    /// Spawns the [`PayloadBuilderService`] and returns the handle to it for use by the engine.
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
