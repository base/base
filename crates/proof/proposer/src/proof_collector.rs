//! Polls and orchestrates prover-service collection for proposer TEE proofs.
//!
//! The [`ProofCollector`] does not care about the proof dispatcher. It assumes that
//! the eligible block range either has, or will have, a `prove_block_range` request
//! initiated against the prover service (whether by this proposer instance or a
//! previous one). Given a target block, the collector fetches its canonical L2
//! output root, derives the deterministic prover-service session ID via
//! [`ProposerProofAdapter::tee_session_id_for_root`], calls `get_proof`, and
//! returns a [`TargetPoll`] outcome that tells the caller exactly what to do
//! next: submit the proof, dispatch a new request, wait, or treat as a transient
//! error.
//!
//! Because session derivation is deterministic and independent of in-memory
//! dispatch state, the collector can rediscover and complete sessions across
//! proposer restarts.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_proof_primitives::ProofResult;
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
use base_prover_service_protocol::{GetProofRequest, GetProofResponse, ProofStatus, TeeKind};
use futures::{StreamExt, stream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    driver::RecoveredState,
    error::ProposerError,
    metrics::Metrics,
    proof_adapter::ProposerProofAdapter,
    proof_dispatcher::{ProofDispatchAttempt, ProofDispatcher},
    proof_submitter::{ProofSubmitter, SubmitAction},
};

/// Cached result from the last successful recovery walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofRecoveryCache {
    /// Factory `game_count` at the time of the walk.
    pub game_count: u64,
    /// Recovered on-chain state from the walk.
    pub state: RecoveredState,
}

/// Recovery hook used by collector orchestration after successful submissions.
#[async_trait]
pub trait ProofCollectorRecoveryProvider: Send + Sync {
    /// Refreshes the recovery cache and returns the latest on-chain state.
    async fn recover_latest_state(
        &self,
        cache: &mut Option<ProofRecoveryCache>,
    ) -> Result<RecoveredState, ProposerError>;
}

/// Runtime settings for collector orchestration.
#[derive(Debug, Clone, Copy)]
pub struct ProofCollectorRuntimeConfig {
    /// Number of L2 blocks between output proposals.
    pub block_interval: u64,
    /// Maximum proof failures or discard retries before dropping recovery state.
    pub max_retries: u32,
    /// Maximum duration for a single inline submit attempt.
    pub submit_timeout: Duration,
}

/// Mutable collector-side orchestration state.
#[derive(Debug, Default)]
pub struct ProofCollectorState {
    /// Recovered chain state that the current cursor was derived from.
    pub recovered: Option<RecoveredState>,
    /// Latest block the collector has submitted through.
    pub cursor: Option<RecoveredState>,
    /// Per-target proof/dispatch retry counts.
    pub retry_counts: HashMap<u64, u32>,
    /// Per-target discard retry counts.
    pub discard_retry_counts: HashMap<u64, u32>,
    /// Active retry-specific prover-service sessions by target block.
    pub retry_sessions: HashMap<u64, String>,
    /// Targets whose root-derived proof was discarded and need a retry-specific session.
    pub pending_discard_roots: HashMap<u64, B256>,
    /// Terminal failed sessions already counted while waiting for replacement dispatch.
    pub counted_failed_sessions: HashMap<u64, String>,
}

/// Result of one collector tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofCollectorTickResult {
    /// The collector made progress or reached a natural wait point.
    Continue,
    /// The owning pipeline session should restart from recovered chain state.
    Restart,
}

/// Result of an inline submit attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofSubmitEffect {
    /// The proof was submitted or the game already existed.
    Submitted {
        /// Latest recovered chain state after the submit was observed.
        recovered: RecoveredState,
    },
    /// The pipeline should restart before collecting more proofs.
    Restart,
    /// The submitted proof should be replaced by a retry-specific session.
    Redispatch {
        /// Claimed L2 output root for the discarded proof.
        claimed_l2_output_root: B256,
    },
}

/// Owns collector-side polling, sequential submit, and retry-session orchestration.
pub struct ProofCollectorOrchestrator<L1, L2, R, Recovery>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    Recovery: ProofCollectorRecoveryProvider,
{
    collector: ProofCollector<R>,
    dispatcher: ProofDispatcher<L1, L2, R>,
    submitter: ProofSubmitter<L1, R>,
    recovery: Arc<Recovery>,
    runtime: ProofCollectorRuntimeConfig,
}

/// Outcome of polling the prover service for a single target block.
///
/// Returned by [`ProofCollector::poll`] and consumed by the proposer's
/// proposer pipeline to choose the next action: submit, dispatch, wait, or
/// retry.
#[derive(Debug)]
pub enum TargetPoll {
    /// The prover service produced a successful, decoded proof for the target.
    Ready {
        /// Deterministic prover-service session identifier.
        session_id: String,
        /// Decoded proof result ready for inline on-chain submission.
        proof: ProofResult,
    },
    /// The prover service reported a terminal failed session, or its result
    /// could not be decoded.
    Failed {
        /// Deterministic prover-service session identifier.
        session_id: String,
        /// Canonical L2 output root at the target block.
        claimed_l2_output_root: B256,
        /// Underlying error returned by the prover service or the decoder.
        error: ProposerError,
    },
    /// The prover service has the session but it has not finished proving.
    Pending {
        /// Deterministic prover-service session identifier.
        session_id: String,
        /// Either [`ProofStatus::Queued`] or [`ProofStatus::Running`].
        status: ProofStatus,
    },
    /// The prover service has no record of a session for this target's
    /// canonical output root. The caller should dispatch a new request.
    NotFound {
        /// Deterministic prover-service session identifier the caller should
        /// expect when dispatching the matching `prove_block_range` request.
        session_id: String,
        /// Canonical L2 output root at the target block, already fetched
        /// while deriving the session id. Threaded through so the dispatch
        /// path does not need to call `output_at_block(target_block)` a
        /// second time.
        claimed_l2_output_root: B256,
    },
    /// A transient error prevented the poll from completing (RPC failure,
    /// canonical root fetch error, etc.). The caller should sleep and retry
    /// on the next iteration without counting against the retry budget.
    Unknown {
        /// Optional session identifier when it could be derived before the
        /// failure. `None` if the canonical output-root fetch itself failed.
        session_id: Option<String>,
        /// The underlying transient error.
        error: ProposerError,
    },
}

/// Outcome returned by [`ProofCollector::collect`] for a single target block.
#[derive(Debug)]
pub enum CollectedProof {
    /// A successfully proved target whose result was decoded into a [`ProofResult`].
    Ready {
        /// Target L2 block number for the completed proof.
        target_block: u64,
        /// Deterministic prover-service session identifier.
        session_id: String,
        /// Decoded proof result ready for sequential on-chain submission.
        proof: ProofResult,
    },
    /// The prover service reported a failed session, or its result could not be decoded.
    Failed {
        /// Target L2 block number whose proof failed.
        target_block: u64,
        /// Deterministic prover-service session identifier.
        session_id: String,
        /// Underlying error returned by the prover service or the result decoder.
        error: ProposerError,
    },
}

/// Polls the prover service for the proof of a single derived target block.
///
/// Behavior is independent of the proof dispatcher: the target block is
/// supplied by the caller, and the session ID is derived from the canonical
/// claimed output root, so a restarted proposer can still pick up sessions
/// previously initiated by another instance.
pub struct ProofCollector<R> {
    proof_requester: Arc<dyn ProofRequesterProvider>,
    rollup_client: Arc<R>,
    block_interval: u64,
    output_fetch_concurrency: usize,
    tee_kind: TeeKind,
}

