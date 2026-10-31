//! A dispute attempt's policy, without prover, chain, or transaction I/O.
//!
//! Each tick calls `need(now)` first. Poll the prover only for `PollProver`, then
//! apply its response. `Wait(Ready | NeedsRetry)` continues with a chain check
//! in the same tick; other waits end the tick. `CheckChain` goes straight to
//! that check, carrying a newly applied timeout for reporting. Apply the chain
//! response, execute `Submit` or `Reprove`, and apply its result in the same
//! tick. Transport failures during polling or chain reads end the tick without
//! an event. Never call `need` again within a tick: `Submit.just_obtained` is
//! cleared there so a failed chain read cannot count a TEE proof next tick.
//! Terminal actions remove the attempt. Out-of-order events are inert.

use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256, Bytes};
use base_proof_contracts::GameStatus;
use base_proof_submission::KnownRevert;
use base_prover_service_protocol::{GetProofResponse, ProofStatus, SnarkPlonkProofRequest};
use base_tx_manager::TxManagerError;

use crate::{ChallengeSubmitError, ChallengerProofAdapter, DisputeIntent};

/// Retry and prover-duration limits for one dispute attempt.
#[derive(Debug, Clone, Copy)]
pub struct AttemptLimits {
    /// Maximum duration of one prover session (inclusive).
    pub max_proof_duration: Duration,
    /// Generic TEE transaction failures retried before ZK fallback.
    pub tee_submit_retry_limit: u32,
    /// Maximum proof failure count allowed when starting a reprove.
    pub max_proof_retries: u32,
}

/// The invalid intermediate root being disputed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisputeTarget {
    /// Index of the invalid intermediate root.
    pub invalid_index: u64,
    /// Correct root at that index.
    pub expected_root: B256,
}

/// A pre-built ZK fallback and its onchain action.
#[derive(Debug, Clone)]
pub struct ZkFallback {
    /// Request to start after TEE failure.
    pub request: SnarkPlonkProofRequest,
    /// Action for the ZK proof, independent of the TEE's fixed nullification.
    pub intent: DisputeIntent,
}

/// Current proof source, without mutable policy state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptProofSource {
    /// TEE attestation, always used to nullify.
    Tee,
    /// ZK proof, used to challenge or nullify.
    Zk,
}

/// A failure that spends the proof retry budget.
#[derive(Debug)]
pub enum RetryReason {
    /// The prover session exceeded its inclusive duration limit.
    Timeout {
        /// Time spent awaiting the prover.
        elapsed: Duration,
        /// Configured inclusive limit.
        limit: Duration,
    },
    /// The prover reported success without a result.
    Malformed,
    /// TEE proof validation failed.
    TeeValidation(eyre::Report),
    /// The prover reported failure.
    ProverFailed(Option<String>),
}

/// I/O needed at the start of a tick.
#[derive(Debug)]
pub enum Need {
    /// Poll this prover session.
    PollProver(String),
    /// Read game status and both provers; report a newly applied timeout first.
    CheckChain {
        /// Timeout applied by this call, if any.
        timeout: Option<RetryReason>,
    },
}

/// External result to apply to a dispute attempt.
#[derive(Debug)]
pub enum Event {
    /// Result of polling the current prover session.
    ProverStatus(Box<GetProofResponse>),
    /// Current onchain game state.
    ChainState {
        /// Current game status.
        status: GameStatus,
        /// Current TEE prover address.
        tee_prover: Address,
        /// Current ZK prover address.
        zk_prover: Address,
    },
    /// Result of submitting the ready proof.
    Submitted(Result<(), ChallengeSubmitError>),
    /// Result of starting the requested reprove.
    ReproveStarted(Result<String, eyre::Report>),
}

/// Information accompanying a wait; only `Ready` and `NeedsRetry` continue this tick.
#[derive(Debug)]
pub enum AttemptUpdate {
    /// The prover is queued or running, or an event is out of order.
    Pending,
    /// A proof was obtained; check the chain in this tick.
    Ready(AttemptProofSource),
    /// A proof failed; check the chain before retry handling.
    NeedsRetry(RetryReason),
    /// ZK decoding failed without spending a retry or leaving the awaiting state.
    ZkDecodeError(eyre::Report),
    /// A generic TEE submission failed and remains ready.
    TeeSubmitRetry {
        /// TEE submission retry count.
        count: u32,
        /// Configured inclusive limit.
        limit: u32,
        /// Whether a ZK fallback request exists.
        has_zk_fallback: bool,
        /// Failure returned by the external operation.
        error: ChallengeSubmitError,
    },
    /// A generic ZK submission failed and remains ready without a retry bound.
    ZkSubmitRetry(ChallengeSubmitError),
    /// A reprove started; count is the value before any fallback reset.
    ReproveStarted {
        /// Prover session identifier.
        session_id: String,
        /// Proof failure count before retry handling or its transport failure.
        retry_count: u32,
    },
    /// A reprove transport failure spent a retry; count precedes the increment.
    ReproveFailed {
        /// Failure returned by the external operation.
        error: eyre::Report,
        /// Proof failure count before retry handling or its transport failure.
        retry_count: u32,
    },
}

