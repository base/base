//! Polls the prover service for completed proofs of derived target blocks.
//!
//! The [`ProofCollector`] does not care about the proof dispatcher. It assumes that
//! eligible block ranges have already had `prove_block_range` requests initiated
//! (whether by this proposer instance or a previous one), derives the next-expected
//! L2 block targets from the recovered on-chain state and the safe head, derives
//! the deterministic prover-service session ID via
//! [`ProposerProofAdapter::tee_session_id_for_root`], calls `get_proof`, and returns
//! ready/failed outcomes to the caller.
//!
//! Because session derivation is deterministic and independent of in-memory dispatch
//! state, the collector can rediscover and complete sessions across proposer restarts.
//! The pipeline owns its [`PipelineState`](crate::pipeline) and is responsible for
//! applying retry/recovery bookkeeping to the returned outcomes.

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::B256;
use base_proof_primitives::ProofResult;
use base_proof_rpc::RollupProvider;
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{GetProofRequest, ProofStatus, TeeKind};
use futures::{StreamExt, stream};
use tracing::debug;

use crate::{driver::RecoveredState, error::ProposerError, proof_adapter::ProposerProofAdapter};

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

/// Polls the prover service for completed proofs of the next-expected target blocks.
///
/// Behavior is independent of the proof dispatcher: target blocks are derived
/// deterministically from `recovered` and `safe_head`, and session IDs are derived
/// from the canonical claimed output root, so a restarted proposer can still pick
/// up sessions previously initiated by another instance.
pub struct ProofCollector<R> {
    proof_requester: Arc<dyn ProofRequesterProvider>,
    rollup_client: Arc<R>,
    block_interval: u64,
    max_parallel_proofs: usize,
    output_fetch_concurrency: usize,
    tee_kind: TeeKind,
}

impl<R> Clone for ProofCollector<R> {
    fn clone(&self) -> Self {
        Self {
            proof_requester: Arc::clone(&self.proof_requester),
            rollup_client: Arc::clone(&self.rollup_client),
            block_interval: self.block_interval,
            max_parallel_proofs: self.max_parallel_proofs,
            output_fetch_concurrency: self.output_fetch_concurrency,
            tee_kind: self.tee_kind,
        }
    }
}

impl<R> std::fmt::Debug for ProofCollector<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofCollector")
            .field("block_interval", &self.block_interval)
            .field("max_parallel_proofs", &self.max_parallel_proofs)
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
        max_parallel_proofs: usize,
        output_fetch_concurrency: usize,
    ) -> Self {
        Self::new(
            proof_requester,
            rollup_client,
            block_interval,
            max_parallel_proofs,
            output_fetch_concurrency,
            TeeKind::AwsNitro,
        )
    }

    /// Creates a proof collector for the given TEE implementation.
    pub const fn new(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        rollup_client: Arc<R>,
        block_interval: u64,
        max_parallel_proofs: usize,
        output_fetch_concurrency: usize,
        tee_kind: TeeKind,
    ) -> Self {
        Self {
            proof_requester,
            rollup_client,
            block_interval,
            max_parallel_proofs,
            output_fetch_concurrency,
            tee_kind,
        }
    }

    /// Returns the TEE implementation this collector polls proofs for.
    pub const fn tee_kind(&self) -> TeeKind {
        self.tee_kind
    }

    /// Returns the next-expected target blocks to poll, derived from `recovered`,
    /// the L2 `safe_head`, and the caller-supplied `is_excluded` predicate.
    ///
    /// `is_excluded(target)` should return `true` when the caller already has a
    /// completed proof for `target` or has begun submitting it; such targets are
    /// skipped without affecting the per-tick parallel-poll budget.
    pub fn collectable_targets(
        &self,
        recovered: &RecoveredState,
        safe_head: u64,
        is_excluded: impl Fn(u64) -> bool,
    ) -> Vec<u64> {
        let mut cursor = match recovered.l2_block_number.checked_add(self.block_interval) {
            Some(cursor) => cursor,
            None => return Vec::new(),
        };
        let mut targets = Vec::new();

        while cursor <= safe_head && targets.len() < self.max_parallel_proofs {
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
    ///
    /// Pending sessions (`Queued` or `Running`) are not returned and are left for the
    /// caller to retry on the next tick. Targets whose canonical output root cannot
    /// be fetched are skipped silently; the underlying RPC failure is logged at debug
    /// level via the standard rollup-client error path.
    pub async fn collect(&self, targets: &[u64]) -> Vec<CollectedProof> {
        if targets.is_empty() {
            return Vec::new();
        }

        let roots = self.fetch_canonical_root_results(targets).await;

        let mut outcomes = Vec::new();
        for &target in targets {
            let Some(Ok(root)) = roots.get(&target) else { continue };
            let session_id = ProposerProofAdapter::tee_session_id_for_root(*root, self.tee_kind);

            let response = match self
                .proof_requester
                .get_proof(GetProofRequest { session_id: session_id.clone() })
                .await
            {
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
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use alloy_primitives::Address;

    use super::*;
    use crate::{
        driver::RecoveredState,
        test_utils::{MockProofRequester, MockRollupClient, test_sync_status},
    };

    fn make_collector(
        block_interval: u64,
        max_parallel_proofs: usize,
    ) -> ProofCollector<MockRollupClient> {
        let proof_requester: Arc<dyn ProofRequesterProvider> =
            Arc::new(MockProofRequester::default());
        let rollup_client = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        ProofCollector::aws_nitro(
            proof_requester,
            rollup_client,
            block_interval,
            max_parallel_proofs,
            4,
        )
    }

    fn recovered(block: u64) -> RecoveredState {
        RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::ZERO,
            l2_block_number: block,
        }
    }

    #[test]
    fn collectable_targets_returns_next_expected_blocks_excluding_proved_and_submitting() {
        let collector = make_collector(100, 5);

        let proved: BTreeSet<u64> = [200, 400].into_iter().collect();
        let submitting: Option<u64> = Some(300);

        let targets = collector.collectable_targets(&recovered(100), 700, |t| {
            proved.contains(&t) || submitting == Some(t)
        });

        // From recovered=100, step=100, safe_head=700 → candidates 200..=700.
        // Excluded: 200 (proved), 300 (submitting), 400 (proved).
        // Expected: 500, 600, 700.
        assert_eq!(targets, vec![500, 600, 700]);
    }

    #[test]
    fn collectable_targets_caps_at_max_parallel_proofs() {
        let collector = make_collector(100, 3);

        let targets = collector.collectable_targets(&recovered(100), 1000, |_| false);

        // Even though 100..=1000 yields 9 candidates, max_parallel_proofs=3 caps it.
        assert_eq!(targets, vec![200, 300, 400]);
    }

    #[test]
    fn collectable_targets_returns_empty_when_safe_head_below_first_target() {
        let collector = make_collector(100, 5);

        let targets = collector.collectable_targets(&recovered(500), 550, |_| false);

        // Next target = 600 > safe_head=550, so no targets.
        assert!(targets.is_empty());
    }
}