impl<R> Clone for ProofCollector<R> {
    fn clone(&self) -> Self {
        Self {
            proof_requester: Arc::clone(&self.proof_requester),
            rollup_client: Arc::clone(&self.rollup_client),
            block_interval: self.block_interval,
            output_fetch_concurrency: self.output_fetch_concurrency,
            tee_kind: self.tee_kind,
        }
    }
}

impl<R> std::fmt::Debug for ProofCollector<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofCollector")
            .field("block_interval", &self.block_interval)
            .field("output_fetch_concurrency", &self.output_fetch_concurrency)
            .field("tee_kind", &self.tee_kind)
            .finish_non_exhaustive()
    }
}

impl<R: RollupProvider + 'static> ProofCollector<R> {
    /// Creates a TEE proof collector for AWS Nitro proofs.
    pub fn aws_nitro(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<R>,
        block_interval: u64,
        output_fetch_concurrency: usize,
    ) -> Self {
        Self::new(
            proof_requester,
            rollup_client,
            block_interval,
            output_fetch_concurrency,
            TeeKind::AwsNitro,
        )
    }

    /// Creates a TEE proof collector for single-target AWS Nitro polling.
    pub fn target_poller_aws_nitro(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<R>,
    ) -> Self {
        Self::new(proof_requester, rollup_client, 0, 1, TeeKind::AwsNitro)
    }

    /// Creates a proof collector for the given TEE implementation.
    pub const fn new(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<R>,
        block_interval: u64,
        output_fetch_concurrency: usize,
        tee_kind: TeeKind,
    ) -> Self {
        Self { proof_requester, rollup_client, block_interval, output_fetch_concurrency, tee_kind }
    }

    /// Returns the TEE implementation this collector polls proofs for.
    pub const fn tee_kind(&self) -> TeeKind {
        self.tee_kind
    }

    /// Returns the next-expected target blocks to poll, derived from `recovered`,
    /// the L2 `safe_head`, and the caller-supplied `is_excluded` predicate.
    pub fn collectable_targets(
        &self,
        recovered: &RecoveredState,
        safe_head: u64,
        is_excluded: impl Fn(u64) -> bool,
    ) -> Vec<u64> {
        if self.block_interval == 0 {
            return Vec::new();
        }

        let mut cursor = match recovered.l2_block_number.checked_add(self.block_interval) {
            Some(cursor) => cursor,
            None => return Vec::new(),
        };
        let mut targets = Vec::new();

        while cursor <= safe_head {
            if !is_excluded(cursor) {
                targets.push(cursor);
            }
            cursor = match cursor.checked_add(self.block_interval) {
                Some(cursor) => cursor,
                None => break,
            };
        }

        targets
    }

    /// Polls the prover service for each target and returns ready/failed outcomes.
    pub async fn collect(&self, targets: &[u64]) -> Vec<CollectedProof> {
        if targets.is_empty() {
            return Vec::new();
        }

        let roots = self.fetch_canonical_root_results(targets).await;

        let mut outcomes = Vec::new();
        for &target in targets {
            let Some(Ok(root)) = roots.get(&target) else { continue };
            let session_id = ProposerProofAdapter::tee_session_id_for_root(*root, self.tee_kind);

            let response = match self.get_proof(session_id.clone()).await {
                Ok(response) => response,
                Err(e) => {
                    debug!(
                        error = %e,
                        target_block = target,
                        session_id = %session_id,
                        "Failed to poll proof status"
                    );
                    continue;
                }
            };

            Metrics::proof_status_received_total(Self::status_label(response.status)).increment(1);
            match response.status {
                ProofStatus::Queued | ProofStatus::Running => {
                    debug!(
                        target_block = target,
                        session_id = %session_id,
                        status = ?response.status,
                        "Proof request still pending"
                    );
                }
                ProofStatus::Failed => {
                    let message = response.error_message.unwrap_or_else(|| {
                        format!("proof session {session_id} failed without an error message")
                    });
                    outcomes.push(CollectedProof::Failed {
                        target_block: target,
                        session_id,
                        error: ProposerError::Prover(message),
                    });
                }
                ProofStatus::Succeeded => {
                    let result = match response.result {
                        Some(result) => result,
                        None => {
                            let error = ProposerError::Prover(format!(
                                "proof session {session_id} succeeded without a result"
                            ));
                            outcomes.push(CollectedProof::Failed {
                                target_block: target,
                                session_id,
                                error,
                            });
                            continue;
                        }
                    };

                    match ProposerProofAdapter::tee_proof_result(result, self.tee_kind) {
                        Ok(proof) => outcomes.push(CollectedProof::Ready {
                            target_block: target,
                            session_id,
                            proof,
                        }),
                        Err(error) => outcomes.push(CollectedProof::Failed {
                            target_block: target,
                            session_id,
                            error,
                        }),
                    }
                }
            }
        }

        outcomes
    }

    /// Polls the prover service for the proof of `target_block`.
    ///
    /// Returns a [`TargetPoll`] describing the next action the caller should
    /// take. The session ID is derived deterministically from the canonical
    /// L2 output root at `target_block` and the configured TEE kind, so a
    /// freshly constructed collector can rediscover an in-flight session
    /// dispatched by a previous proposer instance.
    pub async fn poll(&self, target_block: u64) -> TargetPoll {
        // 1. Fetch the canonical output root needed to derive the session id.
        let output = match self.rollup_client.output_at_block(target_block).await {
            Ok(out) => out,
            Err(e) => {
                debug!(
                    target_block,
                    error = %e,
                    "Failed to fetch canonical output root for target",
                );
                return TargetPoll::Unknown { session_id: None, error: ProposerError::Rpc(e) };
            }
        };

        let session_id =
            ProposerProofAdapter::tee_session_id_for_root(output.output_root, self.tee_kind);

        let response = match self.get_proof(session_id.clone()).await {
            Ok(response) => response,
            Err(e) if e.is_not_found() => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    "No prover-service session for target, dispatch needed",
                );
                return TargetPoll::NotFound {
                    session_id,
                    claimed_l2_output_root: output.output_root,
                };
            }
            Err(e) => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    error = %e,
                    "Transient failure polling prover service",
                );
                return TargetPoll::Unknown {
                    session_id: Some(session_id),
                    error: Self::error_from_client(e),
                };
            }
        };

        self.response_to_poll(target_block, session_id, output.output_root, response)
    }

    /// Polls a specific prover-service session for `target_block`.
    ///
    /// Used after a discard retry is dispatched under a retry-specific session
    /// id. The canonical root is fetched lazily only for outcomes that need it
    /// (`NotFound` and `Failed`) so pending retry sessions do not depend on an
    /// unrelated rollup RPC.
    pub async fn poll_session(&self, target_block: u64, session_id: String) -> TargetPoll {
        let response = match self.get_proof(session_id.clone()).await {
            Ok(response) => response,
            Err(e) if e.is_not_found() => {
                let claimed_l2_output_root =
                    match self.retry_session_output_root(target_block, &session_id).await {
                        Ok(root) => root,
                        Err(error) => {
                            return TargetPoll::Unknown { session_id: Some(session_id), error };
                        }
                    };
                debug!(
                    target_block,
                    session_id = %session_id,
                    "Retry session missing from prover service, dispatch needed",
                );
                return TargetPoll::NotFound { session_id, claimed_l2_output_root };
            }
            Err(e) => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    error = %e,
                    "Transient failure polling retry session",
                );
                return TargetPoll::Unknown {
                    session_id: Some(session_id),
                    error: Self::error_from_client(e),
                };
            }
        };

        Metrics::proof_status_received_total(Self::status_label(response.status)).increment(1);
        match response.status {
            ProofStatus::Queued | ProofStatus::Running => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    status = ?response.status,
                    "Proof request still pending",
                );
                TargetPoll::Pending { session_id, status: response.status }
            }
            ProofStatus::Failed => {
                let claimed_l2_output_root =
                    match self.retry_session_output_root(target_block, &session_id).await {
                        Ok(root) => root,
                        Err(error) => {
                            return TargetPoll::Unknown { session_id: Some(session_id), error };
                        }
                    };
                let message = response.error_message.unwrap_or_else(|| {
                    format!("proof session {session_id} failed without an error message")
                });
                TargetPoll::Failed {
                    session_id,
                    claimed_l2_output_root,
                    error: ProposerError::Prover(message),
                }
            }
            ProofStatus::Succeeded => {
                let result = match response.result {
                    Some(result) => result,
                    None => {
                        let claimed_l2_output_root =
                            match self.retry_session_output_root(target_block, &session_id).await {
                                Ok(root) => root,
                                Err(root_error) => {
                                    return TargetPoll::Unknown {
                                        session_id: Some(session_id),
                                        error: root_error,
                                    };
                                }
                            };
                        let error = ProposerError::Prover(format!(
                            "proof session {session_id} succeeded without a result"
                        ));
                        return TargetPoll::Failed { session_id, claimed_l2_output_root, error };
                    }
                };
                match ProposerProofAdapter::tee_proof_result(result, self.tee_kind) {
                    Ok(proof) => TargetPoll::Ready { session_id, proof },
                    Err(decode_error) => {
                        let claimed_l2_output_root =
                            match self.retry_session_output_root(target_block, &session_id).await {
                                Ok(root) => root,
                                Err(root_error) => {
                                    return TargetPoll::Unknown {
                                        session_id: Some(session_id),
                                        error: root_error,
                                    };
                                }
                            };
                        TargetPoll::Failed {
                            session_id,
                            claimed_l2_output_root,
                            error: decode_error,
                        }
                    }
                }
            }
        }
    }

    async fn retry_session_output_root(
        &self,
        target_block: u64,
        session_id: &str,
    ) -> Result<B256, ProposerError> {
        match self.rollup_client.output_at_block(target_block).await {
            Ok(out) => Ok(out.output_root),
            Err(e) => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    error = %e,
                    "Failed to fetch canonical output root for retry session",
                );
                Err(ProposerError::Rpc(e))
            }
        }
    }

    async fn get_proof(
        &self,
        session_id: String,
    ) -> Result<GetProofResponse, ProverServiceClientError> {
        self.proof_requester.get_proof(GetProofRequest { session_id }).await
    }

    async fn fetch_canonical_root_results(
        &self,
        blocks: &[u64],
    ) -> HashMap<u64, Result<B256, ProposerError>> {
        if blocks.is_empty() {
            return HashMap::new();
        }
        let rollup = &self.rollup_client;
        stream::iter(blocks.iter().copied())
            .map(|block_number| async move {
                let result = rollup
                    .output_at_block(block_number)
                    .await
                    .map(|out| out.output_root)
                    .map_err(ProposerError::Rpc);
                (block_number, result)
            })
            .buffered(self.output_fetch_concurrency)
            .collect()
            .await
    }

    fn response_to_poll(
        &self,
        target_block: u64,
        session_id: String,
        claimed_l2_output_root: B256,
        response: GetProofResponse,
    ) -> TargetPoll {
        Metrics::proof_status_received_total(Self::status_label(response.status)).increment(1);
        match response.status {
            ProofStatus::Queued | ProofStatus::Running => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    status = ?response.status,
                    "Proof request still pending",
                );
                TargetPoll::Pending { session_id, status: response.status }
            }
            ProofStatus::Failed => {
                let message = response.error_message.unwrap_or_else(|| {
                    format!("proof session {session_id} failed without an error message")
                });
                TargetPoll::Failed {
                    session_id,
                    claimed_l2_output_root,
                    error: ProposerError::Prover(message),
                }
            }
            ProofStatus::Succeeded => {
                let result = match response.result {
                    Some(result) => result,
                    None => {
                        let error = ProposerError::Prover(format!(
                            "proof session {session_id} succeeded without a result"
                        ));
                        return TargetPoll::Failed { session_id, claimed_l2_output_root, error };
                    }
                };
                match ProposerProofAdapter::tee_proof_result(result, self.tee_kind) {
                    Ok(proof) => TargetPoll::Ready { session_id, proof },
                    Err(error) => TargetPoll::Failed { session_id, claimed_l2_output_root, error },
                }
            }
        }
    }

    /// Maps a [`ProverServiceClientError`] into the proposer's error type.
    fn error_from_client(error: ProverServiceClientError) -> ProposerError {
        ProposerError::Prover(error.to_string())
    }

    /// Maps a [`ProofStatus`] to its `proof_status_received_total` label value.
    const fn status_label(status: ProofStatus) -> &'static str {
        match status {
            ProofStatus::Queued => Metrics::PROOF_STATUS_QUEUED,
            ProofStatus::Running => Metrics::PROOF_STATUS_RUNNING,
            ProofStatus::Succeeded => Metrics::PROOF_STATUS_SUCCEEDED,
            ProofStatus::Failed => Metrics::PROOF_STATUS_FAILED,
        }
    }
}

