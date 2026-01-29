//! `eth_` PubSub RPC extension for flashblocks and standard subscriptions
//!
//! This module provides an extended `eth_subscribe` implementation that supports both
//! standard Ethereum subscription types (newHeads, logs, newPendingTransactions, syncing)
//! and Base-specific flashblocks subscriptions (newFlashblocks, pendingLogs).

use std::sync::Arc;

use alloy_primitives::B256;
use alloy_rpc_types_eth::{Filter, Log, pubsub::Params};
use futures_util::stream;
use jsonrpsee::{
    PendingSubscriptionSink, SubscriptionSink,
    core::{SubscriptionResult, async_trait},
    proc_macros::rpc,
    server::SubscriptionMessage,
};
use op_alloy_network::Optimism;
use op_alloy_rpc_types::Transaction;
use reth_rpc::eth::EthPubSub as RethEthPubSub;
use reth_rpc_eth_api::{
    EthApiTypes, RpcBlock, RpcNodeCore, RpcTransaction,
    pubsub::EthPubSubApiServer as RethEthPubSubApiServer,
};
use serde::Serialize;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tracing::error;

use crate::{
    FlashblocksAPI,
    rpc::types::{BaseSubscriptionKind, ExtendedSubscriptionKind},
};

/// Eth pub-sub RPC extension for flashblocks and standard subscriptions.
///
/// This trait defines the `eth_subscribe` and `eth_unsubscribe` methods that handle
/// both standard Ethereum subscriptions and Base-specific flashblocks subscriptions.
#[rpc(server, namespace = "eth")]
pub trait EthPubSubApi {
    /// Create an Eth subscription for the given kind.
    ///
    /// Supports standard subscription types (newHeads, logs, newPendingTransactions, syncing)
    /// as well as Base-specific subscriptions (newFlashblocks, pendingLogs).
    #[subscription(
        name = "subscribe" => "subscription",
        unsubscribe = "unsubscribe",
        item = serde_json::Value
    )]
    async fn subscribe(
        &self,
        kind: ExtendedSubscriptionKind,
        params: Option<Params>,
    ) -> SubscriptionResult;
}

/// `Eth` pubsub RPC implementation that extends reth's standard implementation
/// with flashblocks support.
///
/// This handles `eth_subscribe` RPC calls for both standard Ethereum subscriptions
/// and Base-specific flashblocks subscriptions.
#[derive(Clone, Debug)]
pub struct EthPubSub<Eth, FB> {
    /// Reth's standard EthPubSub for handling standard subscription types
    inner: RethEthPubSub<Eth>,
    /// Flashblocks state for accessing pending blocks stream
    flashblocks_state: Arc<FB>,
}

