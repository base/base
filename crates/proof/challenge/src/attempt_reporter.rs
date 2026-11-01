//! Maps dispute attempt outcomes to lifecycle logs and metrics.
//! Existing tracing targets remain stable for log consumers.

use alloy_primitives::Address;
use base_proof_submission::KnownRevert;
use tracing::{debug, error, info, warn};

use crate::{
    Action, AttemptProofSource, AttemptUpdate, ChallengeSubmitError, ChallengerMetrics, DropReason,
    IgnoreReason, RetryReason, TeeSubmissionFailure,
};

/// Reports typed dispute attempt outcomes without changing policy.
#[derive(Debug)]
pub struct AttemptReporter;

impl AttemptReporter {
    /// Reports one policy action before its effects are executed.
    pub fn action(game: Address, action: &Action) {
        match action {
            Action::Wait(update) => match update {
                AttemptUpdate::Pending => {
                    debug!(
                        target: "base_challenger::proof_manager",
                        game = %game,
                        "proof not ready, will retry next tick"
                    )
                }
                AttemptUpdate::Ready(_) => {}
                AttemptUpdate::NeedsRetry(reason) => Self::retry(game, reason),
                AttemptUpdate::ZkDecodeError(error) => {
                    warn!(
                        target: "base_challenger::proof_manager",
                        error = %error,
                        game = %game,
                        "failed to poll/submit pending proof"
                    )
                }
                AttemptUpdate::TeeSubmitRetry { count, limit, has_zk_fallback, error } => {
                    warn!(
                        target: "base_challenger::proof_manager",
                        error = %error,
                        game = %game,
                        retry_count = count,
                        retry_limit = limit,
                        has_zk_fallback,
                        "TEE dispute tx failed, will retry next tick"
                    );
                }
                AttemptUpdate::ZkSubmitRetry(error) => {
                    warn!(
                        target: "base_challenger::proof_manager",
                        error = %error,
                        game = %game,
                        "dispute tx failed, will retry next tick"
                    )
                }
                AttemptUpdate::ReproveStarted { session_id, retry_count } => {
                    info!(
                        target: "base_challenger::proof_manager",
                        game = %game,
                        session_id = %session_id,
                        retry_count = retry_count,
                        "proof job re-initiated"
                    )
                }
                AttemptUpdate::ReproveFailed { error, retry_count } => {
                    warn!(
                        target: "base_challenger::proof_manager",
                        error = %error,
                        game = %game,
                        retry_count = retry_count,
                        "proveBlockRange failed on retry, will retry next tick"
                    )
                }
            },
            Action::Submit { proof_bytes, intent, source, just_obtained, .. } => {
                info!(
                    target: "base_challenger::proof_manager",
                    game = %game,
                    proof_len = proof_bytes.len(),
                    action = intent.label(),
                    "proof ready, submitting dispute transaction"
                );
                if *source == AttemptProofSource::Tee && *just_obtained {
                    ChallengerMetrics::tee_proof_obtained_total().increment(1);
                }
            }
            Action::Reprove(reprove) => {
                Self::submission_failure(game, reprove.submission_failure.as_ref());
                if reprove.fallback {
                    debug!(
                        target: "base_challenger::proof_manager",
                        game = %game,
                        "TEE proof needs retry, falling back to ZK"
                    );
                    ChallengerMetrics::tee_proof_fallback_total().increment(1);
                }
                ChallengerMetrics::proof_retries_total().increment(1);
            }
            Action::Drop(reason) => match reason {
                DropReason::Disputed => {}
                DropReason::GameNotInProgress(status) => {
                    debug!(
                        target: "base_challenger::proof_manager",
                        game = %game,
                        status = ?status,
                        "game no longer in progress, dropping pending proof"
                    )
                }
                DropReason::AlreadyResolved { intent, tee_prover, zk_prover } => {
                    debug!(
                        target: "base_challenger::proof_manager",
                        game = %game,
                        intent = ?intent,
                        tee_prover = %tee_prover,
                        zk_prover = %zk_prover,
                        "game already resolved, dropping pending proof"
                    )
                }
                DropReason::AlreadyResolvedRevert(revert) => {
                    let error = ChallengeSubmitError::KnownRevert(*revert);
                    info!(
                        target: "base_challenger::proof_manager",
                        error = %error,
                        game = %game,
                        revert = ?revert,
                        "dispute already resolved, dropping pending proof"
                    );
                }
                DropReason::RetriesExhausted { retry_count, submission_failure } => {
                    Self::submission_failure(game, submission_failure.as_ref());
                    warn!(
                        target: "base_challenger::proof_manager",
                        game = %game,
                        retry_count = retry_count,
                        "proof retries exhausted, dropping entry"
                    );
                    ChallengerMetrics::proof_retries_exhausted_total().increment(1);
                }
                DropReason::NoZkFallback { submission_failure } => {
                    Self::submission_failure(game, submission_failure.as_ref());
                    debug!(
                        target: "base_challenger::proof_manager",
                        game = %game,
                        "TEE proof has no ZK fallback request, dropping entry"
                    );
                }
                DropReason::ZkInvalidSigner => {
                    let error = ChallengeSubmitError::KnownRevert(KnownRevert::InvalidSigner);
                    warn!(
                        target: "base_challenger::proof_manager",
                        error = %error,
                        game = %game,
                        "dispute proof signer is invalid onchain, dropping pending proof"
                    );
                }
            },
            Action::Ignore(reason) => match reason {
                IgnoreReason::InvalidParentGame | IgnoreReason::L1OriginTooOld => {
                    let revert = match reason {
                        IgnoreReason::InvalidParentGame => KnownRevert::InvalidParentGame,
                        _ => KnownRevert::L1OriginTooOld,
                    };
                    let error = ChallengeSubmitError::KnownRevert(revert);
                    warn!(
                        target: "base_challenger::proof_manager",
                        error = %error,
                        game = %game,
                        revert = ?revert,
                        "dispute cannot be resolved, ignoring game"
                    );
                }
                IgnoreReason::ZkInvalidProof { invalid_index } => {
                    let error = ChallengeSubmitError::KnownRevert(KnownRevert::InvalidProof);
                    // Reproving a deterministic ZK proof cannot fix mismatched prover programs.
                    error!(
                        target: "base_challenger::proof_manager",
                        error = %error,
                        game = %game,
                        invalid_index,
                        "onchain verifier rejected the ZK proof (InvalidProof); the prover \
                         likely runs a program other than the game's ZK hashes. Ignoring the \
                         game until restart; it remains unchallenged"
                    );
                    ChallengerMetrics::zk_invalid_proof_total().increment(1);
                }
            },
        }
    }

