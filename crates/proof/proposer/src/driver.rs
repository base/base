//! Proposer driver types and lifecycle management.
//!
//! Contains configuration types ([`DriverConfig`], [`RecoveredState`]) shared
//! by the [`crate::ProvingPipeline`], and the [`PipelineHandle`] that wraps a
//! pipeline with start/stop/is-running semantics exposed through the
//! [`ProposerDriverControl`] trait for the admin JSON-RPC server.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_proof_rpc::RollupProvider;
use eyre::Result;
use tokio::{sync::Mutex as TokioMutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::pipeline::ProvingPipeline;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Driver configuration.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
    /// Maximum number of concurrent RPC calls during the recovery scan.
    pub recovery_scan_concurrency: usize,
    /// Optional maximum duration for a single inline submit (validation + L1
    /// transaction). `None` disables the outer pipeline timeout.
    pub submit_timeout: Option<Duration>,
    /// Number of L2 blocks between proposals (read from `AggregateVerifier` at startup).
    pub block_interval: u64,
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
    /// Game type ID for `AggregateVerifier` dispute games.
    pub game_type: u32,
    /// If true, use `safe_l2` (derived from L1 but L1 not yet finalized).
    /// If false (default), use `finalized_l2` (derived from finalized L1).
    pub allow_non_finalized: bool,
    /// Address of the proposer that submits proof transactions onchain.
    /// Included in the proof journal so the enclave signs over the correct `msg.sender`.
    pub proposer_address: Address,
    /// Keccak256 hash of the expected enclave PCR0 measurement.
    /// Passed to the prover in each proof request so multi-enclave provers
    /// can select the correct enclave.
    pub tee_image_hash: B256,
    /// Address of the `AnchorStateRegistry` contract on L1.
    /// Used as the "no parent" sentinel when creating the first game from anchor state.
    pub anchor_state_registry_address: Address,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(12),
            recovery_scan_concurrency: 8,
            submit_timeout: None,
            block_interval: 512,
            intermediate_block_interval: 512,
            game_type: 0,
            allow_non_finalized: false,
            proposer_address: Address::ZERO,
            tee_image_hash: B256::ZERO,
            anchor_state_registry_address: Address::ZERO,
        }
    }
}

/// Onchain state recovered by the pipeline.
///
/// This is either a game found in the `DisputeGameFactory` or the
/// anchor root from the `AnchorStateRegistry` when no games exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveredState {
    /// Proxy address of the parent game, or the `AnchorStateRegistry` address
    /// when creating the first game from anchor state (no parent game exists).
    pub parent_address: Address,
    /// Output root claimed by the game or anchor state.
    pub output_root: B256,
    /// L2 block number of the claim.
    pub l2_block_number: u64,
}

// ---------------------------------------------------------------------------
// Lifecycle management
// ---------------------------------------------------------------------------

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