impl<Eth, FB> EthPubSub<Eth, FB> {
    /// Creates a new instance with the given eth API and flashblocks state.
    pub fn new(eth_api: Eth, flashblocks_state: Arc<FB>) -> Self {
        Self { inner: RethEthPubSub::new(eth_api), flashblocks_state }
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

    /// Returns a stream that yields individual logs from pending flashblocks matching the filter.
    ///
    /// Each matching log is emitted as a separate stream item (one log per WebSocket message).
    fn pending_logs_stream(
        flashblocks_state: Arc<FB>,
        filter: Filter,
    ) -> impl Stream<Item = Log>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures_util::StreamExt::flat_map(
            StreamExt::filter_map(
                BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()),
                move |result| {
                    let pending_blocks = match result {
                        Ok(blocks) => blocks,
                        Err(err) => {
                            error!(
                                message = "Error in flashblocks stream for pending logs",
                                error = %err
                            );
                            return None;
                        }
                    };
                    let logs = pending_blocks.get_pending_logs(&filter);
                    if logs.is_empty() { None } else { Some(logs) }
                },
            ),
            stream::iter,
        )
    }

    /// Returns a stream that yields individual full transactions from pending flashblocks.
    ///
    /// Each transaction is emitted as a separate stream item (one transaction per WebSocket message).
    fn new_flashblock_transactions_full_stream(
        flashblocks_state: Arc<FB>,
    ) -> impl Stream<Item = Transaction>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures_util::StreamExt::flat_map(
            StreamExt::filter_map(
                BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()),
                |result| {
                    let pending_blocks = match result {
                        Ok(blocks) => blocks,
                        Err(err) => {
                            error!(
                                message = "Error in flashblocks stream for transactions",
                                error = %err
                            );
                            return None;
                        }
                    };
                    let txs = pending_blocks.get_pending_transactions();
                    if txs.is_empty() { None } else { Some(txs) }
                },
            ),
            stream::iter,
        )
    }

    /// Returns a stream that yields individual transaction hashes from pending flashblocks.
    ///
    /// Each hash is emitted as a separate stream item (one hash per WebSocket message).
    fn new_flashblock_transactions_hash_stream(
        flashblocks_state: Arc<FB>,
    ) -> impl Stream<Item = B256>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures_util::StreamExt::flat_map(
            StreamExt::filter_map(
                BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()),
                |result| {
                    let pending_blocks = match result {
                        Ok(blocks) => blocks,
                        Err(err) => {
                            error!(
                                message = "Error in flashblocks stream for transaction hashes",
                                error = %err
                            );
                            return None;
                        }
                    };
                    let hashes = pending_blocks.get_pending_transaction_hashes();
                    if hashes.is_empty() { None } else { Some(hashes) }
                },
            ),
            stream::iter,
        )
    }
}

#[async_trait]
impl<Eth, FB> EthPubSubApiServer for EthPubSub<Eth, FB>
where
    Eth: RpcNodeCore + EthApiTypes + Clone + Send + Sync + 'static,
    RethEthPubSub<Eth>: RethEthPubSubApiServer<RpcTransaction<Eth::NetworkTypes>>,
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    /// Handler for `eth_subscribe`
    ///
    /// Routes standard subscription types to reth's implementation and handles
    /// flashblocks subscriptions directly.
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        kind: ExtendedSubscriptionKind,
        params: Option<Params>,
    ) -> SubscriptionResult {
        // For standard subscription types, delegate to reth's implementation
        if let Some(standard_kind) = kind.as_standard() {
            return RethEthPubSubApiServer::subscribe(&self.inner, pending, standard_kind, params)
                .await;
        }

        // Handle flashblocks-specific subscriptions
        let ExtendedSubscriptionKind::Base(base_kind) = kind else {
            unreachable!("Standard subscription types should be delegated to inner");
        };

        let sink = pending.accept().await?;

        match base_kind {
            BaseSubscriptionKind::NewFlashblocks => {
                let stream = Self::new_flashblocks_stream(Arc::clone(&self.flashblocks_state));

                tokio::spawn(async move {
                    pipe_from_stream(sink, stream).await;
                });
            }
            BaseSubscriptionKind::PendingLogs => {
                // Extract filter from params, default to empty filter (match all)
                let filter = match params {
                    Some(Params::Logs(filter)) => *filter,
                    _ => Filter::default(),
                };

                let stream = Self::pending_logs_stream(Arc::clone(&self.flashblocks_state), filter);

                tokio::spawn(async move {
                    pipe_from_stream(sink, stream).await;
                });
            }
            BaseSubscriptionKind::NewFlashblockTransactions => {
                // Extract full_transactions param, default to false (hash only)
                let full = match params {
                    Some(Params::Bool(full)) => full,
                    _ => false,
                };

                if full {
                    let stream = Self::new_flashblock_transactions_full_stream(Arc::clone(
                        &self.flashblocks_state,
                    ));
                    tokio::spawn(async move {
                        pipe_from_stream(sink, stream).await;
                    });
                } else {
                    let stream = Self::new_flashblock_transactions_hash_stream(Arc::clone(
                        &self.flashblocks_state,
                    ));
                    tokio::spawn(async move {
                        pipe_from_stream(sink, stream).await;
                    });
                }
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
