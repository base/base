//! In-process L1-free consensus for snapshot-backed development networks.

use std::sync::Arc;

use alloy_rpc_types_engine::JwtSecret;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_consensus_node::{EngineConfig, NodeMode, StandalonePrefund, StandaloneSequencerNode};
use base_protocol::L1BlockInfoTx;
use eyre::{Result, WrapErr};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Configuration for an L1-free snapshot sequencer.
#[derive(Debug)]
pub struct InProcessStandaloneConsensusConfig {
    /// Canonical rollup configuration for the snapshot chain.
    pub rollup_config: RollupConfig,
    /// JWT secret for the builder Engine API.
    pub jwt_secret: JwtSecret,
    /// Builder Engine API URL.
    pub l2_engine_url: Url,
    /// L1-info transaction decoded from the snapshot head.
    pub l1_info: L1BlockInfoTx,
    /// Effective system configuration at the snapshot head.
    pub system_config: SystemConfig,
    /// Optional one-time funding for a benchmark account.
    pub prefund: Option<StandalonePrefund>,
}

/// A running L1-free snapshot sequencer.
pub struct InProcessStandaloneConsensus {
    cancellation: CancellationToken,
    error_rx: mpsc::Receiver<String>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for InProcessStandaloneConsensus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessStandaloneConsensus").finish_non_exhaustive()
    }
}

impl InProcessStandaloneConsensus {
    /// Starts standalone consensus against a snapshot-backed builder.
    pub async fn start(config: InProcessStandaloneConsensusConfig) -> Result<Self> {
        let rollup_config = Arc::new(config.rollup_config);
        let engine_config = EngineConfig {
            config: Arc::clone(&rollup_config),
            l2_url: config.l2_engine_url,
            l2_jwt_secret: config.jwt_secret,
            // The engine client constructs this provider lazily. Standalone sequencing never
            // issues an L1 request.
            l1_url: Url::parse("http://127.0.0.1:1").expect("valid unused L1 URL"),
            mode: NodeMode::Sequencer,
            l1_rpc_timeout: base_consensus_providers::L1_RPC_TIMEOUT,
        };
        let engine_client = Arc::new(
            engine_config
                .build_engine_client()
                .await
                .map_err(eyre::Report::from)
                .wrap_err("failed to build standalone engine client")?,
        );
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

impl Drop for InProcessStandaloneConsensus {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
