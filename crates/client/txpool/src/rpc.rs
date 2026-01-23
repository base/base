//! RPC implementation for transaction status queries.

use tokio::sync::mpsc::Receiver;

use alloy_primitives::TxHash;
use jsonrpsee::{
    core::{RpcResult, async_trait, client::ClientT},
    http_client::{HttpClient, HttpClientBuilder},
    proc_macros::rpc,
    rpc_params,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_transaction_pool::TransactionPool;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use moka::sync::Cache;
use std::sync::Arc;

/// The status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub enum Status {
    /// Transaction is not known to the node.
    Unknown,
    /// Transaction is known to the node (in mempool or confirmed).
    Known,
}

/// Response containing the status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct TransactionStatusResponse {
    /// The status of the queried transaction.
    pub status: Status,
}

/// Response containing the lifecycle events of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub enum TransactionLifecycleResponse {
    /// The lifecycle events of the queried transaction.
    Events(Vec<String>),
    /// The transaction is not known to the node.
    Unknown(String),
}

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse>;

    /// Gets the lifecycle events of a transaction
    #[method(name = "transactionLifecycle")]
    async fn transaction_lifecycle(&self, tx_hash: TxHash) -> RpcResult<TransactionLifecycleResponse>;
}

/// Implementation of the transaction status RPC API.
#[derive(Debug)]
pub struct TransactionStatusApiImpl<Pool: TransactionPool> {
    sequencer_client: Option<HttpClient>,
    pool: Pool,
    tx_events_cache: Arc<Cache<TxHash, Vec<String>>>,
}

impl<Pool: TransactionPool + 'static> TransactionStatusApiImpl<Pool> {
    /// Creates a new transaction status API instance.
    ///
    /// If `sequencer_url` is provided, status queries will be forwarded to the sequencer.
    /// Otherwise, the local transaction pool will be queried.
    pub fn new(
        sequencer_url: Option<String>,
        pool: Pool,
        mut tx_recv: Receiver<(TxHash, Vec<String>)>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let sequencer_client = if let Some(ref url) = sequencer_url {
            info!("fetching transaction status from sequencer");
            Some(HttpClientBuilder::default().build(url)?)
        } else {
            info!("fetching transaction status from local transaction pool");
            None
        };

        // 2 mins = 60 blocks (120s/2s), with each block having ~300 txs, so 18000 txs, round up for some buffer
        // TODO: make this configurable?
        let tx_events_cache = Arc::new(Cache::new(20_000));
        let cache_clone = Arc::clone(&tx_events_cache);
        tokio::spawn(async move {
            while let Some((tx_hash, events)) = tx_recv.recv().await {
                cache_clone.insert(tx_hash, events);
            }
        });

        Ok(Self {
            sequencer_client,
            pool,
            tx_events_cache,
        })
    }
}

