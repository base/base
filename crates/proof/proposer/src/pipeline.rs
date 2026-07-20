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
use tracing::warn;

use crate::{
    Metrics,
    driver::{DriverConfig, RecoveredState},
    proof_collector::ProofCollector,
    proof_dispatcher::ProofDispatcher,
};

/// The proving pipeline.
///
/// Runs concurrent dispatcher and collector tasks for one recovery session.
pub struct ProvingPipeline<R>
where
    R: RollupProvider + 'static,
{
    config: DriverConfig,
    proof_dispatcher: ProofDispatcher,
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
        proof_collector: ProofCollector<R>,
    ) -> Self {
        Self { config, proof_dispatcher, proof_collector }
    }

    /// Runs the dispatcher and collector loops for one recovery session.
    ///
    /// Returns `true` when the driver should start a fresh recovery session.
    pub async fn run(
        &self,
        cancel: CancellationToken,
        recovery_rx: watch::Receiver<Option<(RecoveredState, u64)>>,
    ) -> bool {
        let dispatched_through = Arc::new(AtomicU64::new(0));

        // dispatcher_loop intentionally does not return; this branch keeps it
        // polled while collector_loop remains the session restart signal.
        // Dropping either loop mid-tick is safe: the next recovery session
        // rediscovers any already-broadcast L1 transaction from onchain state.
        let session = async {
            tokio::select! {
                biased;
                () = cancel.cancelled() => false,
                () = self.dispatcher_loop(
                    recovery_rx.clone(),
                    Arc::clone(&dispatched_through),
                ) => true,
                () = self.collector_loop(
                    &cancel,
                    recovery_rx,
                    Arc::clone(&dispatched_through),
                ) => true,
            }
        };
        AssertUnwindSafe(session).catch_unwind().await.unwrap_or_else(|panic| {
            let panic = panic
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic payload");
            warn!(panic = %panic, "Pipeline loop panicked, restarting session");
            true
        })
    }

    async fn dispatcher_loop(
        &self,
        mut recovery_rx: watch::Receiver<Option<(RecoveredState, u64)>>,
        dispatched_through: Arc<AtomicU64>,
    ) {
        let mut cursor_source: Option<RecoveredState> = None;
        let mut cursor: Option<RecoveredState> = None;

        while recovery_rx.changed().await.is_ok() {
            {
                let _tick_timer = base_metrics::timed!(Metrics::tick_duration_seconds());
                let plan = *recovery_rx.borrow_and_update();

                if let Some((recovered, finalized_head)) = plan {
                    // Dispatch failures retry from the in-memory cursor. The recovery publisher
                    // resets it only when onchain state changes the recovered state.
                    if cursor_source != Some(recovered) || cursor.is_none() {
                        cursor_source = Some(recovered);
                        cursor = Some(recovered);
                    }

                    let current = cursor
                        .as_mut()
                        .expect("dispatcher cursor initialized from recovered state");
                    self.proof_dispatcher.tick(current, finalized_head).await;

                    dispatched_through.store(current.l2_block_number, Ordering::Relaxed);
                }
            }

            // A pending watch update must not bypass the configured retry delay.
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    async fn collector_loop(
        &self,
        cancel: &CancellationToken,
        mut recovery_rx: watch::Receiver<Option<(RecoveredState, u64)>>,
        dispatched_through: Arc<AtomicU64>,
    ) {
        let mut cursor_source: Option<RecoveredState> = None;
        let mut cursor: Option<RecoveredState> = None;

        while recovery_rx.changed().await.is_ok() {
            let restart = {
                let _tick_timer = base_metrics::timed!(Metrics::collector_tick_duration_seconds());
                let plan = *recovery_rx.borrow_and_update();

                if let Some((recovered, finalized_head)) = plan {
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

            // A pending watch update must not bypass the configured retry delay.
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
    use base_proof_contracts::DisputeGameFactoryClient;
    use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
    use base_prover_service_protocol::{
        DeleteProofRequest, GetProofRequest, GetProofResponse, ListProofsRequest,
        ListProofsResponse, ProveBlockRangeRequest, ProveBlockRangeResponse,
    };
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        OutputProposer, ProofDispatcherConfig, ProofSubmitter,
        test_utils::{
            MockAggregateVerifier, MockDisputeGameFactory, MockL1, MockL2, MockOutputProposer,
            MockRollupClient, test_sync_status,
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
            unimplemented!("pipeline tests do not collect proofs")
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
        let l2 = Arc::new(MockL2);
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let factory: Arc<dyn DisputeGameFactoryClient> =
            Arc::new(MockDisputeGameFactory::default());
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
        let proof_submitter = ProofSubmitter::new(
            output_proposer,
            Arc::<MockRollupClient>::clone(&rollup),
            factory,
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
        ProvingPipeline::new(config, proof_dispatcher, proof_collector)
    }

    #[tokio::test]
    async fn dispatcher_panic_requests_recovery_session_restart() {
        let requester =
            Arc::new(RejectingProofRequester { panic_on_first_prove: true, ..Default::default() });
        let pipeline = test_pipeline(Arc::clone(&requester));
        let (recovery_tx, recovery_rx) = watch::channel(None);
        recovery_tx
            .send(Some((
                RecoveredState {
                    parent_address: Address::ZERO,
                    output_root: B256::ZERO,
                    l2_block_number: 0,
                },
                200,
            )))
            .expect("pipeline should retain the recovery receiver");

        let restart = tokio::time::timeout(
            Duration::from_millis(100),
            pipeline.run(CancellationToken::new(), recovery_rx),
        )
        .await
        .expect("pipeline should return after a dispatcher panic");

        assert!(restart, "dispatcher panic should restart the recovery session");
        assert_eq!(requester.prove_count.load(Ordering::SeqCst), 1);
    }
}