/// Why a dispute attempt ended without ignoring its game.
#[derive(Debug)]
pub enum DropReason {
    /// A dispute transaction succeeded.
    Disputed,
    /// The game is no longer in progress.
    GameNotInProgress(GameStatus),
    /// The chain state no longer needs this dispute.
    AlreadyResolved {
        /// Intended onchain dispute action.
        intent: DisputeIntent,
        /// Current TEE prover address.
        tee_prover: Address,
        /// Current ZK prover address.
        zk_prover: Address,
    },
    /// Submission reported that the dispute already exists.
    AlreadyResolvedRevert(KnownRevert),
    /// Proof failures exceeded the configured budget.
    RetriesExhausted {
        /// Proof failure count before retry handling or its transport failure.
        retry_count: u32,
        /// TEE submission failure that triggered retry handling, if any.
        submission_failure: Option<TeeSubmissionFailure>,
    },
    /// TEE failure cannot fall back to ZK.
    NoZkFallback {
        /// TEE submission failure that triggered retry handling, if any.
        submission_failure: Option<TeeSubmissionFailure>,
    },
    /// A ZK proof's signer is invalid onchain.
    ZkInvalidSigner,
}

/// Why the game must be ignored until restart.
#[derive(Debug)]
pub enum IgnoreReason {
    /// The game's parent is invalid.
    InvalidParentGame,
    /// The L1 origin is outside the history window.
    L1OriginTooOld,
    /// The onchain verifier rejected the ZK proof.
    ZkInvalidProof,
}

/// Why a TEE submission triggered retry handling.
#[derive(Debug)]
pub enum TeeSubmissionFailure {
    /// The TEE transaction was rejected.
    Rejected(ChallengeSubmitError),
    /// Generic transaction failures reached the limit.
    RetryLimit {
        /// TEE submission retry count.
        count: u32,
        /// Configured inclusive limit.
        limit: u32,
        /// Whether a ZK fallback request exists.
        has_zk_fallback: bool,
        /// Failure returned by the external operation.
        error: ChallengeSubmitError,
    },
}

/// Request and reporting data for one reprove, including ZK fallback.
#[derive(Debug)]
pub struct AttemptReprove {
    /// Prover request to execute.
    pub request: SnarkPlonkProofRequest,
    /// Failure count before the fallback reset, matching retry-start reporting.
    pub retry_count: u32,
    /// Whether this reprove switched from TEE to ZK.
    pub fallback: bool,
    /// TEE submission failure, when retry handling began at submission.
    pub submission_failure: Option<TeeSubmissionFailure>,
}

/// Policy result; the caller performs all effects.
#[derive(Debug)]
pub enum Action {
    /// End this tick unless the update explicitly requires a chain check.
    Wait(AttemptUpdate),
    /// Submit the ready proof.
    Submit {
        /// Encoded dispute proof.
        proof_bytes: Bytes,
        /// Root and index to dispute.
        target: DisputeTarget,
        /// Onchain action.
        intent: DisputeIntent,
        /// Current proof source.
        source: AttemptProofSource,
        /// Proof was obtained in this tick, after its initial `need`.
        just_obtained: bool,
    },
    /// Start a new ZK prover session.
    Reprove(AttemptReprove),
    /// Remove the attempt.
    Drop(DropReason),
    /// Remove the attempt and ignore the game.
    Ignore(IgnoreReason),
}

/// TEE-specific policy state; its intent is always nullification.
#[derive(Debug)]
pub struct TeeAttempt {
    zk_fallback: Option<ZkFallback>,
    retry_count: u32,
    submit_retry_count: u32,
}

/// ZK-specific policy state, with no TEE counters.
#[derive(Debug)]
pub struct ZkAttempt {
    request: SnarkPlonkProofRequest,
    intent: DisputeIntent,
    retry_count: u32,
}

/// Proof-source states with source-specific data.
#[derive(Debug)]
pub enum AttemptSource {
    /// TEE policy state.
    Tee(TeeAttempt),
    /// ZK policy state.
    Zk(ZkAttempt),
}

impl AttemptSource {
    /// Records one proof failure against the current proof source.
    pub const fn increment_retry(&mut self) {
        match self {
            Self::Tee(state) => state.retry_count = state.retry_count.saturating_add(1),
            Self::Zk(state) => state.retry_count = state.retry_count.saturating_add(1),
        }
    }

    /// Reports the current proof source and its fixed or requested intent.
    pub const fn proof_source(&self) -> (AttemptProofSource, DisputeIntent) {
        match self {
            Self::Tee(_) => (AttemptProofSource::Tee, DisputeIntent::Nullify),
            Self::Zk(state) => (AttemptProofSource::Zk, state.intent),
        }
    }

