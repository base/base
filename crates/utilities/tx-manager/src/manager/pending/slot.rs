//! State and transitions for one pending transaction nonce.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
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
    },
    /// At least one backend may have made this nonce live.
    Committed,
}

/// Preparation state for the next signed version.
#[derive(Debug, Clone, Copy)]
pub enum ReplacementState {
    /// No replacement is requested or being prepared.
    Idle,
    /// A replacement is waiting for the coordinator to start its worker.
    Requested {
        /// Why another signed version is needed.
        reason: ReplacementReason,
        /// Nonce to encode into the replacement.
        nonce: u64,
    },
    /// A worker is preparing a replacement from this version.
    Preparing {
        /// Version that must still be active when preparation finishes.
        base_version: VersionId,
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

/// Ledger-level effects produced by one slot transition or planning pass.
#[derive(Debug, Default)]
pub struct SlotEffects {
    /// Worker action selected for this slot, only ever set by planning.
    pub work: Option<PendingWork>,
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
            state: SlotState::Provisional { deadline },
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
    ///
    /// Every replacement originates here so a single place decides what the slot
    /// needs next: cancel, fee bump, re-sign, retry, or safe failure.
    pub fn plan(&mut self, now: Duration, policy: PendingPolicy) -> SlotEffects {
        // Phase 1: finish nonce recovery before changing or republishing this slot.
        if matches!(self.nonce_fetch, NonceFetch::Required) {
            self.nonce_fetch = NonceFetch::InFlight;
            return SlotEffects {
                work: Some(PendingWork::FetchAccountNonce { submission_id: self.submission_id }),
                ..Default::default()
            };
        }
        if matches!(self.nonce_fetch, NonceFetch::InFlight) {
            return SlotEffects::default();
        }

        // Phase 2: choose the next replacement. An explicit cancellation takes
        // priority over the fee bump that resolves fee rejections and stale
        // unconfirmed transactions. A reached deadline fails instead of bumping.
        if matches!(self.replacement, ReplacementState::Idle)
            && !matches!(self.active.kind, VersionKind::Cancel)
            && self.active.all_responded()
            && self.cancel.is_some()
        {
            self.request_replacement(ReplacementReason::Cancel, self.nonce);
        }

        if matches!(self.replacement, ReplacementState::Idle)
            && self.active.all_responded()
            && now >= self.replacement_ready_at
            && !self.provisional_deadline_reached(now)
        {
            let fee_rejected = self.active.rejections().is_some_and(|rejections| {
                matches!(PublishReject::verdict(&rejections), RejectionVerdict::FeeTooLow(_))
            });
            let stale = self.is_committed()
                && self.active.live_at.is_some_and(|live_at| {
                    now >= live_at.saturating_add(policy.resubmission_timeout)
                });
            if fee_rejected || stale {
                self.request_replacement(ReplacementReason::FeeBump, self.nonce);
            }
        }

        // Phase 3: hand one requested replacement to the builder. The baseline
        // version lets us discard a stale worker result safely.
        if let ReplacementState::Requested { reason, nonce } = self.replacement {
            let base_version = self.active.id;
            let candidate = self.replacement_candidate(reason);
            self.replacement = ReplacementState::Preparing { base_version };
            return SlotEffects {
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
            return SlotEffects::default();
        }

        // Phase 4: recycle a provisional nonce once every publisher finished its
        // pass; each response is a definitive rejection, so a verdict exists.
        if self.provisional_deadline_reached(now)
            && let Some(rejections) = self.active.rejections()
        {
            return SlotEffects {
                failed: Some(PublishReject::verdict(&rejections).error()),
                ..Default::default()
            };
        }

        // Phase 5: open the next due publication pass.
        if self.active.next_pass_due(now, policy.publish_retry_delay) {
            self.active.open_next_pass(now);
            return SlotEffects { snapshot_changed: true, ..Default::default() };
        }

        SlotEffects::default()
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

        // Phase 3: nothing is decided while a replacement is requested or being
        // built. The next version arrives with fresh outcomes and is reassessed.
        if !matches!(self.replacement, ReplacementState::Idle) {
            return effects;
        }

        // Only a complete pass made entirely of definitive rejections may drive
        // nonce recovery, restoration, or safe failure. Fee bumps and plain
        // retries are left to `plan`, which owns every replacement decision.
        let Some(rejections) = self.active.rejections() else {
            return effects;
        };
        let verdict = PublishReject::verdict(&rejections);

        if self.is_provisional() {
            match verdict {
                _ if self.provisional_deadline_reached(now) => {
                    effects.failed = Some(verdict.error())
                }
                RejectionVerdict::NonceTooLow => self.nonce_fetch = NonceFetch::Required,
                RejectionVerdict::Deterministic(error) => effects.failed = Some(error),
                RejectionVerdict::FeeTooLow(_) | RejectionVerdict::Retry(_) => {}
            }
            return effects;
        }

        match verdict {
            RejectionVerdict::NonceTooLow => effects.sweep_requested = true,
            RejectionVerdict::Deterministic(error) => {
                self.restore_previous(error, now, &mut effects)
            }
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
        // Ignore a worker result if its baseline version was superseded while
        // transaction preparation was in flight. At most one build runs per slot,
        // so the baseline uniquely identifies the in-flight request.
        if !matches!(
            self.replacement,
            ReplacementState::Preparing { base_version: current } if current == base_version
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
        let candidate = self.replacement_candidate(reason);
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
    pub fn record_account_nonce(&mut self, result: TxManagerResult<u64>) -> SlotEffects {
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
        if self.is_committed() {
            return SlotEffects { sweep_requested: true, ..Default::default() };
        }

        self.request_replacement(ReplacementReason::Resign, latest);
        SlotEffects { next_nonce_at_least: Some(latest), ..Default::default() }
    }

    /// Requests cancellation or returns the caller when another request owns the slot.
    pub fn request_cancel(
        &mut self,
        sender: Address,
        result: oneshot::Sender<TxManagerResult<()>>,
    ) -> Result<(), oneshot::Sender<TxManagerResult<()>>> {
        // A cancel already owns the slot. Report success only once its bytes may
        // be live; otherwise the caller keeps waiting on the in-progress cancel.
        if matches!(self.active.kind, VersionKind::Cancel) {
            return if self.active.live_at.is_some() {
                let _ = result.send(Ok(()));
                Ok(())
            } else {
                Err(result)
            };
        }
        if self.cancel.is_some() {
            return Err(result);
        }

        self.cancel = Some(CancelRequest {
            candidate: TxCandidate::cancel(sender, Arc::clone(&self.active.candidate.blobs)),
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
    const fn request_replacement(&mut self, reason: ReplacementReason, nonce: u64) {
        self.replacement = ReplacementState::Requested { reason, nonce };
    }

    /// Returns the candidate encoded into the next replacement version.
    ///
    /// A cancellation carries the self-transfer candidate; every other reason
    /// re-uses the active intent (a fee bump or re-sign of the same payload).
    fn replacement_candidate(&self, reason: ReplacementReason) -> TxCandidate {
        match reason {
            ReplacementReason::Cancel => self
                .cancel
                .as_ref()
                .expect("cancel request outlives its replacement")
                .candidate
                .clone(),
            ReplacementReason::FeeBump | ReplacementReason::Resign => self.active.candidate.clone(),
        }
    }

    /// Returns whether the provisional admission window has elapsed.
    fn provisional_deadline_reached(&self, now: Duration) -> bool {
        matches!(
            self.state,
            SlotState::Provisional { deadline: Some(deadline) } if now >= deadline
        )
    }

    /// Retains one potentially-live hash for canonical resolution.
    fn record_attempt(&mut self, version: VersionId, kind: VersionKind, hash: B256) {
        if self.attempts.iter().all(|attempt| attempt.hash != hash) {
            self.attempts.push(PublishedAttempt { version, kind, hash });
        }
    }

    /// Restores the last potentially-live version after a deterministic rejection.
    ///
    /// When no earlier version survives, the rejected version stays active and
    /// `plan` reopens its pass after `publish_retry_delay`; reopening here would
    /// republish it at backend-response speed.
    fn restore_previous(
        &mut self,
        error: TxManagerError,
        now: Duration,
        effects: &mut SlotEffects,
    ) {
        let rejected_kind = self.active.kind;
        self.replacement_ready_at = now;
        if matches!(rejected_kind, VersionKind::Cancel) {
            self.finish_cancel(Err(error));
        }
        if let Some(mut previous) = self.previous.take() {
            previous.open_next_pass(now);
            self.active = previous;
            self.notify_status();
            effects.snapshot_changed = true;
        }
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
    use alloy_primitives::Bytes;

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

        let effects = slot.record_account_nonce(Ok(5));
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

    #[test]
    fn deterministic_rejection_without_previous_waits_for_retry_delay() {
        let (mut slot, _) = slot(1);
        // Commit the nonce while leaving it unconfirmed, so no earlier version
        // is retained to fall back to.
        slot.record_publish(&event(&slot, 0, PublishOutcome::Ambiguous), Duration::ZERO);
        assert!(slot.is_committed());
        assert!(slot.previous.is_none());

        // The retry pass republishes the same version and is rejected everywhere.
        assert!(slot.plan(Duration::from_secs(1), policy()).snapshot_changed);
        let reject = PublishOutcome::Rejected(PublishReject::Deterministic(
            TxManagerError::InsufficientFunds,
        ));
        let effects = slot.record_publish(&event(&slot, 0, reject), Duration::from_secs(1));

        // Nothing reopens immediately: the next pass only starts after the delay.
        assert!(!effects.snapshot_changed);
        assert!(slot.plan(Duration::from_secs(1), policy()).work.is_none());
        assert!(!slot.plan(Duration::from_secs(1), policy()).snapshot_changed);
        assert!(slot.plan(Duration::from_secs(2), policy()).snapshot_changed);
    }

    #[test]
    fn deterministic_rejection_restores_previous_version() {
        let (mut slot, _) = slot(1);
        slot.record_publish(&event(&slot, 0, PublishOutcome::Ambiguous), Duration::ZERO);
        let live_version = slot.active_version();

        let plan = slot.plan(Duration::from_secs(10), policy());
        let Some(PendingWork::PrepareReplacementTx { base_version, reason, .. }) = plan.work else {
            panic!("stale live transaction should request a fee bump")
        };
        slot.replacement_prepared(
            base_version,
            reason,
            Ok(prepared(0, 2)),
            Duration::from_secs(10),
            policy(),
        );
        assert_ne!(slot.active_version(), live_version);
        assert!(slot.previous.is_some());

        let reject = PublishOutcome::Rejected(PublishReject::Deterministic(
            TxManagerError::InsufficientFunds,
        ));
        let effects = slot.record_publish(&event(&slot, 0, reject), Duration::from_secs(10));
        assert!(effects.snapshot_changed);
        assert_eq!(slot.active_version(), live_version);
        assert!(slot.previous.is_none());
    }

    #[test]
    fn rejected_cancel_restores_previous_and_reports_error() {
        let (mut slot, _) = slot(1);
        slot.record_publish(&event(&slot, 0, PublishOutcome::Ambiguous), Duration::ZERO);
        let live_version = slot.active_version();

        let (result, mut result_rx) = oneshot::channel();
        slot.request_cancel(Address::with_last_byte(9), result).unwrap();
        let plan = slot.plan(Duration::ZERO, policy());
        let Some(PendingWork::PrepareReplacementTx {
            base_version,
            reason: ReplacementReason::Cancel,
            ..
        }) = plan.work
        else {
            panic!("a pending cancel should prepare a cancel replacement")
        };
        slot.replacement_prepared(
            base_version,
            ReplacementReason::Cancel,
            Ok(prepared(0, 2)),
            Duration::ZERO,
            policy(),
        );

        let reject = PublishOutcome::Rejected(PublishReject::Deterministic(
            TxManagerError::InsufficientFunds,
        ));
        slot.record_publish(&event(&slot, 0, reject), Duration::ZERO);

        assert_eq!(slot.active_version(), live_version);
        assert_eq!(result_rx.try_recv().unwrap(), Err(TxManagerError::InsufficientFunds));
    }

    #[test]
    fn nothing_is_decided_while_a_replacement_is_being_built() {
        let (mut slot, _) = slot(2);
        slot.record_publish(&event(&slot, 0, PublishOutcome::Accepted), Duration::ZERO);
        slot.record_publish(&event(&slot, 1, PublishOutcome::Accepted), Duration::ZERO);

        let plan = slot.plan(Duration::from_secs(10), policy());
        assert!(matches!(
            plan.work,
            Some(PendingWork::PrepareReplacementTx { reason: ReplacementReason::FeeBump, .. })
        ));
        assert!(matches!(slot.replacement, ReplacementState::Preparing { .. }));

        // A late rejection observed during the build must not re-plan anything.
        slot.record_publish(
            &event(
                &slot,
                0,
                PublishOutcome::Rejected(PublishReject::FeeTooLow(TxManagerError::Underpriced)),
            ),
            Duration::from_secs(10),
        );
        assert!(matches!(slot.replacement, ReplacementState::Preparing { .. }));
    }

    #[test]
    fn account_nonce_read_failure_leaves_the_slot_untouched() {
        let (mut slot, _) = slot(1);
        slot.record_publish(
            &event(&slot, 0, PublishOutcome::Rejected(PublishReject::NonceTooLow)),
            Duration::ZERO,
        );
        slot.plan(Duration::ZERO, policy());

        let effects = slot.record_account_nonce(Err(TxManagerError::Transport("down".to_string())));
        assert!(effects.next_nonce_at_least.is_none());
        assert!(effects.failed.is_none());
        assert!(matches!(slot.replacement, ReplacementState::Idle));
    }

    #[test]
    fn cancel_is_rejected_until_its_bytes_may_be_live() {
        let (mut slot, _) = slot(1);
        slot.record_publish(&event(&slot, 0, PublishOutcome::Ambiguous), Duration::ZERO);

        let (first, _first_rx) = oneshot::channel();
        slot.request_cancel(Address::with_last_byte(9), first).unwrap();
        let plan = slot.plan(Duration::ZERO, policy());
        let Some(PendingWork::PrepareReplacementTx { base_version, .. }) = plan.work else {
            panic!("a pending cancel should prepare a cancel replacement")
        };
        slot.replacement_prepared(
            base_version,
            ReplacementReason::Cancel,
            Ok(prepared(0, 2)),
            Duration::ZERO,
            policy(),
        );

        // The cancel version is active but has not been accepted anywhere yet.
        let (blocked, _blocked_rx) = oneshot::channel();
        assert!(slot.request_cancel(Address::with_last_byte(9), blocked).is_err());

        // Once its bytes may be live, another request reports success.
        slot.record_publish(&event(&slot, 0, PublishOutcome::Ambiguous), Duration::ZERO);
        let (live, mut live_rx) = oneshot::channel();
        slot.request_cancel(Address::with_last_byte(9), live).unwrap();
        assert_eq!(live_rx.try_recv().unwrap(), Ok(()));
    }
}
