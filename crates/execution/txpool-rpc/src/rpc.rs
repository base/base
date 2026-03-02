//! RPC implementation for transaction status queries and pool management.

use alloy_primitives::{Address, TxHash};
use jsonrpsee::{
    core::{RpcResult, async_trait, client::ClientT},
    http_client::{HttpClient, HttpClientBuilder},
    proc_macros::rpc,
    rpc_params,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_transaction_pool::TransactionPool;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// The status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub enum Status {
    /// Transaction is not known to the node.
    Unknown,
    /// Transaction is known to the node (in mempool or confirmed).
    Known,
}

/// Response containing the status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct TransactionStatusResponse {
    /// The status of the queried transaction.
    pub status: Status,
}

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse>;
}

/// Admin RPC API for transaction pool management operations.
///
/// Complements the upstream `admin_clearTxpool` method provided by reth,
/// which removes all transactions from the pool.
#[rpc(server, namespace = "admin")]
pub trait AdminTxPoolApi {
    /// Drops all transactions from a specific sender address.
    #[method(name = "dropSenderTransactions")]
    async fn drop_sender_transactions(&self, sender: Address) -> RpcResult<Vec<TxHash>>;

    /// Drops a single transaction by its hash.
    #[method(name = "dropTransaction")]
    async fn drop_transaction(&self, tx_hash: TxHash) -> RpcResult<bool>;
}

/// Implementation of the transaction status RPC API.
#[derive(Debug)]
pub struct TransactionStatusApiImpl<Pool: TransactionPool> {
    sequencer_client: Option<HttpClient>,
    pool: Pool,
}

impl<Pool: TransactionPool + 'static> TransactionStatusApiImpl<Pool> {
    /// Creates a new transaction status API instance.
    ///
    /// If `sequencer_url` is provided, status queries will be forwarded to the sequencer.
    /// Otherwise, the local transaction pool will be queried.
    pub fn new(
        sequencer_url: Option<String>,
        pool: Pool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let sequencer_client = if let Some(ref url) = sequencer_url {
            debug!("fetching transaction status from sequencer");
            Some(HttpClientBuilder::default().build(url)?)
        } else {
            debug!("fetching transaction status from local transaction pool");
            None
        };

        Ok(Self { sequencer_client, pool })
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
                warn!(tx_hash = %tx_hash, error = %e, "failed to fetch transaction status");
                Err(ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("failed to fetch transaction status: {e}"),
                    None::<()>,
                ))
            }
        }
    }
}

/// Implementation of the admin transaction pool management RPC API.
#[derive(Debug)]
pub struct AdminTxPoolApiImpl<Pool: TransactionPool> {
    pool: Pool,
}

impl<Pool: TransactionPool + 'static> AdminTxPoolApiImpl<Pool> {
    /// Creates a new admin transaction pool management API instance.
    pub const fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl<Pool: TransactionPool + 'static> AdminTxPoolApiServer for AdminTxPoolApiImpl<Pool> {
    async fn drop_sender_transactions(&self, sender: Address) -> RpcResult<Vec<TxHash>> {
        let removed = self.pool.remove_transactions_by_sender(sender);
        let hashes: Vec<TxHash> = removed.iter().map(|tx| *tx.hash()).collect();
        info!(sender = %sender, count = hashes.len(), "dropped transactions by sender");
        Ok(hashes)
    }

    async fn drop_transaction(&self, tx_hash: TxHash) -> RpcResult<bool> {
        let removed = self.pool.remove_transactions(vec![tx_hash]);
        let was_removed = !removed.is_empty();
        info!(tx_hash = %tx_hash, removed = was_removed, "dropped transaction");
        Ok(was_removed)
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

    use super::*;

    #[tokio::test]
    async fn test_transaction_status() -> eyre::Result<()> {
        let pool = testing_pool();
        let rpc =
            TransactionStatusApiImpl::new(None, pool.clone()).expect("should be able to init rpc");

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

        let rpc = TransactionStatusApiImpl::new(Some(sequencer.base_url()), testing_pool())
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
        let rpc = TransactionStatusApiImpl::new(Some(sequencer.base_url()), testing_pool())
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
    async fn test_drop_sender_no_transactions() {
        let pool = testing_pool();
        let rpc = AdminTxPoolApiImpl::new(pool);

        let sender = Address::random();
        let removed = rpc.drop_sender_transactions(sender).await.expect("should succeed");
        assert!(removed.is_empty());
    }

    #[tokio::test]
    async fn test_drop_sender_with_transactions() {
        let pool = testing_pool();

        let sender1 = Address::random();
        let sender2 = Address::random();

        // Add transactions from two different senders (different nonces to avoid replacement)
        let tx1 = MockTransaction::eip1559().with_sender(sender1).with_nonce(0);
        let tx2 = MockTransaction::eip1559().with_sender(sender1).with_nonce(1);
        let tx3 = MockTransaction::eip1559().with_sender(sender2).with_nonce(0);

        let hash1 = *tx1.hash();
        let hash2 = *tx2.hash();
        let hash3 = *tx3.hash();

        pool.add_transaction(TransactionOrigin::Local, tx1).await.expect("should add tx1");
        pool.add_transaction(TransactionOrigin::Local, tx2).await.expect("should add tx2");
        pool.add_transaction(TransactionOrigin::Local, tx3).await.expect("should add tx3");

        let rpc = AdminTxPoolApiImpl::new(pool.clone());

        // Remove sender1's transactions
        let removed = rpc.drop_sender_transactions(sender1).await.expect("should succeed");
        assert_eq!(2, removed.len());
        assert!(removed.contains(&hash1));
        assert!(removed.contains(&hash2));

        // sender2's transaction should still be in pool
        let remaining = pool.all_transaction_hashes();
        assert_eq!(1, remaining.len());
        assert!(remaining.contains(&hash3));
    }

    #[tokio::test]
    async fn test_drop_transaction_not_found() {
        let pool = testing_pool();
        let rpc = AdminTxPoolApiImpl::new(pool);

        let result = rpc.drop_transaction(TxHash::random()).await.expect("should succeed");
        assert!(!result);
    }

    #[tokio::test]
    async fn test_drop_transaction_found() {
        let pool = testing_pool();

        let tx = MockTransaction::eip1559();
        let hash = *tx.hash();

        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("should add tx");

        let rpc = AdminTxPoolApiImpl::new(pool.clone());

        let result = rpc.drop_transaction(hash).await.expect("should succeed");
        assert!(result);

        // Verify tx is gone
        assert!(pool.get(&hash).is_none());
    }

    #[tokio::test]
    async fn test_drop_transaction_idempotent() {
        let pool = testing_pool();

        let tx = MockTransaction::eip1559();
        let hash = *tx.hash();

        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("should add tx");

        let rpc = AdminTxPoolApiImpl::new(pool);

        let first = rpc.drop_transaction(hash).await.expect("should succeed");
        assert!(first);

        // Second removal should return false
        let second = rpc.drop_transaction(hash).await.expect("should succeed");
        assert!(!second);
    }
}
