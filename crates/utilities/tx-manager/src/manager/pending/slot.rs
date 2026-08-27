//! State and transitions for one pending transaction nonce.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, Bytes, U256};
use tokio::sync::oneshot;

use super::super::{
    build::PreparedTx,
    publisher::{PublishOutcome, PublishReject, PublisherEvent, PublisherId, PublisherTx},
    sweep::{SweepOutcome, SweepTarget},
};
use super::work::{
    PendingPolicy, PendingWork, PublishedAttempt, RejectionVerdict, ReplacementReason,
    StagedSubmission, VersionId, VersionKind,
};
use crate::{
    SubmissionCompletion, SubmissionId, SubmissionStatus, TxCandidate, TxManagerError,
    TxManagerResult,
};

/// One signed transaction version and its current publication pass.
#[derive(Debug, Clone)]
pub struct SignedVersion {
    /// Monotonic identity within this nonce slot.
    id: VersionId,
    /// Semantic purpose used to interpret canonical confirmation.
    kind: VersionKind,
    /// Transaction intent represented by this signed version.
    candidate: TxCandidate,
    /// Immutable signed bytes shared by every backend.
    prepared: PreparedTx,
    /// Coordinator-controlled publication pass.
    epoch: u64,
    /// Time at which the current pass opened.
    pass_opened_at: Duration,
    /// Latest outcome from each backend for the current pass.
    outcomes: Vec<Option<PublishOutcome>>,
    /// First time this exact version became potentially live.
    live_at: Option<Duration>,
}

impl SignedVersion {
    /// Creates a signed version ready for immediate publication.
    pub fn new(
        id: VersionId,
        kind: VersionKind,
        candidate: TxCandidate,
        prepared: PreparedTx,
        publisher_count: usize,
        now: Duration,
    ) -> Self {
        Self {
            id,
            kind,
            candidate,
            prepared,
            epoch: 0,
            pass_opened_at: now,
            outcomes: vec![None; publisher_count],
            live_at: None,
        }
    }

    /// Returns whether this version can record an event from `publisher`.
    pub const fn contains_publisher(&self, publisher: PublisherId) -> bool {
        publisher.index() < self.outcomes.len()
    }

    /// Records an outcome when it belongs to this version and pass.
    pub fn record(&mut self, event: &PublisherEvent) -> bool {
        if self.id != event.version || self.epoch != event.epoch {
            return false;
        }
        let Some(outcome) = self.outcomes.get_mut(event.publisher.index()) else {
            return false;
        };
        *outcome = Some(event.outcome.clone());
        true
    }

    /// Returns whether every configured backend completed this pass.
    pub fn all_responded(&self) -> bool {
        self.outcomes.iter().all(Option::is_some)
    }

    /// Returns whether every configured backend accepted this version.
    pub fn all_accepted(&self) -> bool {
        self.outcomes.iter().all(|outcome| matches!(outcome, Some(PublishOutcome::Accepted)))
    }

    /// Returns the definitive rejections from a fully rejected pass.
    pub fn rejections(&self) -> Option<Vec<PublishReject>> {
        self.outcomes
            .iter()
            .map(|outcome| match outcome {
                Some(PublishOutcome::Rejected(rejection)) => Some(rejection.clone()),
                _ => None,
            })
            .collect()
    }

    /// Opens another publication pass while preserving accepted backends.
    pub fn open_next_pass(&mut self, now: Duration) {
        self.epoch = self.epoch.saturating_add(1);
        self.pass_opened_at = now;
        for outcome in &mut self.outcomes {
            if !matches!(outcome, Some(PublishOutcome::Accepted)) {
                *outcome = None;
            }
        }
    }

    /// Returns whether another pass may start.
    pub fn next_pass_due(&self, now: Duration, delay: Duration) -> bool {
        self.all_responded()
            && !self.all_accepted()
            && now >= self.pass_opened_at.saturating_add(delay)
    }

    /// Returns the publisher-facing representation of this version.
    pub fn publisher_tx(&self, submission_id: SubmissionId, nonce: u64) -> PublisherTx {
        PublisherTx {
            submission_id,
            nonce,
            version: self.id,
            epoch: self.epoch,
            kind: self.kind,
            prepared: self.prepared.clone(),
        }
    }
}

