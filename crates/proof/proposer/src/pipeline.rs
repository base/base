//! Proving pipeline for the proposer.

use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base_proof_rpc::RollupProvider;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    Metrics,
    driver::{DriverConfig, RecoveredState},
    proof_collector::ProofCollector,
    proof_dispatcher::ProofDispatcher,
    proof_recovery::{ProofRecovery, ProofRecoveryCache},
};

/// The proving pipeline.
///
/// Runs concurrent dispatcher and collector tasks per [`Self::run`] session.
/// The collector chains ready proofs internally and restarts both tasks only
/// when it needs fresh onchain state.
pub struct ProvingPipeline<R>
where
    R: RollupProvider + 'static,
{
    config: DriverConfig,
    proof_dispatcher: ProofDispatcher,
    proof_recovery: Arc<ProofRecovery>,
    proof_collector: ProofCollector<R>,
}

impl<R> std::fmt::Debug for ProvingPipeline<R>
where
    R: RollupProvider + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProvingPipeline").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<R> ProvingPipeline<R>
where
    R: RollupProvider + 'static,
{
    /// Creates a new proving pipeline.
    pub const fn new(
        config: DriverConfig,
        proof_dispatcher: ProofDispatcher,
        proof_recovery: Arc<ProofRecovery>,
        proof_collector: ProofCollector<R>,
    ) -> Self {
        Self { config, proof_dispatcher, proof_recovery, proof_collector }
    }

    /// Runs the proving pipeline until cancelled.
    ///
    /// Each session starts a dispatcher task and a collector task. The
    /// dispatcher can run ahead up to the safe head, while the collector
    /// submits ready proofs in order from an internal cursor. Outcomes that
    /// cannot safely advance that cursor restart both tasks from a fresh
    /// recovery walk.
    pub async fn run(&self, cancel: CancellationToken) {
        info!(
            block_interval = self.config.block_interval,
            poll_interval_secs = self.config.poll_interval.as_secs(),
            submit_timeout_secs = ?self.config.submit_timeout.map(|timeout| timeout.as_secs()),
            "Starting proving pipeline"
        );

        loop {
            let dispatched_through = Arc::new(AtomicU64::new(0));

            // dispatcher_loop intentionally does not return; this branch keeps it
            // polled while collector_loop remains the session restart signal.
            // Dropping either loop mid-tick is safe: the next recovery walk
            // rediscovers any already-broadcast L1 transaction from onchain state.
            let session = async {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => false,
                    () = self.dispatcher_loop(Arc::clone(&dispatched_through)) => true,
                    () = self.collector_loop(&cancel, Arc::clone(&dispatched_through)) => true,
                }
            };
            // Unwind safety: this assertion is scoped to one pipeline session. Session progress
            // lives in the loop-local caches/cursors above and is discarded on panic; `self` only
            // carries shared clients and static config that are reused after a fresh recovery walk.
            let restart = AssertUnwindSafe(session).catch_unwind().await.unwrap_or_else(|panic| {
                let panic = panic
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("unknown panic payload");
                warn!(panic = %panic, "Pipeline loop panicked, restarting session");
                true
            });

            if !restart {
                break;
            }

            info!("Restarting proving pipeline session");
        }

        info!("Proving pipeline stopped");
    }

    async fn dispatcher_loop(&self, dispatched_through: Arc<AtomicU64>) {
        let mut cache: Option<ProofRecoveryCache> = None;
        let mut cursor_source: Option<RecoveredState> = None;
        let mut cursor: Option<RecoveredState> = None;

        loop {
            {
                let _tick_timer = base_metrics::timed!(Metrics::tick_duration_seconds());

                if let Some((recovered, safe_head)) =
                    self.proof_recovery.try_recover_and_plan(&mut cache).await
                {
                    Metrics::safe_head().set(safe_head as f64);
                    Metrics::last_proposed_block().set(recovered.l2_block_number as f64);

                    // Dispatch failures retry from the in-memory cursor. A fresh recovery walk is
                    // only needed when onchain state changes; try_recover_and_plan returns that as
                    // a different recovered state, which resets the cursor here.
                    if cursor_source != Some(recovered) || cursor.is_none() {
                        cursor_source = Some(recovered);
                        cursor = Some(recovered);
                    }

                    let current = cursor
                        .as_mut()
                        .expect("dispatcher cursor initialized from recovered state");
                    self.proof_dispatcher.tick(current, safe_head).await;

                    dispatched_through.store(current.l2_block_number, Ordering::Relaxed);
                }
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    async fn collector_loop(&self, cancel: &CancellationToken, dispatched_through: Arc<AtomicU64>) {
        let mut cache: Option<ProofRecoveryCache> = None;
        let mut cursor_source: Option<RecoveredState> = None;
        let mut cursor: Option<RecoveredState> = None;

        loop {
            let restart = {
                let _tick_timer = base_metrics::timed!(Metrics::collector_tick_duration_seconds());

                if let Some((recovered, safe_head)) =
                    self.proof_recovery.try_recover_and_plan(&mut cache).await
                {
                    Metrics::safe_head().set(safe_head as f64);
                    Metrics::last_proposed_block().set(recovered.l2_block_number as f64);

                    if cursor_source != Some(recovered) || cursor.is_none() {
                        cursor_source = Some(recovered);
                        cursor = Some(recovered);
                    }

                    let current =
                        cursor.as_mut().expect("collector cursor initialized from recovered state");
                    self.proof_collector
                        .tick(
                            current,
                            safe_head,
                            dispatched_through.load(Ordering::Relaxed),
                            cancel,
                        )
                        .await
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

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use alloy_primitives::{Address, B256};
    use async_trait::async_trait;
    use base_proof_contracts::{AnchorStateRegistryClient, DisputeGameFactoryClient};
    use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
    use base_prover_service_protocol::{
        DeleteProofRequest, GetProofRequest, GetProofResponse, ListProofsRequest,
        ListProofsResponse, ProveBlockRangeRequest, ProveBlockRangeResponse,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        OutputProposer, ProofDispatcherConfig, ProofRecoveryConfig, ProofSubmitter,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockRollupClient, test_anchor_root, test_sync_status,
        },
    };

    #[derive(Debug, Default)]
    struct RejectingProofRequester {
        prove_count: AtomicUsize,
        panic_on_first_prove: bool,
    }

    #[async_trait]
    impl ProofRequesterProvider for RejectingProofRequester {
        async fn prove_block_range(
            &self,
            _request: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            let prove_count = self.prove_count.fetch_add(1, Ordering::SeqCst);
            if self.panic_on_first_prove && prove_count == 0 {
                panic!("simulated dispatch panic");
            }
            Err(ProverServiceClientError::Timeout("simulated dispatch failure".into()))
        }

        async fn get_proof(
            &self,
            _request: GetProofRequest,
        ) -> Result<GetProofResponse, ProverServiceClientError> {
            Err(ProverServiceClientError::Timeout("simulated poll failure".into()))
        }

        async fn delete_proof_request(
            &self,
            _request: DeleteProofRequest,
        ) -> Result<(), ProverServiceClientError> {
            unimplemented!("pipeline tests do not delete proofs")
        }

        async fn list_proofs(
            &self,
            _request: ListProofsRequest,
        ) -> Result<ListProofsResponse, ProverServiceClientError> {
            unimplemented!("pipeline tests do not list proofs")
        }
    }

    fn test_pipeline(requester: Arc<RejectingProofRequester>) -> ProvingPipeline<MockRollupClient> {
        let proof_requester: Arc<dyn ProofRequesterProvider> =
            Arc::<RejectingProofRequester>::clone(&requester);
        let l1 = Arc::new(MockL1::new(1000));
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let anchor_registry: Arc<dyn AnchorStateRegistryClient> =
            Arc::new(MockAnchorStateRegistry {
                anchor_root: test_anchor_root(0),
                anchor_game: Address::ZERO,
            });
        let factory: Arc<dyn DisputeGameFactoryClient> =
            Arc::new(MockDisputeGameFactory::with_games(vec![]));
        let verifier = Arc::new(MockAggregateVerifier::default());
        let output_proposer: Arc<dyn OutputProposer> = Arc::new(MockOutputProposer::default());
        let config = DriverConfig {
            poll_interval: Duration::from_millis(10),
            block_interval: 100,
            intermediate_block_interval: 100,
            ..Default::default()
        };
        let proof_dispatcher = ProofDispatcher::new(
            Arc::clone(&proof_requester),
            Arc::<MockL1>::clone(&l1),
            l2,
            Arc::<MockRollupClient>::clone(&rollup),
            ProofDispatcherConfig::from(&config),
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
        let proof_submitter = ProofSubmitter::new(
            output_proposer,
            Arc::<MockRollupClient>::clone(&rollup),
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            verifier,
            &config,
        );
        let proof_collector = ProofCollector::new(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup),
            proof_submitter,
            config.block_interval,
            config.submit_timeout,
        );
        ProvingPipeline::new(config, proof_dispatcher, proof_recovery, proof_collector)
    }

    #[tokio::test]
    async fn dispatcher_failure_keeps_dispatcher_loop_running() {
        let requester = Arc::new(RejectingProofRequester::default());
        let pipeline = test_pipeline(Arc::clone(&requester));

        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                pipeline.dispatcher_loop(Arc::new(AtomicU64::new(0)))
            )
            .await
            .is_err(),
            "dispatcher failures should not end the dispatcher loop"
        );
        assert!(
            requester.prove_count.load(Ordering::SeqCst) > 1,
            "dispatcher should keep retrying after failures"
        );
    }

    #[tokio::test]
    async fn dispatcher_panic_restarts_pipeline_session() {
        let requester =
            Arc::new(RejectingProofRequester { panic_on_first_prove: true, ..Default::default() });
        let pipeline = test_pipeline(Arc::clone(&requester));
        let cancel = CancellationToken::new();
        let run = pipeline.run(cancel.clone());
        tokio::pin!(run);

        tokio::select! {
            () = &mut run => panic!("pipeline should restart instead of exiting"),
            result = tokio::time::timeout(Duration::from_millis(200), async {
                while requester.prove_count.load(Ordering::SeqCst) < 2 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }) => result.expect("pipeline should retry after dispatcher panic"),
        }

        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(100), &mut run)
            .await
            .expect("pipeline should stop after cancellation");
    }
}
