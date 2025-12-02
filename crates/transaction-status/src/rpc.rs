use alloy_primitives::TxHash;
use jsonrpsee::{
    core::{RpcResult, async_trait, client::ClientT},
    http_client::{HttpClient, HttpClientBuilder},
    proc_macros::rpc,
    rpc_params,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_transaction_pool::TransactionPool;
use tracing::{info, warn};

use crate::{Status, TransactionStatusResponse};

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse>;
}

pub struct TransactionStatusApiImpl<Pool: TransactionPool> {
    sequencer_client: Option<HttpClient>,
    pool: Pool,
}

impl<Pool: TransactionPool + 'static> TransactionStatusApiImpl<Pool> {
    pub fn new(
        sequencer_url: Option<String>,
        pool: Pool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let sequencer_client = if let Some(ref url) = sequencer_url {
            info!("fetching transaction status from sequencer");
            Some(HttpClientBuilder::default().build(url)?)
        } else {
            info!("fetching transaction status from local transaction pool");
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
                warn!(message = "failed to fetch transaction status", tx_hash = %tx_hash, error = %e);
                Err(ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("failed to fetch transaction status: {e}"),
                    None::<()>,
                ))
            }
        }
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
    use crate::Status::{Known, Unknown};

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
        assert_eq!(Unknown, result);

        let tx = MockTransaction::eip1559();
        let hash = tx.hash().clone();

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

        assert_eq!(Unknown, before);
        assert_eq!(Known, after);

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
                .body(serde_json::to_string(&response(0, Known)).unwrap());
        });

        let status = rpc
            .transaction_status(known_tx)
            .await
            .expect("should be able to fetch transaction status");
        assert_eq!(Known, status.status);
        known_mock.assert();

        let unknown_mock = sequencer.mock(|when, then| {
            when.method(POST)
                .path("/")
                .json_body(json!({"jsonrpc": "2.0", "id": 1, "method": "base_transactionStatus", "params": [unknown_tx]}));
            then.status(200)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&response(1, Unknown)).unwrap());
        });

        let status = rpc
            .transaction_status(unknown_tx)
            .await
            .expect("should be able to fetch transaction status");
        assert_eq!(Unknown, status.status);
        unknown_mock.assert();

        Ok(())
    }
}