/// Whether a nonce can still be recycled or is irrevocably committed.
#[derive(Debug)]
pub enum SlotState {
    /// Every completed publication pass was rejected definitively.
    Provisional {
        /// Time after which no additional publication pass may start.
        deadline: Option<Duration>,
        /// Most useful rejection from the latest completed pass.
        last_rejection: Option<TxManagerError>,
    },
    /// At least one backend may have made this nonce live.
    Committed,
}

/// Preparation state for the next signed version.
#[derive(Debug, Clone)]
pub enum ReplacementState {
    /// No replacement is requested or being prepared.
    Idle,
    /// A replacement is waiting for the coordinator to start its worker.
    Requested {
        /// Why another signed version is needed.
        reason: ReplacementReason,
        /// Candidate to encode into the replacement.
        candidate: TxCandidate,
        /// Nonce to encode into the replacement.
        nonce: u64,
    },
    /// A worker is preparing a replacement from this version.
    Preparing {
        /// Version that must still be active when preparation finishes.
        base_version: VersionId,
        /// Reason attached to the in-flight worker.
        reason: ReplacementReason,
    },
}

/// Account nonce refresh state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceFetch {
    /// No account nonce read is needed.
    Idle,
    /// The next planning pass must start an account nonce read.
    Required,
    /// A worker is currently reading the account nonce.
    InFlight,
}

/// Caller waiting for a cancellation transaction to become potentially live.
#[derive(Debug)]
pub struct CancelRequest {
    /// Self-transfer candidate used to replace the active transaction.
    candidate: TxCandidate,
    /// Completion channel for the cancellation request.
    result: oneshot::Sender<TxManagerResult<()>>,
}

/// Work and ledger mutation selected by one slot planning pass.
#[derive(Debug, Default)]
pub struct SlotPlan {
    /// Worker action selected for this slot.
    pub work: Option<PendingWork>,
    /// Whether publishers need a new ledger snapshot.
    pub snapshot_changed: bool,
    /// Terminal error for a safely recyclable provisional slot.
    pub failed: Option<TxManagerError>,
}

/// Ledger-level effects produced by one slot transition.
#[derive(Debug, Default)]
pub struct SlotEffects {
    /// Whether publishers need a new ledger snapshot.
    pub snapshot_changed: bool,
    /// Whether canonical state should be inspected promptly.
    pub sweep_requested: bool,
    /// Terminal error for a safely recyclable provisional slot.
    pub failed: Option<TxManagerError>,
    /// Minimum nonce that the ledger must reserve.
    pub next_nonce_at_least: Option<u64>,
}

/// Complete local state for one nonce and one logical caller submission.
#[derive(Debug)]
pub struct NonceSlot {
    /// Stable identity of the logical caller submission.
    submission_id: SubmissionId,
    /// Account nonce exclusively represented by this slot.
    nonce: u64,
    /// Whether clean nonce recycling remains legal.
    state: SlotState,
    /// Current version exposed atomically to every publisher.
    active: SignedVersion,
    /// Last potentially-live version retained while replacing it.
    previous: Option<SignedVersion>,
    /// Next monotonic version identifier reserved for this slot.
    next_version: VersionId,
    /// Unique hashes that may have reached a backend.
    attempts: Vec<PublishedAttempt>,
    /// Original caller completion retained until canonical resolution.
    completion: Option<SubmissionCompletion>,
    /// Preparation state for the next signed version.
    replacement: ReplacementState,
    /// Earliest time at which another replacement may be prepared.
    replacement_ready_at: Duration,
    /// Account nonce refresh state.
    nonce_fetch: NonceFetch,
    /// Explicit cancellation request, if any.
    cancel: Option<CancelRequest>,
}

impl NonceSlot {
    /// Creates a provisional nonce slot from a freshly prepared submission.
    pub fn new(
        staged: StagedSubmission,
        prepared: PreparedTx,
        publisher_count: usize,
        policy: PendingPolicy,
        now: Duration,
    ) -> Self {
        let deadline = policy.tx_not_in_mempool_timeout.map(|timeout| now.saturating_add(timeout));
        let active = SignedVersion::new(
            VersionId::INITIAL,
            staged.kind,
            staged.candidate,
            prepared,
            publisher_count,
            now,
        );
        let slot = Self {
            submission_id: staged.id,
            nonce: active.prepared.nonce,
            state: SlotState::Provisional { deadline, last_rejection: None },
            active,
            previous: None,
            next_version: VersionId::INITIAL.next(),
            attempts: Vec::new(),
            completion: Some(staged.completion),
            replacement: ReplacementState::Idle,
            replacement_ready_at: now,
            nonce_fetch: NonceFetch::Idle,
            cancel: None,
        };
        slot.notify_status();
        slot
    }

