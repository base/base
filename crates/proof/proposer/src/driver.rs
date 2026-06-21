//! Proposer driver types and lifecycle management.
//!
//! Contains configuration types ([`DriverConfig`], [`RecoveredState`]) shared
//! by the [`crate::ProvingPipeline`], and the [`PipelineHandle`] that wraps a
//! pipeline with start/stop/is-running semantics for the admin JSON-RPC server.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::{AnchorStateRegistryClient, DisputeGameFactoryClient};
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use eyre::Result;
use tokio::{sync::Mutex as TokioMutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::pipeline::ProvingPipeline;

/// Driver configuration.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
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

/// Manages the lifecycle of a [`ProvingPipeline`], allowing it to be started
/// and stopped at runtime (e.g. via the admin RPC).
#[derive(Debug)]
pub struct PipelineHandle<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    pipeline: ProvingPipeline<L1, L2, R, ASR, F>,
    session: TokioMutex<Option<(CancellationToken, JoinHandle<Result<()>>)>>,
    global_cancel: CancellationToken,
    running: Arc<AtomicBool>,
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
        Self {
            pipeline,
            session: TokioMutex::new(None),
            global_cancel,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the proving pipeline.
    pub async fn start_proposer(&self) -> std::result::Result<(), &'static str> {
        let mut session = self.session.lock().await;

        if self.running.load(Ordering::Acquire)
            || session.as_ref().is_some_and(|(_, task)| !task.is_finished())
        {
            return Err("proposer is already running");
        }

        // Drain any stale task from a self-terminated pipeline run so panics
        // are surfaced and the JoinHandle resources are properly reclaimed.
        if let Some((_, task)) = session.take() {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!(error = %e, "previous pipeline run exited with error"),
                Err(e) => error!(error = %e, "previous pipeline run panicked"),
            }
        }

        let cancel = self.global_cancel.child_token();
        let mut pipeline = self.pipeline.clone();
        pipeline.set_cancel(cancel.clone());

        self.running.store(true, Ordering::Release);
        let running = Arc::clone(&self.running);
        let handle = tokio::spawn(async move {
            let result = pipeline.run().await;
            running.store(false, Ordering::Release);
            result
        });

        *session = Some((cancel, handle));

        info!("proving pipeline started");
        Ok(())
    }

    /// Stop the proving pipeline.
    pub async fn stop_proposer(&self) -> std::result::Result<(), &'static str> {
        let mut session = self.session.lock().await;

        if !self.running.load(Ordering::Acquire)
            && session.as_ref().is_none_or(|(_, task)| task.is_finished())
        {
            return Err("proposer is not running");
        }

        if let Some((cancel, task)) = session.take() {
            cancel.cancel();
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!(error = %e, "proving pipeline exited with error"),
                Err(e) => error!(error = %e, "proving pipeline task panicked"),
            }
        }

        self.running.store(false, Ordering::Release);
        info!("proving pipeline stopped");
        Ok(())
    }

    /// Returns whether the proving pipeline is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_utils::test_pipeline_handle;

    #[tokio::test]
    async fn test_pipeline_handle_double_start_errors() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        handle.start_proposer().await.unwrap();
        assert!(handle.start_proposer().await.unwrap_err().contains("already running"));
        handle.stop_proposer().await.unwrap();
    }

    #[tokio::test]
    async fn test_pipeline_handle_stop_when_not_running() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        assert!(handle.stop_proposer().await.unwrap_err().contains("not running"));
    }

    #[tokio::test]
    async fn test_pipeline_handle_restart() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        assert!(!handle.is_running());
        handle.start_proposer().await.unwrap();
        handle.stop_proposer().await.unwrap();
        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());
        handle.stop_proposer().await.unwrap();
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_pipeline_handle_stop_recovers_after_panic() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel);

        {
            let mut session = handle.session.lock().await;
            let task: tokio::task::JoinHandle<Result<()>> = tokio::spawn(async {
                panic!("pipeline task panic");
            });
            handle.running.store(true, Ordering::Release);
            *session = Some((CancellationToken::new(), task));
        }

        loop {
            let session = handle.session.lock().await;
            if session.as_ref().is_some_and(|(_, task)| task.is_finished()) {
                break;
            }
            drop(session);
            tokio::task::yield_now().await;
        }

        handle.stop_proposer().await.unwrap();
        assert!(!handle.is_running());
        handle.start_proposer().await.unwrap();
        handle.stop_proposer().await.unwrap();
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
    async fn test_pipeline_handle_stop_after_self_terminated_errors() {
        let cancel = CancellationToken::new();
        let handle = test_pipeline_handle(cancel.clone());

        handle.start_proposer().await.unwrap();
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let task_finished = {
                    let session = handle.session.lock().await;
                    session.as_ref().is_some_and(|(_, task)| task.is_finished())
                };
                if !handle.is_running() && task_finished {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(handle.stop_proposer().await.unwrap_err().contains("not running"));
    }
}
