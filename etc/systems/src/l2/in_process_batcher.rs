//! In-process batcher for L2 system test stacks.
//!
//! Runs `base-batcher-service` directly in the test process, eliminating the Docker
//! dependency for the batch submission layer. Mirrors the pattern used by
//! [`InProcessConsensus`](super::InProcessConsensus).

use alloy_primitives::B256;
use alloy_signer_local::PrivateKeySigner;
use base_batcher_encoder::DaType;
use base_batcher_service::{BatcherConfig, BatcherService};
use base_runtime::TokioRuntime;
use base_tx_manager::SignerConfig;
use eyre::Result;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Configuration for starting an in-process batcher.
#[derive(Debug, Clone)]
pub struct InProcessBatcherConfig {
    /// L1 RPC endpoint for batch transaction submission.
    pub l1_rpc_url: Url,
    /// L2 execution RPC endpoint for reading L2 blocks.
    pub l2_rpc_url: Url,
    /// Rollup node RPC endpoint for fetching the rollup config.
    pub rollup_rpc_url: Url,
    /// Batcher private key for signing L1 transactions.
    pub batcher_key: B256,
    /// Whether to use short-lived calldata channels for deterministic tests.
    pub force_batch_submission: bool,
}

/// A running in-process batcher.
pub struct InProcessBatcher {
    cancellation: CancellationToken,
    failure_rx: watch::Receiver<Option<String>>,
    _handle: JoinHandle<()>,
}

impl std::fmt::Debug for InProcessBatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessBatcher").finish_non_exhaustive()
    }
}

impl InProcessBatcher {
    /// Starts an in-process batcher with the given configuration.
    pub async fn start(config: InProcessBatcherConfig) -> Result<Self> {
        let signer = PrivateKeySigner::from_bytes(&config.batcher_key)
            .map_err(|e| eyre::eyre!("invalid batcher key: {e}"))?;
        let mut batcher_config = BatcherConfig {
            l1_rpc_url: vec![config.l1_rpc_url],
            l2_rpc_url: vec![config.l2_rpc_url],
            rollup_rpc_url: vec![config.rollup_rpc_url],
            signer: Some(SignerConfig::local(signer)),
            // SystemTestStack defaults come from the shared batcher config:
            // poll_interval: 1s, num_confirmations: 1, resubmission_timeout: 48s —
            // all set by BatcherConfig::default().
            ..BatcherConfig::default()
        };
        if config.force_batch_submission {
            batcher_config.encoder_config.da_type = DaType::Calldata;
        }
        let cancellation = CancellationToken::new();
        let runtime = TokioRuntime::with_token(cancellation.clone());
        let ready = BatcherService::new(batcher_config).setup(runtime).await?;
        let (failure_tx, failure_rx) = watch::channel(None);
        let handle = tokio::spawn(async move {
            if let Err(e) = ready.run().await {
                tracing::error!(error = %e, "in-process batcher exited with error");
                failure_tx.send_replace(Some(e.to_string()));
            }
        });
        Ok(Self { cancellation, failure_rx, _handle: handle })
    }

    /// Returns the batcher failure if its service task has exited with an error.
    pub fn failure(&self) -> Option<String> {
        self.failure_rx.borrow().clone()
    }

    /// Stops batch submission while retaining the service handle for test-stack ownership.
    pub fn stop(&self) {
        self.cancellation.cancel();
    }
}

impl Drop for InProcessBatcher {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
