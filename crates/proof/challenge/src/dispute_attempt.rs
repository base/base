//! A dispute attempt's policy, without prover, chain, or transaction I/O.
//!
//! Drive each tick by calling `need(now)` in a loop with the same `now`. A
//! different `now` starts a new tick. For `PollProver` or `CheckChain`, perform
//! that I/O and apply its response. Transport failures end the tick without an
//! event. Execute each returned `Submit` or `Reprove` and apply its result
//! before calling `need` again, including any action returned by that result.
//! `Wait` continues the loop; its update is only for reporting. `Idle` ends
//! the tick. Terminal actions remove the attempt. Out-of-order events are inert.
//!
//! The attempt requests at most one poll and one chain check per tick. A poll
//! that obtains a proof or fails proceeds to the chain check in the same tick.
//! A queued or running prover, a ZK decode error, or a completed chain check
//! leaves the tick idle. Only a proof obtained in the current tick sets
//! `Submit.just_obtained`.

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

/// Next I/O within a tick, or an indication that the tick is idle.
#[derive(Debug)]
pub enum Need {
    /// Nothing more to do this tick.
    Idle,
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

/// Reporting data accompanying a wait; use `need` to determine the next step.
#[derive(Debug)]
pub enum AttemptUpdate {
    /// The prover is queued or running, or an event is out of order.
    Pending,
    /// A proof was obtained.
    Ready(AttemptProofSource),
    /// A proof failed.
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
    /// Index of the invalid intermediate root.
    pub invalid_index: u64,
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
    /// Report the update and call `need` again with this tick's `now`.
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
    tick: Option<Instant>,
    polled: bool,
    checked_chain: bool,
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
            tick: None,
            polled: false,
            checked_chain: false,
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
            tick: None,
            polled: false,
            checked_chain: false,
            phase: AttemptPhase::Awaiting { session_id, started_at: now },
        }
    }

    /// Requests the next step, applying timeout at tick start before any prover I/O.
    ///
    /// Reuse `now` within a tick; a different value starts a new tick. Apply each
    /// requested I/O response and any resulting action's result before calling again.
    pub fn need(&mut self, now: Instant) -> Need {
        if self.tick != Some(now) {
            self.tick = Some(now);
            self.polled = false;
            self.checked_chain = false;
            if let AttemptPhase::Ready { just_obtained, .. } = &mut self.phase {
                *just_obtained = false;
            }
        }
        if self.checked_chain {
            return Need::Idle;
        }
        if let AttemptPhase::Awaiting { started_at, .. } = &self.phase {
            if self.polled {
                return Need::Idle;
            }
            let elapsed = now.saturating_duration_since(*started_at);
            if elapsed > self.limits.max_proof_duration {
                self.increment_retry();
                self.phase = AttemptPhase::NeedsRetry;
                self.checked_chain = true;
                return Need::CheckChain {
                    timeout: Some(RetryReason::Timeout {
                        elapsed,
                        limit: self.limits.max_proof_duration,
                    }),
                };
            }
        }
        match &self.phase {
            AttemptPhase::Awaiting { session_id, .. } => {
                self.polled = true;
                Need::PollProver(session_id.clone())
            }
            AttemptPhase::Ready { .. } | AttemptPhase::NeedsRetry => {
                self.checked_chain = true;
                Need::CheckChain { timeout: None }
            }
            AttemptPhase::Reproving { .. } | AttemptPhase::Done => Need::Idle,
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
                            let (source, _) = self.proof_source();
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
                self.increment_retry();
                self.phase = AttemptPhase::NeedsRetry;
                Action::Wait(AttemptUpdate::NeedsRetry(reason))
            }
            Event::ChainState { status, tee_prover, zk_prover }
                if matches!(self.phase, AttemptPhase::NeedsRetry | AttemptPhase::Ready { .. }) =>
            {
                let (source, intent) = self.proof_source();
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
                    _ => self.reprove(None),
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
                    return self.reprove(Some(TeeSubmissionFailure::Rejected(error)));
                }
                let AttemptSource::Tee(state) = &mut self.source else { unreachable!() };
                let count = state.submit_retry_count;
                let limit = self.limits.tee_submit_retry_limit;
                let has_zk_fallback = state.zk_fallback.is_some();
                if count >= limit {
                    return self.reprove(Some(TeeSubmissionFailure::RetryLimit {
                        count,
                        limit,
                        has_zk_fallback,
                        error,
                    }));
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
                        self.increment_retry();
                        self.phase = AttemptPhase::NeedsRetry;
                        Action::Wait(AttemptUpdate::ReproveFailed { error, retry_count })
                    }
                }
            }
            _ => Action::Wait(AttemptUpdate::Pending),
        }
    }

    const fn retry_count_mut(&mut self) -> &mut u32 {
        match &mut self.source {
            AttemptSource::Tee(state) => &mut state.retry_count,
            AttemptSource::Zk(state) => &mut state.retry_count,
        }
    }

    const fn increment_retry(&mut self) {
        let retry_count = self.retry_count_mut();
        *retry_count = retry_count.saturating_add(1);
    }

    const fn proof_source(&self) -> (AttemptProofSource, DisputeIntent) {
        match &self.source {
            AttemptSource::Tee(_) => (AttemptProofSource::Tee, DisputeIntent::Nullify),
            AttemptSource::Zk(state) => (AttemptProofSource::Zk, state.intent),
        }
    }

    fn reprove(&mut self, submission_failure: Option<TeeSubmissionFailure>) -> Action {
        let retry_count = *self.retry_count_mut();
        if retry_count > self.limits.max_proof_retries {
            self.phase = AttemptPhase::Done;
            return Action::Drop(DropReason::RetriesExhausted { retry_count, submission_failure });
        }
        let fallback = matches!(self.source, AttemptSource::Tee(_));
        let request = match &self.source {
            AttemptSource::Tee(state) => {
                let Some(fallback) = state.zk_fallback.clone() else {
                    self.phase = AttemptPhase::Done;
                    return Action::Drop(DropReason::NoZkFallback { submission_failure });
                };
                self.source = AttemptSource::Zk(ZkAttempt {
                    request: fallback.request.clone(),
                    intent: fallback.intent,
                    retry_count: 0,
                });
                fallback.request
            }
            AttemptSource::Zk(state) => state.request.clone(),
        };
        self.phase = AttemptPhase::Reproving { retry_count };
        Action::Reprove(AttemptReprove {
            request,
            invalid_index: self.target.invalid_index,
            retry_count,
            fallback,
            submission_failure,
        })
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

    fn drive_tick(
        attempt: &mut DisputeAttempt,
        now: Instant,
        events: Vec<Event>,
    ) -> Vec<&'static str> {
        let mut events = events.into_iter();
        let mut trace = Vec::new();
        for _ in 0..8 {
            let need = attempt.need(now);
            trace.push(match need {
                Need::Idle => "idle",
                Need::PollProver(_) => "poll",
                Need::CheckChain { timeout: None } => "chain",
                Need::CheckChain { timeout: Some(_) } => "timeout",
            });
            if matches!(need, Need::Idle) {
                assert!(events.next().is_none(), "unused scripted response");
                return trace;
            }
            let Some(event) = events.next() else {
                trace.push("transport error");
                return trace;
            };
            let mut action = attempt.apply(event, now);
            loop {
                match action {
                    Action::Wait(_) => break,
                    Action::Submit { .. } => trace.push("submit"),
                    Action::Reprove(_) => trace.push("reprove"),
                    Action::Drop(_) | Action::Ignore(_) => {
                        trace.push("remove");
                        return trace;
                    }
                }
                action = attempt.apply(events.next().expect("action result"), now);
            }
        }
        trace
    }

    fn submit_failure() -> Event {
        Event::Submitted(Err(ChallengeSubmitError::TxManager(TxManagerError::NonceTooLow)))
    }

    #[test]
    fn tick_sequences_follow_needs_without_inspecting_updates() {
        let now = Instant::now();
        let mut ready = tee(now, true);
        ready.apply(success(true), now);
        let mut retry = zk(now);
        retry.apply(response(ProofStatus::Failed), now);
        let cases = [
            ("running", zk(now), now, vec![response(ProofStatus::Running)], vec!["poll", "idle"]),
            ("decode error", zk(now), now, vec![success(true)], vec!["poll", "idle"]),
            (
                "tee success",
                tee(now, true),
                now,
                vec![success(true), chain(), submit_failure()],
                vec!["poll", "chain", "submit", "idle"],
            ),
            (
                "zk success",
                zk(now),
                now,
                vec![success(false), chain(), submit_failure()],
                vec!["poll", "chain", "submit", "idle"],
            ),
            (
                "failed",
                zk(now),
                now,
                vec![
                    response(ProofStatus::Failed),
                    chain(),
                    Event::ReproveStarted(Ok("next".into())),
                ],
                vec!["poll", "chain", "reprove", "idle"],
            ),
            (
                "tee failed",
                tee(now, true),
                now,
                vec![
                    response(ProofStatus::Failed),
                    chain(),
                    Event::ReproveStarted(Ok("fallback".into())),
                ],
                vec!["poll", "chain", "reprove", "idle"],
            ),
            (
                "needs retry",
                retry,
                now,
                vec![chain(), Event::ReproveStarted(Ok("next".into()))],
                vec!["chain", "reprove", "idle"],
            ),
            (
                "reprove error",
                zk(now),
                now,
                vec![
                    response(ProofStatus::Failed),
                    chain(),
                    Event::ReproveStarted(Err(eyre::eyre!("transport"))),
                ],
                vec!["poll", "chain", "reprove", "idle"],
            ),
            ("ready", ready, now, vec![chain(), submit_failure()], vec!["chain", "submit", "idle"]),
            (
                "timeout",
                zk(now),
                now + Duration::from_secs(11),
                vec![chain(), Event::ReproveStarted(Ok("next".into()))],
                vec!["timeout", "reprove", "idle"],
            ),
            (
                "fallback",
                tee(now, true),
                now,
                vec![
                    success(true),
                    chain(),
                    Event::Submitted(Err(ChallengeSubmitError::KnownRevert(
                        KnownRevert::InvalidProof,
                    ))),
                    Event::ReproveStarted(Ok("fallback".into())),
                ],
                vec!["poll", "chain", "submit", "reprove", "idle"],
            ),
        ];
        let mut failures = Vec::new();
        for (name, mut attempt, tick, events, expected) in cases {
            let trace = drive_tick(&mut attempt, tick, events);
            if trace != expected {
                failures.push(format!("{name}: {trace:?} != {expected:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn idle_ticks_resume_only_with_a_new_now() {
        let now = Instant::now();
        for (name, mut attempt, events, next) in [
            ("running", zk(now), vec![response(ProofStatus::Running)], "poll"),
            ("decode error", zk(now), vec![success(true)], "poll"),
            ("tee retry", tee(now, true), vec![success(true), chain(), submit_failure()], "chain"),
            ("zk retry", zk(now), vec![success(false), chain(), submit_failure()], "chain"),
            (
                "reprove",
                zk(now),
                vec![
                    response(ProofStatus::Failed),
                    chain(),
                    Event::ReproveStarted(Ok("next".into())),
                ],
                "poll",
            ),
            (
                "reprove error",
                zk(now),
                vec![
                    response(ProofStatus::Failed),
                    chain(),
                    Event::ReproveStarted(Err(eyre::eyre!("transport"))),
                ],
                "chain",
            ),
            ("done", zk(now), vec![success(false), chain(), Event::Submitted(Ok(()))], "idle"),
        ] {
            drive_tick(&mut attempt, now, events);
            for _ in 0..2 {
                assert_eq!(drive_tick(&mut attempt, now, vec![]), ["idle"], "{name}");
            }
            let expected =
                if next == "idle" { vec!["idle"] } else { vec![next, "transport error"] };
            assert_eq!(
                drive_tick(&mut attempt, now + Duration::from_secs(1), vec![]),
                expected,
                "{name}",
            );
        }
    }

    #[test]
    fn tee_proof_is_just_obtained_only_in_its_poll_tick() {
        let now = Instant::now();
        for chain_read_fails in [false, true] {
            let mut attempt = tee(now, true);
            assert!(matches!(attempt.need(now), Need::PollProver(_)));
            attempt.apply(success(true), now);
            assert!(matches!(attempt.need(now), Need::CheckChain { .. }));
            let tick = if chain_read_fails {
                let next = now + Duration::from_secs(1);
                assert!(matches!(attempt.need(next), Need::CheckChain { .. }));
                next
            } else {
                now
            };
            let Action::Submit { just_obtained, .. } = attempt.apply(chain(), tick) else {
                panic!("ready TEE proof must submit");
            };
            assert_eq!(just_obtained, !chain_read_fails);
            attempt.apply(submit_failure(), tick);
            assert!(matches!(attempt.need(tick), Need::Idle));
        }
    }

    #[test]
    fn proof_results_validate_each_field_and_report_without_io() {
        let now = Instant::now();
        for is_tee in [false, true] {
            let mut attempt = if is_tee { tee(now, true) } else { zk(now) };
            let mut tick = now;
            for status in [ProofStatus::Queued, ProofStatus::Running] {
                assert!(matches!(attempt.need(tick), Need::PollProver(_)));
                assert!(matches!(
                    attempt.apply(response(status), tick),
                    Action::Wait(AttemptUpdate::Pending)
                ));
                assert!(matches!(attempt.need(tick), Need::Idle));
                tick += Duration::from_secs(1);
            }
            assert!(matches!(attempt.need(tick), Need::PollProver(_)));
            assert!(matches!(
                attempt.apply(success(is_tee), tick),
                Action::Wait(AttemptUpdate::Ready(_))
            ));
            assert!(matches!(attempt.need(tick), Need::CheckChain { .. }));
            let Action::Submit { proof_bytes, .. } = attempt.apply(chain(), tick) else {
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
                            attempt.apply(submit_failure(), now);
                            let next_tick = now + Duration::from_secs(1);
                            attempt.need(next_tick);
                            assert!(matches!(
                                attempt.apply(
                                    Event::ChainState { status, tee_prover, zk_prover },
                                    next_tick
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
                let mut now = now;
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
                    now += Duration::from_secs(1);
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
                        now += Duration::from_secs(1);
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
    fn out_of_order_events_preserve_the_next_need_and_action() {
        let now = Instant::now();
        for phase in ["awaiting", "ready", "needs_retry", "reproving", "done"] {
            for event_kind in ["prover", "chain", "submitted", "reprove_started"] {
                if matches!(
                    (phase, event_kind),
                    ("awaiting", "prover")
                        | ("ready" | "needs_retry", "chain")
                        | ("ready", "submitted")
                        | ("reproving", "reprove_started")
                ) {
                    continue;
                }
                let mut attempt = zk(now);
                match phase {
                    "ready" | "done" => {
                        attempt.apply(success(false), now);
                        if phase == "done" {
                            attempt.apply(Event::Submitted(Ok(())), now);
                        }
                    }
                    "needs_retry" | "reproving" => {
                        attempt.apply(response(ProofStatus::Failed), now);
                        if phase == "reproving" {
                            attempt.apply(chain(), now);
                        }
                    }
                    _ => {}
                }
                let event = match event_kind {
                    "prover" => response(ProofStatus::Failed),
                    "chain" => chain(),
                    "submitted" => Event::Submitted(Ok(())),
                    _ => Event::ReproveStarted(Ok("out-of-order".into())),
                };
                let action = attempt.apply(event, now);
                assert!(
                    matches!(action, Action::Wait(AttemptUpdate::Pending)),
                    "{phase}/{event_kind}: {action:?}"
                );
                let need = attempt.need(now);
                if phase == "awaiting" {
                    assert!(matches!(need, Need::PollProver(id) if id == "first"));
                    assert!(matches!(
                        attempt.apply(success(false), now),
                        Action::Wait(AttemptUpdate::Ready(AttemptProofSource::Zk))
                    ));
                } else if matches!(phase, "reproving" | "done") {
                    assert!(matches!(need, Need::Idle));
                } else {
                    assert!(matches!(need, Need::CheckChain { timeout: None }));
                }
                match phase {
                    "awaiting" | "ready" => {
                        let Action::Submit { proof_bytes, .. } = attempt.apply(chain(), now) else {
                            panic!("out-of-order event must preserve the ready proof");
                        };
                        assert_eq!(proof_bytes.as_ref(), &[1, 0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);
                    }
                    "needs_retry" => assert!(matches!(
                        attempt.apply(chain(), now),
                        Action::Reprove(AttemptReprove { retry_count: 1, .. })
                    )),
                    "reproving" => {
                        assert!(matches!(
                            attempt.apply(Event::ReproveStarted(Ok("next".into())), now),
                            Action::Wait(AttemptUpdate::ReproveStarted { retry_count: 1, .. })
                        ));
                        let next_tick = now + Duration::from_secs(1);
                        let need = attempt.need(next_tick);
                        assert!(matches!(need, Need::PollProver(id) if id == "next"));
                    }
                    _ => assert!(matches!(
                        attempt.apply(chain(), now),
                        Action::Wait(AttemptUpdate::Pending)
                    )),
                }
            }
        }
    }

    #[test]
    fn retry_before_first_proof_preserves_dispute_index() {
        let now = Instant::now();
        for is_tee in [false, true] {
            for timed_out in [false, true] {
                let mut attempt = if is_tee { tee(now, true) } else { zk(now) };
                if timed_out {
                    assert!(matches!(
                        attempt.need(now + Duration::from_secs(11)),
                        Need::CheckChain { timeout: Some(_) }
                    ));
                } else {
                    attempt.apply(response(ProofStatus::Failed), now);
                }
                let Action::Reprove(reprove) = attempt.apply(chain(), now) else {
                    panic!("failed initial proof must request a reprove");
                };
                assert_eq!(reprove.invalid_index, 3);
            }
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
                assert!(matches!(attempt.need(now + elapsed), Need::Idle));
            } else {
                assert!(matches!(need, Need::PollProver(id) if id == "first"));
            }
        }
    }
}
