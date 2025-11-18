use alloy_primitives::TxHash;
use jsonrpsee::{
    core::{RpcResult, async_trait, client::ClientT},
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
    types::{ErrorCode, ErrorObjectOwned},
};
use tips_audit::BundleHistory;
use tracing::{error, info};

use crate::rpc::TransactionStatusApiServer;

/// Proxy that forwards transaction status requests to an external endpoint
pub struct TransactionStatusProxyImpl {
    proxy_client: HttpClient,
    proxy_url: String,
}

impl TransactionStatusProxyImpl {
    pub fn new(proxy_url: String) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let proxy_client = HttpClientBuilder::default().build(&proxy_url)?;
        info!(message = "initializing transaction status proxy client", url = %proxy_url);

        Ok(Self { proxy_client, proxy_url })
    }
}

#[async_trait]
impl TransactionStatusApiServer for TransactionStatusProxyImpl {
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<Option<BundleHistory>> {
        info!(message = "forwarding transaction status request to proxy", tx_hash = %tx_hash, proxy_url = %self.proxy_url);

        match self
            .proxy_client
            .request::<Option<BundleHistory>, _>("base_transactionStatus", rpc_params![tx_hash])
            .await
        {
            Ok(result) => {
                info!(message = "successfully received response from proxy", tx_hash = %tx_hash);
                return Ok(result);
            }
            Err(e) => {
                error!(message = "proxy request failed", tx_hash = %tx_hash, error = %e);
                return Err(ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("Proxy request failed: {e}"),
                    None::<()>,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::TxHash;
    use tokio::time::timeout;

    use crate::{proxy::TransactionStatusProxyImpl, rpc::TransactionStatusApiServer};

    #[tokio::test]
    async fn test_proxy_creation_valid_url() {
        let result = TransactionStatusProxyImpl::new("https://mainnet.base.org".to_string());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_proxy_creation_invalid_url() {
        let result = TransactionStatusProxyImpl::new("invalid-url".to_string());
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_proxy_connection_failure() {
        // Use a non-existent URL that's valid format but unreachable
        let proxy = TransactionStatusProxyImpl::new("http://127.0.0.1:9999".to_string())
            .expect("Failed to create proxy");

        let tx_hash = TxHash::from([1u8; 32]);

        // This should fail due to connection error
        let result = proxy.transaction_status(tx_hash).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_proxy_timeout() {
        let proxy = TransactionStatusProxyImpl::new("http://127.0.0.1:9999".to_string())
            .expect("Failed to create proxy");

        let tx_hash = TxHash::from([1u8; 32]);

        // Test with timeout - should fail quickly
        let result = timeout(Duration::from_millis(100), proxy.transaction_status(tx_hash)).await;

        // Should timeout or return connection error
        assert!(result.is_err() || result.unwrap().is_err());
    }
}
