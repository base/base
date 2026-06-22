//! Polls and submits prover-service proofs for proposer TEE checkpoints.

use std::{collections::HashMap, sync::Arc, time::Duration};

use base_proof_primitives::ProofResult;
use base_proof_rpc::{L1Provider, RollupProvider};
use base_proof_submission::ProofSubmissionError;
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{GetProofRequest, ProofStatus};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    driver::RecoveredState,
    error::ProposerError,
    metrics::Metrics,
    proof_adapter::ProposerProofAdapter,
    proof_recovery::{ProofRecovery, ProofRecoveryCache},
    proof_submitter::{ProofSubmitter, SubmitAction},
    proof_target::ProofTarget,
};

/// Owns proof polling and sequential submission.
pub struct ProofCollector<L1, R>
where
    L1: L1Provider,
    R: RollupProvider,
{
    proof_requester: Arc<dyn ProofRequesterProvider>,
    rollup_client: Arc<R>,
    submitter: ProofSubmitter<L1, R>,
    recovery: Arc<ProofRecovery<R>>,
    block_interval: u64,
    submit_timeout: Option<Duration>,
}

/// Mutable collector-side retry state.
#[derive(Debug, Default)]
pub struct ProofCollectorState {
    /// Per-target terminal proof failure counts.
    pub retry_counts: HashMap<u64, u32>,
}

#[derive(Debug)]
enum TargetPoll {
    Ready(ProofResult),
    Failed(ProposerError),
    Pending,
    NotFound,
}

impl<L1, R> std::fmt::Debug for ProofCollector<L1, R>
where
    L1: L1Provider,
    R: RollupProvider,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofCollector")
            .field("block_interval", &self.block_interval)
            .field("submit_timeout", &self.submit_timeout)
            .finish_non_exhaustive()
    }
}