    /// Reports a proof failure, including a timeout returned by `need`.
    pub fn retry(game: Address, reason: &RetryReason) {
        let label = match reason {
            RetryReason::Timeout { elapsed, limit } => {
                warn!(
                    target: "base_challenger::pending",
                    game = %game,
                    elapsed = ?elapsed,
                    limit = ?limit,
                    "proof session timed out, will retry"
                );
                ChallengerMetrics::PROOF_FAILURE_TIMEOUT
            }
            RetryReason::Malformed => {
                warn!(
                    target: "base_challenger::pending",
                    game = %game,
                    "proof succeeded without result, treating as retryable failure"
                );
                ChallengerMetrics::PROOF_FAILURE_MALFORMED
            }
            RetryReason::TeeValidation(error) => {
                warn!(
                    target: "base_challenger::pending",
                    game = %game,
                    error = %error,
                    "TEE proof validation failed, falling back to ZK"
                );
                ChallengerMetrics::PROOF_FAILURE_TEE_VALIDATION
            }
            RetryReason::ProverFailed(error_message) => {
                warn!(
                    target: "base_challenger::pending",
                    game = %game,
                    error_message = ?error_message,
                    "proof job failed"
                );
                ChallengerMetrics::PROOF_FAILURE_FAILED
            }
        };
        ChallengerMetrics::proof_session_failures_total(label).increment(1);
    }

    /// Reports the submission failure that preceded retry handling.
    pub fn submission_failure(game: Address, failure: Option<&TeeSubmissionFailure>) {
        match failure {
            Some(TeeSubmissionFailure::Rejected(error)) => {
                warn!(
                    target: "base_challenger::proof_manager",
                    error = %error,
                    game = %game,
                    "TEE dispute tx failed, falling back to ZK"
                )
            }
            Some(TeeSubmissionFailure::RetryLimit { count, limit, has_zk_fallback, error }) => {
                warn!(
                    target: "base_challenger::proof_manager",
                    error = %error,
                    game = %game,
                    retry_count = count,
                    retry_limit = limit,
                    has_zk_fallback,
                    "TEE dispute tx retry limit reached"
                )
            }
            None => {}
        }
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use metrics_util::{
        MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };

    use super::*;

    #[test]
    fn proof_exhaustion_emits_metric() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            AttemptReporter::action(
                Address::ZERO,
                &Action::Drop(DropReason::RetriesExhausted {
                    retry_count: 4,
                    submission_failure: None,
                }),
            );
        });
        let count = snapshotter.snapshot().into_vec().into_iter().find_map(|(key, _, _, value)| {
            if key.kind() == MetricKind::Counter
                && key.key().name() == "base_challenger.proof_retries_exhausted_total"
                && let DebugValue::Counter(count) = value
            {
                return Some(count);
            }
            None
        });
        assert_eq!(count, Some(1));
    }
}
