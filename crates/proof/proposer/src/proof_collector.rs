//! Polls and submits prover-service proofs for proposer TEE checkpoints.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use base_proof_rpc::RollupProvider;
use base_proof_submission::ProofSubmissionError;
use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
use base_prover_service_protocol::{
    DeleteProofRequest, DeleteProofsByUnregisteredTeeSignersRequest, GetProofRequest, ProofStatus,
    TeeProofResult,
};
use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorCode};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    DriverConfig, IntervalResolver, Metrics, ProofSubmitter, ProofTarget, ProposerError,
    ProposerProofAdapter, RecoveredState, SubmitAction, TeeSignerRegistry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitOutcome {
    Advanced(RecoveredState),
    Restart,
    Idle,
}

/// Polls the next expected proof and submits it when ready.
pub struct ProofCollector<R>
where
    R: RollupProvider,
{
    proof_requester: Arc<dyn ProofRequesterProvider>,
    rollup_client: Arc<R>,
    submitter: ProofSubmitter,
    intervals: Arc<IntervalResolver>,
    tee_signer_registry: Arc<dyn TeeSignerRegistry>,
    cleanup_retry_delay: Duration,
    submit_timeout: Option<Duration>,
}

impl<R> std::fmt::Debug for ProofCollector<R>
where
    R: RollupProvider,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofCollector")
            .field("submit_timeout", &self.submit_timeout)
            .field("cleanup_retry_delay", &self.cleanup_retry_delay)
            .finish_non_exhaustive()
    }
}

