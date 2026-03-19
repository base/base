//! Lifecycle management for the proposer driver.

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

use super::core::Driver;

/// Trait for controlling the proposer driver at runtime.
///
/// This is the type-erased interface consumed by the admin JSON-RPC server.
/// [`DriverHandle`] is the concrete implementation.
#[async_trait]
pub trait ProposerDriverControl: Send + Sync {
    /// Start the proposer driver loop.
    async fn start_proposer(&self) -> Result<(), String>;
    /// Stop the proposer driver loop.
    async fn stop_proposer(&self) -> Result<(), String>;
    /// Returns whether the proposer driver is currently running.
    fn is_running(&self) -> bool;
}

/// Manages the lifecycle of a [`Driver`], allowing it to be started and
/// stopped at runtime (e.g. via the admin RPC).
///
/// Internally the driver is placed behind a [`TokioMutex`] so it can be moved
/// into a spawned task for the duration of a session.
pub struct DriverHandle<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    #[allow(clippy::type_complexity)]
    driver: Arc<TokioMutex<Driver<L1, L2, R, ASR, F>>>,
    session_cancel: TokioMutex<CancellationToken>,
    global_cancel: CancellationToken,
    task: TokioMutex<Option<JoinHandle<Result<()>>>>,
    running: Arc<AtomicBool>,
}

impl<L1, L2, R, ASR, F> std::fmt::Debug for DriverHandle<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverHandle")
            .field("running", &self.running.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<L1, L2, R, ASR, F> DriverHandle<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    /// Wraps a [`Driver`] in a lifecycle-managed handle.
    ///
    /// The driver is **not** started automatically — call
    /// [`start_proposer`](ProposerDriverControl::start_proposer) to begin the
    /// polling loop.
    pub fn new(driver: Driver<L1, L2, R, ASR, F>, global_cancel: CancellationToken) -> Self {
        let session_cancel = global_cancel.child_token();
        Self {
            driver: Arc::new(TokioMutex::new(driver)),
            session_cancel: TokioMutex::new(session_cancel),
            global_cancel,
            task: TokioMutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl<L1, L2, R, ASR, F> ProposerDriverControl for DriverHandle<L1, L2, R, ASR, F>
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
            let mut driver = self.driver.lock().await;
            driver.set_cancel(cancel.clone());
        }
        *self.session_cancel.lock().await = cancel;

        let driver = Arc::clone(&self.driver);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);

        let handle = tokio::spawn(async move {
            let guard = driver.lock().await;
            let result = guard.run().await;
            running.store(false, Ordering::SeqCst);
            result
        });

        *self.task.lock().await = Some(handle);
        info!("Proposer driver started");
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

        info!("Proposer driver stopped");
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::B256;
    use base_proof_rpc::SyncStatus;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        driver::core::{Driver, DriverConfig},
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockRollupClient, test_anchor_root, test_prover, test_sync_status,
        },
    };

    fn test_driver(
        sync_status: SyncStatus,
        cancel: CancellationToken,
    ) -> Driver<MockL1, MockL2, MockRollupClient, MockAnchorStateRegistry, MockDisputeGameFactory>
    {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover = test_prover();
        let rollup = Arc::new(MockRollupClient { sync_status });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(0) });
        let factory = Arc::new(MockDisputeGameFactory { game_count: 1 });

        Driver::new(
            DriverConfig {
                poll_interval: Duration::from_secs(3600),
                block_interval: 10,
                ..Default::default()
            },
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier),
            Arc::new(MockOutputProposer),
            cancel,
        )
    }

    fn test_driver_handle(
        global_cancel: CancellationToken,
    ) -> DriverHandle<
        MockL1,
        MockL2,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    > {
        let driver = test_driver(test_sync_status(200, B256::ZERO), global_cancel.child_token());
        DriverHandle::new(driver, global_cancel)
    }

    #[tokio::test]
    async fn test_driver_handle_start_stop() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        assert!(!handle.is_running());
        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());
        handle.stop_proposer().await.unwrap();
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_driver_handle_double_start_errors() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        handle.start_proposer().await.unwrap();
        let result = handle.start_proposer().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already running"));
        handle.stop_proposer().await.unwrap();
    }

    #[tokio::test]
    async fn test_driver_handle_stop_when_not_running() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        let result = handle.stop_proposer().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));
    }

    #[tokio::test]
    async fn test_driver_handle_restart() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        handle.start_proposer().await.unwrap();
        handle.stop_proposer().await.unwrap();
        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());
        handle.stop_proposer().await.unwrap();
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_driver_handle_global_cancel_stops_driver() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel.clone());

        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());

        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_driver_handle_debug() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        let debug = format!("{handle:?}");
        assert!(debug.contains("DriverHandle"));
        assert!(debug.contains("running"));
    }
}