    /// Requests a reprove after chain validation or a rejected TEE submission.
    pub fn reprove(
        &mut self,
        phase: &mut AttemptPhase,
        limits: AttemptLimits,
        submission_failure: Option<TeeSubmissionFailure>,
    ) -> Action {
        let retry_count = match self {
            Self::Tee(state) => state.retry_count,
            Self::Zk(state) => state.retry_count,
        };
        if retry_count > limits.max_proof_retries {
            *phase = AttemptPhase::Done;
            return Action::Drop(DropReason::RetriesExhausted { retry_count, submission_failure });
        }
        let fallback = matches!(self, Self::Tee(_));
        let request = match self {
            Self::Tee(state) => {
                let Some(fallback) = state.zk_fallback.clone() else {
                    *phase = AttemptPhase::Done;
                    return Action::Drop(DropReason::NoZkFallback { submission_failure });
                };
                *self = Self::Zk(ZkAttempt {
                    request: fallback.request.clone(),
                    intent: fallback.intent,
                    retry_count: 0,
                });
                fallback.request
            }
            Self::Zk(state) => state.request.clone(),
        };
        *phase = AttemptPhase::Reproving { retry_count };
        Action::Reprove(AttemptReprove { request, retry_count, fallback, submission_failure })
    }
}

/// Lifecycle state of a dispute attempt.
#[derive(Debug)]
pub enum AttemptPhase {
    /// Waiting for the prover.
    Awaiting {
        /// Prover session identifier.
        session_id: String,
        /// Start time supplied by the caller.
        started_at: Instant,
    },
    /// Proof ready for chain validation and submission.
    Ready {
        /// Encoded dispute proof.
        proof_bytes: Bytes,
        /// Whether this proof was obtained after this tick started.
        just_obtained: bool,
    },
    /// Proof failed; chain validation precedes retry handling.
    NeedsRetry,
    /// Reprove requested; count preserves pre-fallback reporting.
    Reproving {
        /// Proof failure count before retry handling or its transport failure.
        retry_count: u32,
    },
    /// No further events have an effect.
    Done,
}

/// One game's dispute attempt across prover sessions and proof sources.
#[derive(Debug)]
pub struct DisputeAttempt {
    target: DisputeTarget,
    limits: AttemptLimits,
    source: AttemptSource,
    phase: AttemptPhase,
}

impl DisputeAttempt {
    /// Starts a TEE attempt, always targeting nullification.
    pub const fn awaiting_tee(
        session_id: String,
        target: DisputeTarget,
        zk_fallback: Option<ZkFallback>,
        limits: AttemptLimits,
        now: Instant,
    ) -> Self {
        Self {
            target,
            limits,
            source: AttemptSource::Tee(TeeAttempt {
                zk_fallback,
                retry_count: 0,
                submit_retry_count: 0,
            }),
            phase: AttemptPhase::Awaiting { session_id, started_at: now },
        }
    }

    /// Starts a ZK attempt with the given request and intent.
    pub const fn awaiting_zk(
        session_id: String,
        target: DisputeTarget,
        request: SnarkPlonkProofRequest,
        intent: DisputeIntent,
        limits: AttemptLimits,
        now: Instant,
    ) -> Self {
        Self {
            target,
            limits,
            source: AttemptSource::Zk(ZkAttempt { request, intent, retry_count: 0 }),
            phase: AttemptPhase::Awaiting { session_id, started_at: now },
        }
    }

    /// Applies timeout before requesting any prover I/O.
    pub fn need(&mut self, now: Instant) -> Need {
        if let AttemptPhase::Ready { just_obtained, .. } = &mut self.phase {
            *just_obtained = false;
        }
        if let AttemptPhase::Awaiting { started_at, .. } = &self.phase {
            let elapsed = now.saturating_duration_since(*started_at);
            if elapsed > self.limits.max_proof_duration {
                self.source.increment_retry();
                self.phase = AttemptPhase::NeedsRetry;
                return Need::CheckChain {
                    timeout: Some(RetryReason::Timeout {
                        elapsed,
                        limit: self.limits.max_proof_duration,
                    }),
                };
            }
        }
        match &self.phase {
            AttemptPhase::Awaiting { session_id, .. } => Need::PollProver(session_id.clone()),
            _ => Need::CheckChain { timeout: None },
        }
    }

