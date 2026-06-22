//! Polls and submits prover-service proofs for proposer TEE checkpoints.

use std::{ops::ControlFlow, sync::Arc, time::Duration};

use alloy_primitives::B256;
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
};

/// Mutable collector-side state.
#[derive(Debug, Default)]
pub struct ProofCollectorState {
    /// Latest block the collector has submitted through.
    pub cursor: Option<RecoveredState>,
    /// Cached onchain recovery state used by the collector loop.
    pub cache: Option<ProofRecoveryCache>,
}

/// Owns proof polling and sequential submission.
#[derive(Debug)]
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

#[derive(Debug)]
enum TargetPoll {
    Ready { session_id: String, proof: ProofResult },
    Failed { session_id: String, error: ProposerError },
    Pending,
    Transient,
    NotFound,
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

    async fn poll_target(&self, target_block: u64, claimed_l2_output_root: B256) -> TargetPoll {
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_l2_output_root);
        let response = match self
            .proof_requester
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
                warn!(
                    target_block,
                    session_id = %session_id,
                    error = %e,
                    "Transient failure polling prover service",
                );
                return TargetPoll::Transient;
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
                TargetPoll::Failed { session_id, error: ProposerError::Prover(message) }
            }
            ProofStatus::Succeeded => {
                let result = match response.result {
                    Some(result) => result,
                    None => {
                        let error = ProposerError::Prover(format!(
                            "proof session {session_id} succeeded without a result"
                        ));
                        return TargetPoll::Failed { session_id, error };
                    }
                };
                match ProposerProofAdapter::tee_proof_result(result) {
                    Ok(proof) => TargetPoll::Ready { session_id, proof },
                    Err(error) => TargetPoll::Failed { session_id, error },
                }
            }
        }
    }

    /// Runs one collector tick from the supplied recovered state and safe head.
    pub async fn tick(
        &self,
        state: &mut ProofCollectorState,
        recovered: RecoveredState,
        safe_head: u64,
        cancel: &CancellationToken,
    ) -> bool {
        if state.cursor != Some(recovered) {
            state.cursor = Some(recovered);
        }

        let restart = loop {
            let Some(current) = state.cursor else {
                break false;
            };

            if cancel.is_cancelled() {
                break false;
            }

            let Some(target_block) =
                Self::next_target_block(current.l2_block_number, self.block_interval)
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

            let Some(claimed_l2_output_root) = self.canonical_output_root(target_block).await
            else {
                break false;
            };

            match self.poll_target(target_block, claimed_l2_output_root).await {
                TargetPoll::Ready { session_id, proof } => {
                    info!(target_block, session_id = %session_id, "Proof ready, submitting inline");
                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_READY).increment(1);
                    Metrics::last_collected_block().set(target_block as f64);

                    match self
                        .submit_inline(target_block, &current, proof, &mut state.cache, cancel)
                        .await
                    {
                        ControlFlow::Continue(recovered) => {
                            state.cursor = Some(recovered);
                            if recovered.l2_block_number > current.l2_block_number {
                                continue;
                            }
                            break false;
                        }
                        ControlFlow::Break(()) => break true,
                    }
                }
                TargetPoll::Pending | TargetPoll::Transient => break false,
                TargetPoll::NotFound => {
                    info!(target_block, "Proof missing, restarting pipeline to request it");
                    break true;
                }
                TargetPoll::Failed { session_id, error } => {
                    warn!(
                        target_block,
                        session_id = %session_id,
                        error = %error,
                        "Proof session failed, restarting pipeline to request it again"
                    );
                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED)
                        .increment(1);
                    Metrics::errors_total(error.metric_label()).increment(1);
                    Metrics::proof_retries_total().increment(1);
                    break true;
                }
            }
        };

        Metrics::pipeline_retries().set(0.0);
        restart
    }

    async fn submit_inline(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        proof: ProofResult,
        cache: &mut Option<ProofRecoveryCache>,
        cancel: &CancellationToken,
    ) -> ControlFlow<(), RecoveredState> {
        let parent_address = recovered.parent_address;
        info!(target_block, parent_address = %parent_address, "Submitting proof inline");

        let mut submit_timer = base_metrics::timed!(Metrics::proposal_total_duration_seconds());
        let submit = async {
            match self.submit_timeout {
                Some(timeout) => {
                    tokio::time::timeout(
                        timeout,
                        self.submitter.submit(&proof, target_block, parent_address),
                    )
                    .await
                }
                None => Ok(self.submitter.submit(&proof, target_block, parent_address).await),
            }
        };
        let Some(result) = cancel.run_until_cancelled(submit).await else {
            submit_timer.disarm();
            warn!(target_block, "Inline submit cancelled, restarting pipeline session");
            return ControlFlow::Break(());
        };

        match result {
            Err(_) => {
                submit_timer.disarm();
                Metrics::submit_timeouts_total().increment(1);
                warn!(
                    target_block,
                    timeout_secs = ?self.submit_timeout.map(|timeout| timeout.as_secs()),
                    "Inline submit timed out, restarting pipeline session"
                );
                ControlFlow::Break(())
            }
            Ok(Err(SubmitAction::RootMismatch)) => {
                submit_timer.disarm();
                warn!(target_block, "Output root mismatch at submit time, restarting pipeline");
                Metrics::root_mismatch_total().increment(1);
                *cache = None;
                ControlFlow::Break(())
            }
            Ok(Err(SubmitAction::Failed(error))) => {
                submit_timer.disarm();
                Metrics::errors_total(error.metric_label()).increment(1);
                if matches!(
                    error,
                    ProposerError::Submission(ProofSubmissionError::InvalidParentGame)
                ) {
                    warn!(
                        target_block,
                        error = %error,
                        "Submission rejected: parent game invalid, restarting pipeline"
                    );
                    *cache = None;
                } else {
                    warn!(target_block, error = %error, "Submission failed, restarting pipeline");
                }
                ControlFlow::Break(())
            }
            Ok(Err(SubmitAction::Discard(error))) => {
                submit_timer.disarm();
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    target_block,
                    error = %error,
                    "Proof discarded by submitter, restarting pipeline to request it again"
                );
                ControlFlow::Break(())
            }
            Ok(action @ (Ok(()) | Err(SubmitAction::GameAlreadyExists))) => {
                if matches!(action, Err(SubmitAction::GameAlreadyExists)) {
                    submit_timer.disarm();
                    info!(target_block, "Game already exists onchain");
                    if let Some(cached) = cache.as_mut() {
                        cached.game_count = cached.game_count.saturating_sub(1);
                    }
                } else {
                    drop(submit_timer);
                    info!(target_block, "Submission successful");
                }
                Metrics::last_proposed_block().set(target_block as f64);
                match self.recovery.recover_latest_state(cache).await {
                    Ok(recovered) => ControlFlow::Continue(recovered),
                    Err(e) => {
                        warn!(error = %e, "Failed to recover state after inline submit");
                        ControlFlow::Break(())
                    }
                }
            }
        }
    }

    async fn canonical_output_root(&self, target_block: u64) -> Option<B256> {
        match self.rollup_client.output_at_block(target_block).await {
            Ok(output) => Some(output.output_root),
            Err(e) => {
                warn!(
                    target_block,
                    error = %e,
                    "Failed to fetch canonical output root for collection target"
                );
                None
            }
        }
    }

    fn next_target_block(current_block: u64, block_interval: u64) -> Option<u64> {
        if block_interval == 0 {
            error!("Block interval must be non-zero");
            return None;
        }

        current_block.checked_add(block_interval).map_or_else(
            || {
                error!(current_block, block_interval, "Overflow computing next target block");
                None
            },
            Some,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::{Address, B256};
    use async_trait::async_trait;
    use base_proof_primitives::{ProofRequest, Proposal};

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

    #[derive(Debug)]
    struct DiscardingOutputProposer;

    #[async_trait]
    impl OutputProposer for DiscardingOutputProposer {
        async fn propose_output(
            &self,
            _proposal: &Proposal,
            _parent_address: Address,
            _intermediate_roots: &[B256],
        ) -> Result<(), ProposerError> {
            Err(ProposerError::Submission(ProofSubmissionError::L1OriginTooOld))
        }

        async fn verify_proposal_proof(
            &self,
            _game_address: Address,
            _proposal: &Proposal,
        ) -> Result<(), ProposerError> {
            Err(ProposerError::Submission(ProofSubmissionError::L1OriginTooOld))
        }
    }

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
            output_roots: root.map_or_else(HashMap::new, |root| HashMap::from([(block, root)])),
            max_safe_block: None,
        })
    }

    fn failed_session_requester(claimed_root: B256) -> Arc<MockProofRequester> {
        let requester = Arc::new(MockProofRequester::default());
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_root);
        requester
            .failed_sessions
            .lock()
            .unwrap()
            .insert(session_id, "simulated proof failure".to_owned());
        requester
    }

    fn proof_request(
        target_block: u64,
        claimed_l2_output_root: B256,
        intermediate_block_interval: u64,
        l1_head_number: u64,
    ) -> ProofRequest {
        ProofRequest {
            l1_head: B256::repeat_byte(0x01),
            agreed_l2_head_hash: B256::repeat_byte(0x02),
            agreed_l2_output_root: B256::ZERO,
            claimed_l2_output_root,
            claimed_l2_block_number: target_block,
            proposer: Address::repeat_byte(0x04),
            intermediate_block_interval,
            l1_head_number,
            image_hash: B256::repeat_byte(0x05),
        }
    }

    struct OrchestratorParts {
        proof_requester: Arc<dyn ProofRequesterProvider>,
        l1: Arc<MockL1>,
        rollup_client: Arc<MockRollupClient>,
        output_proposer: Arc<dyn OutputProposer>,
        intermediate_block_interval: u64,
        factory_client: MockDisputeGameFactory,
    }

    const BLOCK_INTERVAL: u64 = 100;

    impl Default for OrchestratorParts {
        fn default() -> Self {
            Self {
                proof_requester: Arc::new(MockProofRequester::default()),
                l1: Arc::new(MockL1 { latest_block_number: 1000, ..Default::default() }),
                rollup_client: rollup_client(0, None),
                output_proposer: Arc::new(MockOutputProposer),
                intermediate_block_interval: 100,
                factory_client: MockDisputeGameFactory::with_games(vec![]),
            }
        }
    }

    fn make_orchestrator(parts: OrchestratorParts) -> ProofCollector<MockL1, MockRollupClient> {
        let recovery = Arc::new(ProofRecovery::new(
            ProofRecoveryConfig {
                block_interval: BLOCK_INTERVAL,
                intermediate_block_interval: parts.intermediate_block_interval,
                game_type: 0,
                allow_non_finalized: false,
                anchor_state_registry_address: Address::ZERO,
                scan_concurrency: 1,
            },
            Arc::clone(&parts.rollup_client),
            Arc::new(MockAnchorStateRegistry {
                anchor_root: test_anchor_root(0),
                anchor_game: Address::ZERO,
            }),
            Arc::new(parts.factory_client),
        ));
        let submitter = ProofSubmitter::new(
            parts.output_proposer,
            Arc::clone(&parts.rollup_client),
            parts.l1,
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            Arc::new(MockAggregateVerifier::default()),
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                game_type: 0,
                block_interval: BLOCK_INTERVAL,
                intermediate_block_interval: parts.intermediate_block_interval,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );

        ProofCollector::new(
            parts.proof_requester,
            parts.rollup_client,
            submitter,
            recovery,
            BLOCK_INTERVAL,
            Some(std::time::Duration::from_secs(60)),
        )
    }

    #[tokio::test]
    async fn tick_resets_cursor_when_recovery_rewinds() {
        let orchestrator = make_orchestrator(Default::default());
        let mut state = ProofCollectorState { cursor: Some(recovered(500)), cache: cache() };
        let cancel = CancellationToken::new();

        let result = orchestrator.tick(&mut state, recovered(100), 200, &cancel).await;

        assert!(result);
        assert_eq!(state.cursor, Some(recovered(100)));
    }

    #[tokio::test]
    async fn submit_inline_restarts_when_post_submit_recovery_fails() {
        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.games_should_fail = true;
        let orchestrator = make_orchestrator(OrchestratorParts {
            rollup_client: rollup_client(200, None),
            factory_client: factory,
            ..Default::default()
        });
        let proposal = test_proposal(200);
        let proof =
            ProofResult::Tee { aggregate_proposal: proposal.clone(), proposals: vec![proposal] };
        let mut cache = cache();
        let cancel = CancellationToken::new();

        let effect =
            orchestrator.submit_inline(200, &recovered(100), proof, &mut cache, &cancel).await;

        assert_eq!(effect, ControlFlow::Break(()));
    }

    #[tokio::test]
    async fn missing_session_restarts_pipeline_without_dispatching() {
        let requester = Arc::new(MockProofRequester::default());
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: requester.clone(),
            rollup_client: rollup_client(target_block, Some(claimed_root)),
            ..Default::default()
        });
        let mut state = ProofCollectorState::default();
        state.cache = cache();
        let cancel = CancellationToken::new();

        let result = orchestrator.tick(&mut state, recovered(100), target_block, &cancel).await;

        assert!(result);
        assert!(requester.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_session_restarts_pipeline_without_dispatching() {
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let requester = failed_session_requester(claimed_root);
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: requester.clone(),
            rollup_client: rollup_client(target_block, Some(claimed_root)),
            ..Default::default()
        });
        let mut state = ProofCollectorState::default();
        state.cache = cache();
        let cancel = CancellationToken::new();

        let result = orchestrator.tick(&mut state, recovered(100), target_block, &cancel).await;

        assert!(result);
        assert!(requester.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn discarded_proof_restarts_pipeline_without_dispatching() {
        let requester = Arc::new(MockProofRequester::default());
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(proof_request(
                target_block,
                claimed_root,
                100,
                1000,
            )))
            .await
            .expect("test setup should dispatch root session");
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: requester.clone(),
            rollup_client: rollup_client(target_block, Some(claimed_root)),
            output_proposer: Arc::new(DiscardingOutputProposer),
            ..Default::default()
        });
        let mut state = ProofCollectorState::default();
        state.cache = cache();
        let cancel = CancellationToken::new();

        let result = orchestrator.tick(&mut state, recovered(100), target_block, &cancel).await;

        assert!(result);
        assert_eq!(requester.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn poll_recovers_in_flight_session_across_restart() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let target_block = 600u64;
        let canonical_root = B256::repeat_byte(0xcc);
        let rollup_client = rollup_client(target_block, Some(canonical_root));
        let proof_request = ProofRequest {
            agreed_l2_output_root: B256::repeat_byte(0x03),
            ..proof_request(target_block, canonical_root, 300, 1200)
        };
        let expected_session_id = proof_requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(proof_request))
            .await
            .unwrap()
            .session_id;
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester,
            l1: Arc::new(MockL1 { latest_block_number: 1200, ..Default::default() }),
            rollup_client,
            intermediate_block_interval: 300,
            ..Default::default()
        });

        match orchestrator.poll_target(target_block, canonical_root).await {
            TargetPoll::Ready { session_id, .. } => {
                assert_eq!(session_id, expected_session_id);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_returns_not_found_for_unknown_session() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let target_block = 200u64;
        let rollup_client = rollup_client(target_block, Some(B256::repeat_byte(0xaa)));
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester,
            l1: Arc::new(MockL1 { latest_block_number: 1200, ..Default::default() }),
            rollup_client,
            intermediate_block_interval: 300,
            ..Default::default()
        });

        match orchestrator.poll_target(target_block, B256::repeat_byte(0xaa)).await {
            TargetPoll::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