impl ProofCollectorState {
    /// Creates empty collector state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Removes state for targets already recovered on-chain.
    pub fn prune_recovered(&mut self, recovered_block: u64) {
        self.retry_counts.retain(|&target, _| target > recovered_block);
        self.discard_retry_counts.retain(|&target, _| target > recovered_block);
        self.retry_sessions.retain(|&target, _| target > recovered_block);
        self.pending_discard_roots.retain(|&target, _| target > recovered_block);
        self.counted_failed_sessions.retain(|&target, _| target > recovered_block);
    }

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

        let count = self.retry_counts.entry(target).or_insert(0);
        *count += 1;
        if *count >= max_retries {
            error!(
                target_block = target,
                attempts = *count,
                error = %error,
                "Proof failed after max retries, dropping cached recovery"
            );
            self.retry_counts.remove(&target);
            *cache = None;
            false
        } else {
            warn!(
                target_block = target,
                attempt = *count,
                error = %error,
                "Proof failed, re-dispatching"
            );
            true
        }
    }
}

impl<L1, L2, R, Recovery> Clone for ProofCollectorOrchestrator<L1, L2, R, Recovery>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    Recovery: ProofCollectorRecoveryProvider,
{
    fn clone(&self) -> Self {
        Self {
            collector: self.collector.clone(),
            dispatcher: self.dispatcher.clone(),
            submitter: self.submitter.clone(),
            recovery: Arc::clone(&self.recovery),
            runtime: self.runtime,
        }
    }
}

impl<L1, L2, R, Recovery> std::fmt::Debug for ProofCollectorOrchestrator<L1, L2, R, Recovery>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    Recovery: ProofCollectorRecoveryProvider,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofCollectorOrchestrator")
            .field("collector", &self.collector)
            .field("dispatcher", &self.dispatcher)
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

impl<L1, L2, R, Recovery> ProofCollectorOrchestrator<L1, L2, R, Recovery>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    Recovery: ProofCollectorRecoveryProvider + 'static,
{
    /// Creates a collector orchestrator from low-level proof components.
    pub const fn new(
        collector: ProofCollector<R>,
        dispatcher: ProofDispatcher<L1, L2, R>,
        submitter: ProofSubmitter<L1, R>,
        recovery: Arc<Recovery>,
        runtime: ProofCollectorRuntimeConfig,
    ) -> Self {
        Self { collector, dispatcher, submitter, recovery, runtime }
    }

