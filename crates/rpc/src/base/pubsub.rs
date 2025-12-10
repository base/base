//! `eth_` PubSub RPC extension for flashblocks subscriptions

use std::sync::Arc;

use base_reth_flashblocks::FlashblocksAPI;
use jsonrpsee::{
    PendingSubscriptionSink, SubscriptionSink,
    core::{SubscriptionResult, async_trait},
    proc_macros::rpc,
    server::SubscriptionMessage,
};
use op_alloy_network::Optimism;
use reth_rpc_eth_api::RpcBlock;
use serde::Serialize;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tracing::error;

use crate::EthSubscriptionKind;

/// Eth pub-sub RPC extension for flashblocks subscriptions.
#[rpc(server, namespace = "eth")]
pub trait EthPubSubApi {
    /// Create an Eth subscription for the given kind
    #[subscription(
        name = "subscribe" => "subscription",
        unsubscribe = "unsubscribe",
        item = RpcBlock<Optimism>
    )]
    async fn subscribe(&self, kind: EthSubscriptionKind) -> SubscriptionResult;
}

/// `Eth` pubsub RPC extension implementation.
///
/// This handles `eth_subscribe` RPC calls for flashblocks-specific subscriptions.
#[derive(Clone, Debug)]
pub struct EthPubSub<FB> {
    /// Flashblocks state for accessing pending blocks stream
    flashblocks_state: Arc<FB>,
}

impl<FB> EthPubSub<FB> {
    /// Creates a new instance with the given flashblocks state
    pub const fn new(flashblocks_state: Arc<FB>) -> Self {
        Self { flashblocks_state }
    }

    /// Returns a stream that yields all new flashblocks as RPC blocks
    fn new_flashblocks_stream(flashblocks_state: Arc<FB>) -> impl Stream<Item = RpcBlock<Optimism>>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()).filter_map(|result| {
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
        })
    }
}

#[async_trait]
impl<FB> EthPubSubApiServer for EthPubSub<FB>
where
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    /// Handler for `eth_subscribe`
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        kind: EthSubscriptionKind,
    ) -> SubscriptionResult {
        let sink = pending.accept().await?;

        match kind {
            EthSubscriptionKind::NewFlashblocks => {
                let stream = Self::new_flashblocks_stream(Arc::clone(&self.flashblocks_state));

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
