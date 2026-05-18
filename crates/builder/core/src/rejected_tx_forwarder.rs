//! Rejected transaction forwarder.
//!
//! Forwards rejected transactions from the builder to the audit-archiver
//! service via the `base_persistRejectedTransactionBatch` RPC method.

use core::time::Duration;

use base_bundles::RejectedTransaction;
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::BuilderMetrics;

/// Forwards rejected transactions to the audit-archiver via RPC.
///
/// Runs as a background task, reading batches of rejected transactions
/// (one batch per canonical block) from a bounded channel and making
/// fire-and-forget RPC calls to the audit-archiver service.
#[derive(Debug)]
pub struct RejectedTxForwarder {
    client: HttpClient,
    rx: mpsc::Receiver<Vec<RejectedTransaction>>,
}

impl RejectedTxForwarder {
    /// Creates a new `RejectedTxForwarder`.
    pub fn new(
        audit_archiver_url: &str,
        rx: mpsc::Receiver<Vec<RejectedTransaction>>,
    ) -> eyre::Result<Self> {
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(1))
            .build(audit_archiver_url)
            .map_err(|e| eyre::eyre!("Failed to build audit archiver HTTP client: {e}"))?;
        Ok(Self { client, rx })
    }

    /// Runs the forwarder loop, consuming per-block batches of rejected transactions
    /// from the channel and forwarding each batch to the audit-archiver via RPC.
    pub async fn run(mut self) {
        info!("Rejected transaction forwarder started");
        while let Some(batch) = self.rx.recv().await {
            let batch_size = batch.len();
            let block_number = batch.first().map(|tx| tx.block_number);
            match self
                .client
                .request::<u32, _>("base_persistRejectedTransactionBatch", rpc_params![&batch])
                .await
            {
                Ok(persisted) if persisted as usize == batch_size => {
                    BuilderMetrics::rejected_txs_forwarded().increment(batch_size as u64);
                    info!(
                        batch_size,
                        block_number = ?block_number,
                        "Forwarded rejected transaction batch to audit-archiver"
                    );
                }
                Ok(persisted) => {
                    BuilderMetrics::rejected_txs_forwarded().increment(persisted as u64);
                    warn!(
                        persisted,
                        failed = batch_size.saturating_sub(persisted as usize),
                        batch_size,
                        block_number = ?block_number,
                        "Partial failure persisting rejected transaction batch"
                    );
                }
                Err(e) => {
                    BuilderMetrics::rejected_tx_forward_failures().increment(1);
                    warn!(
                        error = %e,
                        batch_size,
                        block_number = ?block_number,
                        "Failed to forward rejected transaction batch to audit-archiver"
                    );
                }
            }
        }
        warn!("Rejected transaction forwarder channel closed");
    }
}
