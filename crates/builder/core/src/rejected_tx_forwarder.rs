//! Rejected transaction forwarder.
//!
//! Forwards rejected transactions from the builder to the audit-archiver
//! service via the `base_persistTransaction` RPC method.

use core::time::Duration;

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Information about a rejected transaction to be forwarded to the audit-archiver.
#[derive(Debug, Clone)]
pub struct RejectedTxInfo {
    /// The transaction hash.
    pub tx_hash: TxHash,
    /// The block number the transaction was intended for.
    pub block_number: u64,
    /// The reason the transaction was rejected.
    pub reason: String,
    /// Unix timestamp when the rejection occurred.
    pub timestamp: u64,
    /// The metering simulation response that informed the rejection decision.
    pub metering: MeterBundleResponse,
}

/// Forwards rejected transactions to the audit-archiver via RPC.
///
/// Runs as a background task, reading from an unbounded channel and making
/// fire-and-forget RPC calls to the audit-archiver service.
#[derive(Debug)]
pub struct RejectedTxForwarder {
    client: HttpClient,
    rx: mpsc::UnboundedReceiver<RejectedTxInfo>,
}

impl RejectedTxForwarder {
    /// Creates a new `RejectedTxForwarder`.
    pub fn new(
        audit_archiver_url: &str,
        rx: mpsc::UnboundedReceiver<RejectedTxInfo>,
    ) -> eyre::Result<Self> {
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(1))
            .build(audit_archiver_url)
            .map_err(|e| eyre::eyre!("Failed to build audit archiver HTTP client: {e}"))?;
        Ok(Self { client, rx })
    }

    /// Runs the forwarder loop, consuming rejected transaction info from the channel
    /// and forwarding each to the audit-archiver via RPC.
    pub async fn run(mut self) {
        info!("Rejected transaction forwarder started");
        while let Some(info) = self.rx.recv().await {
            let params = vec![json!({
                "block_number": info.block_number,
                "tx_hash": info.tx_hash,
                "reason": info.reason,
                "timestamp": info.timestamp,
                "metering": info.metering,
            })];

            match self.client.request::<bool, _>("base_persistTransaction", params).await {
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        error = %e,
                        tx_hash = %info.tx_hash,
                        block_number = info.block_number,
                        "Failed to forward rejected transaction to audit-archiver"
                    );
                }
            }
        }
        warn!("Rejected transaction forwarder channel closed");
    }
}
