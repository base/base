//! In-process runner for the standalone L1-free sequencer node.

use std::sync::Arc;

use base_common_chain_config::{RollupConfig, SystemConfig};
use base_consensus_batch_types::L1BlockInfoTx;
use base_consensus_driver_service::{StandalonePrefund, StandaloneSequencerNode};
use eyre::Result;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Configuration for an L1-free snapshot sequencer.
#[derive(Debug)]
pub struct InProcessStandaloneSequencerConfig {
    /// Canonical rollup configuration for the snapshot chain.
    pub rollup_config: RollupConfig,
    /// Native execution client for the co-located execution node.
    pub execution: base_consensus_driver_service::LocalEngineClient,
    /// L1-info transaction decoded from the snapshot head.
    pub l1_info: L1BlockInfoTx,
    /// Effective system configuration at the snapshot head.
    pub system_config: SystemConfig,
    /// Optional one-time funding for a benchmark account.
    pub prefund: Option<StandalonePrefund>,
}

/// A running L1-free snapshot sequencer.
pub struct InProcessStandaloneSequencer {
    cancellation: CancellationToken,
    error_rx: mpsc::Receiver<String>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for InProcessStandaloneSequencer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessStandaloneSequencer").finish_non_exhaustive()
    }
}

impl InProcessStandaloneSequencer {
    /// Starts standalone consensus against a snapshot-backed builder.
    pub async fn start(config: InProcessStandaloneSequencerConfig) -> Result<Self> {
        let rollup_config = Arc::new(config.rollup_config);
        let mut engine_client = config.execution;
        engine_client.l1 = base_consensus_source_providers::L1RpcProvider::new_http(
            Url::parse("http://127.0.0.1:1").expect("valid unused L1 URL"),
        );
        engine_client.l2.rollup_config = Arc::clone(&rollup_config);

        let engine_client = Arc::new(engine_client);
        let node = StandaloneSequencerNode::new(
            rollup_config,
            engine_client,
            config.l1_info,
            config.system_config,
            config.prefund,
        );
        let cancellation = CancellationToken::new();
        let node_cancellation = cancellation.clone();
        let (error_tx, error_rx) = mpsc::channel(1);
        let handle = tokio::spawn(async move {
            if let Err(error) = node.start_with_cancellation(node_cancellation).await {
                tracing::error!(error = %error, "standalone consensus node failed");
                let _ = error_tx.send(error).await;
            }
        });

        Ok(Self { cancellation, error_rx, handle: Some(handle) })
    }

    /// Waits for the standalone node to report a fatal runtime error.
    pub async fn next_error(&mut self) -> String {
        self.error_rx
            .recv()
            .await
            .unwrap_or_else(|| "standalone consensus task exited unexpectedly".to_string())
    }

    /// Stops standalone consensus and waits for the task to observe cancellation.
    pub async fn shutdown(mut self) {
        self.cancellation.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for InProcessStandaloneSequencer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
