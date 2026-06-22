//! Polls and orchestrates prover-service collection for proposer TEE proofs.

use std::{ops::ControlFlow, sync::Arc, time::Duration};

use alloy_primitives::B256;
use base_proof_primitives::ProofResult;
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
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
    proof_dispatcher::{ProofDispatchAttempt, ProofDispatcher},
    proof_recovery::{ProofRecovery, ProofRecoveryCache},
    proof_submitter::{ProofSubmitter, SubmitAction},
};

/// Mutable collector-side orchestration state.
#[derive(Debug, Default)]
pub struct ProofCollectorState {
    /// Recovered chain state that the current cursor was derived from.
    pub recovered: Option<RecoveredState>,
    /// Latest block the collector has submitted through.
    pub cursor: Option<RecoveredState>,
    /// Retry bookkeeping for the current target.
    pub target: Option<(u64, ProofCollectorTargetState)>,
}

/// Collector retry state for a single target block.
#[derive(Debug, Default)]
pub struct ProofCollectorTargetState {
    /// Proof polling and dispatch transport retry count.
    pub retry_count: u32,
    /// Accepted discard-specific proof retry count.
    pub discard_retry_count: u32,
    /// Active retry-specific prover-service session.
    pub retry_session: Option<String>,
    /// Root waiting for a retry-specific dispatch.
    pub pending_discard_root: Option<B256>,
}

/// Owns collector-side polling, sequential submit, and retry-session orchestration.
#[derive(Debug)]
pub struct ProofCollectorOrchestrator<L1, L2, R>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
{
    proof_requester: Arc<dyn ProofRequesterProvider>,
    dispatcher: ProofDispatcher<L1, L2, R>,
    submitter: ProofSubmitter<L1, R>,
    recovery: Arc<ProofRecovery<R>>,
    block_interval: u64,
    max_retries: u32,
    submit_timeout: Option<Duration>,
}

/// Outcome of polling the prover service for a single target block.
///
/// Used by the proposer's collector pipeline to choose the next action:
/// submit, dispatch, wait, or retry.
#[derive(Debug)]
enum TargetPoll {
    Ready { session_id: String, proof: ProofResult },
    Failed { session_id: String, error: ProposerError },
    Pending,
    Transient,
    NotFound,
}

impl ProofCollectorState {
    /// Records a proof failure and returns whether retrying is still allowed.
    #[must_use]
    pub fn handle_proof_failure(
        &mut self,
        target: u64,
        error: ProposerError,
        max_retries: u32,
        cache: &mut Option<ProofRecoveryCache>,
    ) -> bool {
        Metrics::errors_total(error.metric_label()).increment(1);
        Metrics::proof_retries_total().increment(1);

        let attempts = {
            let state = self.target_for(target);
            state.retry_count += 1;
            state.retry_count
        };
        if attempts >= max_retries {
            error!(
                target_block = target,
                attempts,
                error = %error,
                "Proof failed after max retries, dropping cached recovery"
            );
            let _ = self.target.take_if(|(block, _)| *block == target);
            *cache = None;
            false
        } else {
            warn!(
                target_block = target,
                attempt = attempts,
                error = %error,
                "Proof failed, re-dispatching"
            );
            true
        }
    }

    /// Returns target state for `target`, replacing stale target state if needed.
    pub fn target_for(&mut self, target: u64) -> &mut ProofCollectorTargetState {
        if self.target.as_ref().is_none_or(|(block, _)| *block != target) {
            self.target = Some((target, ProofCollectorTargetState::default()));
        }
        &mut self.target.as_mut().expect("target set").1
    }

    /// Returns target state for `target` when present.
    pub fn target(&self, target: u64) -> Option<&ProofCollectorTargetState> {
        self.target.as_ref().and_then(|(block, state)| (*block == target).then_some(state))
    }
}

impl<L1, L2, R> ProofCollectorOrchestrator<L1, L2, R>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
{
    /// Creates a collector orchestrator from low-level proof components.
    pub const fn new(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        dispatcher: ProofDispatcher<L1, L2, R>,
        submitter: ProofSubmitter<L1, R>,
        recovery: Arc<ProofRecovery<R>>,
        block_interval: u64,
        max_retries: u32,
        submit_timeout: Option<Duration>,
    ) -> Self {
        Self {
            proof_requester,
            dispatcher,
            submitter,
            recovery,
            block_interval,
            max_retries,
            submit_timeout,
        }
    }

