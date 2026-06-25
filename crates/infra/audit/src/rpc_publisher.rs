//! HTTP-RPC bundle event publisher.
//!
//! `RpcBundleEventPublisher` implements [`BundleEventPublisher`] by sending
//! batches of [`BundleEvent`]s to the audit-archiver service over JSON-RPC
//! using the `base_persistBatchedBundleEvent` method. The batching cadence
//! (size + max-wait) is owned by `AuditConnector::connect_batched`; this
//! type only takes a `Vec<BundleEvent>` and ships it.

use core::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use tracing::debug;

use crate::{publisher::BundleEventPublisher, types::BundleEvent};

/// Default request timeout for the audit-archiver RPC client.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(2);

/// Publishes bundle events to the audit-archiver service over JSON-RPC.
#[derive(Debug, Clone)]
pub struct RpcBundleEventPublisher {
    client: HttpClient,
}

impl RpcBundleEventPublisher {
    /// Creates a new RPC publisher targeting the audit-archiver at `url` with
    /// the given request timeout.
    pub fn new(url: &str, request_timeout: Duration) -> Result<Self> {
        let client = HttpClientBuilder::default()
            .request_timeout(request_timeout)
            .build(url)
            .map_err(|e| anyhow::anyhow!("Failed to build audit-archiver HTTP client: {e}"))?;
        Ok(Self { client })
    }

    /// Convenience constructor using [`DEFAULT_RPC_TIMEOUT`].
    pub fn with_default_timeout(url: &str) -> Result<Self> {
        Self::new(url, DEFAULT_RPC_TIMEOUT)
    }
}

#[async_trait]
impl BundleEventPublisher for RpcBundleEventPublisher {
    async fn publish(&self, event: BundleEvent) -> Result<()> {
        self.publish_all(vec![event]).await
    }

    async fn publish_all(&self, events: Vec<BundleEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let batch_size = events.len();
        match self
            .client
            .request::<u32, _>("base_persistBatchedBundleEvent", rpc_params![&events])
            .await
        {
            Ok(forwarded) => {
                debug!(batch_size, forwarded, "Forwarded bundle event batch to audit-archiver");
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("RPC call failed: {e}")),
        }
    }
}