/// Active session state: the cancellation token and spawned task for a running
/// pipeline.
struct Session {
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

/// Manages the lifecycle of a [`ProvingPipeline`], allowing it to be started
/// and stopped at runtime (e.g. via the admin RPC).
pub struct PipelineHandle<R>
where
    R: RollupProvider + 'static,
{
    pipeline: Arc<ProvingPipeline<R>>,
    session: TokioMutex<Session>,
    global_cancel: CancellationToken,
    running: Arc<AtomicBool>,
}

impl<R> std::fmt::Debug for PipelineHandle<R>
where
    R: RollupProvider + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineHandle")
            .field("running", &self.running.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<R> PipelineHandle<R>
where
    R: RollupProvider + 'static,
{
    /// Creates a new [`PipelineHandle`] wrapping the given proving pipeline.
    pub fn new(pipeline: ProvingPipeline<R>, global_cancel: CancellationToken) -> Self {
        let session = Session { cancel: global_cancel.child_token(), task: None };
        Self {
            pipeline: Arc::new(pipeline),
            session: TokioMutex::new(session),
            global_cancel,
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl<R> ProposerDriverControl for PipelineHandle<R>
where
    R: RollupProvider + 'static,
{
    async fn start_proposer(&self) -> Result<(), String> {
        let mut session = self.session.lock().await;

        if self.running.load(Ordering::Acquire) {
            return Err("proposer is already running".into());
        }

        // Drain any stale task from a self-terminated pipeline run so panics
        // are surfaced and the JoinHandle resources are properly reclaimed.
        if let Some(task) = session.task.take() {
            match task.await {
                Ok(()) => {}
                Err(e) => error!(error = %e, "previous pipeline run panicked"),
            }
        }

        self.running.store(true, Ordering::Release);

        let cancel = self.global_cancel.child_token();
        let pipeline = Arc::clone(&self.pipeline);

        let running = Arc::clone(&self.running);
        let run_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            pipeline.run(run_cancel).await;
            running.store(false, Ordering::Release);
        });

        session.cancel = cancel;
        session.task = Some(handle);

        info!("proving pipeline started");
        Ok(())
    }

    async fn stop_proposer(&self) -> Result<(), String> {
        let mut session = self.session.lock().await;

        if !self.running.load(Ordering::Acquire) {
            return Err("proposer is not running".into());
        }

        session.cancel.cancel();

        if let Some(task) = session.task.take() {
            match task.await {
                Ok(()) => {}
                Err(e) => error!(error = %e, "proving pipeline task panicked"),
            }
        }

        self.running.store(false, Ordering::Release);
        info!("proving pipeline stopped");
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use alloy_primitives::B256;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        ProofCollector, ProofDispatcher, ProofDispatcherConfig, ProofRecovery, ProofRecoveryConfig,
        ProofSubmitter,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockProofRequester, MockRollupClient, test_anchor_root,
            test_sync_status,
        },
    };

    fn test_pipeline_handle(global_cancel: CancellationToken) -> PipelineHandle<MockRollupClient> {
        let l1 = Arc::new(MockL1::new(1000));
        let l2 = Arc::new(MockL2);
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let anchor_registry: Arc<dyn base_proof_contracts::AnchorStateRegistryClient> =
            Arc::new(MockAnchorStateRegistry {
                anchor_root: test_anchor_root(0),
                anchor_game: Address::ZERO,
            });
        let factory: Arc<dyn base_proof_contracts::DisputeGameFactoryClient> =
            Arc::new(MockDisputeGameFactory::default());
        let proof_requester: Arc<dyn base_prover_service_client::ProofRequesterProvider> =
            Arc::new(MockProofRequester::default());
        let verifier: Arc<dyn base_proof_contracts::AggregateVerifierClient> =
            Arc::new(MockAggregateVerifier::default());
        let output_proposer: Arc<dyn crate::OutputProposer> =
            Arc::new(MockOutputProposer::default());
        let config = DriverConfig {
            poll_interval: Duration::from_secs(3600),
            submit_timeout: Some(std::time::Duration::from_secs(60)),
            recovery_scan_concurrency: 8,
            block_interval: 512,
            intermediate_block_interval: 512,
            ..Default::default()
        };

        let proof_dispatcher = ProofDispatcher::new(
            Arc::clone(&proof_requester),
            Arc::<MockL1>::clone(&l1),
            l2,
            Arc::<MockRollupClient>::clone(&rollup),
            ProofDispatcherConfig::from(&config),
        );
        let proof_submitter = ProofSubmitter::new(
            output_proposer,
            Arc::<MockRollupClient>::clone(&rollup),
            Arc::clone(&factory),
            verifier,
            &config,
        );
        let proof_recovery = Arc::new(ProofRecovery::new(
            ProofRecoveryConfig {
                block_interval: config.block_interval,
                intermediate_block_interval: config.intermediate_block_interval,
                game_type: config.game_type,
                allow_non_finalized: config.allow_non_finalized,
                anchor_state_registry_address: config.anchor_state_registry_address,
                scan_concurrency: config.recovery_scan_concurrency,
            },
            Arc::<MockRollupClient>::clone(&rollup),
            anchor_registry,
            factory,
        ));
        let proof_collector = ProofCollector::new(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup),
            proof_submitter,
            config.block_interval,
            config.submit_timeout,
        );
        let pipeline =
            ProvingPipeline::new(config, proof_dispatcher, proof_recovery, proof_collector);
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
