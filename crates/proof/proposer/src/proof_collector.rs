//! Polls and submits prover-service proofs for proposer TEE checkpoints.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use base_proof_primitives::Proposal;
use base_proof_rpc::RollupProvider;
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{DeleteProofRequest, GetProofRequest, ProofStatus};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    Metrics, ProofSubmitter, ProofTarget, ProposerError, ProposerProofAdapter, RecoveredState,
    SubmitAction,
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
    block_interval: u64,
    submit_timeout: Option<Duration>,
}

impl<R> std::fmt::Debug for ProofCollector<R>
where
    R: RollupProvider,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofCollector")
            .field("block_interval", &self.block_interval)
            .field("submit_timeout", &self.submit_timeout)
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
        block_interval: u64,
        submit_timeout: Option<Duration>,
    ) -> Self {
        Self { proof_requester, rollup_client, submitter, block_interval, submit_timeout }
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
        safe_head: u64,
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

            let Some(target_block) =
                ProofTarget::next_block(current.l2_block_number, self.block_interval)
            else {
                return false;
            };

            if target_block > safe_head {
                debug!(
                    current_block = current.l2_block_number,
                    target_block,
                    safe_head,
                    "Safe head below collection target, waiting for L2 head to advance"
                );
                return false;
            }

            let Some(claimed_l2_output_root) = ProofTarget::canonical_output_root(
                self.rollup_client.as_ref(),
                target_block,
                "collector",
            )
            .await
            else {
                return false;
            };

            let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_l2_output_root);
            let (aggregate_proposal, proposals) = match Self::poll_proof(
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
                    target_block,
                    &session_id,
                    aggregate_proposal,
                    proposals,
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
    ) -> Result<Option<(Proposal, Vec<Proposal>)>, ProposerError> {
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

    async fn submit_proof(
        &self,
        target_block: u64,
        session_id: &str,
        aggregate_proposal: Proposal,
        proposals: Vec<Proposal>,
        parent_address: Address,
        cancel: &CancellationToken,
    ) -> SubmitOutcome {
        info!(target_block, parent_address = %parent_address, "Submitting proof");

        let mut submit_timer = base_metrics::timed!(Metrics::proposal_total_duration_seconds());
        let result = match cancel
            .run_until_cancelled(async {
                let submit = self.submitter.submit(
                    &aggregate_proposal,
                    &proposals,
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
                return if self.delete_proof_request(session_id, target_block).await {
                    SubmitOutcome::Restart
                } else {
                    SubmitOutcome::Idle
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{Address, B256};
    use base_proof_contracts::{
        AggregateVerifierClient, DisputeGameFactoryClient, encode_extra_data,
    };
    use base_proof_primitives::ProofRequest;
    use base_proof_submission::ProofSubmissionError;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        DriverConfig, OutputProposer,
        test_utils::{
            MockAggregateVerifier, MockDisputeGameFactory, MockOutputProposer, MockProofRequester,
            MockRollupClient, test_proposal, test_sync_status,
        },
    };

    const BLOCK_INTERVAL: u64 = 100;
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
        make_collector_with_contracts(
            proof_requester,
            rollup_client,
            output_proposer,
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            Arc::new(MockAggregateVerifier::default()),
        )
    }

    fn make_collector_with_contracts(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<MockRollupClient>,
        output_proposer: Arc<dyn OutputProposer>,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
    ) -> ProofCollector<MockRollupClient> {
        let submitter = ProofSubmitter::new(
            output_proposer,
            Arc::<MockRollupClient>::clone(&rollup_client),
            factory_client,
            verifier_client,
            &DriverConfig {
                block_interval: BLOCK_INTERVAL,
                intermediate_block_interval: BLOCK_INTERVAL,
                recovery_scan_concurrency: 1,
                ..Default::default()
            },
        );

        ProofCollector::new(
            proof_requester,
            rollup_client,
            submitter,
            BLOCK_INTERVAL,
            Some(std::time::Duration::from_secs(60)),
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
            intermediate_block_interval: BLOCK_INTERVAL,
            l1_head_number: 1000,
            ..Default::default()
        };
        requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(proof_request))
            .await
            .expect("test setup should dispatch root session");
        let collector = make_collector(
            requester,
            rollup_client(target_block, Some(claimed_root)),
            Arc::new(MockOutputProposer::default()),
        );

        let mut current = recovered(100);
        let restart = collector
            .tick(&mut current, target_block, target_block, &CancellationToken::new())
            .await;

        assert!(restart);
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
                intermediate_block_interval: BLOCK_INTERVAL,
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

        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.uuid_games.insert(
            (0, first_root, encode_extra_data(first_target, Address::ZERO, &[first_root])),
            first_game,
        );
        factory.uuid_games.insert(
            (0, second_root, encode_extra_data(second_target, first_game, &[second_root])),
            second_game,
        );

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
        );
        let mut current = recovered(100);

        let restart = collector.tick(&mut current, 400, 400, &CancellationToken::new()).await;

        assert!(restart);
        assert_eq!(current.l2_block_number, second_target);
        assert_eq!(current.parent_address, second_game);
    }

    #[tokio::test]
    async fn tick_deletes_succeeded_proof_when_submission_discards_it() {
        let requester = Arc::new(MockProofRequester::default());
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_root);
        let proof_request = ProofRequest {
            claimed_l2_output_root: claimed_root,
            claimed_l2_block_number: target_block,
            intermediate_block_interval: BLOCK_INTERVAL,
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
        let restart = collector
            .tick(&mut current, target_block, target_block, &CancellationToken::new())
            .await;

        assert!(restart);
        assert!(!requester.requests.lock().unwrap().contains_key(&session_id));
    }

    #[tokio::test]
    async fn tick_waits_when_delete_after_discard_fails() {
        let requester = Arc::new(MockProofRequester::default());
        requester.reject_delete.store(true, std::sync::atomic::Ordering::SeqCst);
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_root);
        let proof_request = ProofRequest {
            claimed_l2_output_root: claimed_root,
            claimed_l2_block_number: target_block,
            intermediate_block_interval: BLOCK_INTERVAL,
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
        let restart = collector
            .tick(&mut current, target_block, target_block, &CancellationToken::new())
            .await;

        assert!(!restart);
        assert!(requester.requests.lock().unwrap().contains_key(&session_id));
    }

    #[tokio::test]
    async fn root_mismatch_restarts_even_when_delete_fails() {
        let requester = Arc::new(MockProofRequester::default());
        requester.reject_delete.store(true, std::sync::atomic::Ordering::SeqCst);
        let target_block = 200;
        let stale_root = B256::repeat_byte(0xaa);
        let fresh_root = B256::repeat_byte(0xbb);
        let session_id = ProposerProofAdapter::tee_session_id_for_root(stale_root);
        let proof_request = ProofRequest {
            claimed_l2_output_root: stale_root,
            claimed_l2_block_number: target_block,
            intermediate_block_interval: BLOCK_INTERVAL,
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
                target_block,
                &session_id,
                aggregate_proposal,
                vec![],
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
}
