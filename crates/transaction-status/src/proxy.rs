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