    async fn poll_target(
        &self,
        target_block: u64,
        claimed_l2_output_root: B256,
        session_id: Option<String>,
    ) -> TargetPoll {
        let session_id = session_id.unwrap_or_else(|| {
            ProposerProofAdapter::tee_session_id_for_root(claimed_l2_output_root)
        });
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
        cache: &mut Option<ProofRecoveryCache>,
        recovered: RecoveredState,
        safe_head: u64,
        cancel: &CancellationToken,
    ) -> bool {
        let _ = state.target.take_if(|(target, _)| *target <= recovered.l2_block_number);

        if state.recovered != Some(recovered) || state.cursor.is_none() {
            state.recovered = Some(recovered);
            state.cursor = Some(recovered);
        }

        let restart = loop {
            let Some(current) = state.cursor else {
                break false;
            };

            if cancel.is_cancelled() {
                break false;
            }

            let Some(target_block) = ProofDispatcher::<L1, L2, R>::next_target_block(
                current.l2_block_number,
                self.block_interval,
            ) else {
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

            if let Some(claimed_l2_output_root) =
                state.target(target_block).and_then(|target| target.pending_discard_root)
            {
                break self
                    .dispatch_discard_retry(
                        target_block,
                        &current,
                        claimed_l2_output_root,
                        state,
                        cache,
                        true,
                    )
                    .await;
            }

            let retry_session =
                state.target(target_block).and_then(|target| target.retry_session.clone());
            let Some(claimed_l2_output_root) =
                self.dispatcher.canonical_output_root(target_block).await
            else {
                debug!(
                    target_block,
                    session_id = ?retry_session,
                    "Waiting for canonical output root"
                );
                break false;
            };
            let poll = self.poll_target(target_block, claimed_l2_output_root, retry_session).await;

            match poll {
                TargetPoll::Ready { session_id, proof } => {
                    info!(target_block, session_id = %session_id, "Proof ready, submitting inline");
                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_READY).increment(1);
                    Metrics::last_collected_block().set(target_block as f64);
                    match self
                        .submit_inline(target_block, &current, proof, state, cache, cancel)
                        .await
                    {
                        ControlFlow::Continue(recovered) => {
                            state.cursor = Some(recovered);
                            if recovered.l2_block_number > current.l2_block_number {
                                continue;
                            }
                            break false;
                        }
                        ControlFlow::Break(None) => {
                            break true;
                        }
                        ControlFlow::Break(Some(claimed_l2_output_root)) => {
                            break self
                                .dispatch_discard_retry(
                                    target_block,
                                    &current,
                                    claimed_l2_output_root,
                                    state,
                                    cache,
                                    true,
                                )
                                .await;
                        }
                    }
                }
                TargetPoll::Pending | TargetPoll::Transient => break false,
                TargetPoll::NotFound => {
                    if state
                        .target(target_block)
                        .is_some_and(|target| target.retry_session.is_some())
                    {
                        warn!(
                            target_block,
                            "Discard retry session missing, dispatching a fresh retry"
                        );
                        break self
                            .dispatch_discard_retry(
                                target_block,
                                &current,
                                claimed_l2_output_root,
                                state,
                                cache,
                                true,
                            )
                            .await;
                    }
                    debug!(
                        target_block,
                        claimed_l2_output_root = %claimed_l2_output_root,
                        "No prover-service session for target, waiting for dispatcher"
                    );
                    break false;
                }
                TargetPoll::Failed { session_id, error } => {
                    warn!(
                        target_block,
                        session_id = %session_id,
                        error = %error,
                        "Prover service reported failed session, re-dispatching"
                    );
                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED)
                        .increment(1);
                    if !state.handle_proof_failure(target_block, error, self.max_retries, cache) {
                        break true;
                    }
                    if state.target(target_block).and_then(|target| target.retry_session.as_ref())
                        == Some(&session_id)
                    {
                        let dispatch = self
                            .dispatch_discard_retry(
                                target_block,
                                &current,
                                claimed_l2_output_root,
                                state,
                                cache,
                                false,
                            )
                            .await;
                        break dispatch;
                    } else {
                        let dispatch = self
                            .dispatcher
                            .dispatch_for(target_block, &current, claimed_l2_output_root)
                            .await;
                        Metrics::proof_dispatch_total(dispatch.metric_label()).increment(1);
                        match dispatch {
                            ProofDispatchAttempt::Accepted(session_id) => {
                                info!(
                                    target_block,
                                    session_id = %session_id,
                                    from_block = current.l2_block_number,
                                    "Proof request accepted by prover service"
                                );
                            }
                            ProofDispatchAttempt::BuildFailed(error) => {
                                error!(
                                    target_block,
                                    error = %error,
                                    "Failed to build proof request for root retry"
                                );
                                break false;
                            }
                            ProofDispatchAttempt::DispatchFailed(error) => {
                                warn!(
                                    target_block,
                                    error = %error,
                                    "Immediate re-dispatch failed after failed proof session"
                                );
                                break false;
                            }
                        }
                    }
                    break false;
                }
            }
        };