    /// Runs one collector tick from the supplied recovered state and safe head.
    pub async fn tick(
        &self,
        state: &mut ProofCollectorState,
        cache: &mut Option<ProofRecoveryCache>,
        recovered: RecoveredState,
        safe_head: u64,
        cancel: &CancellationToken,
    ) -> ProofCollectorTickResult {
        state.prune_recovered(recovered.l2_block_number);

        if state.recovered != Some(recovered) || state.cursor.is_none() {
            state.recovered = Some(recovered);
            state.cursor = Some(recovered);
        }

        while let Some(current) = state.cursor {
            if cancel.is_cancelled() {
                break;
            }

            let Some(target_block) = self.next_target_block(current.l2_block_number) else {
                break;
            };

            if target_block > safe_head {
                debug!(
                    current_block = current.l2_block_number,
                    target_block,
                    safe_head,
                    "Safe head below collection target, waiting for L2 head to advance"
                );
                break;
            }

            if let Some(claimed_l2_output_root) =
                state.pending_discard_roots.get(&target_block).copied()
            {
                if !self
                    .dispatch_discard_retry(
                        target_block,
                        &current,
                        claimed_l2_output_root,
                        state,
                        cache,
                        true,
                    )
                    .await
                {
                    Metrics::pipeline_retries()
                        .set(state.retry_counts.values().sum::<u32>() as f64);
                    return ProofCollectorTickResult::Restart;
                }
                break;
            }

            let poll = if let Some(session_id) = state.retry_sessions.get(&target_block).cloned() {
                self.collector.poll_session(target_block, session_id).await
            } else {
                self.collector.poll(target_block).await
            };

            match poll {
                TargetPoll::Ready { session_id, proof } => {
                    info!(target_block, session_id = %session_id, "Proof ready, submitting inline");
                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_READY).increment(1);
                    Metrics::last_collected_block().set(target_block as f64);
                    match self
                        .submit_inline(target_block, &current, proof, state, cache, cancel)
                        .await
                    {
                        ProofSubmitEffect::Submitted { recovered } => {
                            state.retry_sessions.remove(&target_block);
                            state.discard_retry_counts.remove(&target_block);
                            state.pending_discard_roots.remove(&target_block);
                            state.counted_failed_sessions.remove(&target_block);
                            state.cursor = Some(recovered);
                            if recovered.l2_block_number > current.l2_block_number {
                                continue;
                            }
                            break;
                        }
                        ProofSubmitEffect::Restart => {
                            Metrics::pipeline_retries()
                                .set(state.retry_counts.values().sum::<u32>() as f64);
                            return ProofCollectorTickResult::Restart;
                        }
                        ProofSubmitEffect::Redispatch { claimed_l2_output_root } => {
                            if !self
                                .dispatch_discard_retry(
                                    target_block,
                                    &current,
                                    claimed_l2_output_root,
                                    state,
                                    cache,
                                    true,
                                )
                                .await
                            {
                                Metrics::pipeline_retries()
                                    .set(state.retry_counts.values().sum::<u32>() as f64);
                                return ProofCollectorTickResult::Restart;
                            }
                            break;
                        }
                    }
                }
                TargetPoll::Pending { session_id, status } => {
                    debug!(
                        target_block,
                        session_id = %session_id,
                        ?status,
                        "Proof pending, waiting for prover service"
                    );
                    break;
                }
                TargetPoll::NotFound { session_id, claimed_l2_output_root } => {
                    if state.retry_sessions.get(&target_block).is_some_and(|id| id == &session_id) {
                        warn!(
                            target_block,
                            session_id = %session_id,
                            "Discard retry session missing, dispatching a fresh retry"
                        );
                        if !self
                            .dispatch_discard_retry(
                                target_block,
                                &current,
                                claimed_l2_output_root,
                                state,
                                cache,
                                true,
                            )
                            .await
                        {
                            Metrics::pipeline_retries()
                                .set(state.retry_counts.values().sum::<u32>() as f64);
                            return ProofCollectorTickResult::Restart;
                        }
                    } else {
                        debug!(
                            target_block,
                            session_id = %session_id,
                            claimed_l2_output_root = %claimed_l2_output_root,
                            "No prover-service session for target, waiting for dispatcher"
                        );
                    }
                    break;
                }
                TargetPoll::Failed { session_id, claimed_l2_output_root, error } => {
                    warn!(
                        target_block,
                        session_id = %session_id,
                        error = %error,
                        "Prover service reported failed session, re-dispatching"
                    );
                    Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED)
                        .increment(1);
                    let already_counted = state
                        .counted_failed_sessions
                        .get(&target_block)
                        .is_some_and(|id| id == &session_id);
                    if already_counted
                        || state.handle_proof_failure(
                            target_block,
                            error,
                            self.runtime.max_retries,
                            cache,
                        )
                    {
                        state.counted_failed_sessions.insert(target_block, session_id.clone());
                        if state
                            .retry_sessions
                            .get(&target_block)
                            .is_some_and(|id| id == &session_id)
                        {
                            if !self
                                .dispatch_discard_retry(
                                    target_block,
                                    &current,
                                    claimed_l2_output_root,
                                    state,
                                    cache,
                                    false,
                                )
                                .await
                            {
                                Metrics::pipeline_retries()
                                    .set(state.retry_counts.values().sum::<u32>() as f64);
                                return ProofCollectorTickResult::Restart;
                            }
                        } else {
                            if !self
                                .dispatch_root_retry(
                                    target_block,
                                    &current,
                                    claimed_l2_output_root,
                                    state,
                                    cache,
                                    false,
                                )
                                .await
                            {
                                Metrics::pipeline_retries()
                                    .set(state.retry_counts.values().sum::<u32>() as f64);
                                return ProofCollectorTickResult::Restart;
                            }
                        }
                    } else {
                        Metrics::pipeline_retries()
                            .set(state.retry_counts.values().sum::<u32>() as f64);
                        return ProofCollectorTickResult::Restart;
                    }
                    break;
                }
                TargetPoll::Unknown { session_id, error } => {
                    debug!(
                        target_block,
                        session_id = ?session_id,
                        error = %error,
                        "Transient poll failure, will retry next iteration"
                    );
                    break;
                }
            }
        }

        Metrics::pipeline_retries().set(state.retry_counts.values().sum::<u32>() as f64);
        ProofCollectorTickResult::Continue
    }

    /// Validates and submits a ready proof inline.
    pub async fn submit_inline(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        proof: ProofResult,
        state: &mut ProofCollectorState,
        cache: &mut Option<ProofRecoveryCache>,
        cancel: &CancellationToken,
    ) -> ProofSubmitEffect {
        let claimed_l2_output_root = match &proof {
            ProofResult::Tee { aggregate_proposal, .. } => aggregate_proposal.output_root,
            ProofResult::Zk { .. } => {
                warn!(target_block, "Unexpected ZK proof result in TEE proposer path");
                return ProofSubmitEffect::Restart;
            }
        };
        let parent_address = recovered.parent_address;
        info!(target_block, parent_address = %parent_address, "Submitting proof inline");

        let mut submit_timer = base_metrics::timed!(Metrics::proposal_total_duration_seconds());
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                submit_timer.disarm();
                warn!(target_block, "Inline submit cancelled, restarting pipeline session");
                return ProofSubmitEffect::Restart;
            }
            result = tokio::time::timeout(
                self.runtime.submit_timeout,
                self.submitter.submit(&proof, target_block, parent_address),
            ) => result,
        };

        match result {
            Err(_) => {
                submit_timer.disarm();
                Metrics::submit_timeouts_total().increment(1);
                warn!(
                    target_block,
                    timeout_secs = self.runtime.submit_timeout.as_secs(),
                    "Inline submit timed out, restarting pipeline session"
                );
                ProofSubmitEffect::Restart
            }
            Ok(Ok(())) => {
                drop(submit_timer);
                info!(target_block, "Submission successful");
                Metrics::last_proposed_block().set(target_block as f64);
                state.retry_counts.remove(&target_block);
                match self.recovery.recover_latest_state(cache).await {
                    Ok(recovered) => ProofSubmitEffect::Submitted { recovered },
                    Err(e) => {
                        warn!(error = %e, "Failed to recover state after submission");
                        ProofSubmitEffect::Restart
                    }
                }
            }
            Ok(Err(SubmitAction::RootMismatch)) => {
                submit_timer.disarm();
                warn!(target_block, "Output root mismatch at submit time, restarting pipeline");
                Metrics::root_mismatch_total().increment(1);
                *cache = None;
                ProofSubmitEffect::Restart
            }
            Ok(Err(SubmitAction::GameAlreadyExists)) => {
                submit_timer.disarm();
                info!(target_block, "Game already exists on chain");
                Metrics::last_proposed_block().set(target_block as f64);
                state.retry_counts.remove(&target_block);
                if let Some(cached) = cache.as_mut() {
                    cached.game_count = cached.game_count.saturating_sub(1);
                }
                match self.recovery.recover_latest_state(cache).await {
                    Ok(recovered) => ProofSubmitEffect::Submitted { recovered },
                    Err(e) => {
                        warn!(error = %e, "Failed to recover state after GameAlreadyExists");
                        ProofSubmitEffect::Restart
                    }
                }
            }
            Ok(Err(SubmitAction::Failed(error))) => {
                submit_timer.disarm();
                Metrics::errors_total(error.metric_label()).increment(1);
                if error.is_invalid_parent_game() {
                    warn!(
                        target_block,
                        error = %error,
                        "Submission rejected: parent game invalid, restarting pipeline"
                    );
                    *cache = None;
                } else {
                    warn!(target_block, error = %error, "Submission failed, restarting pipeline");
                }
                ProofSubmitEffect::Restart
            }
            Ok(Err(SubmitAction::Discard(error))) => {
                submit_timer.disarm();
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    target_block,
                    error = %error,
                    "Proof discarded by submitter, dispatching fresh retry proof"
                );
                ProofSubmitEffect::Redispatch { claimed_l2_output_root }
            }
        }
    }

    /// Dispatches a root-derived retry after a failed prover-service session.
    ///
    /// Returns `false` when the caller should restart before collecting more proofs.
    pub async fn dispatch_root_retry(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        state: &mut ProofCollectorState,
        cache: &mut Option<ProofRecoveryCache>,
        count_dispatch_failure: bool,
    ) -> bool {
        match self.dispatcher.dispatch_for(target_block, recovered, claimed_l2_output_root).await {
            ProofDispatchAttempt::Accepted(dispatched) => {
                info!(
                    target_block,
                    session_id = %dispatched.session_id,
                    from_block = recovered.l2_block_number,
                    "Proof request accepted by prover service"
                );
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_ACCEPTED).increment(1);
                state.counted_failed_sessions.remove(&target_block);
            }
            ProofDispatchAttempt::BuildFailed(error) => {
                error!(
                    target_block,
                    error = %error,
                    "Failed to build proof request for root retry"
                );
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_BUILD_FAILED).increment(1);
                return false;
            }
            ProofDispatchAttempt::DispatchFailed(error) => {
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_FAILED).increment(1);
                if count_dispatch_failure {
                    if !state.handle_proof_failure(
                        target_block,
                        error,
                        self.runtime.max_retries,
                        cache,
                    ) {
                        return false;
                    }
                } else {
                    warn!(
                        target_block,
                        error = %error,
                        "Immediate re-dispatch failed after failed proof session"
                    );
                    return false;
                }
            }
        }

        true
    }

    /// Dispatches a retry-specific proof request after a discarded proof.
    ///
    /// Returns `false` when the caller should restart before collecting more proofs.
    pub async fn dispatch_discard_retry(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        state: &mut ProofCollectorState,
        cache: &mut Option<ProofRecoveryCache>,
        count_dispatch_failure: bool,
    ) -> bool {
        let current_attempt = state.discard_retry_counts.get(&target_block).copied().unwrap_or(0);
        if current_attempt >= self.runtime.max_retries {
            error!(
                target_block,
                attempts = current_attempt,
                max_retries = self.runtime.max_retries,
                "Discard retry budget exhausted, dropping recovery cache"
            );
            state.discard_retry_counts.remove(&target_block);
            state.retry_sessions.remove(&target_block);
            state.pending_discard_roots.remove(&target_block);
            state.counted_failed_sessions.remove(&target_block);
            *cache = None;
            return false;
        }

        let attempt = current_attempt + 1;
        state.pending_discard_roots.insert(target_block, claimed_l2_output_root);
        let request = match self
            .dispatcher
            .build_request(target_block, recovered, claimed_l2_output_root)
            .await
        {
            Ok(request) => request,
            Err(error) => {
                warn!(target_block, error = %error, "Failed to build discard retry proof request");
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_BUILD_FAILED).increment(1);
                state.retry_sessions.remove(&target_block);
                return false;
            }
        };
        let session_id = ProposerProofAdapter::tee_discard_retry_session_id(
            &request,
            self.collector.tee_kind(),
            attempt,
        );

        let dispatch_error = match self
            .dispatcher
            .requester_dispatcher()
            .dispatch_tee_with_session_id(request, session_id.clone())
            .await
        {
            Ok(dispatched) if dispatched.session_id == session_id => {
                info!(
                    target_block,
                    session_id = %dispatched.session_id,
                    attempt,
                    "Discard retry proof request accepted by prover service"
                );
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_ACCEPTED).increment(1);
                state.discard_retry_counts.insert(target_block, attempt);
                state.retry_sessions.insert(target_block, dispatched.session_id);
                state.pending_discard_roots.remove(&target_block);
                state.counted_failed_sessions.remove(&target_block);
                None
            }
            Ok(dispatched) => {
                error!(
                    target_block,
                    expected_session_id = %session_id,
                    actual_session_id = %dispatched.session_id,
                    "Prover service returned mismatched discard retry session id"
                );
                Some(ProposerError::Prover(format!(
                    "prover service returned mismatched session_id: expected {}, got {}",
                    session_id, dispatched.session_id
                )))
            }
            Err(error) => Some(error),
        };

        if let Some(error) = dispatch_error {
            Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_FAILED).increment(1);
            state.retry_sessions.remove(&target_block);
            if count_dispatch_failure {
                if !state.handle_proof_failure(target_block, error, self.runtime.max_retries, cache)
                {
                    state.discard_retry_counts.remove(&target_block);
                    state.retry_sessions.remove(&target_block);
                    state.pending_discard_roots.remove(&target_block);
                    state.counted_failed_sessions.remove(&target_block);
                    return false;
                }
            } else {
                warn!(
                    target_block,
                    error = %error,
                    "Immediate discard retry dispatch failed after failed proof session"
                );
                return false;
            }
        }

        true
    }

    /// Computes the next collector target from a current block.
    pub fn next_target_block(&self, current_block: u64) -> Option<u64> {
        if self.runtime.block_interval == 0 {
            error!("Block interval must be non-zero");
            return None;
        }

        current_block.checked_add(self.runtime.block_interval).map_or_else(
            || {
                error!(
                    current_block,
                    block_interval = self.runtime.block_interval,
                    "Overflow computing next target block"
                );
                None
            },
            Some,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use alloy_primitives::{Address, B256};
    use base_proof_primitives::Proposal;

    use super::*;
    use crate::{
        output_proposer::OutputProposer,
        proof_adapter::{ProofRequesterDispatcher, ProposerProofAdapter},
        proof_dispatcher::{ProofDispatcher, ProofDispatcherConfig},
        proof_submitter::{ProofSubmitter, ProofSubmitterConfig},
        test_utils::{
            MockL1, MockL2, MockOutputProposer, MockProofRequester, MockRollupClient,
            test_proposal, test_sync_status,
        },
    };

    #[derive(Debug)]
    struct NoopRecovery;

    #[derive(Debug)]
    struct FailingRecovery;

    #[derive(Debug)]
    struct FailingL1;

    #[derive(Debug)]
    struct DiscardingOutputProposer;

    #[derive(Debug)]
    struct RejectingProofRequester;

    #[derive(Debug)]
    struct MismatchedProofRequester;

    #[async_trait]
    impl ProofCollectorRecoveryProvider for NoopRecovery {
        async fn recover_latest_state(
            &self,
            _cache: &mut Option<ProofRecoveryCache>,
        ) -> Result<RecoveredState, ProposerError> {
            Ok(recovered(0))
        }
    }

    #[async_trait]
    impl ProofCollectorRecoveryProvider for FailingRecovery {
        async fn recover_latest_state(
            &self,
            _cache: &mut Option<ProofRecoveryCache>,
        ) -> Result<RecoveredState, ProposerError> {
            Err(ProposerError::Internal("simulated recovery failure".to_owned()))
        }
    }

    #[async_trait]
    impl OutputProposer for DiscardingOutputProposer {
        async fn propose_output(
            &self,
            _proposal: &Proposal,
            _parent_address: Address,
            _intermediate_roots: &[B256],
        ) -> Result<(), ProposerError> {
            Err(ProposerError::L1OriginTooOld)
        }
    }

    #[async_trait]
    impl ProofRequesterProvider for RejectingProofRequester {
        async fn prove_block_range(
            &self,
            _request: base_prover_service_protocol::ProveBlockRangeRequest,
        ) -> Result<base_prover_service_protocol::ProveBlockRangeResponse, ProverServiceClientError>
        {
            Err(ProverServiceClientError::Timeout("simulated dispatch failure".into()))
        }

        async fn get_proof(
            &self,
            _request: base_prover_service_protocol::GetProofRequest,
        ) -> Result<base_prover_service_protocol::GetProofResponse, ProverServiceClientError>
        {
            unimplemented!("tests do not poll proofs")
        }

        async fn list_proofs(
            &self,
            _request: base_prover_service_protocol::ListProofsRequest,
        ) -> Result<base_prover_service_protocol::ListProofsResponse, ProverServiceClientError>
        {
            unimplemented!("tests do not list proofs")
        }
    }

    #[async_trait]
    impl ProofRequesterProvider for MismatchedProofRequester {
        async fn prove_block_range(
            &self,
            _request: base_prover_service_protocol::ProveBlockRangeRequest,
        ) -> Result<base_prover_service_protocol::ProveBlockRangeResponse, ProverServiceClientError>
        {
            Ok(base_prover_service_protocol::ProveBlockRangeResponse {
                session_id: "unexpected-session".to_owned(),
            })
        }

        async fn get_proof(
            &self,
            _request: base_prover_service_protocol::GetProofRequest,
        ) -> Result<base_prover_service_protocol::GetProofResponse, ProverServiceClientError>
        {
            unimplemented!("tests do not poll proofs")
        }

        async fn list_proofs(
            &self,
            _request: base_prover_service_protocol::ListProofsRequest,
        ) -> Result<base_prover_service_protocol::ListProofsResponse, ProverServiceClientError>
        {
            unimplemented!("tests do not list proofs")
        }
    }

    #[async_trait]
    impl L1Provider for FailingL1 {
        async fn block_number(&self) -> base_proof_rpc::RpcResult<u64> {
            Ok(1000)
        }

        async fn header_by_number(
            &self,
            _: Option<u64>,
        ) -> base_proof_rpc::RpcResult<alloy_rpc_types_eth::Header> {
            Err(base_proof_rpc::RpcError::Transport("simulated L1 outage".into()))
        }

        async fn header_by_hash(
            &self,
            _: B256,
        ) -> base_proof_rpc::RpcResult<alloy_rpc_types_eth::Header> {
            unimplemented!("tests do not fetch L1 headers by hash")
        }

        async fn block_receipts(
            &self,
            _: B256,
        ) -> base_proof_rpc::RpcResult<Vec<alloy_rpc_types_eth::TransactionReceipt>> {
            unimplemented!("tests do not fetch receipts")
        }

        async fn code_at(
            &self,
            _: Address,
            _: Option<u64>,
        ) -> base_proof_rpc::RpcResult<alloy_primitives::Bytes> {
            unimplemented!("tests do not fetch code")
        }

        async fn call_contract(
            &self,
            _: Address,
            _: alloy_primitives::Bytes,
            _: Option<u64>,
        ) -> base_proof_rpc::RpcResult<alloy_primitives::Bytes> {
            unimplemented!("tests do not call contracts")
        }

        async fn get_balance(
            &self,
            _: Address,
        ) -> base_proof_rpc::RpcResult<alloy_primitives::U256> {
            Ok(alloy_primitives::U256::ZERO)
        }
    }

    fn recovered(block: u64) -> RecoveredState {
        RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::ZERO,
            l2_block_number: block,
        }
    }

    fn make_collector(block_interval: u64) -> ProofCollector<MockRollupClient> {
        let proof_requester: Arc<dyn ProofRequesterProvider> =
            Arc::new(MockProofRequester::default());
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        ProofCollector::aws_nitro(proof_requester, rollup_client, block_interval, 4)
    }

    fn make_orchestrator(
        block_interval: u64,
    ) -> ProofCollectorOrchestrator<MockL1, MockL2, MockRollupClient, NoopRecovery> {
        make_orchestrator_with_l1(Arc::new(MockL1 { latest_block_number: 1000 }), block_interval)
    }

    fn make_orchestrator_with_l1<L1>(
        l1: Arc<L1>,
        block_interval: u64,
    ) -> ProofCollectorOrchestrator<L1, MockL2, MockRollupClient, NoopRecovery>
    where
        L1: L1Provider + 'static,
    {
        let proof_requester: Arc<dyn ProofRequesterProvider> =
            Arc::new(MockProofRequester::default());
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            proof_requester,
            Arc::clone(&l1),
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 300,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer),
            rollup_client,
            l1,
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval,
                intermediate_block_interval: 300,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );

        ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(NoopRecovery),
            ProofCollectorRuntimeConfig {
                block_interval,
                max_retries: 3,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        )
    }

    #[test]
    fn collectable_targets_returns_next_expected_blocks_excluding_proved_and_submitting() {
        let collector = make_collector(100);

        let proved: BTreeSet<u64> = [200, 400].into_iter().collect();
        let submitting: Option<u64> = Some(300);

        let targets = collector.collectable_targets(&recovered(100), 700, |t| {
            proved.contains(&t) || submitting == Some(t)
        });

        assert_eq!(targets, vec![500, 600, 700]);
    }

    #[test]
    fn collectable_targets_returns_all_eligible_targets_up_to_safe_head() {
        let collector = make_collector(100);

        let targets = collector.collectable_targets(&recovered(100), 1000, |_| false);

        assert_eq!(targets, vec![200, 300, 400, 500, 600, 700, 800, 900, 1000]);
    }

    #[test]
    fn collectable_targets_returns_empty_when_safe_head_below_first_target() {
        let collector = make_collector(100);

        let targets = collector.collectable_targets(&recovered(500), 550, |_| false);

        assert!(targets.is_empty());
    }

    #[test]
    fn collectable_targets_returns_empty_for_zero_block_interval() {
        let proof_requester: Arc<dyn ProofRequesterProvider> =
            Arc::new(MockProofRequester::default());
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(100, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let collector = ProofCollector::target_poller_aws_nitro(proof_requester, rollup_client);

        let targets = collector.collectable_targets(&recovered(0), 100, |_| false);

        assert!(targets.is_empty());
    }

    #[test]
    fn orchestrator_next_target_block_returns_none_for_zero_interval() {
        let orchestrator = make_orchestrator(0);

        assert_eq!(orchestrator.next_target_block(100), None);
    }

    #[tokio::test]
    async fn tick_resets_cursor_when_recovery_rewinds() {
        let orchestrator = make_orchestrator(100);
        let mut state = ProofCollectorState::new();
        state.recovered = Some(recovered(300));
        state.cursor = Some(recovered(500));
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });
        let cancel = CancellationToken::new();

        let result = orchestrator.tick(&mut state, &mut cache, recovered(100), 200, &cancel).await;

        assert_eq!(result, ProofCollectorTickResult::Continue);
        assert_eq!(state.recovered, Some(recovered(100)));
        assert_eq!(state.cursor, Some(recovered(100)));
    }

    #[tokio::test]
    async fn submit_inline_restarts_when_post_submit_recovery_fails() {
        let proof_requester: Arc<dyn ProofRequesterProvider> =
            Arc::new(MockProofRequester::default());
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            proof_requester,
            Arc::clone(&l1),
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer),
            rollup_client,
            l1,
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval: 100,
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );
        let orchestrator = ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(FailingRecovery),
            ProofCollectorRuntimeConfig {
                block_interval: 100,
                max_retries: 2,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        );
        let proposal = test_proposal(200);
        let proof =
            ProofResult::Tee { aggregate_proposal: proposal.clone(), proposals: vec![proposal] };
        let mut state = ProofCollectorState::new();
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });
        let cancel = CancellationToken::new();

        let effect = orchestrator
            .submit_inline(200, &recovered(100), proof, &mut state, &mut cache, &cancel)
            .await;

        assert_eq!(effect, ProofSubmitEffect::Restart);
    }

    #[tokio::test]
    async fn discard_retry_build_failure_removes_stale_retry_session() {
        let orchestrator = make_orchestrator_with_l1(Arc::new(FailingL1), 100);
        let target_block = 200;
        let mut state = ProofCollectorState::new();
        state.retry_sessions.insert(target_block, "stale-failed-session".to_owned());
        state.discard_retry_counts.insert(target_block, 1);
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });

        let should_continue = orchestrator
            .dispatch_discard_retry(
                target_block,
                &recovered(100),
                B256::repeat_byte(0xaa),
                &mut state,
                &mut cache,
                true,
            )
            .await;

        assert!(!should_continue);
        assert!(!state.retry_sessions.contains_key(&target_block));
        assert_eq!(
            state.pending_discard_roots.get(&target_block).copied(),
            Some(B256::repeat_byte(0xaa))
        );
        assert_eq!(state.discard_retry_counts.get(&target_block).copied(), Some(1));
        assert!(state.retry_counts.is_empty());
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn discard_retry_dispatch_failure_does_not_store_unaccepted_session() {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(RejectingProofRequester);
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&requester),
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            requester,
            Arc::clone(&l1),
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer),
            rollup_client,
            l1,
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval: 100,
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );
        let orchestrator = ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(NoopRecovery),
            ProofCollectorRuntimeConfig {
                block_interval: 100,
                max_retries: 2,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        );
        let target_block = 200;
        let claimed_root = B256::repeat_byte(0xaa);
        let mut state = ProofCollectorState::new();
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });

        let should_continue = orchestrator
            .dispatch_discard_retry(
                target_block,
                &recovered(100),
                claimed_root,
                &mut state,
                &mut cache,
                false,
            )
            .await;

        assert!(!should_continue);
        assert!(state.retry_sessions.is_empty());
        assert_eq!(state.pending_discard_roots.get(&target_block).copied(), Some(claimed_root));
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn discard_retry_session_mismatch_does_not_store_unaccepted_session() {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(MismatchedProofRequester);
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&requester),
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            requester,
            Arc::clone(&l1),
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer),
            rollup_client,
            l1,
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval: 100,
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );
        let orchestrator = ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(NoopRecovery),
            ProofCollectorRuntimeConfig {
                block_interval: 100,
                max_retries: 2,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        );
        let target_block = 200;
        let claimed_root = B256::repeat_byte(0xaa);
        let mut state = ProofCollectorState::new();
        state.retry_sessions.insert(target_block, "stale-session".to_owned());
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });

        let should_continue = orchestrator
            .dispatch_discard_retry(
                target_block,
                &recovered(100),
                claimed_root,
                &mut state,
                &mut cache,
                true,
            )
            .await;

        assert!(should_continue);
        assert!(state.retry_sessions.is_empty());
        assert_eq!(state.pending_discard_roots.get(&target_block).copied(), Some(claimed_root));
        assert_eq!(state.retry_counts.get(&target_block).copied(), Some(1));
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn root_retry_build_failure_restarts_without_exhausting_retry_budget() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let l1 = Arc::new(FailingL1);
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let session_id =
            ProposerProofAdapter::tee_session_id_for_root(claimed_root, TeeKind::AwsNitro);
        proof_requester
            .failed_sessions
            .lock()
            .unwrap()
            .insert(session_id.clone(), "simulated proof failure".to_owned());
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(target_block, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            l1,
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer),
            rollup_client,
            Arc::new(FailingL1),
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval: 100,
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );
        let orchestrator = ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(NoopRecovery),
            ProofCollectorRuntimeConfig {
                block_interval: 100,
                max_retries: 2,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        );
        let mut state = ProofCollectorState::new();
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });
        let cancel = CancellationToken::new();

        let result =
            orchestrator.tick(&mut state, &mut cache, recovered(100), target_block, &cancel).await;

        assert_eq!(result, ProofCollectorTickResult::Restart);
        assert_eq!(state.retry_counts.get(&target_block).copied(), Some(1));
        assert_eq!(state.counted_failed_sessions.get(&target_block), Some(&session_id));
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn discard_retry_dispatch_failure_returns_false_on_retry_exhaustion() {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(RejectingProofRequester);
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&requester),
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            requester,
            Arc::clone(&l1),
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer),
            rollup_client,
            l1,
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval: 100,
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );
        let orchestrator = ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(NoopRecovery),
            ProofCollectorRuntimeConfig {
                block_interval: 100,
                max_retries: 1,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        );
        let target_block = 200;
        let mut state = ProofCollectorState::new();
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });

        let should_continue = orchestrator
            .dispatch_discard_retry(
                target_block,
                &recovered(100),
                B256::repeat_byte(0xaa),
                &mut state,
                &mut cache,
                true,
            )
            .await;

        assert!(!should_continue);
        assert!(cache.is_none());
        assert!(state.retry_counts.is_empty());
        assert!(state.discard_retry_counts.is_empty());
        assert!(state.retry_sessions.is_empty());
        assert!(state.pending_discard_roots.is_empty());
        assert!(state.counted_failed_sessions.is_empty());
    }

    #[tokio::test]
    async fn root_retry_dispatch_failure_returns_false_after_failed_session_was_counted() {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(RejectingProofRequester);
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&requester),
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            requester,
            Arc::clone(&l1),
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer),
            rollup_client,
            l1,
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval: 100,
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );
        let orchestrator = ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(NoopRecovery),
            ProofCollectorRuntimeConfig {
                block_interval: 100,
                max_retries: 2,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        );
        let target_block = 200;
        let session_id = "failed-session".to_owned();
        let mut state = ProofCollectorState::new();
        state.retry_counts.insert(target_block, 1);
        state.counted_failed_sessions.insert(target_block, session_id);
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });

        let should_continue = orchestrator
            .dispatch_root_retry(
                target_block,
                &recovered(100),
                B256::repeat_byte(0xaa),
                &mut state,
                &mut cache,
                false,
            )
            .await;

        assert!(!should_continue);
        assert_eq!(state.retry_counts.get(&target_block).copied(), Some(1));
        assert!(cache.is_some());
    }

    #[tokio::test]
    async fn tick_returns_restart_when_discard_retry_budget_exhausts() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(target_block, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let request = base_proof_primitives::ProofRequest {
            l1_head: B256::repeat_byte(0x01),
            agreed_l2_head_hash: B256::repeat_byte(0x02),
            agreed_l2_output_root: B256::ZERO,
            claimed_l2_output_root: claimed_root,
            claimed_l2_block_number: target_block,
            proposer: Address::repeat_byte(0x04),
            intermediate_block_interval: 100,
            l1_head_number: 1000,
            image_hash: B256::repeat_byte(0x05),
        };
        ProofRequesterDispatcher::aws_nitro(
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>
        )
        .dispatch_tee(request)
        .await
        .expect("test setup should dispatch root session");
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            Arc::clone(&l1),
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(DiscardingOutputProposer),
            rollup_client,
            l1,
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval: 100,
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );
        let orchestrator = ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(NoopRecovery),
            ProofCollectorRuntimeConfig {
                block_interval: 100,
                max_retries: 0,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        );
        let mut state = ProofCollectorState::new();
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });
        let cancel = CancellationToken::new();

        let result =
            orchestrator.tick(&mut state, &mut cache, recovered(100), target_block, &cancel).await;

        assert_eq!(result, ProofCollectorTickResult::Restart);
        assert!(cache.is_none());
        assert!(state.discard_retry_counts.is_empty());
        assert!(state.retry_sessions.is_empty());
    }

    #[tokio::test]
    async fn tick_returns_restart_when_failed_session_exhausts_retries() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let target_block = 200;
        let claimed_root = B256::repeat_byte(target_block as u8);
        let session_id =
            ProposerProofAdapter::tee_session_id_for_root(claimed_root, TeeKind::AwsNitro);
        proof_requester
            .failed_sessions
            .lock()
            .unwrap()
            .insert(session_id, "simulated proof failure".to_owned());
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(target_block, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            Arc::clone(&rollup_client),
        );
        let dispatcher = ProofDispatcher::aws_nitro(
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            Arc::clone(&l1),
            l2,
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer),
            rollup_client,
            l1,
            ProofSubmitterConfig {
                proposer_address: Address::repeat_byte(0x04),
                block_interval: 100,
                intermediate_block_interval: 100,
                tee_image_hash: B256::repeat_byte(0x05),
                tee_prover_registry_address: None,
                output_fetch_concurrency: 1,
            },
        );
        let orchestrator = ProofCollectorOrchestrator::new(
            collector,
            dispatcher,
            submitter,
            Arc::new(NoopRecovery),
            ProofCollectorRuntimeConfig {
                block_interval: 100,
                max_retries: 1,
                submit_timeout: std::time::Duration::from_secs(60),
            },
        );
        let mut state = ProofCollectorState::new();
        let mut cache = Some(ProofRecoveryCache { game_count: 0, state: recovered(100) });
        let cancel = CancellationToken::new();

        let result =
            orchestrator.tick(&mut state, &mut cache, recovered(100), target_block, &cancel).await;

        assert_eq!(result, ProofCollectorTickResult::Restart);
        assert!(cache.is_none());
        assert!(state.retry_counts.is_empty());
    }

    /// Restart/recovery: the collector derives the prover-service session id
    /// solely from the canonical L2 output root + tee kind, so a freshly
    /// constructed collector (mirroring a proposer restart) can pick up an
    /// in-flight session that a previous run dispatched.
    #[tokio::test]
    async fn poll_recovers_in_flight_session_across_restart() {
        let proof_requester: Arc<dyn ProofRequesterProvider> =
            Arc::new(MockProofRequester::default());

        let target_block = 600u64;
        let canonical_root = B256::repeat_byte(0xCC);
        let mut output_roots = HashMap::new();
        output_roots.insert(target_block, canonical_root);
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(target_block, B256::ZERO),
            output_roots,
            max_safe_block: None,
        });

        // First "run": dispatch a TEE proof for `target_block` via a dispatcher
        // that shares the prover-service stub.
        let dispatcher = ProofRequesterDispatcher::aws_nitro(Arc::clone(&proof_requester));
        let proof_request = base_proof_primitives::ProofRequest {
            l1_head: B256::repeat_byte(0x01),
            agreed_l2_head_hash: B256::repeat_byte(0x02),
            agreed_l2_output_root: B256::repeat_byte(0x03),
            claimed_l2_output_root: canonical_root,
            claimed_l2_block_number: target_block,
            proposer: Address::repeat_byte(0x04),
            intermediate_block_interval: 300,
            l1_head_number: 1200,
            image_hash: B256::repeat_byte(0x05),
        };
        let dispatched = dispatcher.dispatch_tee(proof_request).await.unwrap();
        let expected_session_id = dispatched.session_id.clone();
        // Drop the dispatcher to simulate the prior proposer process exiting.
        drop(dispatcher);

        // "Restart": build a fresh collector with no in-memory dispatch state.
        // It must rederive the session id from the canonical chain root and
        // recover the in-flight session.
        let collector =
            ProofCollector::target_poller_aws_nitro(Arc::clone(&proof_requester), rollup_client);

        match collector.poll(target_block).await {
            TargetPoll::Ready { session_id, .. } => {
                assert_eq!(session_id, expected_session_id);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// When the prover service has no record of a session, `poll()` returns
    /// [`TargetPoll::NotFound`] so the caller can dispatch a new request.
    #[tokio::test]
    async fn poll_returns_not_found_for_unknown_session() {
        let proof_requester: Arc<dyn ProofRequesterProvider> =
            Arc::new(MockProofRequester::default());
        let target_block = 200u64;
        let mut output_roots = HashMap::new();
        output_roots.insert(target_block, B256::repeat_byte(0xAA));
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(target_block, B256::ZERO),
            output_roots,
            max_safe_block: None,
        });

        let collector = ProofCollector::target_poller_aws_nitro(proof_requester, rollup_client);
        match collector.poll(target_block).await {
            TargetPoll::NotFound { session_id, claimed_l2_output_root } => {
                assert!(!session_id.is_empty());
                assert_eq!(
                    claimed_l2_output_root,
                    B256::repeat_byte(0xAA),
                    "should surface the canonical root already fetched",
                );
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
