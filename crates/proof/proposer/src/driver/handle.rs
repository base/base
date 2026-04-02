//! Lifecycle management for the proving pipeline.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use base_proof_contracts::{AnchorStateRegistryClient, DisputeGameFactoryClient};
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use eyre::Result;
use tokio::{sync::Mutex as TokioMutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::pipeline::ProvingPipeline;

/// Trait for controlling the proposer at runtime.
///
/// This is the type-erased interface consumed by the admin JSON-RPC server.
/// [`PipelineHandle`] is the concrete implementation.
#[async_trait]
pub trait ProposerDriverControl: Send + Sync {
    /// Start the proving pipeline.
    async fn start_proposer(&self) -> Result<(), String>;
    /// Stop the proving pipeline.
    async fn stop_proposer(&self) -> Result<(), String>;
    /// Returns whether the proving pipeline is currently running.
    fn is_running(&self) -> bool;
}

/// Manages the lifecycle of a [`ProvingPipeline`], allowing it to be started
/// and stopped at runtime (e.g. via the admin RPC).
pub struct PipelineHandle<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    #[allow(clippy::type_complexity)]
    pipeline: Arc<TokioMutex<ProvingPipeline<L1, L2, R, ASR, F>>>,
    session_cancel: TokioMutex<CancellationToken>,
    global_cancel: CancellationToken,
    task: TokioMutex<Option<JoinHandle<Result<()>>>>,
    running: Arc<AtomicBool>,
}

impl<L1, L2, R, ASR, F> std::fmt::Debug for PipelineHandle<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineHandle")
            .field("running", &self.running.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<L1, L2, R, ASR, F> PipelineHandle<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    /// Creates a new [`PipelineHandle`] wrapping the given proving pipeline.
    pub fn new(
        pipeline: ProvingPipeline<L1, L2, R, ASR, F>,
        global_cancel: CancellationToken,
    ) -> Self {
        let session_cancel = global_cancel.child_token();
        Self {
            pipeline: Arc::new(TokioMutex::new(pipeline)),
            session_cancel: TokioMutex::new(session_cancel),
            global_cancel,
            task: TokioMutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl<L1, L2, R, ASR, F> ProposerDriverControl for PipelineHandle<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    async fn start_proposer(&self) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("proposer is already running".into());
        }

        let cancel = self.global_cancel.child_token();
        {
            let mut pipeline = self.pipeline.lock().await;
            pipeline.set_cancel(cancel.clone());
        }
        *self.session_cancel.lock().await = cancel;

        let pipeline = Arc::clone(&self.pipeline);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);

        let handle = tokio::spawn(async move {
            let guard = pipeline.lock().await;
            let result = guard.run().await;
            running.store(false, Ordering::SeqCst);
            result
        });

        *self.task.lock().await = Some(handle);
        info!("Proving pipeline started");
        Ok(())
    }

    async fn stop_proposer(&self) -> Result<(), String> {
        if !self.running.load(Ordering::SeqCst) {
            return Err("proposer is not running".into());
        }

        self.session_cancel.lock().await.cancel();

        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }

        info!("Proving pipeline stopped");
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use alloy_primitives::{B256, Bytes};
    use async_trait::async_trait;
    use base_proof_primitives::{ProofResult, Proposal, ProverClient};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        driver::{core::DriverConfig, pipeline::PipelineConfig},
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockRollupClient, test_anchor_root, test_sync_status,
        },
    };

    #[derive(Debug)]
    struct InstantMockProver;

    #[async_trait]
    impl ProverClient for InstantMockProver {
        async fn prove(
            &self,
            request: base_proof_primitives::ProofRequest,
        ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>> {
            let n = request.claimed_l2_block_number;
            let proposal = Proposal {
                output_root: B256::repeat_byte(n as u8),
                signature: Bytes::from(vec![0xab; 65]),
                l1_origin_hash: B256::repeat_byte(0x02),
                l1_origin_number: 100 + n,
                l2_block_number: n,
                prev_output_root: B256::repeat_byte(0x03),
                config_hash: B256::repeat_byte(0x04),
            };
            let start = n.saturating_sub(512);
            let proposals: Vec<Proposal> = ((start + 1)..=n)
                .map(|b| Proposal { output_root: B256::repeat_byte(b as u8), ..proposal.clone() })
                .collect();
            Ok(ProofResult::Tee { aggregate_proposal: proposal, proposals })
        }
    }

    fn test_pipeline_handle(
        global_cancel: CancellationToken,
    ) -> PipelineHandle<
        MockL1,
        MockL2,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    > {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> = Arc::new(InstantMockProver);
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(0) });
        let factory = Arc::new(MockDisputeGameFactory::with_count(0));

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_game_recovery_lookback: 5000,
                max_retries: 3,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    poll_interval: Duration::from_secs(3600),
                    block_interval: 512,
                    intermediate_block_interval: 512,
                    ..Default::default()
                },
            },
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier::empty()),
            Arc::new(MockOutputProposer),
            global_cancel.child_token(),
        );
        PipelineHandle::new(pipeline, global_cancel)
    }

    #[tokio::test]
    async fn test_pipeline_handle_start_stop() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        assert!(!handle.is_running());
        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());
        handle.stop_proposer().await.unwrap();
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_pipeline_handle_double_start_errors() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        handle.start_proposer().await.unwrap();
        let result = handle.start_proposer().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already running"));
        handle.stop_proposer().await.unwrap();
    }

    #[tokio::test]
    async fn test_pipeline_handle_stop_when_not_running() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        let result = handle.stop_proposer().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));
    }

    #[tokio::test]
    async fn test_pipeline_handle_restart() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        handle.start_proposer().await.unwrap();
        handle.stop_proposer().await.unwrap();
        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());
        handle.stop_proposer().await.unwrap();
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_pipeline_handle_global_cancel_stops_pipeline() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel.clone());

        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());

        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_pipeline_handle_debug() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        let debug = format!("{handle:?}");
        assert!(debug.contains("PipelineHandle"));
        assert!(debug.contains("running"));
    }
}