    /// Returns the logical submission identifier.
    pub const fn submission_id(&self) -> SubmissionId {
        self.submission_id
    }

    /// Returns the nonce represented by this slot.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Returns the active signed version identifier.
    pub const fn active_version(&self) -> VersionId {
        self.active.id
    }

    /// Returns whether no publication can yet have made this nonce live.
    pub const fn is_provisional(&self) -> bool {
        matches!(self.state, SlotState::Provisional { .. })
    }

    /// Returns whether this nonce may already be live.
    pub const fn is_committed(&self) -> bool {
        matches!(self.state, SlotState::Committed)
    }

    /// Returns the number of potentially-live hashes retained for sweeping.
    pub const fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    /// Returns this slot's canonical sweep input.
    pub fn sweep_target(&self) -> SweepTarget {
        SweepTarget {
            submission_id: self.submission_id,
            nonce: self.nonce,
            attempts: self.attempts.clone(),
        }
    }

    /// Returns the current transaction exposed to publisher workers.
    pub fn publisher_tx(&self) -> PublisherTx {
        self.active.publisher_tx(self.submission_id, self.nonce)
    }

    /// Selects at most one worker action or publication pass transition.
    pub fn plan(&mut self, now: Duration, policy: PendingPolicy) -> SlotPlan {
        // Phase 1: finish nonce recovery before changing or republishing this slot.
        if matches!(self.nonce_fetch, NonceFetch::Required) {
            self.nonce_fetch = NonceFetch::InFlight;
            return SlotPlan {
                work: Some(PendingWork::FetchAccountNonce {
                    submission_id: self.submission_id,
                    version: self.active.id,
                }),
                ..Default::default()
            };
        }
        if matches!(self.nonce_fetch, NonceFetch::InFlight) {
            return SlotPlan::default();
        }

        // Phase 2: choose the next replacement. An explicit cancellation takes
        // priority over the normal timeout-based fee bump.
        if matches!(self.replacement, ReplacementState::Idle)
            && !matches!(self.active.kind, VersionKind::Cancel)
            && self.active.all_responded()
            && let Some(cancel) = self.cancel.as_ref()
        {
            self.replacement = ReplacementState::Requested {
                reason: ReplacementReason::Cancel,
                candidate: cancel.candidate.clone(),
                nonce: self.nonce,
            };
        }

        if matches!(self.replacement, ReplacementState::Idle)
            && matches!(self.state, SlotState::Committed)
            && self.active.all_responded()
            && now >= self.replacement_ready_at
            && self
                .active
                .live_at
                .is_some_and(|live_at| now >= live_at.saturating_add(policy.resubmission_timeout))
        {
            self.request_replacement(
                ReplacementReason::FeeBump,
                self.active.candidate.clone(),
                self.nonce,
            );
        }

        // Phase 3: hand one requested replacement to the builder. The baseline
        // version lets us discard a stale worker result safely.
        if let ReplacementState::Requested { reason, candidate, nonce } = self.replacement.clone() {
            let base_version = self.active.id;
            self.replacement = ReplacementState::Preparing { base_version, reason };
            return SlotPlan {
                work: Some(PendingWork::PrepareReplacementTx {
                    submission_id: self.submission_id,
                    base_version,
                    nonce,
                    candidate,
                    base: self.active.prepared.clone(),
                    reason,
                }),
                ..Default::default()
            };
        }
        if matches!(self.replacement, ReplacementState::Preparing { .. }) {
            return SlotPlan::default();
        }

        // Phase 4: recycle a provisional nonce only after every publisher
        // finished its pass. Otherwise, open the next due publication pass.
        if self.provisional_deadline_reached(now) && self.active.all_responded() {
            return SlotPlan { failed: Some(self.last_rejection()), ..Default::default() };
        }

        if self.active.next_pass_due(now, policy.publish_retry_delay) {
            self.active.open_next_pass(now);
            return SlotPlan { snapshot_changed: true, ..Default::default() };
        }

        SlotPlan::default()
    }

