//! Polls the prover service for the proof of a single derived target block.
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

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::B256;
use base_proof_primitives::ProofResult;
use base_proof_rpc::RollupProvider;
use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
use base_prover_service_protocol::{GetProofRequest, GetProofResponse, ProofStatus, TeeKind};
use futures::{StreamExt, stream};
use tracing::debug;

use crate::{
    driver::RecoveredState, error::ProposerError, metrics::Metrics,
    proof_adapter::ProposerProofAdapter,
};

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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use alloy_primitives::{Address, B256};

    use super::*;
    use crate::{
        proof_adapter::ProofRequesterDispatcher,
        test_utils::{MockProofRequester, MockRollupClient, test_sync_status},
    };

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