impl<L1, R> ProofCollector<L1, R>
where
    L1: L1Provider + 'static,
    R: RollupProvider + 'static,
{
    /// Creates a collector orchestrator from low-level proof components.
    pub const fn new(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<R>,
        submitter: ProofSubmitter<L1, R>,
        recovery: Arc<ProofRecovery<R>>,
        block_interval: u64,
        submit_timeout: Option<Duration>,
    ) -> Self {
        Self { proof_requester, rollup_client, submitter, recovery, block_interval, submit_timeout }
    }

    async fn poll_target(
        proof_requester: &dyn ProofRequesterProvider,
        target_block: u64,
        claimed_l2_output_root: alloy_primitives::B256,
    ) -> TargetPoll {
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_l2_output_root);
        let response = match proof_requester
            .get_proof(GetProofRequest { session_id: session_id.clone() })
            .await
        {
            Ok(response) => response,
            Err(e) if e.is_not_found() => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    "Prover-service session missing for target",
                );
                return TargetPoll::NotFound;
            }
            Err(e) => {
                Metrics::errors_total("prover").increment(1);
                warn!(
                    target_block,
                    session_id = %session_id,
                    error = %e,
                    "Transient failure polling prover service",
                );
                return TargetPoll::Pending;
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
                    "Proof request still pending",
                );
                TargetPoll::Pending
            }
            ProofStatus::Failed => {
                let message = response.error_message.unwrap_or_else(|| {
                    format!("proof session {session_id} failed without an error message")
                });
                let error = ProposerError::Prover(message);
                warn!(
                    target_block,
                    session_id = %session_id,
                    error = %error,
                    "Proof session failed"
                );
                TargetPoll::Failed(error)
            }
            ProofStatus::Succeeded => {
                let result = match response.result {
                    Some(result) => result,
                    None => {
                        let error = ProposerError::Prover(format!(
                            "proof session {session_id} succeeded without a result"
                        ));
                        warn!(
                            target_block,
                            session_id = %session_id,
                            error = %error,
                            "Proof session returned no result"
                        );
                        return TargetPoll::Failed(error);
                    }
                };
                match ProposerProofAdapter::tee_proof_result(result) {
                    Ok(proof) => {
                        info!(target_block, session_id = %session_id, "Proof request succeeded");
                        TargetPoll::Ready(proof)
                    }
                    Err(error) => {
                        warn!(
                            target_block,
                            session_id = %session_id,
                            error = %error,
                            "Proof result rejected"
                        );
                        TargetPoll::Failed(error)
                    }
                }
            }
        }
    }

    /// Runs one collector tick from the supplied recovered state and safe head.
    pub async fn tick(
        &self,
        state: &mut ProofCollectorState,
        cache: &mut Option<ProofRecoveryCache>,
        recovered: RecoveredState,
        safe_head: u64,
        max_retries: u32,
        cancel: &CancellationToken,
    ) -> bool {
        let mut current = recovered;
        state.retry_counts.retain(|&target, _| target > current.l2_block_number);

        let restart = loop {
            if cancel.is_cancelled() {
                break false;
            }

            let Some(target_block) =
                ProofTarget::next_block(current.l2_block_number, self.block_interval)
            else {
                break false;
            };

            if target_block > safe_head {
                debug!(
                    current_block = current.l2_block_number,
                    target_block,
                    safe_head,
                    "Safe head below collection target, waiting for L2 head to advance"
                );
                break false;
            }

            let exhausted_attempts = state
                .retry_counts
                .get(&target_block)
                .copied()
                .filter(|count| *count >= max_retries);
            if let Some(attempts) = exhausted_attempts {
                debug!(target_block, attempts, "Collector retry budget already exhausted");
                break false;
            }

            let Some(claimed_l2_output_root) = ProofTarget::canonical_output_root(
                self.rollup_client.as_ref(),
                target_block,
                "collector",
            )
            .await
            else {
                break false;
            };

            match Self::poll_target(
                self.proof_requester.as_ref(),
                target_block,
                claimed_l2_output_root,
            )
            .await
            {
                TargetPoll::Ready(proof) => {
                    info!(target_block, "Proof ready, submitting inline");
                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_READY).increment(1);
                    Metrics::last_collected_block().set(target_block as f64);

                    match self.submit_inline(target_block, &current, proof, cache, cancel).await {
                        Some(recovered) => {
                            state.retry_counts.remove(&target_block);
                            if recovered.l2_block_number > current.l2_block_number {
                                current = recovered;
                                continue;
                            }
                            break false;
                        }
                        None if cancel.is_cancelled() => break false,
                        None => {
                            let count = state.retry_counts.entry(target_block).or_insert(0);
                            if *count >= max_retries {
                                debug!(
                                    target_block,
                                    attempts = *count,
                                    "Inline submission retry budget already exhausted"
                                );
                                break false;
                            }

                            *count = count.saturating_add(1);
                            if *count >= max_retries {
                                error!(
                                    target_block,
                                    attempts = *count,
                                    "Inline submission failed after max retries"
                                );
                                break false;
                            }

                            Metrics::proof_retries_total().increment(1);
                            warn!(
                                target_block,
                                attempt = *count,
                                "Inline submission failed, restarting pipeline"
                            );
                            break true;
                        }
                    }
                }
                TargetPoll::Pending => break false,
                TargetPoll::NotFound => {
                    state.retry_counts.remove(&target_block);
                    info!(target_block, "Proof missing, restarting pipeline to request it");
                    break true;
                }
                TargetPoll::Failed(error) => {
                    let count = state.retry_counts.entry(target_block).or_insert(0);
                    if *count >= max_retries {
                        debug!(
                            target_block,
                            attempts = *count,
                            "Proof collection retry budget already exhausted"
                        );
                        break false;
                    }

                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED)
                        .increment(1);
                    Metrics::errors_total(error.metric_label()).increment(1);
                    *count = count.saturating_add(1);
                    if *count >= max_retries {
                        error!(
                            target_block,
                            attempts = *count,
                            error = %error,
                            "Proof collection failed after max retries"
                        );
                        break false;
                    }

                    Metrics::proof_retries_total().increment(1);
                    warn!(
                        target_block,
                        attempt = *count,
                        error = %error,
                        "Proof collection failed, restarting pipeline to re-dispatch"
                    );
                    break true;
                }
            }
        };

        restart
    }

    async fn submit_inline(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        proof: ProofResult,
        cache: &mut Option<ProofRecoveryCache>,
        cancel: &CancellationToken,
    ) -> Option<RecoveredState> {
        let parent_address = recovered.parent_address;
        info!(target_block, parent_address = %parent_address, "Submitting proof inline");

        let mut submit_timer = base_metrics::timed!(Metrics::proposal_total_duration_seconds());
        let result = match cancel
            .run_until_cancelled(async {
                let submit = self.submitter.submit(&proof, target_block, parent_address);
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
                    "Inline submit timed out, restarting pipeline session"
                );
                return None;
            }
            None => {
                submit_timer.disarm();
                warn!(target_block, "Inline submit cancelled, restarting pipeline session");
                return None;
            }
        };

        if !matches!(result, Ok(())) {
            submit_timer.disarm();
        }

        match result {
            Err(SubmitAction::RootMismatch) => {
                warn!(target_block, "Output root mismatch at submit time, restarting pipeline");
                Metrics::root_mismatch_total().increment(1);
                *cache = None;
                return None;
            }
            Err(SubmitAction::Failed(error)) => {
                Metrics::errors_total(error.metric_label()).increment(1);
                let invalid_parent_game = matches!(
                    error,
                    ProposerError::Submission(ProofSubmissionError::InvalidParentGame)
                );
                warn!(
                    target_block,
                    error = %error,
                    invalid_parent_game,
                    "Submission failed, restarting pipeline"
                );
                if invalid_parent_game {
                    *cache = None;
                }
                return None;
            }
            Err(SubmitAction::Discard(error)) => {
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    target_block,
                    error = %error,
                    "Proof discarded by submitter, restarting pipeline to request it again"
                );
                return None;
            }
            Ok(()) => {
                drop(submit_timer);
                info!(target_block, "Submission successful");
            }
            Err(SubmitAction::GameAlreadyExists) => {
                info!(target_block, "Game already exists onchain");
                if let Some(cached) = cache.as_mut() {
                    cached.game_count = cached.game_count.saturating_sub(1);
                }
            }
        }

        Metrics::last_proposed_block().set(target_block as f64);
        match self.recovery.recover_latest_state(cache).await {
            Ok(recovered) => Some(recovered),
            Err(e) => {
                warn!(error = %e, "Failed to recover state after inline submit");
                None
            }
        }
    }
}

