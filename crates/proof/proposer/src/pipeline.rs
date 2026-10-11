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
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    Metrics,
    driver::{DriverConfig, RecoveredState},
    proof_collector::ProofCollector,
    proof_dispatcher::ProofDispatcher,
    proof_recovery::{ProofRecovery, ProofRecoveryCache},
    tee_proof_recovery::TeeProofRecovery,
};

/// The proving pipeline.
///
/// Runs concurrent dispatcher, collector, and optional signer recovery futures per session.
/// The collector chains ready proofs internally. Cleanup or onchain recovery restarts
/// the dispatcher and collector so deleted proofs are redispatched.
pub struct ProvingPipeline<R>
where
    R: RollupProvider + 'static,
{
    config: DriverConfig,
    proof_dispatcher: ProofDispatcher,
    proof_recovery: Arc<ProofRecovery>,
    proof_collector: ProofCollector<R>,
    tee_proof_recovery: Option<TeeProofRecovery>,
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
        Self { config, proof_dispatcher, proof_recovery, proof_collector, tee_proof_recovery: None }
    }

    /// Enables proactive cleanup of proofs from unregistered TEE signers.
    #[must_use]
    pub fn with_tee_proof_recovery(mut self, recovery: TeeProofRecovery) -> Self {
        self.tee_proof_recovery = Some(recovery);
        self
    }

    /// Runs the proving pipeline until cancelled.
    ///
    /// Each session polls dispatcher, collector, and optional signer recovery loops concurrently.
    /// The dispatcher can run ahead up to the finalized head, while the collector
    /// submits ready proofs in order from an internal cursor. Cleanup outcomes that
    /// may have deleted proofs restart the session from a fresh recovery walk.
    pub async fn run(&self, cancel: CancellationToken) {
        info!(
            poll_interval_secs = self.config.poll_interval.as_secs(),
            submit_timeout_secs = ?self.config.submit_timeout.map(|timeout| timeout.as_secs()),
            "Starting proving pipeline"
        );

        // Cleanup restarts must not starve later scan pages. Only a change in the complete
        // recovered onchain state invalidates this read-only pagination state.
        let mut signer_cursor_source = None;
        let mut signer_scan_cursor = None;
        loop {
            let dispatched_through = Arc::new(AtomicU64::new(0));
            let (backlog_tx, backlog_rx) = watch::channel(None);

            // Cleanup must reset the dispatch cursor too, otherwise deleted ranges behind
            // that cursor are never requeued. No loop is spawned: cancellation or restart
            // drops all session futures, including any bounded sweep in flight. A fresh
            // recovery walk rediscovers any already-broadcast L1 transaction.
            let session = async {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => false,
                    () = self.dispatcher_loop(Arc::clone(&dispatched_through), backlog_tx) => true,
                    () = self.collector_loop(&cancel, Arc::clone(&dispatched_through)) => true,
                    () = self.tee_recovery_loop(
                        backlog_rx,
                        &mut signer_cursor_source,
                        &mut signer_scan_cursor,
                    ), if self.tee_proof_recovery.is_some() => true,
                }
            };
            // Unwind safety: dispatch/collection caches and cursors are discarded on panic.
            // Only read-only scan pagination survives; it is checked against fresh recovered
            // state before reuse. Shared clients and static config are reused after recovery.
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

    async fn dispatcher_loop(
        &self,
        dispatched_through: Arc<AtomicU64>,
        backlog: watch::Sender<Option<(RecoveredState, u64)>>,
    ) {
        let mut cache: Option<ProofRecoveryCache> = None;
        let mut cursor_source: Option<RecoveredState> = None;
        let mut cursor: Option<RecoveredState> = None;

        loop {
            {
                let _tick_timer = base_metrics::timed!(Metrics::tick_duration_seconds());

                if let Some((recovered, finalized_head)) =
                    self.proof_recovery.try_recover_and_plan(&mut cache).await
                {
                    Metrics::safe_head().set(finalized_head as f64);
                    Metrics::finalized_head().set(finalized_head as f64);
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
                    self.proof_dispatcher.tick(current, finalized_head).await;

                    dispatched_through.store(current.l2_block_number, Ordering::Relaxed);
                    // Publish both sweep bounds together; a recovered-state change must never
                    // be paired with the previous state's dispatch cursor.
                    backlog.send_replace(Some((recovered, current.l2_block_number)));
                }
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Sweeps coherent dispatched-backlog snapshots independently of dispatch and collection.
    ///
    /// Sleeps one poll interval before every sweep and returns when cleanup requires a
    /// pipeline restart, or immediately if signer recovery is disabled. Callers must drop
    /// this future on cancellation. Pagination may survive restarts while the full recovered
    /// state is unchanged; a root, block, or parent change resets it before the next sweep.
    pub async fn tee_recovery_loop(
        &self,
        backlog: watch::Receiver<Option<(RecoveredState, u64)>>,
        cursor_source: &mut Option<RecoveredState>,
        scan_cursor: &mut Option<u64>,
    ) {
        let Some(recovery) = &self.tee_proof_recovery else { return };
        loop {
            // Sleep before every sweep, including after cleanup restarts. Persistent unknown
            // delete outcomes must back off without gating normal dispatch or collection.
            tokio::time::sleep(self.config.poll_interval).await;
            let bounds = *backlog.borrow();
            let Some((recovered, dispatched_through)) = bounds else { continue };
            if *cursor_source != Some(recovered) {
                *cursor_source = Some(recovered);
                *scan_cursor = None;
            }
            if recovery.tick(recovered.l2_block_number, dispatched_through, scan_cursor).await {
                return;
            }
        }
    }

    async fn collector_loop(&self, cancel: &CancellationToken, dispatched_through: Arc<AtomicU64>) {
        let mut cache: Option<ProofRecoveryCache> = None;
        let mut cursor_source: Option<RecoveredState> = None;
        let mut cursor: Option<RecoveredState> = None;

        loop {
            let restart = {
                let _tick_timer = base_metrics::timed!(Metrics::collector_tick_duration_seconds());

                if let Some((recovered, finalized_head)) =
                    self.proof_recovery.try_recover_and_plan(&mut cache).await
                {
                    Metrics::safe_head().set(finalized_head as f64);
                    Metrics::finalized_head().set(finalized_head as f64);
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
                            finalized_head,
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
        DeleteProofRequest, DeleteProofsByTeeSignerRequest, GetProofRequest, GetProofResponse,
        ListProofsRequest, ListProofsResponse, ProveBlockRangeRequest, ProveBlockRangeResponse,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        OutputProposer, ProofRecoveryConfig, ProofSubmitter,
        tee_proof_recovery::MockTeeSignerRegistry,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockProofRequester, MockRollupClient, test_anchor_root,
            test_fixed_interval_resolver, test_sync_status,
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

        async fn delete_proofs_by_tee_signer(
            &self,
            _request: DeleteProofsByTeeSignerRequest,
        ) -> Result<u64, ProverServiceClientError> {
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
        let l2 = Arc::new(MockL2);
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
            Arc::new(MockDisputeGameFactory::default());
        let verifier = Arc::new(MockAggregateVerifier::default());
        let output_proposer: Arc<dyn OutputProposer> = Arc::new(MockOutputProposer::default());
        let config =
            DriverConfig { poll_interval: Duration::from_millis(10), ..Default::default() };
        let intervals = test_fixed_interval_resolver(100, 100);
        let proof_dispatcher = ProofDispatcher::new(
            Arc::clone(&proof_requester),
            Arc::<MockL1>::clone(&l1),
            l2,
            Arc::<MockRollupClient>::clone(&rollup),
            Arc::clone(&intervals),
            config.proposer_address,
        );
        let proof_recovery = Arc::new(ProofRecovery::new(
            ProofRecoveryConfig {
                game_type: config.game_type,
                anchor_state_registry_address: config.anchor_state_registry_address,
                scan_concurrency: config.recovery_scan_concurrency,
            },
            Arc::<MockRollupClient>::clone(&rollup),
            anchor_registry,
            factory,
            Arc::clone(&intervals),
        ));
        let proof_submitter = ProofSubmitter::new(
            output_proposer,
            Arc::<MockRollupClient>::clone(&rollup),
            Arc::new(MockDisputeGameFactory::default()),
            verifier,
            Arc::clone(&intervals),
            &config,
        );
        let proof_collector = ProofCollector::new(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup),
            proof_submitter,
            intervals,
            config.submit_timeout,
        );
        ProvingPipeline::new(config, proof_dispatcher, proof_recovery, proof_collector)
    }

    #[tokio::test]
    async fn disabled_signer_recovery_returns_without_touching_pagination() {
        let pipeline = test_pipeline(Arc::new(RejectingProofRequester::default()));
        let (_tx, rx) = watch::channel(None);
        let mut source = None;
        let mut cursor = Some(100);
        pipeline.tee_recovery_loop(rx, &mut source, &mut cursor).await;
        assert_eq!(source, None);
        assert_eq!(cursor, Some(100));
    }

    #[tokio::test(start_paused = true)]
    async fn signer_pagination_resets_on_root_or_parent_change_but_not_cleanup_restart() {
        let previous = RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::ZERO,
            l2_block_number: 0,
        };
        for recovered in [
            previous,
            RecoveredState { output_root: B256::repeat_byte(1), ..previous },
            RecoveredState { parent_address: Address::repeat_byte(1), ..previous },
        ] {
            let rollup = Arc::new(MockRollupClient {
                sync_status: test_sync_status(6500, B256::ZERO),
                output_roots: HashMap::new(),
                max_safe_block: None,
            });
            let mut registry = MockTeeSignerRegistry::new();
            registry.expect_registered_signers().never();
            let pipeline = test_pipeline(Arc::new(RejectingProofRequester::default()))
                .with_tee_proof_recovery(TeeProofRecovery::new(
                    Arc::new(MockProofRequester::default()),
                    rollup,
                    test_fixed_interval_resolver(100, 100),
                    Arc::new(registry),
                    Duration::from_secs(1),
                ));
            let (_tx, rx) = watch::channel(Some((recovered, 6500)));
            let mut source = Some(previous);
            let mut cursor = Some(100);
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(15),
                    pipeline.tee_recovery_loop(rx, &mut source, &mut cursor),
                )
                .await
                .is_err()
            );
            assert_eq!(source, Some(recovered));
            assert_eq!(cursor, Some(if recovered == previous { 6500 } else { 6400 }));
        }
    }

    #[tokio::test]
    async fn dispatcher_failure_keeps_dispatcher_loop_running() {
        let requester = Arc::new(RejectingProofRequester::default());
        let pipeline = test_pipeline(Arc::clone(&requester));

        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                pipeline.dispatcher_loop(Arc::new(AtomicU64::new(0)), watch::channel(None).0)
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