impl<R> ProofCollector<R>
where
    R: RollupProvider + 'static,
{
    /// Creates a proof collector.
    pub const fn new(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<R>,
        submitter: ProofSubmitter,
        intervals: Arc<IntervalResolver>,
        tee_signer_registry: Arc<dyn TeeSignerRegistry>,
        config: &DriverConfig,
    ) -> Self {
        Self {
            proof_requester,
            rollup_client,
            submitter,
            intervals,
            tee_signer_registry,
            cleanup_retry_delay: config.poll_interval,
            submit_timeout: config.submit_timeout,
        }
    }

    /// Runs one collector tick.
    ///
    /// Returns `true` when the pipeline should drop stale recovery caches and
    /// reload state from chain immediately.
    ///
    /// `dispatched_through` is the highest target block accepted by the
    /// dispatcher in the current pipeline session.
    pub async fn tick(
        &self,
        current: &mut RecoveredState,
        finalized_head: u64,
        dispatched_through: u64,
        cancel: &CancellationToken,
    ) -> bool {
        if cancel.is_cancelled() {
            return false;
        }

        loop {
            if cancel.is_cancelled() {
                return false;
            }

            // Resolved per target: the interval changes at the Denim activation
            // block, so the cursor's own starting block picks the cadence.
            let starting_block = current.l2_block_number;
            let block_interval = match self.intervals.for_starting_block(starting_block).await {
                Ok(intervals) => intervals.block_interval,
                Err(error) => {
                    Metrics::errors_total(error.metric_label()).increment(1);
                    warn!(
                        starting_block,
                        error = %error,
                        "Failed to resolve proposal intervals, retrying next tick"
                    );
                    return false;
                }
            };

            let Some(target_block) = ProofTarget::next_block(starting_block, block_interval) else {
                return false;
            };

            if target_block > finalized_head {
                debug!(
                    current_block = current.l2_block_number,
                    target_block,
                    finalized_head,
                    "Finalized head below collection target, waiting for L2 head to advance"
                );
                return false;
            }

            let Some(claimed_l2_output_root) =
                ProofTarget::canonical_output_root(self.rollup_client.as_ref(), target_block).await
            else {
                return false;
            };

            let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_l2_output_root);
            let proof = match Self::poll_proof(
                self.proof_requester.as_ref(),
                target_block,
                &session_id,
                target_block <= dispatched_through,
            )
            .await
            {
                Ok(Some(proof)) => proof,
                Ok(None) => return false,
                Err(error) => {
                    warn!(
                        target_block,
                        session_id = %session_id,
                        error = %error,
                        "Deleting proof request after proof collection failure"
                    );
                    return self.delete_proof_request(&session_id, target_block).await;
                }
            };

            match self
                .submit_proof(
                    starting_block,
                    target_block,
                    &session_id,
                    proof,
                    current.parent_address,
                    cancel,
                )
                .await
            {
                SubmitOutcome::Advanced(next) => *current = next,
                SubmitOutcome::Restart => return true,
                SubmitOutcome::Idle => return false,
            }
        }
    }

    async fn poll_proof(
        proof_requester: &dyn ProofRequesterProvider,
        target_block: u64,
        session_id: &str,
        request_dispatched: bool,
    ) -> Result<Option<TeeProofResult>, ProposerError> {
        let response = match proof_requester
            .get_proof(GetProofRequest { session_id: session_id.to_owned() })
            .await
        {
            Ok(response) => response,
            Err(e) if e.is_not_found() => {
                if !request_dispatched {
                    debug!(
                        target_block,
                        session_id = %session_id,
                        "Proof request not dispatched yet"
                    );
                    return Ok(None);
                }

                let error = ProposerError::Prover(format!("proof session {session_id} not found"));
                Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED).increment(1);
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    target_block,
                    session_id = %session_id,
                    error = %error,
                    "Proof request missing"
                );
                return Err(error);
            }
            Err(e) => {
                Metrics::errors_total("prover").increment(1);
                warn!(
                    target_block,
                    session_id = %session_id,
                    error = %e,
                    "Failed to poll prover service"
                );
                return Ok(None);
            }
        };

        Metrics::proof_status_received_total(match response.status {
            ProofStatus::Queued => Metrics::PROOF_STATUS_QUEUED,
            ProofStatus::Running => Metrics::PROOF_STATUS_RUNNING,
            ProofStatus::Succeeded => Metrics::PROOF_STATUS_SUCCEEDED,
            ProofStatus::Failed => Metrics::PROOF_STATUS_FAILED,
        })
        .increment(1);

        match response.status {
            ProofStatus::Queued | ProofStatus::Running => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    status = ?response.status,
                    "Proof request still pending"
                );
                Ok(None)
            }
            ProofStatus::Failed => {
                let message = response.error_message.unwrap_or_else(|| {
                    format!("proof session {session_id} failed without an error message")
                });
                let error = ProposerError::Prover(message);
                Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED).increment(1);
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    target_block,
                    session_id = %session_id,
                    error = %error,
                    "Proof session failed"
                );
                Err(error)
            }
            ProofStatus::Succeeded => {
                let Some(result) = response.result else {
                    let error = ProposerError::Prover(format!(
                        "proof session {session_id} succeeded without a result"
                    ));
                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED)
                        .increment(1);
                    Metrics::errors_total(error.metric_label()).increment(1);
                    warn!(
                        target_block,
                        session_id = %session_id,
                        error = %error,
                        "Proof session returned no result"
                    );
                    return Err(error);
                };

                match ProposerProofAdapter::tee_proof_result(result) {
                    Ok(proof) => {
                        info!(target_block, session_id = %session_id, "Proof request succeeded");
                        Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_READY)
                            .increment(1);
                        Metrics::last_collected_block().set(target_block as f64);
                        Ok(Some(proof))
                    }
                    Err(error) => {
                        Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED)
                            .increment(1);
                        Metrics::errors_total(error.metric_label()).increment(1);
                        warn!(
                            target_block,
                            session_id = %session_id,
                            error = %error,
                            "Proof result rejected"
                        );
                        Err(error)
                    }
                }
            }
        }
    }

    #[tracing::instrument(
        name = "proposer.collect_submit",
        skip_all,
        fields(target_block = target_block, session_id = %session_id)
    )]
    async fn submit_proof(
        &self,
        starting_block: u64,
        target_block: u64,
        session_id: &str,
        proof: TeeProofResult,
        parent_address: Address,
        cancel: &CancellationToken,
    ) -> SubmitOutcome {
        info!(target_block, parent_address = %parent_address, "Submitting proof");

        let mut submit_timer = base_metrics::timed!(Metrics::proposal_total_duration_seconds());
        let result = match cancel
            .run_until_cancelled(async {
                let submit = self.submitter.submit(
                    &proof.aggregate_proposal,
                    &proof.proposals,
                    starting_block,
                    target_block,
                    parent_address,
                );
                match self.submit_timeout {
                    Some(timeout) => tokio::time::timeout(timeout, submit).await,
                    None => Ok(submit.await),
                }
            })
            .await
        {
            Some(Ok(result)) => result,
            Some(Err(_)) => {
                submit_timer.disarm();
                Metrics::submit_timeouts_total().increment(1);
                warn!(
                    target_block,
                    timeout_secs = ?self.submit_timeout.map(|timeout| timeout.as_secs()),
                    "Submit timed out"
                );
                return SubmitOutcome::Restart;
            }
            None => {
                submit_timer.disarm();
                warn!(target_block, "Submit cancelled");
                return SubmitOutcome::Idle;
            }
        };

        if result.is_err() {
            submit_timer.disarm();
        }

        match result {
            Ok(next) => {
                drop(submit_timer);
                info!(target_block, "Submission successful");
                Metrics::last_proposed_block().set(target_block as f64);
                return SubmitOutcome::Advanced(next);
            }
            Err(SubmitAction::RootMismatch) => {
                Metrics::root_mismatch_total().increment(1);
                warn!(target_block, "Output root mismatch at submit time");
                self.delete_proof_request(session_id, target_block).await;
                return SubmitOutcome::Restart;
            }
            Err(SubmitAction::GameAlreadyExists) => {
                info!(target_block, "Game already exists onchain");
                Metrics::last_proposed_block().set(target_block as f64);
            }
            Err(SubmitAction::Failed(error)) => {
                // Persistent submit failures intentionally keep the proof request
                // so transient RPC/L1 issues can retry the same completed proof.
                // Operators should alert on base_proposer_errors_total if this
                // repeats.
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(target_block, error = %error, "Submission failed");
            }
            Err(SubmitAction::Discard(error)) => {
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    target_block,
                    error = %error,
                    "Submission discarded, deleting proof request for re-prove"
                );
                let deleted = cancel
                    .run_until_cancelled(async {
                        if matches!(
                            error,
                            ProposerError::Submission(ProofSubmissionError::InvalidSigner)
                        ) {
                            self.discard_invalid_signer_proof(
                                session_id,
                                target_block,
                                proof.tee_signer,
                            )
                            .await
                        } else {
                            self.delete_proof_request(session_id, target_block).await
                        }
                    })
                    .await;
                return match deleted {
                    Some(true) => SubmitOutcome::Restart,
                    Some(false) | None => SubmitOutcome::Idle,
                };
            }
        }

        SubmitOutcome::Restart
    }

    async fn delete_proof_request(&self, session_id: &str, target_block: u64) -> bool {
        match self
            .proof_requester
            .delete_proof_request(DeleteProofRequest { session_id: session_id.to_owned() })
            .await
        {
            Ok(()) => {
                info!(
                    target_block,
                    session_id = %session_id,
                    "Deleted proof request for re-prove"
                );
                true
            }
            Err(error) => {
                warn!(
                    target_block,
                    session_id = %session_id,
                    error = %error,
                    "Failed to delete proof request"
                );
                false
            }
        }
    }

    /// Discards a rejected TEE proof and purges other currently unregistered signers.
    ///
    /// Call only after the current submission has returned. Missing registry snapshots
    /// forbid bulk deletion. Registry read failures, definite bulk rejections, and
    /// successful purges that leave the rejected session protected use terminal-session
    /// deletion alone. Unknown destructive outcomes back off before recovery.
    pub async fn discard_invalid_signer_proof(
        &self,
        session_id: &str,
        target_block: u64,
        reported_signer: Address,
    ) -> bool {
        let registered_signers = match self.tee_signer_registry.registered_signers().await {
            Ok(signers) => signers,
            Err(error) => {
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(target_block, error = %error, "Failed to read TEE signer registry for cleanup");
                return self.delete_proof_request(session_id, target_block).await;
            }
        };
        let rejected_signer_is_registered = registered_signers.contains(&reported_signer);
        match self
            .proof_requester
            .delete_proofs_by_unregistered_tee_signers(
                DeleteProofsByUnregisteredTeeSignersRequest { registered_signers },
            )
            .await
        {
            Ok(deleted_count) => {
                info!(
                    target_block,
                    deleted_count, "Deleted unregistered TEE signer proof requests"
                );
                if deleted_count == 0 || rejected_signer_is_registered || reported_signer.is_zero()
                {
                    // A game's pinned verifier or journal can reject a proof even while its
                    // host-reported signer remains registered in the active implementation.
                    // Re-prove only that session; do not purge other registered proofs.
                    let rejected_session_deleted =
                        self.delete_proof_request(session_id, target_block).await;
                    rejected_session_deleted || deleted_count > 0
                } else {
                    true
                }
            }
            Err(ProverServiceClientError::RpcTransport(JsonRpcClientError::Call(ref call)))
                if call.code() == ErrorCode::MethodNotFound.code()
                    || call.code() == ErrorCode::InvalidParams.code() =>
            {
                Metrics::errors_total("prover").increment(1);
                warn!(
                    target_block,
                    error = %call,
                    "Bulk TEE signer cleanup rejected; re-proving only the rejected session"
                );
                self.delete_proof_request(session_id, target_block).await
            }
            Err(error) => {
                Metrics::errors_total("prover").increment(1);
                warn!(
                    target_block,
                    error = %error,
                    "TEE signer cleanup outcome unknown, backing off before recovery"
                );
                tokio::time::sleep(self.cleanup_retry_delay).await;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{Address, B256};
    use base_proof_contracts::{
        AggregateVerifierClient, DisputeGameFactoryClient, game_lookup_key,
    };
    use base_proof_primitives::ProofRequest;
    use base_proof_submission::ProofSubmissionError;
    use base_prover_service_protocol::TeeKind;
    use futures::FutureExt;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        DriverConfig, OutputProposer,
        tee_signer_registry::MockTeeSignerRegistry,
        test_utils::{
            MockAggregateVerifier, MockDisputeGameFactory, MockOutputProposer, MockProofRequester,
            MockRollupClient, test_fixed_interval_resolver, test_proposal, test_sync_status,
        },
    };

    const BLOCK_INTERVAL: u64 = 100;
    const CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(10);
    fn recovered(block: u64) -> RecoveredState {
        RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::ZERO,
            l2_block_number: block,
        }
    }

    fn rollup_client(block: u64, root: Option<B256>) -> Arc<MockRollupClient> {
        Arc::new(MockRollupClient {
            sync_status: test_sync_status(block, B256::ZERO),
            output_roots: root.into_iter().map(|root| (block, root)).collect(),
            max_safe_block: None,
        })
    }

    fn make_collector(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<MockRollupClient>,
        output_proposer: Arc<dyn OutputProposer>,
    ) -> ProofCollector<MockRollupClient> {
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().returning(|| Ok(Vec::new()));
        make_collector_with_contracts(
            proof_requester,
            rollup_client,
            output_proposer,
            Arc::new(MockDisputeGameFactory::default()),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(registry),
        )
    }

    fn make_collector_with_contracts(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<MockRollupClient>,
        output_proposer: Arc<dyn OutputProposer>,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        registry: Arc<dyn TeeSignerRegistry>,
    ) -> ProofCollector<MockRollupClient> {
        let intervals = test_fixed_interval_resolver(BLOCK_INTERVAL, BLOCK_INTERVAL);
        let submitter = ProofSubmitter::new(
            output_proposer,
            Arc::<MockRollupClient>::clone(&rollup_client),
            factory_client,
            verifier_client,
            Arc::clone(&intervals),
            &DriverConfig { recovery_scan_concurrency: 1, ..Default::default() },
        );

        ProofCollector::new(
            proof_requester,
            rollup_client,
            submitter,
            intervals,
            registry,
            &DriverConfig {
                poll_interval: CLEANUP_RETRY_DELAY,
                submit_timeout: Some(Duration::from_secs(60)),
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn tick_waits_when_proof_session_was_not_dispatched() {
        let target_block = 200;
        let collector = make_collector(
            Arc::new(MockProofRequester::default()),
            rollup_client(target_block, Some(B256::repeat_byte(0xaa))),
            Arc::new(MockOutputProposer::default()),
        );

        let mut current = recovered(100);
        let dispatched_through = current.l2_block_number;
        let restart = collector
            .tick(&mut current, target_block, dispatched_through, &CancellationToken::new())
            .await;

        assert!(!restart);
    }

    #[tokio::test]
    async fn tick_restarts_when_proof_session_is_missing() {
        let target_block = 200;
        let collector = make_collector(
            Arc::new(MockProofRequester::default()),
            rollup_client(target_block, Some(B256::repeat_byte(0xaa))),
            Arc::new(MockOutputProposer::default()),
        );

        let mut current = recovered(100);
        let restart = collector
            .tick(&mut current, target_block, target_block, &CancellationToken::new())
            .await;

        assert!(restart);
    }

    #[tokio::test]
    async fn tick_submits_ready_proof_and_restarts_when_game_address_unknown() {
        let requester = Arc::new(MockProofRequester::default());
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let proof_request = ProofRequest {
            claimed_l2_output_root: claimed_root,
            claimed_l2_block_number: target_block,
            l1_head_number: 1000,
            ..Default::default()
        };
        requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(proof_request))
            .await
            .expect("test setup should dispatch root session");
        let collector = make_collector_with_contracts(
            Arc::<MockProofRequester>::clone(&requester),
            rollup_client(target_block, Some(claimed_root)),
            Arc::new(MockOutputProposer::default()),
            Arc::new(MockDisputeGameFactory::default()),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockTeeSignerRegistry::new()),
        );

        let mut current = recovered(100);
        let restart = collector
            .tick(&mut current, target_block, target_block, &CancellationToken::new())
            .await;

        assert!(restart);
        assert!(requester.deleted_tee_signer_allowlists.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tick_collects_ready_proofs_and_restarts_when_next_session_is_missing() {
        let requester = Arc::new(MockProofRequester::default());
        let first_target = 200;
        let second_target = 300;
        let first_root = B256::repeat_byte(first_target as u8);
        let second_root = B256::repeat_byte(second_target as u8);
        let first_game = Address::repeat_byte(0x20);
        let second_game = Address::repeat_byte(0x30);

        for (target_block, claimed_root) in
            [(first_target, first_root), (second_target, second_root)]
        {
            let proof_request = ProofRequest {
                claimed_l2_output_root: claimed_root,
                claimed_l2_block_number: target_block,
                l1_head_number: 1000,
                ..Default::default()
            };
            requester
                .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(
                    proof_request,
                ))
                .await
                .expect("test setup should dispatch root session");
        }

        let mut factory = MockDisputeGameFactory::default();
        let first_key = game_lookup_key(
            first_target - BLOCK_INTERVAL,
            Address::ZERO,
            BLOCK_INTERVAL,
            BLOCK_INTERVAL,
            &[first_root],
        )
        .unwrap();
        let second_key = game_lookup_key(
            second_target - BLOCK_INTERVAL,
            first_game,
            BLOCK_INTERVAL,
            BLOCK_INTERVAL,
            &[second_root],
        )
        .unwrap();
        factory.uuid_games.insert((0, first_key.root_claim, first_key.extra_data), first_game);
        factory.uuid_games.insert((0, second_key.root_claim, second_key.extra_data), second_game);

        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(second_target, B256::ZERO),
            output_roots: [(first_target, first_root), (second_target, second_root)]
                .into_iter()
                .collect(),
            max_safe_block: None,
        });
        let collector = make_collector_with_contracts(
            Arc::clone(&requester) as Arc<dyn ProofRequesterProvider>,
            rollup_client,
            Arc::new(MockOutputProposer::default()),
            Arc::new(factory),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockTeeSignerRegistry::new()),
        );
        let mut current = recovered(100);

        let restart = collector.tick(&mut current, 400, 400, &CancellationToken::new()).await;

        assert!(restart);
        assert_eq!(current.l2_block_number, second_target);
        assert_eq!(current.parent_address, second_game);
    }

    #[tokio::test]
    async fn tick_bulk_deletes_all_unregistered_signers_on_first_invalid_signer() {
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let registered = Address::repeat_byte(0x33);
        let targets = [
            (200, B256::repeat_byte(0xc8), first),
            (300, B256::repeat_byte(0x30), second),
            (400, B256::repeat_byte(0x40), registered),
        ];
        let requester = Arc::new(MockProofRequester {
            tee_signers: targets
                .iter()
                .map(|(_, root, signer)| {
                    (ProposerProofAdapter::tee_session_id_for_root(*root), *signer)
                })
                .collect(),
            ..Default::default()
        });
        for (block, root, _) in targets {
            requester
                .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(
                    ProofRequest {
                        claimed_l2_output_root: root,
                        claimed_l2_block_number: block,
                        l1_head_number: 1000,
                        ..Default::default()
                    },
                ))
                .await
                .unwrap();
        }
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).return_once(move || Ok(vec![registered]));
        let output = Arc::new(MockOutputProposer::with_create_error(ProposerError::Submission(
            ProofSubmissionError::InvalidSigner,
        )));
        let collector = make_collector_with_contracts(
            Arc::<MockProofRequester>::clone(&requester),
            rollup_client(200, Some(targets[0].1)),
            Arc::<MockOutputProposer>::clone(&output),
            Arc::new(MockDisputeGameFactory::default()),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(registry),
        );
        let mut current = recovered(100);

        assert!(collector.tick(&mut current, 400, 400, &CancellationToken::new()).await);

        let requests = requester.requests.lock().unwrap();
        assert!(
            !requests.contains_key(&ProposerProofAdapter::tee_session_id_for_root(targets[0].1))
        );
        assert!(
            !requests.contains_key(&ProposerProofAdapter::tee_session_id_for_root(targets[1].1))
        );
        assert!(
            requests.contains_key(&ProposerProofAdapter::tee_session_id_for_root(targets[2].1))
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(
            *requester.deleted_tee_signer_allowlists.lock().unwrap(),
            vec![vec![registered]]
        );
        assert!(requester.deleted_tee_signers.lock().unwrap().is_empty());
        assert_eq!(*output.created.lock().unwrap(), 1);
        assert_eq!(current.l2_block_number, 100);
    }

    #[tokio::test(start_paused = true)]
    async fn tick_backs_off_and_restarts_when_cleanup_outcome_is_unknown() {
        let requester = Arc::new(MockProofRequester { reject_delete: true, ..Default::default() });
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_root);
        let proof_request = ProofRequest {
            claimed_l2_output_root: claimed_root,
            claimed_l2_block_number: target_block,
            l1_head_number: 1000,
            ..Default::default()
        };
        requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(proof_request))
            .await
            .expect("test setup should dispatch root session");
        let collector = make_collector(
            Arc::clone(&requester) as Arc<dyn ProofRequesterProvider>,
            rollup_client(target_block, Some(claimed_root)),
            Arc::new(MockOutputProposer::with_create_error(ProposerError::Submission(
                ProofSubmissionError::InvalidSigner,
            ))),
        );

        let mut current = recovered(100);
        let started = tokio::time::Instant::now();
        let restart = collector
            .tick(&mut current, target_block, target_block, &CancellationToken::new())
            .await;

        assert!(restart);
        assert!(started.elapsed() >= CLEANUP_RETRY_DELAY);
        assert!(requester.requests.lock().unwrap().contains_key(&session_id));
    }

    #[tokio::test]
    async fn root_mismatch_restarts_even_when_delete_fails() {
        let requester = Arc::new(MockProofRequester { reject_delete: true, ..Default::default() });
        let target_block = 200;
        let stale_root = B256::repeat_byte(0xaa);
        let fresh_root = B256::repeat_byte(0xbb);
        let session_id = ProposerProofAdapter::tee_session_id_for_root(stale_root);
        let proof_request = ProofRequest {
            claimed_l2_output_root: stale_root,
            claimed_l2_block_number: target_block,
            l1_head_number: 1000,
            ..Default::default()
        };
        requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(proof_request))
            .await
            .expect("test setup should dispatch stale root session");
        let collector = make_collector(
            Arc::clone(&requester) as Arc<dyn ProofRequesterProvider>,
            rollup_client(target_block, Some(fresh_root)),
            Arc::new(MockOutputProposer::default()),
        );

        let mut aggregate_proposal = test_proposal(target_block);
        aggregate_proposal.output_root = stale_root;
        let outcome = collector
            .submit_proof(
                target_block - BLOCK_INTERVAL,
                target_block,
                &session_id,
                TeeProofResult {
                    aggregate_proposal,
                    proposals: vec![],
                    tee_kind: TeeKind::AwsNitro,
                    tee_signer: Address::repeat_byte(0x11),
                },
                Address::ZERO,
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome, SubmitOutcome::Restart);
        assert!(requester.requests.lock().unwrap().contains_key(&session_id));
    }

    #[tokio::test]
    async fn tick_deletes_failed_proof_and_restarts() {
        let requester = Arc::new(MockProofRequester::default());
        let target_block = 200;
        let claimed_root = B256::repeat_byte(0xaa);
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_root);
        requester
            .failed_sessions
            .lock()
            .unwrap()
            .insert(session_id.clone(), "simulated proof failure".to_owned());
        let collector = make_collector(
            Arc::clone(&requester) as Arc<dyn ProofRequesterProvider>,
            rollup_client(target_block, Some(claimed_root)),
            Arc::new(MockOutputProposer::default()),
        );

        let mut current = recovered(100);
        let restart = collector
            .tick(&mut current, target_block, target_block, &CancellationToken::new())
            .await;

        assert!(restart);
        assert!(!requester.failed_sessions.lock().unwrap().contains_key(&session_id));
    }

    async fn invalid_signer_collector(
        requester: Arc<MockProofRequester>,
        registry: Arc<dyn TeeSignerRegistry>,
    ) -> (ProofCollector<MockRollupClient>, [String; 2]) {
        let targets = [(200, B256::repeat_byte(0xc8)), (300, B256::repeat_byte(0x2c))];
        let sessions = targets.map(|(_, root)| ProposerProofAdapter::tee_session_id_for_root(root));
        for (block, root) in targets {
            requester
                .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(
                    ProofRequest {
                        claimed_l2_output_root: root,
                        claimed_l2_block_number: block,
                        l1_head_number: 1000,
                        ..Default::default()
                    },
                ))
                .await
                .unwrap();
        }
        let collector = make_collector_with_contracts(
            requester,
            rollup_client(200, Some(targets[0].1)),
            Arc::new(MockOutputProposer::with_create_error(ProposerError::Submission(
                ProofSubmissionError::InvalidSigner,
            ))),
            Arc::new(MockDisputeGameFactory::default()),
            Arc::new(MockAggregateVerifier::default()),
            registry,
        );
        (collector, sessions)
    }

    #[tokio::test]
    async fn registry_failure_reproves_only_the_rejected_session_without_bulk_deletion() {
        let requester = Arc::new(MockProofRequester::default());
        let mut registry = MockTeeSignerRegistry::new();
        registry
            .expect_registered_signers()
            .times(1)
            .return_once(|| Err(ProposerError::Contract("registry RPC unavailable".into())));
        let (collector, sessions) =
            invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
        let mut current = recovered(100);

        assert!(collector.tick(&mut current, 300, 300, &CancellationToken::new()).await);
        assert!(
            requester
                .get_proof(GetProofRequest { session_id: sessions[0].clone() })
                .await
                .unwrap_err()
                .is_not_found()
        );
        assert_eq!(
            requester
                .get_proof(GetProofRequest { session_id: sessions[1].clone() })
                .await
                .unwrap()
                .status,
            ProofStatus::Succeeded
        );
        assert!(requester.deleted_tee_signer_allowlists.lock().unwrap().is_empty());
        assert!(requester.deleted_tee_signers.lock().unwrap().is_empty());
        assert_eq!(current.l2_block_number, 100);
    }

    #[tokio::test]
    async fn registry_failure_idles_when_rejected_session_deletion_fails() {
        let requester =
            Arc::new(MockProofRequester { reject_session_delete: true, ..Default::default() });
        let mut registry = MockTeeSignerRegistry::new();
        registry
            .expect_registered_signers()
            .times(1)
            .return_once(|| Err(ProposerError::Contract("registry RPC unavailable".into())));
        let (collector, sessions) =
            invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
        let mut current = recovered(100);

        assert!(!collector.tick(&mut current, 300, 300, &CancellationToken::new()).await);
        for session_id in sessions {
            assert_eq!(
                requester.get_proof(GetProofRequest { session_id }).await.unwrap().status,
                ProofStatus::Succeeded
            );
        }
        assert!(requester.deleted_tee_signer_allowlists.lock().unwrap().is_empty());
        assert!(requester.deleted_tee_signers.lock().unwrap().is_empty());
        assert_eq!(current.l2_block_number, 100);
    }

    #[tokio::test(start_paused = true)]
    async fn standard_bulk_rejections_idle_when_rejected_session_deletion_fails() {
        for code in [ErrorCode::MethodNotFound.code(), ErrorCode::InvalidParams.code()] {
            let requester = Arc::new(MockProofRequester {
                bulk_delete_error_code: Some(code),
                reject_session_delete: true,
                ..Default::default()
            });
            let mut registry = MockTeeSignerRegistry::new();
            registry.expect_registered_signers().times(1).return_once(|| Ok(Vec::new()));
            let (collector, sessions) =
                invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
            let mut current = recovered(100);
            let started = tokio::time::Instant::now();

            assert!(!collector.tick(&mut current, 300, 300, &CancellationToken::new()).await);
            assert!(started.elapsed() < CLEANUP_RETRY_DELAY);
            for session_id in sessions {
                assert_eq!(
                    requester.get_proof(GetProofRequest { session_id }).await.unwrap().status,
                    ProofStatus::Succeeded
                );
            }
            assert_eq!(requester.deleted_tee_signer_allowlists.lock().unwrap().len(), 1);
            assert!(requester.deleted_tee_signers.lock().unwrap().is_empty());
            assert_eq!(current.l2_block_number, 100);
        }
    }

    #[tokio::test]
    async fn bulk_cleanup_reproves_the_registered_head_and_preserves_other_registered_proofs() {
        let registered = Address::repeat_byte(0x11);
        let stale = Address::repeat_byte(0x22);
        let later = ProposerProofAdapter::tee_session_id_for_root(B256::repeat_byte(0x2c));
        let requester = Arc::new(MockProofRequester {
            tee_signers: [(later, stale)].into_iter().collect(),
            ..Default::default()
        });
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).return_once(move || Ok(vec![registered]));
        let (collector, sessions) =
            invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
        let healthy_root = B256::repeat_byte(0x40);
        let healthy_session = ProposerProofAdapter::tee_session_id_for_root(healthy_root);
        requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(ProofRequest {
                claimed_l2_output_root: healthy_root,
                claimed_l2_block_number: 400,
                l1_head_number: 1000,
                ..Default::default()
            }))
            .await
            .unwrap();
        let mut current = recovered(100);

        assert!(collector.tick(&mut current, 400, 400, &CancellationToken::new()).await);
        let requests = requester.requests.lock().unwrap();
        assert!(!requests.contains_key(&sessions[0]));
        assert!(!requests.contains_key(&sessions[1]));
        assert!(requests.contains_key(&healthy_session));
        assert_eq!(requests.len(), 1);
        assert_eq!(
            *requester.deleted_tee_signer_allowlists.lock().unwrap(),
            vec![vec![registered]]
        );
    }

    #[tokio::test]
    async fn zero_row_cleanup_reproves_only_the_rejected_registered_session() {
        let requester = Arc::new(MockProofRequester::default());
        let registered = Address::repeat_byte(0x11);
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).return_once(move || Ok(vec![registered]));
        let (collector, sessions) =
            invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
        let mut current = recovered(100);

        assert!(collector.tick(&mut current, 300, 300, &CancellationToken::new()).await);
        assert!(
            requester
                .get_proof(GetProofRequest { session_id: sessions[0].clone() })
                .await
                .unwrap_err()
                .is_not_found()
        );
        assert_eq!(
            requester
                .get_proof(GetProofRequest { session_id: sessions[1].clone() })
                .await
                .unwrap()
                .status,
            ProofStatus::Succeeded
        );
        assert_eq!(requester.deleted_tee_signer_allowlists.lock().unwrap().len(), 1);
        assert_eq!(current.l2_block_number, 100);
    }

    #[tokio::test]
    async fn failed_session_fallback_restarts_only_after_committed_bulk_deletion() {
        let registered = Address::repeat_byte(0x11);
        let later = ProposerProofAdapter::tee_session_id_for_root(B256::repeat_byte(0x2c));
        for purge_stale_backlog in [false, true] {
            let requester = Arc::new(MockProofRequester {
                reject_session_delete: true,
                tee_signers: if purge_stale_backlog {
                    [(later.clone(), Address::repeat_byte(0x22))].into_iter().collect()
                } else {
                    Default::default()
                },
                ..Default::default()
            });
            let mut registry = MockTeeSignerRegistry::new();
            registry.expect_registered_signers().times(1).return_once(move || Ok(vec![registered]));
            let (collector, sessions) =
                invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
            let mut current = recovered(100);

            assert_eq!(
                collector.tick(&mut current, 300, 300, &CancellationToken::new()).await,
                purge_stale_backlog
            );
            assert_eq!(
                requester
                    .get_proof(GetProofRequest { session_id: sessions[0].clone() })
                    .await
                    .unwrap()
                    .status,
                ProofStatus::Succeeded
            );
            assert_eq!(
                requester
                    .get_proof(GetProofRequest { session_id: sessions[1].clone() })
                    .await
                    .is_err(),
                purge_stale_backlog
            );
            assert_eq!(current.l2_block_number, 100);
        }
    }

    #[tokio::test]
    async fn invalid_signer_from_existing_game_reproves_only_its_registered_head() {
        let requester = Arc::new(MockProofRequester::default());
        let registered = Address::repeat_byte(0x11);
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).return_once(move || Ok(vec![registered]));
        let (_, sessions) = invalid_signer_collector(
            Arc::clone(&requester),
            Arc::new(MockTeeSignerRegistry::new()),
        )
        .await;
        let target_block = 200;
        let claimed_root = B256::repeat_byte(0xc8);
        let game_address = Address::repeat_byte(0x20);
        let key = game_lookup_key(
            target_block - BLOCK_INTERVAL,
            Address::ZERO,
            BLOCK_INTERVAL,
            BLOCK_INTERVAL,
            &[claimed_root],
        )
        .unwrap();
        let mut factory = MockDisputeGameFactory::default();
        factory.uuid_games.insert((0, key.root_claim, key.extra_data), game_address);
        let output = Arc::new(MockOutputProposer::default());
        *output.verify_error.lock().unwrap() =
            Some(ProposerError::Submission(ProofSubmissionError::InvalidSigner));
        let collector = make_collector_with_contracts(
            Arc::clone(&requester) as Arc<dyn ProofRequesterProvider>,
            rollup_client(target_block, Some(claimed_root)),
            Arc::clone(&output) as Arc<dyn OutputProposer>,
            Arc::new(factory),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(registry),
        );
        let mut current = recovered(100);

        assert!(collector.tick(&mut current, 300, 300, &CancellationToken::new()).await);
        assert!(
            requester
                .get_proof(GetProofRequest { session_id: sessions[0].clone() })
                .await
                .unwrap_err()
                .is_not_found()
        );
        assert_eq!(
            requester
                .get_proof(GetProofRequest { session_id: sessions[1].clone() })
                .await
                .unwrap()
                .status,
            ProofStatus::Succeeded
        );
        assert_eq!(*output.verified.lock().unwrap(), vec![game_address]);
        assert_eq!(*output.created.lock().unwrap(), 0);
        assert_eq!(current.l2_block_number, 100);
    }

    #[tokio::test(start_paused = true)]
    async fn every_bulk_cleanup_attempt_reads_a_fresh_registration_snapshot() {
        let requester = Arc::new(MockProofRequester { reject_delete: true, ..Default::default() });
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let mut registry = MockTeeSignerRegistry::new();
        let mut sequence = mockall::Sequence::new();
        registry
            .expect_registered_signers()
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(move || Ok(vec![first]));
        registry
            .expect_registered_signers()
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(move || Ok(vec![second]));
        let (collector, sessions) =
            invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;

        for _ in 0..2 {
            assert!(collector.discard_invalid_signer_proof(&sessions[0], 200, first).await);
        }
        assert_eq!(
            *requester.deleted_tee_signer_allowlists.lock().unwrap(),
            vec![vec![first], vec![second]]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn standard_bulk_rejections_reprove_only_the_rejected_session_without_backoff() {
        for code in [ErrorCode::MethodNotFound.code(), ErrorCode::InvalidParams.code()] {
            let requester = Arc::new(MockProofRequester {
                bulk_delete_error_code: Some(code),
                ..Default::default()
            });
            let mut registry = MockTeeSignerRegistry::new();
            registry.expect_registered_signers().times(1).return_once(|| Ok(Vec::new()));
            let (collector, sessions) =
                invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
            let mut current = recovered(100);
            let started = tokio::time::Instant::now();

            assert!(collector.tick(&mut current, 300, 300, &CancellationToken::new()).await);
            assert!(started.elapsed() < CLEANUP_RETRY_DELAY);
            assert!(
                requester
                    .get_proof(GetProofRequest { session_id: sessions[0].clone() })
                    .await
                    .unwrap_err()
                    .is_not_found()
            );
            assert_eq!(
                requester
                    .get_proof(GetProofRequest { session_id: sessions[1].clone() })
                    .await
                    .unwrap()
                    .status,
                ProofStatus::Succeeded
            );
            assert_eq!(requester.deleted_tee_signer_allowlists.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn lost_bulk_reply_still_requests_recovery_after_deleting_all_stale_signers() {
        let later = ProposerProofAdapter::tee_session_id_for_root(B256::repeat_byte(0x2c));
        let requester = Arc::new(MockProofRequester {
            tee_signers: [(later, Address::repeat_byte(0x22))].into_iter().collect(),
            lose_bulk_delete_response: true,
            ..Default::default()
        });
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).return_once(|| Ok(Vec::new()));
        let (collector, sessions) =
            invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
        let mut current = recovered(100);
        let started = tokio::time::Instant::now();

        assert!(collector.tick(&mut current, 300, 300, &CancellationToken::new()).await);
        assert!(started.elapsed() >= CLEANUP_RETRY_DELAY);
        let requests = requester.requests.lock().unwrap();
        assert!(sessions.iter().all(|session| !requests.contains_key(session)));
        assert_eq!(requester.deleted_tee_signer_allowlists.lock().unwrap().len(), 1);
        assert_eq!(current.l2_block_number, 100);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_unknown_cleanup_backoff_without_requesting_a_restart() {
        let requester = Arc::new(MockProofRequester { reject_delete: true, ..Default::default() });
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).return_once(|| Ok(Vec::new()));
        let (collector, sessions) =
            invalid_signer_collector(Arc::clone(&requester), Arc::new(registry)).await;
        let mut current = recovered(100);
        let cancel = CancellationToken::new();

        let restart = {
            let tick = collector.tick(&mut current, 300, 300, &cancel);
            tokio::pin!(tick);
            assert!(tick.as_mut().now_or_never().is_none());
            assert_eq!(requester.deleted_tee_signer_allowlists.lock().unwrap().len(), 1);
            cancel.cancel();
            tick.await
        };

        assert!(!restart);
        let requests = requester.requests.lock().unwrap();
        assert!(sessions.iter().all(|session| requests.contains_key(session)));
        assert_eq!(current.l2_block_number, 100);
    }
}
