//! Proving pipeline for the proposer.

use std::sync::Arc;

use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    Metrics,
    driver::DriverConfig,
    proof_collector::{ProofCollectorOrchestrator, ProofCollectorState},
    proof_dispatcher::{ProofDispatcher, ProofDispatcherState},
    proof_recovery::{ProofRecovery, ProofRecoveryCache},
};

/// The proving pipeline.
///
/// Runs concurrent dispatcher and collector tasks per [`Self::run`] session.
/// Submit failures restart both tasks from onchain state; cancellation stops
/// them cleanly.
#[derive(Debug)]
pub struct ProvingPipeline<L1, L2, R>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
{
    config: DriverConfig,
    proof_dispatcher: ProofDispatcher<L1, L2, R>,
    proof_recovery: Arc<ProofRecovery<R>>,
    proof_collector_orchestrator: ProofCollectorOrchestrator<L1, L2, R>,
}

impl<L1, L2, R> ProvingPipeline<L1, L2, R>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
{
    /// Creates a new proving pipeline.
    pub const fn new(
        config: DriverConfig,
        proof_dispatcher: ProofDispatcher<L1, L2, R>,
        proof_recovery: Arc<ProofRecovery<R>>,
        proof_collector_orchestrator: ProofCollectorOrchestrator<L1, L2, R>,
    ) -> Self {
        Self { config, proof_dispatcher, proof_recovery, proof_collector_orchestrator }
    }

    /// Runs the proving pipeline until cancelled.
    ///
    /// Each session starts a dispatcher task and a collector task. The
    /// dispatcher can run ahead up to the safe head, while the collector
    /// submits proofs in order. Submit failures restart both tasks from a
    /// fresh recovery walk.
    pub async fn run(&self, cancel: CancellationToken) {
        info!(
            block_interval = self.config.block_interval,
            poll_interval_secs = self.config.poll_interval.as_secs(),
            submit_timeout_secs = ?self.config.submit_timeout.map(|timeout| timeout.as_secs()),
            "Starting proving pipeline"
        );

        loop {
            let restart = tokio::select! {
                biased;
                () = cancel.cancelled() => false,
                () = self.dispatcher_loop(&cancel) => true,
                () = self.collector_loop(&cancel) => true,
            };

            if !restart {
                break;
            }

            info!("Restarting proving pipeline session");
        }

        info!("Proving pipeline stopped");
    }

    async fn dispatcher_loop(&self, cancel: &CancellationToken) {
        let mut cache: Option<ProofRecoveryCache> = None;
        let mut state = ProofDispatcherState::new();

        loop {
            {
                let _tick_timer = base_metrics::timed!(Metrics::tick_duration_seconds());

                if let Some((recovered, safe_head)) =
                    self.proof_recovery.try_recover_and_plan(&mut cache).await
                {
                    Metrics::safe_head().set(safe_head as f64);
                    Metrics::last_proposed_block().set(recovered.l2_block_number as f64);

                    if self
                        .proof_dispatcher
                        .tick(
                            &mut state,
                            recovered,
                            safe_head,
                            self.config.block_interval,
                            self.config.max_retries,
                            cancel,
                        )
                        .await
                    {
                        cache = None;
                    }
                }
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    async fn collector_loop(&self, cancel: &CancellationToken) {
        let mut cache: Option<ProofRecoveryCache> = None;
        let mut state = ProofCollectorState::new();

        loop {
            let restart = {
                let _tick_timer = base_metrics::timed!(Metrics::collector_tick_duration_seconds());

                if let Some((recovered, safe_head)) =
                    self.proof_recovery.try_recover_and_plan(&mut cache).await
                {
                    self.proof_collector_orchestrator
                        .tick(&mut state, &mut cache, recovered, safe_head, cancel)
                        .await
                        .is_break()
                } else {
                    false
                }
            };

            if restart {
                break;
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }
}
