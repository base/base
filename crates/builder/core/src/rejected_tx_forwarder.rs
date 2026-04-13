//! Rejected transaction forwarder.
//!
//! Forwards rejected transactions from the builder to the audit-archiver
//! service via the `base_persistRejectedTransaction` RPC method.

use core::time::Duration;

use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use base_bundles::RejectedTransaction;

/// Forwards rejected transactions to the audit-archiver via RPC.
///
/// Runs as a background task, reading from an unbounded channel and making
/// fire-and-forget RPC calls to the audit-archiver service.
#[derive(Debug)]
pub struct RejectedTxForwarder {
    client: HttpClient,
        rx: mpsc::UnboundedReceiver<RejectedTransaction>,
}

impl RejectedTxForwarder {
    /// Creates a new `RejectedTxForwarder`.
    pub fn new(
        audit_archiver_url: &str,
    rx: mpsc::UnboundedReceiver<RejectedTransaction>,
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
        while let Some(rejected_tx) = self.rx.recv().await {
            match self
                .client
                .request::<bool, _>("base_persistRejectedTransaction", vec![&rejected_tx])
                .await
            {
                Ok(_) => {
                    info!(
                        tx_hash = %rejected_tx.tx_hash,
                        block_number = rejected_tx.block_number,
                        "Forwarded rejected transaction to audit-archiver"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        tx_hash = %rejected_tx.tx_hash,
                        block_number = rejected_tx.block_number,
                        "Failed to forward rejected transaction to audit-archiver"
                    );
                }
            }
        }
        warn!("Rejected transaction forwarder channel closed");
    }
}