impl ProofCollectorState {
    /// Creates empty collector state.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::{Address, B256};
    use base_proof_primitives::ProofRequest;

    use super::*;
    use crate::{
        output_proposer::OutputProposer,
        proof_adapter::ProposerProofAdapter,
        proof_recovery::ProofRecoveryConfig,
        proof_submitter::{ProofSubmitter, ProofSubmitterConfig},
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1,
            MockOutputProposer, MockProofRequester, MockRollupClient, test_anchor_root,
            test_proposal, test_sync_status,
        },
    };

    fn recovered(block: u64) -> RecoveredState {
        RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::ZERO,
            l2_block_number: block,
        }
    }

    fn cache() -> Option<ProofRecoveryCache> {
        Some(ProofRecoveryCache { game_count: 0, state: recovered(100) })
    }

    fn rollup_client(block: u64, root: Option<B256>) -> Arc<MockRollupClient> {
        Arc::new(MockRollupClient {
            sync_status: test_sync_status(block, B256::ZERO),
            output_roots: root.into_iter().map(|root| (block, root)).collect(),
            max_safe_block: None,
        })
    }

    const BLOCK_INTERVAL: u64 = 100;

    fn make_orchestrator(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<MockRollupClient>,
        output_proposer: Arc<dyn OutputProposer>,
        factory_client: MockDisputeGameFactory,
    ) -> ProofCollector<MockL1, MockRollupClient> {
        let recovery = Arc::new(ProofRecovery::new(
            ProofRecoveryConfig {
                block_interval: BLOCK_INTERVAL,
                intermediate_block_interval: BLOCK_INTERVAL,
                game_type: 0,
                allow_non_finalized: false,
                anchor_state_registry_address: Address::ZERO,
                scan_concurrency: 1,
            },
            Arc::clone(&rollup_client),
            Arc::new(MockAnchorStateRegistry {
                anchor_root: test_anchor_root(0),
                anchor_game: Address::ZERO,
            }),
            Arc::new(factory_client),
        ));
        let submitter = ProofSubmitter::new(
            output_proposer,
            Arc::clone(&rollup_client),
            Arc::new(MockL1 { latest_block_number: 1000, ..Default::default() }),
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            Arc::new(MockAggregateVerifier::default()),
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                game_type: 0,
                block_interval: BLOCK_INTERVAL,
                intermediate_block_interval: BLOCK_INTERVAL,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );

        ProofCollector::new(
            proof_requester,
            rollup_client,
            submitter,
            recovery,
            BLOCK_INTERVAL,
            Some(std::time::Duration::from_secs(60)),
        )
    }

    #[tokio::test]
    async fn submit_inline_restarts_when_post_submit_recovery_fails() {
        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.games_should_fail = true;
        let orchestrator = make_orchestrator(
            Arc::new(MockProofRequester::default()),
            rollup_client(200, None),
            Arc::new(MockOutputProposer::default()),
            factory,
        );
        let proposal = test_proposal(200);
        let proof =
            ProofResult::Tee { aggregate_proposal: proposal.clone(), proposals: vec![proposal] };
        let mut cache = cache();
        let cancel = CancellationToken::new();

        assert!(
            orchestrator
                .submit_inline(200, &recovered(100), proof, &mut cache, &cancel)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn discarded_proof_counts_against_retry_budget() {
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
        let orchestrator = make_orchestrator(
            requester,
            rollup_client(target_block, Some(claimed_root)),
            Arc::new(MockOutputProposer::with_create_error(ProposerError::Submission(
                ProofSubmissionError::L1OriginTooOld,
            ))),
            MockDisputeGameFactory::with_games(vec![]),
        );
        let mut cache = cache();
        let mut state = ProofCollectorState::new();
        let cancel = CancellationToken::new();

        let result = orchestrator
            .tick(&mut state, &mut cache, recovered(100), target_block, 1, &cancel)
            .await;

        assert!(!result);
        assert_eq!(state.retry_counts, HashMap::from([(target_block, 1)]));
    }

    #[tokio::test]
    async fn failed_proof_restarts_until_retry_budget_is_exhausted() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let target_block = 200u64;
        let canonical_root = B256::repeat_byte(0xcc);
        let session_id = ProposerProofAdapter::tee_session_id_for_root(canonical_root);
        proof_requester
            .failed_sessions
            .lock()
            .unwrap()
            .insert(session_id, "simulated proof failure".to_owned());
        let orchestrator = make_orchestrator(
            proof_requester,
            rollup_client(target_block, Some(canonical_root)),
            Arc::new(MockOutputProposer::default()),
            MockDisputeGameFactory::with_games(vec![]),
        );
        let mut cache = cache();
        let mut state = ProofCollectorState::new();
        let cancel = CancellationToken::new();

        assert!(
            orchestrator
                .tick(&mut state, &mut cache, recovered(100), target_block, 2, &cancel)
                .await
        );
        assert!(
            !orchestrator
                .tick(&mut state, &mut cache, recovered(100), target_block, 2, &cancel)
                .await
        );
        assert_eq!(state.retry_counts, HashMap::from([(target_block, 2)]));
        assert!(
            !orchestrator
                .tick(&mut state, &mut cache, recovered(100), target_block, 2, &cancel)
                .await
        );
        assert_eq!(state.retry_counts, HashMap::from([(target_block, 2)]));
    }

    #[tokio::test]
    async fn poll_recovers_in_flight_session_across_restart() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let target_block = 600u64;
        let canonical_root = B256::repeat_byte(0xcc);
        let proof_request = ProofRequest {
            agreed_l2_output_root: B256::repeat_byte(0x03),
            claimed_l2_output_root: canonical_root,
            claimed_l2_block_number: target_block,
            intermediate_block_interval: 300,
            l1_head_number: 1200,
            ..Default::default()
        };
        proof_requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(proof_request))
            .await
            .unwrap();

        let poll = ProofCollector::<MockL1, MockRollupClient>::poll_target(
            proof_requester.as_ref(),
            target_block,
            canonical_root,
        )
        .await;
        let TargetPoll::Ready(_) = poll else {
            panic!("expected Ready, got {poll:?}");
        };
    }

    #[tokio::test]
    async fn poll_returns_not_found_for_unknown_session() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let target_block = 200u64;

        let poll = ProofCollector::<MockL1, MockRollupClient>::poll_target(
            proof_requester.as_ref(),
            target_block,
            B256::repeat_byte(0xaa),
        )
        .await;

        assert!(matches!(&poll, TargetPoll::NotFound), "expected NotFound, got {poll:?}");
    }
}