    /// Applies external results without performing I/O or returning an error.
    pub fn apply(&mut self, event: Event, now: Instant) -> Action {
        match event {
            Event::ProverStatus(response)
                if matches!(self.phase, AttemptPhase::Awaiting { .. }) =>
            {
                let reason = match response.status {
                    ProofStatus::Queued | ProofStatus::Running => {
                        return Action::Wait(AttemptUpdate::Pending);
                    }
                    ProofStatus::Failed => RetryReason::ProverFailed(response.error_message),
                    ProofStatus::Succeeded => {
                        if let Some(result) = response.result {
                            let (source, _) = self.source.proof_source();
                            let decoded = match source {
                                AttemptProofSource::Tee => {
                                    ChallengerProofAdapter::tee_dispute_proof_bytes(
                                        result,
                                        self.target.expected_root,
                                    )
                                }
                                AttemptProofSource::Zk => {
                                    ChallengerProofAdapter::snark_plonk_dispute_proof_bytes(result)
                                }
                            };
                            match decoded {
                                Ok(proof_bytes) => {
                                    self.phase =
                                        AttemptPhase::Ready { proof_bytes, just_obtained: true };
                                    return Action::Wait(AttemptUpdate::Ready(source));
                                }
                                Err(error) if source == AttemptProofSource::Zk => {
                                    return Action::Wait(AttemptUpdate::ZkDecodeError(error));
                                }
                                Err(error) => RetryReason::TeeValidation(error),
                            }
                        } else {
                            RetryReason::Malformed
                        }
                    }
                };
                self.source.increment_retry();
                self.phase = AttemptPhase::NeedsRetry;
                Action::Wait(AttemptUpdate::NeedsRetry(reason))
            }
            Event::ChainState { status, tee_prover, zk_prover }
                if matches!(self.phase, AttemptPhase::NeedsRetry | AttemptPhase::Ready { .. }) =>
            {
                let (source, intent) = self.source.proof_source();
                if status != GameStatus::InProgress {
                    self.phase = AttemptPhase::Done;
                    return Action::Drop(DropReason::GameNotInProgress(status));
                }
                let already_resolved = match intent {
                    DisputeIntent::Challenge => {
                        zk_prover != Address::ZERO || tee_prover == Address::ZERO
                    }
                    DisputeIntent::Nullify if source == AttemptProofSource::Tee => {
                        tee_prover == Address::ZERO
                    }
                    DisputeIntent::Nullify => zk_prover == Address::ZERO,
                };
                if already_resolved {
                    self.phase = AttemptPhase::Done;
                    return Action::Drop(DropReason::AlreadyResolved {
                        intent,
                        tee_prover,
                        zk_prover,
                    });
                }
                match &self.phase {
                    AttemptPhase::Ready { proof_bytes, just_obtained } => Action::Submit {
                        proof_bytes: proof_bytes.clone(),
                        target: self.target,
                        intent,
                        source,
                        just_obtained: *just_obtained,
                    },
                    _ => self.source.reprove(&mut self.phase, self.limits, None),
                }
            }
            Event::Submitted(result) if matches!(self.phase, AttemptPhase::Ready { .. }) => {
                let error = match result {
                    Ok(()) => {
                        self.phase = AttemptPhase::Done;
                        return Action::Drop(DropReason::Disputed);
                    }
                    Err(error) => error,
                };
                let is_tee = matches!(self.source, AttemptSource::Tee(_));
                let terminal = match &error {
                    ChallengeSubmitError::KnownRevert(
                        revert @ (KnownRevert::GameAlreadyExists
                        | KnownRevert::ProofAlreadyVerified),
                    ) => Some(Action::Drop(DropReason::AlreadyResolvedRevert(*revert))),
                    ChallengeSubmitError::KnownRevert(KnownRevert::InvalidParentGame) => {
                        Some(Action::Ignore(IgnoreReason::InvalidParentGame))
                    }
                    ChallengeSubmitError::KnownRevert(KnownRevert::L1OriginTooOld) => {
                        Some(Action::Ignore(IgnoreReason::L1OriginTooOld))
                    }
                    ChallengeSubmitError::KnownRevert(KnownRevert::InvalidProof) if !is_tee => {
                        Some(Action::Ignore(IgnoreReason::ZkInvalidProof))
                    }
                    ChallengeSubmitError::KnownRevert(KnownRevert::InvalidSigner) if !is_tee => {
                        Some(Action::Drop(DropReason::ZkInvalidSigner))
                    }
                    _ => None,
                };
                if let Some(action) = terminal {
                    self.phase = AttemptPhase::Done;
                    return action;
                }
                if !is_tee {
                    return Action::Wait(AttemptUpdate::ZkSubmitRetry(error));
                }
                if matches!(
                    &error,
                    ChallengeSubmitError::KnownRevert(
                        KnownRevert::InvalidSigner | KnownRevert::InvalidProof
                    ) | ChallengeSubmitError::TxReverted { .. }
                        | ChallengeSubmitError::TxManager(TxManagerError::ExecutionReverted { .. })
                ) {
                    return self.source.reprove(
                        &mut self.phase,
                        self.limits,
                        Some(TeeSubmissionFailure::Rejected(error)),
                    );
                }
                let AttemptSource::Tee(state) = &mut self.source else { unreachable!() };
                let count = state.submit_retry_count;
                let limit = self.limits.tee_submit_retry_limit;
                let has_zk_fallback = state.zk_fallback.is_some();
                if count >= limit {
                    return self.source.reprove(
                        &mut self.phase,
                        self.limits,
                        Some(TeeSubmissionFailure::RetryLimit {
                            count,
                            limit,
                            has_zk_fallback,
                            error,
                        }),
                    );
                }
                state.submit_retry_count = count.saturating_add(1);
                Action::Wait(AttemptUpdate::TeeSubmitRetry {
                    count: state.submit_retry_count,
                    limit,
                    has_zk_fallback,
                    error,
                })
            }
            Event::ReproveStarted(result) => {
                let AttemptPhase::Reproving { retry_count } = self.phase else {
                    return Action::Wait(AttemptUpdate::Pending);
                };
                match result {
                    Ok(session_id) => {
                        self.phase = AttemptPhase::Awaiting {
                            session_id: session_id.clone(),
                            started_at: now,
                        };
                        Action::Wait(AttemptUpdate::ReproveStarted { session_id, retry_count })
                    }
                    Err(error) => {
                        self.source.increment_retry();
                        self.phase = AttemptPhase::NeedsRetry;
                        Action::Wait(AttemptUpdate::ReproveFailed { error, retry_count })
                    }
                }
            }
            _ => Action::Wait(AttemptUpdate::Pending),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        num::NonZeroU64,
        time::{Duration, Instant},
    };

    use alloy_primitives::{Address, B256};
    use base_proof_primitives::Proposal;
    use base_proof_submission::test_utils::SnarkReceiptFixture;
    use base_prover_service_protocol::{
        ProofResult, SnarkPlonkProofRequest, SnarkPlonkProofResult, TeeKind, TeeProofResult,
        ZkBackend, ZkProofRequest, ZkProofResult, ZkVm,
    };

    use super::*;

    fn request() -> SnarkPlonkProofRequest {
        SnarkPlonkProofRequest {
            proof: ZkProofRequest {
                start_block_number: 0,
                number_of_blocks_to_prove: 1,
                sequence_window: None,
                l1_head: None,
                schedule_l2_block_number: None,
                intermediate_block_interval: NonZeroU64::MIN,
                zk_vm: ZkVm::Sp1,
                zk_backend: ZkBackend::Cluster,
            },
            prover_address: Address::ZERO,
        }
    }
    fn limits() -> AttemptLimits {
        AttemptLimits {
            max_proof_duration: Duration::from_secs(10),
            tee_submit_retry_limit: 1,
            max_proof_retries: 2,
        }
    }
    fn target() -> DisputeTarget {
        DisputeTarget { invalid_index: 3, expected_root: B256::repeat_byte(0xaa) }
    }
    fn zk(now: Instant) -> DisputeAttempt {
        DisputeAttempt::awaiting_zk(
            "first".into(),
            target(),
            request(),
            DisputeIntent::Challenge,
            limits(),
            now,
        )
    }

    fn tee(now: Instant, fallback: bool) -> DisputeAttempt {
        DisputeAttempt::awaiting_tee(
            "tee".into(),
            target(),
            fallback.then(|| ZkFallback { request: request(), intent: DisputeIntent::Challenge }),
            limits(),
            now,
        )
    }
    fn success(tee: bool) -> Event {
        let result = if tee {
            ProofResult::Tee(TeeProofResult {
                aggregate_proposal: Proposal {
                    output_root: target().expected_root,
                    signature: Bytes::from(vec![0; 65]),
                    l1_origin_hash: B256::ZERO,
                    l1_origin_number: 0,
                    l2_block_number: 0,
                    prev_output_root: B256::ZERO,
                    config_hash: B256::ZERO,
                    schedule_id: B256::ZERO,
                },
                intermediate_roots: vec![],
                tee_kind: TeeKind::AwsNitro,
                tee_signer: Address::ZERO,
            })
        } else {
            ProofResult::SnarkPlonk(SnarkPlonkProofResult {
                proof: ZkProofResult {
                    zk_vm: ZkVm::Sp1,
                    proof: Bytes::from(SnarkReceiptFixture::plonk_receipt_bytes(
                        [0x5a, 0x09, 0x3a, 0x2f],
                        "abcd",
                    )),
                    execution_stats: None,
                },
            })
        };
        Event::ProverStatus(Box::new(GetProofResponse {
            status: ProofStatus::Succeeded,
            result: Some(result),
            error_message: None,
        }))
    }

    #[test]
    fn proof_results_validate_each_field_and_report_without_io() {
        let now = Instant::now();
        for is_tee in [false, true] {
            let mut attempt = if is_tee { tee(now, true) } else { zk(now) };
            for status in [ProofStatus::Queued, ProofStatus::Running] {
                assert!(matches!(
                    attempt.apply(response(status), now),
                    Action::Wait(AttemptUpdate::Pending)
                ));
                assert!(matches!(attempt.need(now), Need::PollProver(_)));
            }
            assert!(matches!(
                attempt.apply(success(is_tee), now),
                Action::Wait(AttemptUpdate::Ready(_))
            ));
            let Action::Submit { proof_bytes, .. } = attempt.apply(chain(), now) else {
                panic!("ready proof must submit")
            };
            if is_tee {
                let mut expected = vec![0; 66];
                expected[65] = 27;
                assert_eq!(proof_bytes.as_ref(), expected);
            } else {
                assert_eq!(proof_bytes.as_ref(), &[1, 0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);
            }
        }
        for bad_root in [true, false] {
            let Event::ProverStatus(mut response) = success(true) else { unreachable!() };
            let Some(ProofResult::Tee(result)) = &mut response.result else { unreachable!() };
            if bad_root {
                result.aggregate_proposal.output_root = B256::ZERO;
            } else {
                result.aggregate_proposal.signature = Bytes::new();
            }
            let mut attempt = tee(now, true);
            let Action::Wait(AttemptUpdate::NeedsRetry(RetryReason::TeeValidation(error))) =
                attempt.apply(Event::ProverStatus(response), now)
            else {
                panic!("invalid TEE proof must spend a retry")
            };
            assert!(error.to_string().contains(if bad_root {
                "unexpected output root"
            } else {
                "encoding failed"
            }));
        }
        let Event::ProverStatus(mut response) = success(false) else { unreachable!() };
        let Some(ProofResult::SnarkPlonk(result)) = &mut response.result else { unreachable!() };
        result.proof.proof = Bytes::new();
        let mut attempt = zk(now);
        assert!(matches!(
            attempt.apply(Event::ProverStatus(response), now),
            Action::Wait(AttemptUpdate::ZkDecodeError(_))
        ));
        assert!(matches!(
            attempt.need(now + Duration::from_secs(11)),
            Need::CheckChain { timeout: Some(RetryReason::Timeout { .. }) }
        ));
        assert!(matches!(
            attempt.apply(chain(), now),
            Action::Reprove(AttemptReprove { retry_count: 1, .. })
        ));
    }

    #[test]
    fn proof_decode_outcomes_and_fallback_reset_budget() {
        let now = Instant::now();
        let mut attempt = zk(now);
        assert!(matches!(
            attempt.apply(success(true), now),
            Action::Wait(AttemptUpdate::ZkDecodeError(_))
        ));
        assert!(matches!(attempt.need(now), Need::PollProver(_)));
        attempt.apply(response(ProofStatus::Failed), now);
        assert!(matches!(
            attempt.apply(chain(), now),
            Action::Reprove(AttemptReprove { retry_count: 1, .. })
        ));

        for fallback in [false, true] {
            let mut attempt = tee(now, fallback);
            assert!(matches!(
                attempt.apply(success(false), now),
                Action::Wait(AttemptUpdate::NeedsRetry(RetryReason::TeeValidation(_)))
            ));
            let action = attempt.apply(chain(), now);
            if !fallback {
                assert!(matches!(action, Action::Drop(DropReason::NoZkFallback { .. })));
                continue;
            }
            assert!(matches!(
                action,
                Action::Reprove(AttemptReprove { retry_count: 1, fallback: true, .. })
            ));
            attempt.apply(Event::ReproveStarted(Ok("zk".into())), now);
            for count in [1, 2] {
                attempt.apply(response(ProofStatus::Failed), now);
                assert!(matches!(
                    attempt.apply(chain(), now),
                    Action::Reprove(AttemptReprove { retry_count, fallback: false, .. })
                        if retry_count == count
                ));
                attempt.apply(Event::ReproveStarted(Ok("zk".into())), now);
            }
            attempt.apply(response(ProofStatus::Failed), now);
            assert!(matches!(
                attempt.apply(chain(), now),
                Action::Drop(DropReason::RetriesExhausted { retry_count: 3, .. })
            ));
        }
    }

    fn chain() -> Event {
        Event::ChainState {
            status: GameStatus::InProgress,
            tee_prover: Address::repeat_byte(1),
            zk_prover: Address::ZERO,
        }
    }
    fn response(status: ProofStatus) -> Event {
        Event::ProverStatus(Box::new(GetProofResponse {
            status,
            result: None,
            error_message: Some("prover failed".into()),
        }))
    }

    #[test]
    fn chain_state_gates_ready_and_retry_for_each_source_and_intent() {
        let now = Instant::now();
        for (is_tee, intent, resolved) in [
            (true, DisputeIntent::Nullify, [true, false, true, false]),
            (false, DisputeIntent::Challenge, [true, false, true, true]),
            (false, DisputeIntent::Nullify, [true, true, false, false]),
        ] {
            for (index, (tee_prover, zk_prover)) in [
                (Address::ZERO, Address::ZERO),
                (Address::repeat_byte(1), Address::ZERO),
                (Address::ZERO, Address::repeat_byte(2)),
                (Address::repeat_byte(1), Address::repeat_byte(2)),
            ]
            .into_iter()
            .enumerate()
            {
                for ready in [false, true] {
                    for status in [
                        GameStatus::InProgress,
                        GameStatus::ChallengerWins,
                        GameStatus::DefenderWins,
                    ] {
                        let mut attempt = if is_tee {
                            tee(now, true)
                        } else {
                            DisputeAttempt::awaiting_zk(
                                "zk".into(),
                                target(),
                                request(),
                                intent,
                                limits(),
                                now,
                            )
                        };
                        attempt.need(now);
                        attempt.apply(
                            if ready { success(is_tee) } else { response(ProofStatus::Failed) },
                            now,
                        );
                        let action =
                            attempt.apply(Event::ChainState { status, tee_prover, zk_prover }, now);
                        if status != GameStatus::InProgress {
                            assert!(matches!(
                                action,
                                Action::Drop(DropReason::GameNotInProgress(actual)) if actual == status
                            ));
                        } else if resolved[index] {
                            assert!(matches!(
                                action,
                                Action::Drop(DropReason::AlreadyResolved {
                                    intent: actual,
                                    tee_prover: t,
                                    zk_prover: z,
                                }) if actual == intent && t == tee_prover && z == zk_prover
                            ));
                        } else if ready {
                            assert!(matches!(
                                action,
                                Action::Submit {
                                    intent: actual,
                                    target: actual_target,
                                    just_obtained: true,
                                    ..
                                } if actual == intent && actual_target == target()
                            ));
                            attempt.need(now);
                            assert!(matches!(
                                attempt.apply(
                                    Event::ChainState { status, tee_prover, zk_prover },
                                    now
                                ),
                                Action::Submit { just_obtained: false, .. }
                            ));
                        } else {
                            assert!(matches!(action, Action::Reprove(_)));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn submit_errors_follow_source_specific_policy() {
        let now = Instant::now();
        for is_tee in [false, true] {
            for fallback in [false, true] {
                for (error, expected) in [
                    (ChallengeSubmitError::KnownRevert(KnownRevert::GameAlreadyExists), "resolved"),
                    (
                        ChallengeSubmitError::KnownRevert(KnownRevert::ProofAlreadyVerified),
                        "resolved",
                    ),
                    (ChallengeSubmitError::KnownRevert(KnownRevert::InvalidParentGame), "parent"),
                    (ChallengeSubmitError::KnownRevert(KnownRevert::L1OriginTooOld), "origin"),
                    (ChallengeSubmitError::KnownRevert(KnownRevert::InvalidProof), "proof"),
                    (ChallengeSubmitError::KnownRevert(KnownRevert::InvalidSigner), "signer"),
                    (ChallengeSubmitError::TxReverted { tx_hash: B256::ZERO }, "revert"),
                    (
                        ChallengeSubmitError::TxManager(TxManagerError::ExecutionReverted {
                            reason: None,
                            data: None,
                        }),
                        "revert",
                    ),
                    (ChallengeSubmitError::TxManager(TxManagerError::NonceTooLow), "generic"),
                ] {
                    let mut attempt = if is_tee { tee(now, fallback) } else { zk(now) };
                    assert!(matches!(
                        attempt.apply(success(is_tee), now),
                        Action::Wait(AttemptUpdate::Ready(_))
                    ));
                    let action = attempt.apply(Event::Submitted(Err(error)), now);
                    match expected {
                        "resolved" => assert!(matches!(
                            action,
                            Action::Drop(DropReason::AlreadyResolvedRevert(_))
                        )),
                        "parent" => assert!(matches!(
                            action,
                            Action::Ignore(IgnoreReason::InvalidParentGame)
                        )),
                        "origin" => {
                            assert!(matches!(action, Action::Ignore(IgnoreReason::L1OriginTooOld)))
                        }
                        "proof" if !is_tee => {
                            assert!(matches!(action, Action::Ignore(IgnoreReason::ZkInvalidProof)))
                        }
                        "signer" if !is_tee => {
                            assert!(matches!(action, Action::Drop(DropReason::ZkInvalidSigner)))
                        }
                        "generic" if is_tee => assert!(matches!(
                            action,
                            Action::Wait(AttemptUpdate::TeeSubmitRetry {
                                count: 1,
                                limit: 1,
                                has_zk_fallback,
                                ..
                            }) if has_zk_fallback == fallback
                        )),
                        "generic" | "revert" if !is_tee => {
                            assert!(matches!(action, Action::Wait(AttemptUpdate::ZkSubmitRetry(_))))
                        }
                        _ if fallback => assert!(matches!(
                            action,
                            Action::Reprove(AttemptReprove {
                                fallback: true,
                                submission_failure: Some(TeeSubmissionFailure::Rejected(_)),
                                ..
                            })
                        )),
                        _ => assert!(matches!(
                            action,
                            Action::Drop(DropReason::NoZkFallback {
                                submission_failure: Some(TeeSubmissionFailure::Rejected(_))
                            })
                        )),
                    }
                }
            }
        }
    }

    #[test]
    fn tee_submit_limit_falls_back_but_zk_resubmits_without_bound() {
        let now = Instant::now();
        for fallback in [false, true] {
            for limit in [0, 1, 3] {
                let mut limits = limits();
                limits.tee_submit_retry_limit = limit;
                let mut attempt = DisputeAttempt::awaiting_tee(
                    "tee".into(),
                    target(),
                    fallback.then(|| ZkFallback {
                        request: request(),
                        intent: DisputeIntent::Challenge,
                    }),
                    limits,
                    now,
                );
                attempt.apply(success(true), now);
                for count in 1..=limit {
                    assert!(matches!(
                        attempt.apply(
                            Event::Submitted(Err(ChallengeSubmitError::TxManager(
                                TxManagerError::NonceTooLow,
                            ))),
                            now,
                        ),
                        Action::Wait(AttemptUpdate::TeeSubmitRetry {
                            count: actual,
                            limit: actual_limit,
                            has_zk_fallback,
                            ..
                        }) if actual == count && actual_limit == limit && has_zk_fallback == fallback
                    ));
                    attempt.need(now);
                    assert!(matches!(
                        attempt.apply(chain(), now),
                        Action::Submit {
                            source: AttemptProofSource::Tee,
                            just_obtained: false,
                            ..
                        }
                    ));
                }
                let action = attempt.apply(
                    Event::Submitted(Err(ChallengeSubmitError::TxManager(
                        TxManagerError::NonceTooLow,
                    ))),
                    now,
                );
                if fallback {
                    assert!(matches!(
                        action,
                        Action::Reprove(AttemptReprove {
                            fallback: true,
                            retry_count: 0,
                            submission_failure: Some(TeeSubmissionFailure::RetryLimit { count, .. }),
                            ..
                        }) if count == limit
                    ));
                    attempt.apply(Event::ReproveStarted(Ok("zk".into())), now);
                    attempt.apply(success(false), now);
                    for _ in 0..100 {
                        assert!(matches!(
                            attempt.apply(
                                Event::Submitted(Err(ChallengeSubmitError::TxManager(
                                    TxManagerError::NonceTooLow
                                ))),
                                now
                            ),
                            Action::Wait(AttemptUpdate::ZkSubmitRetry(_))
                        ));
                        attempt.need(now);
                        assert!(matches!(
                            attempt.apply(chain(), now),
                            Action::Submit {
                                intent: DisputeIntent::Challenge,
                                source: AttemptProofSource::Zk,
                                ..
                            }
                        ));
                    }
                    assert!(matches!(
                        attempt.apply(Event::Submitted(Ok(())), now),
                        Action::Drop(DropReason::Disputed)
                    ));
                    assert!(matches!(
                        attempt.apply(chain(), now),
                        Action::Wait(AttemptUpdate::Pending)
                    ));
                } else {
                    assert!(matches!(
                        action,
                        Action::Drop(DropReason::NoZkFallback {
                            submission_failure: Some(TeeSubmissionFailure::RetryLimit { .. })
                        })
                    ));
                }
            }
        }
    }

    #[test]
    fn retry_budget_includes_reprove_transport_errors() {
        let now = Instant::now();
        for status in [ProofStatus::Failed, ProofStatus::Succeeded] {
            let mut attempt = zk(now);
            assert!(matches!(
                attempt.apply(response(status), now),
                Action::Wait(AttemptUpdate::NeedsRetry(_))
            ));
            for count in [1, 2] {
                assert!(matches!(
                    attempt.apply(chain(), now),
                    Action::Reprove(AttemptReprove { retry_count, fallback: false, .. })
                        if retry_count == count
                ));
                assert!(matches!(
                    attempt.apply(Event::ReproveStarted(Err(eyre::eyre!("transport"))), now),
                    Action::Wait(AttemptUpdate::ReproveFailed { retry_count, .. })
                        if retry_count == count
                ));
            }
            assert!(matches!(
                attempt.apply(chain(), now),
                Action::Drop(DropReason::RetriesExhausted { retry_count: 3, .. })
            ));
        }
    }

    #[test]
    fn timeout_precedes_poll_and_is_strict() {
        let now = Instant::now();
        for (elapsed, timed_out) in [
            (Duration::from_secs(10), false),
            (Duration::from_secs(10) + Duration::from_nanos(1), true),
        ] {
            let mut attempt = zk(now);
            let need = attempt.need(now + elapsed);
            if timed_out {
                assert!(matches!(
                    need,
                    Need::CheckChain {
                        timeout: Some(RetryReason::Timeout { elapsed: actual, limit })
                    } if actual == elapsed && limit == Duration::from_secs(10)
                ));
                assert!(matches!(attempt.need(now + elapsed), Need::CheckChain { timeout: None }));
            } else {
                assert!(matches!(need, Need::PollProver(id) if id == "first"));
            }
        }
    }
}