        Metrics::pipeline_retries()
            .set(state.target.as_ref().map_or(0, |(_, target)| target.retry_count) as f64);
        restart
    }

    /// Validates and submits a ready proof inline.
    async fn submit_inline(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        proof: ProofResult,
        state: &mut ProofCollectorState,
        cache: &mut Option<ProofRecoveryCache>,
        cancel: &CancellationToken,
    ) -> ControlFlow<Option<B256>, RecoveredState> {
        let claimed_l2_output_root = match &proof {
            ProofResult::Tee { aggregate_proposal, .. } => aggregate_proposal.output_root,
            ProofResult::Zk { .. } => {
                warn!(target_block, "Unexpected ZK proof result in TEE proposer path");
                return ControlFlow::Break(None);
            }
        };
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
            return ControlFlow::Break(None);
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
                ControlFlow::Break(None)
            }
            Ok(Err(SubmitAction::RootMismatch)) => {
                submit_timer.disarm();
                warn!(target_block, "Output root mismatch at submit time, restarting pipeline");
                Metrics::root_mismatch_total().increment(1);
                *cache = None;
                ControlFlow::Break(None)
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
                ControlFlow::Break(None)
            }
            Ok(Err(SubmitAction::Discard(error))) => {
                submit_timer.disarm();
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    target_block,
                    error = %error,
                    "Proof discarded by submitter, dispatching fresh retry proof"
                );
                ControlFlow::Break(Some(claimed_l2_output_root))
            }
            Ok(action @ (Ok(()) | Err(SubmitAction::GameAlreadyExists))) => {
                if matches!(action, Err(SubmitAction::GameAlreadyExists)) {
                    submit_timer.disarm();
                    info!(target_block, "Game already exists on chain");
                    if let Some(cached) = cache.as_mut() {
                        cached.game_count = cached.game_count.saturating_sub(1);
                    }
                } else {
                    drop(submit_timer);
                    info!(target_block, "Submission successful");
                }
                Metrics::last_proposed_block().set(target_block as f64);
                let _ = state.target.take_if(|(block, _)| *block == target_block);
                match self.recovery.recover_latest_state(cache).await {
                    Ok(recovered) => ControlFlow::Continue(recovered),
                    Err(e) => {
                        warn!(error = %e, "Failed to recover state after inline submit");
                        ControlFlow::Break(None)
                    }
                }
            }
        }
    }

    /// Dispatches a retry-specific proof request after a discarded proof.
    async fn dispatch_discard_retry(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        state: &mut ProofCollectorState,
        cache: &mut Option<ProofRecoveryCache>,
        count_dispatch_failure: bool,
    ) -> bool {
        let current_attempt =
            state.target(target_block).map_or(0, |target| target.discard_retry_count);
        if current_attempt >= self.max_retries {
            error!(
                target_block,
                attempts = current_attempt,
                max_retries = self.max_retries,
                "Discard retry budget exhausted, dropping recovery cache"
            );
            let _ = state.target.take_if(|(block, _)| *block == target_block);
            *cache = None;
            return true;
        }

        let attempt = current_attempt + 1;
        // Keep the root before dispatch so restart can retry a failed discard dispatch.
        state.target_for(target_block).pending_discard_root = Some(claimed_l2_output_root);

        let dispatch = self
            .dispatcher
            .dispatch_discard_retry(target_block, recovered, claimed_l2_output_root, attempt)
            .await;

        Metrics::proof_dispatch_total(dispatch.metric_label()).increment(1);
        match dispatch {
            ProofDispatchAttempt::Accepted(session_id) => {
                info!(
                    target_block,
                    session_id = %session_id,
                    attempt,
                    "Discard retry proof request accepted by prover service"
                );
                let target = state.target_for(target_block);
                target.discard_retry_count = attempt;
                target.retry_session = Some(session_id);
                target.pending_discard_root = None;
                false
            }
            ProofDispatchAttempt::BuildFailed(error) => {
                warn!(target_block, error = %error, "Failed to build discard retry proof request");
                state.target_for(target_block).retry_session = None;
                true
            }
            ProofDispatchAttempt::DispatchFailed(error) => {
                state.target_for(target_block).retry_session = None;
                if !count_dispatch_failure {
                    warn!(
                        target_block,
                        error = %error,
                        "Immediate discard retry dispatch failed after failed proof session"
                    );
                    return true;
                }
                if state.handle_proof_failure(target_block, error, self.max_retries, cache) {
                    false
                } else {
                    true
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::atomic::Ordering};

    use alloy_primitives::{Address, B256};
    use async_trait::async_trait;
    use base_proof_primitives::{ProofRequest, Proposal};

    use super::*;
    use crate::{
        output_proposer::OutputProposer,
        proof_adapter::ProposerProofAdapter,
        proof_dispatcher::{ProofDispatcher, ProofDispatcherConfig},
        proof_recovery::ProofRecoveryConfig,
        proof_submitter::{ProofSubmitter, ProofSubmitterConfig},
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
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

    fn failing_l1() -> Arc<MockL1> {
        Arc::new(MockL1 {
            latest_block_number: 1000,
            header_by_number_error: Some("simulated L1 outage".to_owned()),
        })
    }

    fn rollup_client(block: u64, root: Option<B256>) -> Arc<MockRollupClient> {
        Arc::new(MockRollupClient {
            sync_status: test_sync_status(block, B256::ZERO),
            output_roots: root.map_or_else(HashMap::new, |root| HashMap::from([(block, root)])),
            max_safe_block: None,
        })
    }

    fn rejecting_requester() -> Arc<MockProofRequester> {
        let requester = Arc::new(MockProofRequester::default());
        requester.reject_prove.store(true, Ordering::SeqCst);
        requester
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
        max_retries: u32,
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
                max_retries: 3,
                factory_client: MockDisputeGameFactory::with_games(vec![]),
            }
        }
    }

    fn make_orchestrator(
        parts: OrchestratorParts,
    ) -> ProofCollectorOrchestrator<MockL1, MockL2, MockRollupClient> {
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let dispatcher = ProofDispatcher::new(
            Arc::clone(&parts.proof_requester),
            Arc::clone(&parts.l1),
            l2,
            Arc::clone(&parts.rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: parts.intermediate_block_interval,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
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

        ProofCollectorOrchestrator::new(
            parts.proof_requester,
            dispatcher,
            submitter,
            recovery,
            BLOCK_INTERVAL,
            parts.max_retries,
            Some(std::time::Duration::from_secs(60)),
        )
    }

    #[tokio::test]
    async fn tick_resets_cursor_when_recovery_rewinds() {
        let orchestrator = make_orchestrator(Default::default());
        let mut state = ProofCollectorState::default();
        state.recovered = Some(recovered(300));
        state.cursor = Some(recovered(500));
        let mut cache = cache();
        let cancel = CancellationToken::new();

        let result = orchestrator.tick(&mut state, &mut cache, recovered(100), 200, &cancel).await;

        assert!(!result);
        assert_eq!(state.recovered, Some(recovered(100)));
        assert_eq!(state.cursor, Some(recovered(100)));
    }

    #[tokio::test]
    async fn submit_inline_restarts_when_post_submit_recovery_fails() {
        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.games_should_fail = true;
        let orchestrator = make_orchestrator(OrchestratorParts {
            rollup_client: rollup_client(200, None),
            max_retries: 2,
            factory_client: factory,
            ..Default::default()
        });
        let proposal = test_proposal(200);
        let proof =
            ProofResult::Tee { aggregate_proposal: proposal.clone(), proposals: vec![proposal] };
        let mut state = ProofCollectorState::default();
        let mut cache = cache();
        let cancel = CancellationToken::new();

        let effect = orchestrator
            .submit_inline(200, &recovered(100), proof, &mut state, &mut cache, &cancel)
            .await;

        assert_eq!(effect, ControlFlow::Break(None));
    }

    #[tokio::test]
    async fn discard_retry_build_failure_removes_stale_retry_session() {
        let orchestrator =
            make_orchestrator(OrchestratorParts { l1: failing_l1(), ..Default::default() });
        let target_block = 200;
        let mut state = ProofCollectorState {
            target: Some((
                target_block,
                ProofCollectorTargetState {
                    retry_session: Some("stale-failed-session".to_owned()),
                    discard_retry_count: 1,
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let mut cache = cache();

        let restart = orchestrator
            .dispatch_discard_retry(
                target_block,
                &recovered(100),
                B256::repeat_byte(0xaa),
                &mut state,
                &mut cache,
                true,
            )
            .await;

        assert!(restart);
        let target = state.target(target_block).expect("target state");
        assert!(target.retry_session.is_none());
        assert_eq!(target.pending_discard_root, Some(B256::repeat_byte(0xaa)));
        assert_eq!(target.discard_retry_count, 1);
        assert_eq!(target.retry_count, 0);
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn discard_retry_dispatch_failure_does_not_store_unaccepted_session() {
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: rejecting_requester(),
            rollup_client: rollup_client(200, None),
            max_retries: 2,
            ..Default::default()
        });
        let target_block = 200;
        let claimed_root = B256::repeat_byte(0xaa);
        let mut state = ProofCollectorState::default();
        let mut cache = cache();

        let restart = orchestrator
            .dispatch_discard_retry(
                target_block,
                &recovered(100),
                claimed_root,
                &mut state,
                &mut cache,
                true,
            )
            .await;

        assert!(!restart);
        let target = state.target(target_block).expect("target state");
        assert!(target.retry_session.is_none());
        assert_eq!(target.pending_discard_root, Some(claimed_root));
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn discard_retry_session_mismatch_does_not_store_unaccepted_session() {
        let requester = Arc::new(MockProofRequester::default());
        *requester.accepted_session_id.lock().unwrap() = Some("unexpected-session".to_owned());
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: requester,
            rollup_client: rollup_client(200, None),
            max_retries: 2,
            ..Default::default()
        });
        let target_block = 200;
        let claimed_root = B256::repeat_byte(0xaa);
        let mut state = ProofCollectorState {
            target: Some((
                target_block,
                ProofCollectorTargetState {
                    retry_session: Some("stale-session".to_owned()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let mut cache = cache();

        let restart = orchestrator
            .dispatch_discard_retry(
                target_block,
                &recovered(100),
                claimed_root,
                &mut state,
                &mut cache,
                true,
            )
            .await;

        assert!(!restart);
        let target = state.target(target_block).expect("target state");
        assert!(target.retry_session.is_none());
        assert_eq!(target.pending_discard_root, Some(claimed_root));
        assert_eq!(target.retry_count, 1);
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn root_retry_build_failure_waits_with_retry_state() {
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: failed_session_requester(claimed_root),
            l1: failing_l1(),
            rollup_client: rollup_client(target_block, None),
            max_retries: 2,
            ..Default::default()
        });
        let mut state = ProofCollectorState::default();
        let mut cache = cache();
        let cancel = CancellationToken::new();

        let result =
            orchestrator.tick(&mut state, &mut cache, recovered(100), target_block, &cancel).await;

        assert!(!result);
        let target = state.target(target_block).expect("target state");
        assert_eq!(target.retry_count, 1);
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn root_retry_dispatch_failure_waits_with_retry_state() {
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let proof_requester = failed_session_requester(claimed_root);
        proof_requester.reject_prove.store(true, Ordering::SeqCst);
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester,
            rollup_client: rollup_client(target_block, Some(claimed_root)),
            max_retries: 2,
            ..Default::default()
        });
        let mut state = ProofCollectorState::default();
        let mut cache = cache();
        let cancel = CancellationToken::new();

        let result =
            orchestrator.tick(&mut state, &mut cache, recovered(100), target_block, &cancel).await;

        assert!(!result);
        let target = state.target(target_block).expect("target state");
        assert_eq!(target.retry_count, 1);
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn discard_retry_dispatch_failure_returns_false_on_retry_exhaustion() {
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: rejecting_requester(),
            rollup_client: rollup_client(200, None),
            max_retries: 1,
            ..Default::default()
        });
        let target_block = 200;
        let mut state = ProofCollectorState::default();
        let mut cache = cache();

        let restart = orchestrator
            .dispatch_discard_retry(
                target_block,
                &recovered(100),
                B256::repeat_byte(0xaa),
                &mut state,
                &mut cache,
                true,
            )
            .await;

        assert!(restart);
        assert!(cache.is_none());
        assert!(state.target.is_none());
    }

    #[tokio::test]
    async fn tick_returns_restart_when_discard_retry_budget_exhausts() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let request = proof_request(target_block, claimed_root, 100, 1000);
        proof_requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(request))
            .await
            .expect("test setup should dispatch root session");
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester,
            rollup_client: rollup_client(target_block, None),
            output_proposer: Arc::new(DiscardingOutputProposer),
            max_retries: 0,
            ..Default::default()
        });
        let mut state = ProofCollectorState::default();
        let mut cache = cache();
        let cancel = CancellationToken::new();

        let result =
            orchestrator.tick(&mut state, &mut cache, recovered(100), target_block, &cancel).await;

        assert!(result);
        assert!(cache.is_none());
        assert!(state.target.is_none());
    }

    #[tokio::test]
    async fn tick_returns_restart_when_failed_session_exhausts_retries() {
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: failed_session_requester(claimed_root),
            rollup_client: rollup_client(target_block, None),
            max_retries: 1,
            ..Default::default()
        });
        let mut state = ProofCollectorState::default();
        let mut cache = cache();
        let cancel = CancellationToken::new();

        let result =
            orchestrator.tick(&mut state, &mut cache, recovered(100), target_block, &cancel).await;

        assert!(result);
        assert!(cache.is_none());
        assert!(state.target.is_none());
    }

    #[tokio::test]
    async fn failed_discard_retry_session_exhaustion_clears_target() {
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let request = proof_request(target_block, claimed_root, 100, 1000);
        let session_id = ProposerProofAdapter::tee_discard_retry_session_id(&request, 1);
        let requester = Arc::new(MockProofRequester::default());
        requester
            .failed_sessions
            .lock()
            .unwrap()
            .insert(session_id.clone(), "simulated proof failure".to_owned());
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester: requester,
            rollup_client: rollup_client(target_block, Some(claimed_root)),
            max_retries: 1,
            ..Default::default()
        });
        let mut state = ProofCollectorState {
            target: Some((
                target_block,
                ProofCollectorTargetState {
                    retry_session: Some(session_id),
                    discard_retry_count: 1,
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let mut cache = cache();
        let cancel = CancellationToken::new();

        let result =
            orchestrator.tick(&mut state, &mut cache, recovered(100), target_block, &cancel).await;

        assert!(result);
        assert!(cache.is_none());
        assert!(state.target.is_none());
    }

    /// Restart/recovery: the collector derives the prover-service session id
    /// solely from the canonical L2 output root + tee kind, so a freshly
    /// constructed collector (mirroring a proposer restart) can pick up an
    /// in-flight session that a previous run dispatched.
    #[tokio::test]
    async fn poll_recovers_in_flight_session_across_restart() {
        let proof_requester = Arc::new(MockProofRequester::default());

        let target_block = 600u64;
        let canonical_root = B256::repeat_byte(0xCC);
        let rollup_client = rollup_client(target_block, Some(canonical_root));

        // First "run": dispatch a TEE proof for `target_block` against the shared
        // prover-service stub.
        let proof_request = ProofRequest {
            agreed_l2_output_root: B256::repeat_byte(0x03),
            ..proof_request(target_block, canonical_root, 300, 1200)
        };
        let expected_session_id = proof_requester
            .prove_block_range(ProposerProofAdapter::tee_prove_block_range_request(proof_request))
            .await
            .unwrap()
            .session_id;
        // "Restart": build a fresh orchestrator with no in-memory dispatch state.
        // It must rederive the session id from the canonical chain root and
        // recover the in-flight session.
        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester,
            l1: Arc::new(MockL1 { latest_block_number: 1200, ..Default::default() }),
            rollup_client,
            intermediate_block_interval: 300,
            ..Default::default()
        });

        match orchestrator.poll_target(target_block, canonical_root, None).await {
            TargetPoll::Ready { session_id, .. } => {
                assert_eq!(session_id, expected_session_id);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// When the prover service has no record of a session, polling returns
    /// [`TargetPoll::NotFound`] so the caller can dispatch a new request.
    #[tokio::test]
    async fn poll_returns_not_found_for_unknown_session() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let target_block = 200u64;
        let rollup_client = rollup_client(target_block, Some(B256::repeat_byte(0xAA)));

        let orchestrator = make_orchestrator(OrchestratorParts {
            proof_requester,
            l1: Arc::new(MockL1 { latest_block_number: 1200, ..Default::default() }),
            rollup_client,
            intermediate_block_interval: 300,
            ..Default::default()
        });
        match orchestrator.poll_target(target_block, B256::repeat_byte(0xAA), None).await {
            TargetPoll::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
