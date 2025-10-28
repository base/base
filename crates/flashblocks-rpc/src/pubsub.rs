//! `base_` PubSub RPC implementation for flashblocks subscriptions

use std::sync::Arc;

use jsonrpsee::{
    core::{async_trait, SubscriptionResult},
    proc_macros::rpc,
    server::SubscriptionMessage,
    PendingSubscriptionSink, SubscriptionSink,
};
use op_alloy_network::Optimism;
use reth_rpc_eth_api::RpcBlock;
use serde::{Deserialize, Serialize};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use tracing::error;

use crate::rpc::FlashblocksAPI;

/// Subscription kind for Base-specific subscriptions
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BaseSubscriptionKind {
    /// New flashblocks subscription.
    ///
    /// Fires a notification each time a new flashblock is processed, providing the current
    /// pending block state. Each flashblock represents an incremental update to the pending
    /// block, so multiple notifications may be emitted for the same block height as new
    /// flashblocks arrive.
    NewFlashblocks,
}

/// Base pub-sub RPC interface for flashblocks subscriptions.
#[rpc(server, namespace = "base")]
pub trait BasePubSubApi {
    /// Create a Base subscription for the given kind
    #[subscription(
        name = "subscribe" => "subscription",
        unsubscribe = "unsubscribe",
        item = RpcBlock<Optimism>
    )]
    async fn subscribe(&self, kind: BaseSubscriptionKind) -> SubscriptionResult;
}

/// `Base` pubsub RPC implementation.
///
/// This handles `base_subscribe` RPC calls for flashblocks-specific subscriptions.
#[derive(Clone, Debug)]
pub struct BasePubSub<FB> {
    /// Flashblocks state for accessing pending blocks stream
    flashblocks_state: Arc<FB>,
}

impl<FB> BasePubSub<FB> {
    /// Creates a new instance with the given flashblocks state
    pub fn new(flashblocks_state: Arc<FB>) -> Self {
        Self { flashblocks_state }
    }

    /// Returns a stream that yields all new flashblocks as RPC blocks
    fn new_flashblocks_stream(&self) -> impl Stream<Item = RpcBlock<Optimism>>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        BroadcastStream::new(self.flashblocks_state.subscribe_to_flashblocks()).filter_map(
            |result| {
                let pending_blocks = match result {
                    Ok(blocks) => blocks,
                    Err(err) => {
                        error!(
                            message = "Error in flashblocks stream",
                            error = %err
                        );
                        return None;
                    }
                };
                Some(pending_blocks.get_latest_block(true))
            },
        )
    }
}

#[async_trait]
impl<FB> BasePubSubApiServer for BasePubSub<FB>
where
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    /// Handler for `base_subscribe`
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        kind: BaseSubscriptionKind,
    ) -> SubscriptionResult {
        let sink = pending.accept().await?;

        match kind {
            BaseSubscriptionKind::NewFlashblocks => {
                let stream = self.new_flashblocks_stream();

                tokio::spawn(async move {
                    pipe_from_stream(sink, stream).await;
                });
            }
        }

        Ok(())
    }
}

/// Pipes all stream items to the subscription sink.
///
/// This function runs until the stream ends, the client disconnects, or a serialization error occurs.
/// All exit conditions result in graceful termination.
async fn pipe_from_stream<T, St>(sink: SubscriptionSink, mut stream: St)
where
    St: Stream<Item = T> + Unpin,
    T: Serialize,
{
    loop {
        tokio::select! {
            // dropped by client
            _ = sink.closed() => return,

            maybe_item = stream.next() => {
                // stream ended
                let Some(item) = maybe_item else {
                    return;
                };

                let msg = match SubscriptionMessage::new(
                    sink.method_name(),
                    sink.subscription_id(),
                    &item
                ) {
                    Ok(msg) => msg,
                    Err(err) => {
                        error!(
                            target: "flashblocks_rpc::pubsub",
                            %err,
                            "Failed to serialize subscription message"
                        );
                        return;
                    }
                };

                // if it fails, client disconnected
                if sink.send(msg).await.is_err() {
                    return;
                }
            }
        }
    }
}