    /// Applies one publisher result and returns its ledger-level effects.
    pub fn record_publish(&mut self, event: &PublisherEvent, now: Duration) -> SlotEffects {
        // Phase 1: reject an invalid publisher index, then apply current-version
        // scheduling state. Stale version results remain useful below.
        if !self.active.contains_publisher(event.publisher) {
            return SlotEffects::default();
        }

        let potentially_live =
            matches!(event.outcome, PublishOutcome::Accepted | PublishOutcome::Ambiguous);
        let current = self.active.record(event);
        let mut effects = SlotEffects::default();

        // Phase 2: Accepted and Ambiguous both mean the nonce may be live.
        // This remains true for a late result from an older signed version.
        if potentially_live {
            self.record_attempt(event.version, event.kind, event.tx_hash);
            if current {
                self.active.live_at.get_or_insert(now);
                self.previous = None;
            }
            if self.is_provisional() {
                self.state = SlotState::Committed;
                effects.next_nonce_at_least = Some(self.nonce.saturating_add(1));
            }
            if matches!(event.kind, VersionKind::Cancel) {
                self.finish_cancel(Ok(()));
                if matches!(self.completion, Some(SubmissionCompletion::Cancel(_)))
                    && let Some(SubmissionCompletion::Cancel(result)) = self.completion.take()
                {
                    let _ = result.send(Ok(()));
                }
            }
            self.notify_status();
        }

        if !current {
            return effects;
        }

        // Phase 3: only a complete pass made entirely of definitive rejections
        // may drive nonce recovery, replacement, retry, or safe failure.
        let Some(rejections) = self.active.rejections() else {
            return effects;
        };
        let verdict = PublishReject::verdict(&rejections);

        if let SlotState::Provisional { last_rejection, .. } = &mut self.state {
            *last_rejection = Some(verdict.error());
            if self.provisional_deadline_reached(now) {
                effects.failed = Some(verdict.error());
                return effects;
            }
            match verdict {
                RejectionVerdict::NonceTooLow => self.nonce_fetch = NonceFetch::Required,
                RejectionVerdict::Deterministic(error) => effects.failed = Some(error),
                RejectionVerdict::FeeTooLow(_) if self.cancel.is_none() => self
                    .request_replacement(
                        ReplacementReason::FeeBump,
                        self.active.candidate.clone(),
                        self.nonce,
                    ),
                RejectionVerdict::FeeTooLow(_) | RejectionVerdict::Retry(_) => {}
            }
            return effects;
        }

        match verdict {
            RejectionVerdict::NonceTooLow => effects.sweep_requested = true,
            RejectionVerdict::Deterministic(error) => {
                self.restore_previous(error, now, &mut effects)
            }
            RejectionVerdict::FeeTooLow(_) if self.cancel.is_none() => self.request_replacement(
                ReplacementReason::FeeBump,
                self.active.candidate.clone(),
                self.nonce,
            ),
            RejectionVerdict::FeeTooLow(_) | RejectionVerdict::Retry(_) => {}
        }
        effects
    }

