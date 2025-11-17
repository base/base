use jsonrpsee::{
    core::{RpcResult, async_trait, client::ClientT},
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
    types::{ErrorCode, ErrorObjectOwned},
};
use tips_audit::BundleHistory;
use tracing::{error, info};
use uuid::Uuid;

use crate::rpc::TransactionStatusApiServer;

/// Proxy wrapper for TransactionStatusApi that can forward requests to external endpoints
pub struct TransactionStatusProxyImpl {
    proxy_client: Option<HttpClient>,
    proxy_url: Option<String>,
}

impl TransactionStatusProxyImpl {
    /// Creates a new proxy instance with optional local implementation and proxy endpoint
    pub fn new(
        proxy_url: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let proxy_client = if let Some(url) = &proxy_url {
            info!(message = "initializing transaction status proxy client", url = %url);
            Some(HttpClientBuilder::default().build(url)?)
        } else {
            None
        };

        Ok(Self { proxy_client, proxy_url })
    }

    /// Creates a proxy-only instance (no local implementation)
    pub fn proxy_only(proxy_url: String) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let proxy_client = HttpClientBuilder::default().build(&proxy_url)?;
        info!(message = "initializing proxy-only transaction status client", url = %proxy_url);

        Ok(Self { proxy_client: Some(proxy_client), proxy_url: Some(proxy_url) })
    }
}

#[async_trait]
impl TransactionStatusApiServer for TransactionStatusProxyImpl {
    async fn transaction_status(&self, id: Uuid) -> RpcResult<Option<BundleHistory>> {
        if let Some(client) = &self.proxy_client {
            info!(message = "forwarding transaction status request to proxy", id = %id, proxy_url = %self.proxy_url.as_ref().unwrap());

            match client
                .request::<Option<BundleHistory>, _>("base_transactionStatus", rpc_params![id])
                .await
            {
                Ok(result) => {
                    info!(message = "successfully received response from proxy", id = %id);
                    return Ok(result);
                }
                Err(e) => {
                    error!(message = "proxy request failed", id = %id, error = %e);
                    return Err(ErrorObjectOwned::owned(
                        ErrorCode::InternalError.code(),
                        format!("Proxy request failed: {e}"),
                        None::<()>,
                    ));
                }
            }
        }

        error!(message = "no transaction status proxy set", id = %id);
        Err(ErrorObjectOwned::owned(
            ErrorCode::InternalError.code(),
            "No transaction status proxy set".to_string(),
            None::<()>,
        ))
    }
}
