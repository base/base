//! Proving pipeline for the proposer.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    Metrics,
    driver::{DriverConfig, RecoveredState},
    proof_collector::ProofCollector,
    proof_dispatcher::{ProofDispatcher, ProofDispatcherState},
    proof_recovery::{ProofRecovery, ProofRecoveryCache},
};

/// The proving pipeline.
///
/// Runs concurrent dispatcher and collector tasks per [`Self::run`] session.
/// The collector chains ready proofs internally and restarts both tasks only
/// when it needs fresh onchain state; dispatcher retry exhaustion clears
/// dispatcher recovery state without interrupting collection.
pub struct ProvingPipeline<L1, L2, R>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
{
    config: DriverConfig,
    proof_dispatcher: ProofDispatcher<L1, L2, R>,
    proof_recovery: Arc<ProofRecovery<R>>,
    proof_collector: ProofCollector<L1, R>,
}

impl<L1, L2, R> std::fmt::Debug for ProvingPipeline<L1, L2, R>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProvingPipeline").field("config", &self.config).finish_non_exhaustive()
    }
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
        proof_collector: ProofCollector<L1, R>,
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
            let restart = tokio::select! {
                biased;
                () = cancel.cancelled() => false,
                () = self.dispatcher_loop(&cancel, Arc::clone(&dispatched_through)) => true,
                () = self.collector_loop(&cancel, Arc::clone(&dispatched_through)) => true,
            };

            if !restart {
                break;
            }

            info!("Restarting proving pipeline session");
        }

        info!("Proving pipeline stopped");
    }

    async fn dispatcher_loop(
        &self,
        cancel: &CancellationToken,
        dispatched_through: Arc<AtomicU64>,
    ) {
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

                    let drop_recovery_cache = self
                        .proof_dispatcher
                        .tick(
                            &mut state,
                            recovered,
                            safe_head,
                            self.config.block_interval,
                            self.config.max_retries,
                            cancel,
                        )
                        .await;

                    if let Some(cursor) = state.cursor {
                        dispatched_through.store(cursor.l2_block_number, Ordering::Relaxed);
                    }

                    if drop_recovery_cache {
                        cache = None;
                        dispatched_through.store(0, Ordering::Relaxed);
                    }
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
        ProofSubmitterConfig,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockRollupClient, test_anchor_root, test_sync_status,
        },
    };

    #[derive(Debug, Default)]
    struct RejectingProofRequester {
        prove_count: AtomicUsize,
    }

    #[async_trait]
    impl ProofRequesterProvider for RejectingProofRequester {
        async fn prove_block_range(
            &self,
            _request: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            self.prove_count.fetch_add(1, Ordering::SeqCst);
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

    #[tokio::test]
    async fn dispatcher_retry_exhaustion_keeps_dispatcher_loop_running() {
        let requester = Arc::new(RejectingProofRequester::default());
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
            max_retries: 1,
            block_interval: 100,
            intermediate_block_interval: 100,
            ..Default::default()
        };
        let proof_dispatcher = ProofDispatcher::new(
            Arc::clone(&proof_requester),
            Arc::clone(&l1),
            Arc::clone(&l2),
            Arc::clone(&rollup),
            ProofDispatcherConfig {
                proposer_address: config.proposer_address,
                allow_non_finalized: config.allow_non_finalized,
                intermediate_block_interval: config.intermediate_block_interval,
                tee_image_hash: config.tee_image_hash,
            },
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
            Arc::clone(&rollup),
            anchor_registry,
            factory,
        ));
        let proof_submitter = ProofSubmitter::new(
            output_proposer,
            Arc::clone(&rollup),
            Arc::clone(&l1),
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            verifier,
            ProofSubmitterConfig {
                proposer_address: config.proposer_address,
                game_type: config.game_type,
                block_interval: config.block_interval,
                intermediate_block_interval: config.intermediate_block_interval,
                tee_image_hash: config.tee_image_hash,
                tee_prover_registry_address: config.tee_prover_registry_address,
                output_fetch_concurrency: config.recovery_scan_concurrency,
            },
        );
        let proof_collector = ProofCollector::new(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup),
            proof_submitter,
            config.block_interval,
            config.submit_timeout,
        );
        let pipeline =
            ProvingPipeline::new(config, proof_dispatcher, proof_recovery, proof_collector);
        let cancel = CancellationToken::new();

        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                pipeline.dispatcher_loop(&cancel, Arc::new(AtomicU64::new(0)))
            )
            .await
            .is_err(),
            "dispatcher retry exhaustion should not end the dispatcher loop"
        );
        assert!(
            requester.prove_count.load(Ordering::SeqCst) > 1,
            "dispatcher should keep recovering and retrying after exhaustion"
        );
    }
}