#[async_trait]
impl<Pool: TransactionPool + 'static> TransactionStatusApiServer
    for TransactionStatusApiImpl<Pool>
{
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse> {
        let Some(ref sequencer_client) = self.sequencer_client else {
            return Ok(match self.pool.get(&tx_hash) {
                Some(_) => TransactionStatusResponse { status: Status::Known },
                None => TransactionStatusResponse { status: Status::Unknown },
            });
        };

        match sequencer_client
            .request::<TransactionStatusResponse, _>("base_transactionStatus", rpc_params![tx_hash])
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                warn!(message = "failed to fetch transaction status", tx_hash = %tx_hash, error = %e);
                Err(ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("failed to fetch transaction status: {e}"),
                    None::<()>,
                ))
            }
        }
    }

    async fn transaction_lifecycle(&self, tx_hash: TxHash) -> RpcResult<TransactionLifecycleResponse> {
        let Some(ref tx_events) = self.tx_events_cache.get(&tx_hash) else {
            return Ok(TransactionLifecycleResponse::Unknown(format!("transaction not found in cache: {tx_hash}, it may have been evicted from the cache")));
        };

        Ok(TransactionLifecycleResponse::Events(tx_events.clone()))
    }
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use reth_transaction_pool::{
        PoolTransaction, TransactionOrigin,
        test_utils::{MockTransaction, testing_pool},
    };
    use serde_json::{self, json};

    use crate::{tracker::Tracker, TxEvent, Pool};

    use super::*;

    #[tokio::test]
    async fn test_transaction_status() -> eyre::Result<()> {
        let pool = testing_pool();
        let (_, tx_recv) = tokio::sync::mpsc::channel::<(TxHash, Vec<String>)>(1000);
        let rpc =
            TransactionStatusApiImpl::new(None, pool.clone(), tx_recv).expect("should be able to init rpc");

        let result = rpc
            .transaction_status(TxHash::random())
            .await
            .expect("should be able to fetch status")
            .status;
        assert_eq!(Status::Unknown, result);

        let tx = MockTransaction::eip1559();
        let hash = *tx.hash();

        let before = rpc
            .transaction_status(hash)
            .await
            .expect("should be able to fetch transaction status")
            .status;
        pool.add_transaction(TransactionOrigin::Local, tx)
            .await
            .expect("should be able to add local transaction");
        let after = rpc
            .transaction_status(hash)
            .await
            .expect("should be able to fetch transaction status")
            .status;

        assert_eq!(Status::Unknown, before);
        assert_eq!(Status::Known, after);

        Ok(())
    }

    #[tokio::test]
    async fn test_remote_status_failures() -> eyre::Result<()> {
        let tx = TxHash::random();

        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST)
                .path("/")
                .json_body(json!({"jsonrpc": "2.0", "id": 0, "method": "base_transactionStatus", "params": [tx]}));
            then.status(500);
        });

        let (_, tx_recv) = tokio::sync::mpsc::channel::<(TxHash, Vec<String>)>(1000);
        let rpc = TransactionStatusApiImpl::new(Some(sequencer.base_url()), testing_pool(), tx_recv)
            .expect("should be able to init rpc");

        let status = rpc.transaction_status(tx).await;
        assert!(status.is_err());

        mock.assert();

        Ok(())
    }

    #[tokio::test]
    async fn test_remote_success() -> eyre::Result<()> {
        let known_tx = TxHash::random();
        let unknown_tx = TxHash::random();

        let sequencer = MockServer::start();
        let (_, tx_recv) = tokio::sync::mpsc::channel::<(TxHash, Vec<String>)>(1000);
        let rpc = TransactionStatusApiImpl::new(Some(sequencer.base_url()), testing_pool(), tx_recv)
            .expect("should be able to init rpc");

        let response = |id: u8, status: Status| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "status": status
                }
            })
        };

        let known_mock = sequencer.mock(|when, then| {
            when.method(POST)
                .path("/")
                .json_body(json!({"jsonrpc": "2.0", "id": 0, "method": "base_transactionStatus", "params": [known_tx]}));
            then.status(200)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&response(0, Status::Known)).unwrap());
        });

        let status = rpc
            .transaction_status(known_tx)
            .await
            .expect("should be able to fetch transaction status");
        assert_eq!(Status::Known, status.status);
        known_mock.assert();

        let unknown_mock = sequencer.mock(|when, then| {
            when.method(POST)
                .path("/")
                .json_body(json!({"jsonrpc": "2.0", "id": 1, "method": "base_transactionStatus", "params": [unknown_tx]}));
            then.status(200)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&response(1, Status::Unknown)).unwrap());
        });

        let status = rpc
            .transaction_status(unknown_tx)
            .await
            .expect("should be able to fetch transaction status");
        assert_eq!(Status::Unknown, status.status);
        unknown_mock.assert();

        Ok(())
    }

    #[tokio::test]
    async fn test_transaction_lifecycle_e2e() -> eyre::Result<()> {
        // Setup
        let pool = testing_pool();
        let (tx_send, tx_recv) = tokio::sync::mpsc::channel::<(TxHash, Vec<String>)>(1000);
        let mut tracker = Tracker::new(false, tx_send);
        let rpc = TransactionStatusApiImpl::new(None, pool.clone(), tx_recv).expect("should be able to init rpc");

        // Insert a transaction as queued first, then move it to pending
        // This will trigger the event to be sent to the channel
        let tx_hash = TxHash::random();
        tracker.transaction_inserted(tx_hash, TxEvent::Queued).await?;
        tracker.transaction_moved(tx_hash, Pool::Queued).await?;

        // Move from queued to pending - this will send the events to the channel
        tracker.transaction_moved(tx_hash, Pool::Pending).await?;

        // Wait for the cache to be populated (the spawned task needs time to process)
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Query the lifecycle - should have Queued and QueuedToPending events
        let lifecycle = rpc.transaction_lifecycle(tx_hash).await?;
        let events = match lifecycle {
            TransactionLifecycleResponse::Events(e) => e,
            TransactionLifecycleResponse::Unknown(msg) => panic!("Expected events, got Unknown: {}", msg),
        };
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("queued"));
        assert!(events[1].contains("queued_to_pending"));

        // Now include it
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion).await?;

        // Wait for the cache to be populated (the spawned task needs time to process)
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Query the lifecycle - should have BlockInclusion event
        let lifecycle = rpc.transaction_lifecycle(tx_hash).await?;
        let events = match lifecycle {
            TransactionLifecycleResponse::Events(e) => e,
            TransactionLifecycleResponse::Unknown(msg) => panic!("Expected events, got Unknown: {}", msg),
        };
        assert_eq!(events.len(), 3);
        assert!(events[0].contains("queued"));
        assert!(events[1].contains("queued_to_pending"));
        assert!(events[2].contains("block_inclusion"));

        Ok(())
    }
}