    /// Applies a replacement worker result if its baseline is still current.
    pub fn replacement_prepared(
        &mut self,
        base_version: VersionId,
        reason: ReplacementReason,
        result: TxManagerResult<PreparedTx>,
        now: Duration,
        policy: PendingPolicy,
    ) -> SlotEffects {
        // Ignore a worker result if its version or reason was superseded while
        // transaction preparation was in flight.
        if !matches!(
            self.replacement,
            ReplacementState::Preparing {
                base_version: current,
                reason: current_reason,
            } if current == base_version && current_reason == reason
        ) || self.active.id != base_version
        {
            return SlotEffects::default();
        }
        self.replacement = ReplacementState::Idle;

        // A provisional slot with no potentially-live hash can still fail
        // safely. Committed slots retain their active version and retry later.
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) if self.is_provisional() && self.attempts.is_empty() => {
                return SlotEffects { failed: Some(error), ..Default::default() };
            }
            Err(error) => {
                self.replacement_ready_at = now.saturating_add(policy.resubmission_timeout);
                if matches!(reason, ReplacementReason::Cancel) {
                    self.finish_cancel(Err(error));
                }
                return SlotEffects::default();
            }
        };

        // Install the new signed bytes atomically. Keep the outgoing version
        // only when it may still be live and the replacement is not live yet.
        let candidate = if matches!(reason, ReplacementReason::Cancel) {
            self.cancel
                .as_ref()
                .map(|cancel| cancel.candidate.clone())
                .unwrap_or_else(|| self.active.candidate.clone())
        } else {
            self.active.candidate.clone()
        };
        let kind = reason.version_kind(self.active.kind);
        if matches!(reason, ReplacementReason::Resign) {
            self.nonce = prepared.nonce;
        } else {
            debug_assert_eq!(
                prepared.nonce, self.nonce,
                "same-slot replacement must preserve its nonce"
            );
        }

        let replacement = SignedVersion::new(
            self.next_version,
            kind,
            candidate,
            prepared,
            self.active.outcomes.len(),
            now,
        );
        self.next_version = self.next_version.next();
        let previous = std::mem::replace(&mut self.active, replacement);
        if previous.live_at.is_some() {
            self.previous = Some(previous);
        }
        self.replacement_ready_at = now;
        self.notify_status();
        SlotEffects { snapshot_changed: true, ..Default::default() }
    }

    /// Applies the account nonce returned by a nonce recovery worker.
    pub fn record_account_nonce(
        &mut self,
        result: TxManagerResult<u64>,
        _now: Duration,
    ) -> SlotEffects {
        // The read belongs to the pass that requested it. A duplicate or stale
        // worker result must not change replacement state.
        if !matches!(self.nonce_fetch, NonceFetch::InFlight) {
            return SlotEffects::default();
        }
        self.nonce_fetch = NonceFetch::Idle;

        // Failed and unchanged reads simply allow another publication pass.
        // A newer nonce either triggers re-signing or canonical resolution.
        let Ok(latest) = result else {
            return SlotEffects::default();
        };
        if latest <= self.nonce {
            return SlotEffects::default();
        }
        if matches!(self.state, SlotState::Committed) {
            return SlotEffects { sweep_requested: true, ..Default::default() };
        }

        self.request_replacement(ReplacementReason::Resign, self.active.candidate.clone(), latest);
        SlotEffects { next_nonce_at_least: Some(latest), ..Default::default() }
    }

    /// Requests cancellation or returns the caller when another request owns the slot.
    pub fn request_cancel(
        &mut self,
        sender: Address,
        result: oneshot::Sender<TxManagerResult<()>>,
    ) -> Result<(), oneshot::Sender<TxManagerResult<()>>> {
        // A cancel version that is already active satisfies the API contract:
        // cancellation bytes may already be live.
        if matches!(self.active.kind, VersionKind::Cancel) {
            let _ = result.send(Ok(()));
            return Ok(());
        }
        if self.cancel.is_some() {
            return Err(result);
        }

        self.cancel = Some(CancelRequest {
            candidate: TxCandidate {
                tx_data: Bytes::new(),
                blobs: Arc::clone(&self.active.candidate.blobs),
                to: Some(sender),
                gas_limit: 0,
                value: U256::ZERO,
            },
            result,
        });
        Ok(())
    }

    /// Resolves this slot from canonical sweep evidence.
    pub fn resolve(mut self, outcome: SweepOutcome) {
        let result = match outcome {
            SweepOutcome::Confirmed { kind: VersionKind::Cancel, .. } => {
                Err(TxManagerError::Cancelled)
            }
            SweepOutcome::Confirmed { receipt, .. } => Ok(*receipt),
            SweepOutcome::Superseded => Err(TxManagerError::Superseded),
        };
        if let Some(completion) = self.completion.take() {
            let cancellation_confirmed = matches!(result, Err(TxManagerError::Cancelled));
            completion.finish(result, cancellation_confirmed);
        }
        self.finish_cancel(Ok(()));
    }

    /// Resolves this safely recyclable slot with a terminal error.
    pub fn fail(mut self, error: TxManagerError) {
        if let Some(completion) = self.completion.take() {
            completion.finish(Err(error.clone()), false);
        }
        self.finish_cancel(Err(error));
    }

    /// Returns whether every backend accepted the active version.
    pub fn all_accepted(&self) -> bool {
        self.active.all_accepted()
    }

    /// Records a replacement request without starting external work.
    fn request_replacement(
        &mut self,
        reason: ReplacementReason,
        candidate: TxCandidate,
        nonce: u64,
    ) {
        self.replacement = ReplacementState::Requested { reason, candidate, nonce };
    }

    /// Returns whether the provisional admission window has elapsed.
    fn provisional_deadline_reached(&self, now: Duration) -> bool {
        matches!(
            self.state,
            SlotState::Provisional { deadline: Some(deadline), .. } if now >= deadline
        )
    }

    /// Returns the latest rejection used when the admission deadline expires.
    fn last_rejection(&self) -> TxManagerError {
        match &self.state {
            SlotState::Provisional { last_rejection: Some(error), .. } => error.clone(),
            _ => TxManagerError::ChannelClosed,
        }
    }

    /// Retains one potentially-live hash for canonical resolution.
    fn record_attempt(&mut self, version: VersionId, kind: VersionKind, hash: B256) {
        if self.attempts.iter().all(|attempt| attempt.hash != hash) {
            self.attempts.push(PublishedAttempt { version, kind, hash });
        }
    }

    /// Restores the last potentially-live version after a clean replacement failure.
    fn restore_previous(
        &mut self,
        error: TxManagerError,
        now: Duration,
        effects: &mut SlotEffects,
    ) {
        let rejected_kind = self.active.kind;
        if let Some(mut previous) = self.previous.take() {
            previous.open_next_pass(now);
            self.active = previous;
        } else {
            self.active.open_next_pass(now);
        }
        self.replacement = ReplacementState::Idle;
        self.replacement_ready_at = now;
        if matches!(rejected_kind, VersionKind::Cancel) {
            self.finish_cancel(Err(error));
        }
        self.notify_status();
        effects.snapshot_changed = true;
    }

    /// Completes and clears an explicit cancellation request.
    fn finish_cancel(&mut self, result: TxManagerResult<()>) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.result.send(result);
        }
    }

    /// Publishes the latest nonce and version to the submission handle.
    fn notify_status(&self) {
        if let Some(SubmissionCompletion::Transaction(tracker)) = self.completion.as_ref() {
            tracker.update(SubmissionStatus::Pending {
                nonce: self.nonce,
                version: self.active.id.get(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SubmissionHandle;

    fn policy() -> PendingPolicy {
        PendingPolicy {
            publish_retry_delay: Duration::from_secs(1),
            resubmission_timeout: Duration::from_secs(10),
            tx_not_in_mempool_timeout: Some(Duration::from_secs(5)),
        }
    }

    fn candidate() -> TxCandidate {
        TxCandidate { to: Some(Address::with_last_byte(1)), ..Default::default() }
    }

    fn prepared(nonce: u64, marker: u8) -> PreparedTx {
        PreparedTx {
            raw_tx: Bytes::from(vec![marker]),
            tx_hash: B256::with_last_byte(marker),
            gas_tip_cap: 1,
            gas_fee_cap: 2,
            blob_fee_cap: None,
            gas_limit: 21_000,
            nonce,
            sidecar: None,
        }
    }

    fn slot(publisher_count: usize) -> (NonceSlot, SubmissionHandle) {
        let (staged, handle) = StagedSubmission::new(SubmissionId::new(1), candidate());
        (NonceSlot::new(staged, prepared(0, 1), publisher_count, policy(), Duration::ZERO), handle)
    }

    fn event(slot: &NonceSlot, publisher: usize, outcome: PublishOutcome) -> PublisherEvent {
        PublisherEvent {
            publisher: PublisherId::new(publisher),
            submission_id: slot.submission_id,
            version: slot.active.id,
            epoch: slot.active.epoch,
            kind: slot.active.kind,
            tx_hash: slot.active.prepared.tx_hash,
            outcome,
        }
    }

    #[test]
    fn expired_deadline_waits_for_every_backend() {
        let (mut slot, _) = slot(2);
        let rejection =
            PublishOutcome::Rejected(PublishReject::Transient(TxManagerError::TxPoolFull));

        let first =
            slot.record_publish(&event(&slot, 0, rejection.clone()), Duration::from_secs(6));
        assert!(first.failed.is_none());

        let second = slot.record_publish(&event(&slot, 1, rejection), Duration::from_secs(6));
        assert_eq!(second.failed, Some(TxManagerError::TxPoolFull));
    }

    #[test]
    fn live_replacement_discards_previous_version() {
        let (mut slot, _) = slot(2);
        slot.record_publish(&event(&slot, 0, PublishOutcome::Accepted), Duration::ZERO);
        slot.record_publish(&event(&slot, 1, PublishOutcome::Accepted), Duration::ZERO);

        let plan = slot.plan(Duration::from_secs(10), policy());
        let Some(PendingWork::PrepareReplacementTx { base_version, reason, .. }) = plan.work else {
            panic!("unconfirmed live transaction should request a fee bump")
        };
        slot.replacement_prepared(
            base_version,
            reason,
            Ok(prepared(0, 2)),
            Duration::from_secs(10),
            policy(),
        );
        assert!(slot.previous.is_some());

        slot.record_publish(&event(&slot, 0, PublishOutcome::Accepted), Duration::from_secs(10));
        assert!(slot.previous.is_none());
    }

    #[test]
    fn nonce_fetch_requests_resigning_without_exposing_old_bytes_at_new_nonce() {
        let (mut slot, _) = slot(1);
        slot.record_publish(
            &event(&slot, 0, PublishOutcome::Rejected(PublishReject::NonceTooLow)),
            Duration::ZERO,
        );
        assert!(matches!(
            slot.plan(Duration::ZERO, policy()).work,
            Some(PendingWork::FetchAccountNonce { .. })
        ));

        let effects = slot.record_account_nonce(Ok(5), Duration::ZERO);
        assert_eq!(effects.next_nonce_at_least, Some(5));
        assert_eq!(slot.publisher_tx().nonce, 0);
        assert!(matches!(
            slot.plan(Duration::ZERO, policy()).work,
            Some(PendingWork::PrepareReplacementTx {
                nonce: 5,
                reason: ReplacementReason::Resign,
                ..
            })
        ));
    }

    #[test]
    fn cancellation_takes_priority_over_fee_replacement() {
        let (mut slot, _) = slot(1);
        let (result, _result_rx) = oneshot::channel();
        slot.request_cancel(Address::with_last_byte(9), result).unwrap();
        slot.record_publish(
            &event(
                &slot,
                0,
                PublishOutcome::Rejected(PublishReject::FeeTooLow(TxManagerError::Underpriced)),
            ),
            Duration::ZERO,
        );

        assert!(matches!(
            slot.plan(Duration::ZERO, policy()).work,
            Some(PendingWork::PrepareReplacementTx { reason: ReplacementReason::Cancel, .. })
        ));
    }

    #[test]
    fn one_underpriced_backend_waits_for_normal_resubmission_timeout() {
        let (mut slot, _) = slot(2);
        slot.record_publish(&event(&slot, 0, PublishOutcome::Accepted), Duration::ZERO);
        slot.record_publish(
            &event(
                &slot,
                1,
                PublishOutcome::Rejected(PublishReject::FeeTooLow(TxManagerError::Underpriced)),
            ),
            Duration::ZERO,
        );

        assert!(slot.plan(Duration::ZERO, policy()).work.is_none());
        assert!(matches!(
            slot.plan(Duration::from_secs(10), policy()).work,
            Some(PendingWork::PrepareReplacementTx { reason: ReplacementReason::FeeBump, .. })
        ));
    }

    #[test]
    fn failed_replacement_preparation_is_deferred() {
        let (mut slot, _) = slot(1);
        slot.record_publish(&event(&slot, 0, PublishOutcome::Accepted), Duration::ZERO);

        let plan = slot.plan(Duration::from_secs(10), policy());
        let Some(PendingWork::PrepareReplacementTx { base_version, reason, .. }) = plan.work else {
            panic!("unconfirmed live transaction should request a fee bump")
        };
        slot.replacement_prepared(
            base_version,
            reason,
            Err(TxManagerError::Transport("unavailable".to_string())),
            Duration::from_secs(10),
            policy(),
        );

        assert!(slot.plan(Duration::from_secs(10), policy()).work.is_none());
        assert!(matches!(
            slot.plan(Duration::from_secs(20), policy()).work,
            Some(PendingWork::PrepareReplacementTx { reason: ReplacementReason::FeeBump, .. })
        ));
    }
}
