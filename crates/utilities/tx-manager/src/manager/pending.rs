//! Ordered in-memory ledger for staged submissions and signed nonce slots.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, Bytes, U256};
use tokio::sync::oneshot;

mod submission;
pub use submission::{PendingAdmission, StagedSubmission, SubmissionCompletion, SubmissionTracker};

mod types;
pub use types::{
    PendingPolicy, PendingWork, PublishedAttempt, ReplacementReason, SweepOutcome, SweepResolution,
    SweepTarget, VersionId, VersionKind,
};

use super::{
    build::PreparedTx,
    publisher::{
        PublishOutcome, PublishReject, PublisherEvent, PublisherId, PublisherSnapshot, PublisherTx,
    },
};
use crate::{SubmissionId, SubmissionStatus, TxCandidate, TxManagerError, TxManagerResult};

/// Bounded clean-rejection budget for one provisionally assigned nonce.
#[derive(Debug, Clone)]
pub struct AdmissionBudget {
    /// Completed publication rounds rejected cleanly by every backend.
    rounds: usize,
    /// Total allowed rounds, including the initial publication.
    max_rounds: usize,
    /// Optional wall-clock deadline for clean mempool admission.
    deadline: Option<Duration>,
    /// Most informative rejection observed in the latest completed round.
    last_rejection: Option<TxManagerError>,
}

impl AdmissionBudget {
    /// Creates a budget whose deadline starts when the signed slot enters the ledger.
    pub fn new(policy: PendingPolicy, now: Duration) -> Self {
        let deadline = (!policy.tx_not_in_mempool_timeout.is_zero())
            .then(|| now.saturating_add(policy.tx_not_in_mempool_timeout));
        Self {
            rounds: 0,
            max_rounds: policy.publish_max_retries.saturating_add(1),
            deadline,
            last_rejection: None,
        }
    }

    /// Records one round in which every backend definitively rejected the bytes.
    pub fn record_rejection(&mut self, error: TxManagerError) {
        self.rounds = self.rounds.saturating_add(1);
        self.last_rejection = Some(error);
    }

    /// Returns whether clean admission may no longer be retried.
    pub fn exhausted(&self, now: Duration) -> bool {
        self.rounds >= self.max_rounds || self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Returns the latest useful rejection for a terminal provisional failure.
    pub fn last_rejection(&self) -> TxManagerError {
        self.last_rejection.clone().expect("an exhausted admission budget contains a rejection")
    }
}

/// Whether a nonce can still be recycled or is irrevocably committed.
#[derive(Debug)]
pub enum SlotState {
    /// Every completed publication attempt was a clean rejection.
    Provisional(AdmissionBudget),
    /// At least one backend may have made this nonce live.
    Committed,
}

/// One immutable signed transaction version plus publication scheduling state.
#[derive(Debug, Clone)]
pub struct SignedVersion {
    /// Monotonic identity within this nonce slot.
    id: VersionId,
    /// Semantic purpose used to interpret canonical confirmation.
    kind: VersionKind,
    /// Immutable signed bytes shared by every backend.
    prepared: PreparedTx,
    /// Coordinator-controlled attempt epoch.
    epoch: u64,
    /// Latest outcome from each backend for the current epoch.
    outcomes: Vec<Option<PublishOutcome>>,
    /// Fast retries started after the initial epoch.
    retries: usize,
    /// Earliest time at which the coordinator may start another epoch.
    retry_at: Option<Duration>,
    /// First time this exact version became potentially live.
    live_at: Option<Duration>,
}

impl SignedVersion {
    /// Creates a signed version ready for immediate publication.
    pub fn new(
        id: VersionId,
        kind: VersionKind,
        prepared: PreparedTx,
        publisher_count: usize,
    ) -> Self {
        Self {
            id,
            kind,
            prepared,
            epoch: 0,
            outcomes: vec![None; publisher_count],
            retries: 0,
            retry_at: None,
            live_at: None,
        }
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

    /// Records an outcome when it belongs to this version and epoch.
    pub fn record(
        &mut self,
        publisher: PublisherId,
        version: VersionId,
        epoch: u64,
        outcome: PublishOutcome,
    ) -> bool {
        if self.id != version || self.epoch != epoch {
            return false;
        }
        let Some(slot) = self.outcomes.get_mut(publisher.index()) else {
            return false;
        };
        *slot = Some(outcome);
        true
    }

    /// Returns whether every configured backend completed this epoch.
    pub fn all_responded(&self) -> bool {
        self.outcomes.iter().all(Option::is_some)
    }

    /// Returns whether every configured backend accepted this version.
    pub fn all_accepted(&self) -> bool {
        self.outcomes.iter().all(|outcome| matches!(outcome, Some(PublishOutcome::Accepted)))
    }

    /// Returns whether the current epoch contains a fee rejection.
    pub fn fee_rejected(&self) -> bool {
        self.outcomes.iter().any(|outcome| {
            matches!(outcome, Some(PublishOutcome::Rejected(PublishReject::FeeTooLow(_))))
        })
    }

    /// Returns definitive rejections from the current epoch.
    pub fn rejections(&self) -> Vec<PublishReject> {
        self.outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                Some(PublishOutcome::Rejected(reject)) => Some(reject.clone()),
                _ => None,
            })
            .collect()
    }

    /// Schedules a new attempt epoch if its fast-retry budget remains.
    pub fn schedule_retry(&mut self, now: Duration, policy: PendingPolicy) {
        if self.retries < policy.publish_max_retries {
            self.retry_at.get_or_insert_with(|| now.saturating_add(policy.publish_retry_delay));
        }
    }

    /// Starts the scheduled epoch while preserving prior acceptances.
    ///
    /// Publisher cursors do not resend an accepted version, so its coordinator
    /// outcome must remain satisfied across retries by other backends.
    pub fn start_retry(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.retries = self.retries.saturating_add(1);
        for outcome in &mut self.outcomes {
            if !matches!(outcome, Some(PublishOutcome::Accepted)) {
                *outcome = None;
            }
        }
        self.retry_at = None;
    }

    /// Restarts publication after reverting an unpublishable replacement.
    pub fn restart(&mut self, now: Duration) {
        self.epoch = self.epoch.saturating_add(1);
        for outcome in &mut self.outcomes {
            if !matches!(outcome, Some(PublishOutcome::Accepted)) {
                *outcome = None;
            }
        }
        self.retries = 0;
        self.retry_at = None;
        self.live_at = Some(now);
    }
}

/// Requested successor to the currently signed version.
#[derive(Debug, Clone)]
pub struct ReplacementRequest {
    /// Version that must remain current for this request to be valid.
    base_version: VersionId,
    /// Nonce to encode into the successor version.
    nonce: u64,
    /// Candidate payload to sign, including cancellation payload when relevant.
    candidate: TxCandidate,
    /// State-machine reason for constructing the successor.
    reason: ReplacementReason,
}

/// Complete local state for one nonce and one logical caller submission.
#[derive(Debug)]
pub struct PendingSlot {
    /// Stable identity of the logical caller submission.
    submission_id: SubmissionId,
    /// Account nonce exclusively represented by this slot.
    nonce: u64,
    /// Original transaction intent retained for fee replacement.
    candidate: TxCandidate,
    /// Whether clean nonce recycling remains legal.
    state: SlotState,
    /// Current version exposed atomically to every publisher.
    active: SignedVersion,
    /// Most recent potentially-live version retained while replacing it.
    fallback: Option<SignedVersion>,
    /// Next monotonic version identifier reserved for this slot.
    next_version: VersionId,
    /// Unique hashes that may have reached a backend.
    attempts: Vec<PublishedAttempt>,
    /// Original caller completion retained until canonical resolution.
    completion: Option<SubmissionCompletion>,
    /// Explicit cancellation caller waiting for potentially-live cancel bytes.
    cancel_result: Option<oneshot::Sender<TxManagerResult<()>>>,
    /// Self-transfer candidate prepared for cancellation construction.
    cancel_candidate: Option<TxCandidate>,
    /// Whether cancellation takes priority over ordinary replacement.
    cancel_requested: bool,
    /// Requested successor not yet owned by a builder.
    replacement_request: Option<ReplacementRequest>,
    /// Baseline version currently owned by a replacement worker.
    replacement_build: Option<VersionId>,
    /// Earliest time at which another replacement build may start.
    replacement_ready_at: Duration,
    /// Whether canonical nonce recovery is required.
    needs_nonce_sync: bool,
    /// Whether a nonce-refresh worker currently owns this slot.
    nonce_sync_in_progress: bool,
}

impl PendingSlot {
    /// Returns whether no publication can yet have made this nonce live.
    pub const fn is_provisional(&self) -> bool {
        matches!(self.state, SlotState::Provisional(_))
    }

    /// Derives the caller-visible state from the current ledger entry.
    pub const fn status(&self) -> SubmissionStatus {
        SubmissionStatus::Pending { nonce: self.nonce, version: self.active.id.get() }
    }

    /// Publishes the latest derived status to an ordinary caller.
    pub fn notify_status(&self) {
        if let Some(SubmissionCompletion::Transaction(tracker)) = self.completion.as_ref() {
            tracker.update(self.status());
        }
    }

    /// Records a potentially-live hash once for later canonical resolution.
    pub fn record_attempt(&mut self, version: VersionId, kind: VersionKind, hash: B256) {
        if self.attempts.iter().all(|attempt| attempt.hash != hash) {
            self.attempts.push(PublishedAttempt { version, kind, hash });
        }
    }

    /// Replaces any older unstarted request with work based on the active version.
    pub fn request_replacement(&mut self, reason: ReplacementReason, candidate: TxCandidate) {
        self.replacement_request = Some(ReplacementRequest {
            base_version: self.active.id,
            nonce: self.nonce,
            candidate,
            reason,
        });
    }

    /// Requests re-signing at a newly observed canonical nonce.
    pub fn request_resign(&mut self, nonce: u64) {
        self.replacement_request = Some(ReplacementRequest {
            base_version: self.active.id,
            nonce,
            candidate: self.candidate.clone(),
            reason: ReplacementReason::Resign,
        });
    }

    /// Returns the candidate appropriate for replacing the active version.
    pub fn replacement_candidate(&self) -> TxCandidate {
        self.cancel_candidate
            .clone()
            .filter(|_| matches!(self.active.kind, VersionKind::Cancel))
            .unwrap_or_else(|| self.candidate.clone())
    }
}

/// Ordered source of truth for staged submissions and signed nonces.
#[derive(Debug)]
pub struct PendingLedger {
    /// Next nonce available for provisional assignment.
    next_nonce: u64,
    /// Caller submissions awaiting construction in FIFO order.
    staged: VecDeque<StagedSubmission>,
    /// Signed nonce ledger ordered from oldest to newest.
    slots: VecDeque<PendingSlot>,
    /// Staged submission currently owned by the sole initial-build worker.
    tail_build: Option<SubmissionId>,
    /// Whether publication handling requested prompt canonical resolution.
    sweep_requested: bool,
    /// Timing and retry policy applied uniformly to nonce slots.
    policy: PendingPolicy,
    /// Number of symmetric publication workers.
    publisher_count: usize,
    /// Monotonic revision of the publisher-visible ledger.
    revision: u64,
}

impl PendingLedger {
    /// Creates an empty ledger starting from the chain reader's latest nonce.
    ///
    /// # Panics
    ///
    /// Panics when `publisher_count` is zero because no nonce could ever become live.
    pub fn new(next_nonce: u64, publisher_count: usize, policy: PendingPolicy) -> Self {
        assert!(publisher_count > 0, "pending ledger requires at least one publisher");
        Self {
            next_nonce,
            staged: VecDeque::new(),
            slots: VecDeque::new(),
            tail_build: None,
            sweep_requested: false,
            policy,
            publisher_count,
            revision: 1,
        }
    }

    /// Appends a tracked admission in caller order.
    pub fn submit(&mut self, admission: PendingAdmission) {
        self.staged.push_back(admission.staged);
    }

    /// Requests cancellation of the oldest committed slot.
    ///
    /// If no local slot exists, a type-2 self-transfer is staged at the latest
    /// nonce so a reservation left by a previous process can be replaced.
    pub fn cancel(&mut self, sender: Address, result: oneshot::Sender<TxManagerResult<()>>) {
        if let Some(slot) =
            self.slots.iter_mut().find(|slot| matches!(slot.state, SlotState::Committed))
        {
            if slot.cancel_result.is_some() {
                let _ = result.send(Err(TxManagerError::CancellationInProgress));
                return;
            }
            slot.cancel_candidate = Some(TxCandidate {
                tx_data: Bytes::new(),
                blobs: Arc::clone(&slot.candidate.blobs),
                to: Some(sender),
                gas_limit: 0,
                value: U256::ZERO,
            });
            slot.cancel_result = Some(result);
            slot.cancel_requested = true;
            return;
        }

        if !self.slots.is_empty() || !self.staged.is_empty() || self.tail_build.is_some() {
            let _ = result.send(Err(TxManagerError::CancellationInProgress));
            return;
        }

        self.staged.push_front(StagedSubmission {
            id: SubmissionId::new(u64::MAX),
            candidate: TxCandidate {
                tx_data: Bytes::new(),
                blobs: Arc::from([]),
                to: Some(sender),
                gas_limit: 0,
                value: U256::ZERO,
            },
            completion: SubmissionCompletion::Cancel(result),
            kind: VersionKind::Cancel,
        });
    }

    /// Selects construction and chain-read work without performing RPC.
    pub fn plan(&mut self, now: Duration) -> Vec<PendingWork> {
        let mut work = Vec::new();

        // Phase 1: materialize at most one provisional tail. A successor is
        // never assigned until every earlier slot is irrevocably committed.
        if self.tail_build.is_none()
            && self.slots.iter().all(|slot| !slot.is_provisional())
            && let Some(staged) = self.staged.front()
        {
            if self.next_nonce == u64::MAX {
                let staged = self.staged.pop_front().expect("front exists");
                staged.completion.finish(Err(TxManagerError::NonceOverflow), false);
            } else {
                self.tail_build = Some(staged.id);
                work.push(PendingWork::BuildInitial {
                    submission_id: staged.id,
                    nonce: self.next_nonce,
                    candidate: staged.candidate.clone(),
                });
            }
        }

        // Phase 2: select at most one external action per signed slot.
        for slot in &mut self.slots {
            if slot.needs_nonce_sync && !slot.nonce_sync_in_progress {
                slot.nonce_sync_in_progress = true;
                work.push(PendingWork::SyncNonce {
                    submission_id: slot.submission_id,
                    version: slot.active.id,
                });
                continue;
            }
            if slot.nonce_sync_in_progress {
                continue;
            }

            // Explicit cancellation always wins over an ordinary fee bump.
            if slot.cancel_requested
                && slot.replacement_request.is_none()
                && slot.replacement_build.is_none()
                && slot.active.all_responded()
            {
                let candidate =
                    slot.cancel_candidate.clone().expect("requested cancellation has a candidate");
                slot.request_replacement(ReplacementReason::Cancel, candidate);
                slot.cancel_requested = false;
            }

            // A backend underpricing response requests a new shared version.
            if slot.replacement_request.is_none()
                && slot.replacement_build.is_none()
                && slot.active.fee_rejected()
                && slot.active.all_responded()
                && now >= slot.replacement_ready_at
            {
                slot.request_replacement(ReplacementReason::FeeBump, slot.replacement_candidate());
            }

            // Canonically unresolved live versions are replaced after the
            // normal resubmission interval, regardless of which backend first
            // made them live.
            if slot.replacement_request.is_none()
                && slot.replacement_build.is_none()
                && matches!(slot.state, SlotState::Committed)
                && slot.active.all_responded()
                && now >= slot.replacement_ready_at
                && slot.active.live_at.is_some_and(|published| {
                    now >= published.saturating_add(self.policy.resubmission_timeout)
                })
            {
                slot.request_replacement(ReplacementReason::FeeBump, slot.replacement_candidate());
            }

            if let Some(request) = slot.replacement_request.clone()
                && slot.replacement_build.is_none()
            {
                slot.replacement_build = Some(request.base_version);
                work.push(PendingWork::BuildReplacement {
                    submission_id: slot.submission_id,
                    base_version: request.base_version,
                    nonce: request.nonce,
                    candidate: request.candidate,
                    base: slot.active.prepared.clone(),
                    reason: request.reason,
                });
                continue;
            }
            if slot.replacement_build.is_some() {
                continue;
            }

            if slot.active.retry_at.is_some_and(|retry_at| now >= retry_at) {
                slot.active.start_retry();
                self.revision = self.revision.saturating_add(1);
            }
        }

        work
    }

    /// Applies completion of an initial build.
    pub fn initial_built(
        &mut self,
        submission_id: SubmissionId,
        result: TxManagerResult<PreparedTx>,
        now: Duration,
    ) {
        if self.tail_build != Some(submission_id) {
            return;
        }
        self.tail_build = None;

        let Some(index) = self.staged.iter().position(|staged| staged.id == submission_id) else {
            return;
        };
        let staged = self.staged.remove(index).expect("position exists");
        match result {
            Ok(prepared) => {
                let slot = PendingSlot {
                    submission_id,
                    nonce: prepared.nonce,
                    candidate: staged.candidate,
                    state: SlotState::Provisional(AdmissionBudget::new(self.policy, now)),
                    active: SignedVersion::new(
                        VersionId::INITIAL,
                        staged.kind,
                        prepared,
                        self.publisher_count,
                    ),
                    fallback: None,
                    next_version: VersionId::INITIAL.next(),
                    attempts: Vec::new(),
                    completion: Some(staged.completion),
                    cancel_result: None,
                    cancel_candidate: None,
                    cancel_requested: false,
                    replacement_request: None,
                    replacement_build: None,
                    replacement_ready_at: now,
                    needs_nonce_sync: false,
                    nonce_sync_in_progress: false,
                };
                slot.notify_status();
                self.slots.push_back(slot);
                self.revision = self.revision.saturating_add(1);
            }
            Err(error) => staged.completion.finish(Err(error), false),
        }
    }

    /// Applies completion of a replacement build if its baseline remains current.
    pub fn replacement_built(
        &mut self,
        submission_id: SubmissionId,
        base_version: VersionId,
        reason: ReplacementReason,
        result: TxManagerResult<PreparedTx>,
        now: Duration,
    ) {
        let Some(index) = self.slots.iter().position(|slot| slot.submission_id == submission_id)
        else {
            return;
        };
        let slot = &mut self.slots[index];
        if slot.replacement_build != Some(base_version) || slot.active.id != base_version {
            return;
        }
        slot.replacement_build = None;

        match result {
            Ok(prepared) => {
                // The ledger switches versions atomically. A potentially-live
                // predecessor remains available only as a safe fallback until
                // the new version itself becomes potentially live. Planning
                // starts replacements only after every publisher completed the
                // predecessor epoch, so no unobserved live result remains.
                let kind = reason.version_kind(slot.active.kind);
                if matches!(reason, ReplacementReason::Resign) {
                    slot.nonce = prepared.nonce;
                } else {
                    debug_assert_eq!(
                        prepared.nonce, slot.nonce,
                        "same-slot replacement must preserve its nonce"
                    );
                }
                let replacement =
                    SignedVersion::new(slot.next_version, kind, prepared, self.publisher_count);
                slot.next_version = slot.next_version.next();
                let previous = std::mem::replace(&mut slot.active, replacement);
                if previous.live_at.is_some() || slot.fallback.is_none() {
                    slot.fallback = Some(previous);
                }
                slot.replacement_request = None;
                slot.replacement_ready_at = now;
                slot.notify_status();
                self.revision = self.revision.saturating_add(1);
            }
            Err(error) if slot.is_provisional() && slot.attempts.is_empty() => {
                self.fail_slot(index, error);
            }
            Err(error) => {
                slot.replacement_request = None;
                slot.replacement_ready_at = now.saturating_add(self.policy.resubmission_timeout);
                if matches!(reason, ReplacementReason::Cancel)
                    && let Some(result) = slot.cancel_result.take()
                {
                    let _ = result.send(Err(error));
                    slot.cancel_candidate = None;
                    slot.cancel_requested = false;
                }
            }
        }
    }

    /// Applies one backend result and ignores stale scheduling metadata safely.
    pub fn published(&mut self, event: PublisherEvent, now: Duration) {
        if event.publisher.index() >= self.publisher_count {
            return;
        }
        let Some(index) =
            self.slots.iter().position(|slot| slot.submission_id == event.submission_id)
        else {
            return;
        };

        // Phase 1: apply scheduling state only when the worker observed the
        // active version and epoch. Late results remain useful for confirmation
        // history, but cannot overwrite newer publisher scheduling.
        let potentially_live =
            matches!(event.outcome, PublishOutcome::Accepted | PublishOutcome::Ambiguous);
        let current = self.slots[index].active.record(
            event.publisher,
            event.version,
            event.epoch,
            event.outcome.clone(),
        );

        // Phase 2: any Accepted or Ambiguous result irrevocably commits the
        // nonce. This rule also applies to late results from an older version.
        if potentially_live {
            let slot = &mut self.slots[index];
            slot.record_attempt(event.version, event.kind, event.tx_hash);
            if current {
                slot.active.live_at.get_or_insert(now);
                slot.fallback = None;
            }
            if slot.is_provisional() {
                slot.state = SlotState::Committed;
                self.next_nonce = self.next_nonce.max(slot.nonce.saturating_add(1));
            }
            if matches!(event.kind, VersionKind::Cancel) {
                if let Some(result) = slot.cancel_result.take() {
                    let _ = result.send(Ok(()));
                }
                if matches!(slot.completion, Some(SubmissionCompletion::Cancel(_)))
                    && let Some(SubmissionCompletion::Cancel(result)) = slot.completion.take()
                {
                    let _ = result.send(Ok(()));
                }
            }
            slot.notify_status();
        }

        if !current {
            return;
        }

        // Phase 3: only the active epoch can drive retries, replacement, nonce
        // recovery, or safe provisional failure.
        if self.slots[index].is_provisional() {
            self.handle_provisional_outcomes(index, now);
        } else {
            self.handle_committed_outcomes(index, now);
        }
    }

    /// Applies a latest-nonce refresh requested after collective `NonceTooLow`.
    pub fn nonce_synced(
        &mut self,
        submission_id: SubmissionId,
        version: VersionId,
        result: TxManagerResult<u64>,
        now: Duration,
    ) {
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.submission_id == submission_id && slot.active.id == version)
        else {
            return;
        };
        slot.needs_nonce_sync = false;
        slot.nonce_sync_in_progress = false;

        let Ok(latest) = result else {
            slot.active.schedule_retry(now, self.policy);
            return;
        };
        if latest <= slot.nonce {
            slot.active.schedule_retry(now, self.policy);
            return;
        }

        if matches!(slot.state, SlotState::Committed) {
            self.sweep_requested = true;
            return;
        }

        // Every backend cleanly rejected the old nonce before this read, so no
        // publication remains in flight and re-signing cannot create a conflict.
        self.next_nonce = self.next_nonce.max(latest);
        slot.request_resign(latest);
    }

    /// Returns committed slots to inspect during the next canonical sweep.
    pub fn sweep_targets(&self) -> Vec<SweepTarget> {
        self.slots
            .iter()
            .take_while(|slot| matches!(slot.state, SlotState::Committed))
            .map(|slot| SweepTarget {
                submission_id: slot.submission_id,
                nonce: slot.nonce,
                attempts: slot.attempts.clone(),
            })
            .collect()
    }

    /// Applies the exact confirmed prefix returned by the chain sweeper.
    pub fn apply_sweep(&mut self, resolutions: Vec<SweepResolution>) {
        for resolution in resolutions {
            let Some(front) = self.slots.front() else {
                break;
            };
            if front.submission_id != resolution.submission_id
                || front.attempts.len() != resolution.attempt_count
            {
                break;
            }
            let slot = self.slots.pop_front().expect("front exists");
            self.resolve_slot(slot, resolution.outcome);
            self.revision = self.revision.saturating_add(1);
        }
    }

    /// Marks a sweep worker as started.
    pub const fn start_sweep(&mut self) {
        self.sweep_requested = false;
    }

    /// Returns whether publication handling requested an immediate sweep.
    pub const fn sweep_requested(&self) -> bool {
        self.sweep_requested
    }

    /// Returns the latest publisher-facing ledger snapshot.
    pub fn publisher_snapshot(&self) -> PublisherSnapshot {
        let transactions = self
            .slots
            .iter()
            .map(|slot| slot.active.publisher_tx(slot.submission_id, slot.nonce))
            .collect::<Vec<_>>()
            .into();
        PublisherSnapshot { revision: self.revision, transactions }
    }

    /// Stops admission while retaining every signed slot until safe resolution.
    pub fn close(&mut self) {
        while let Some(staged) = self.staged.pop_front() {
            staged.completion.finish(Err(TxManagerError::ChannelClosed), false);
        }
        self.tail_build = None;
    }

    /// Resolves every waiter when the owning runtime is shutting down.
    pub fn abort(&mut self) {
        while let Some(staged) = self.staged.pop_front() {
            staged.completion.finish(Err(TxManagerError::ChannelClosed), false);
        }
        self.tail_build = None;
        while let Some(mut slot) = self.slots.pop_front() {
            if let Some(completion) = slot.completion.take() {
                completion.finish(Err(TxManagerError::ChannelClosed), false);
            }
            if let Some(result) = slot.cancel_result.take() {
                let _ = result.send(Err(TxManagerError::ChannelClosed));
            }
        }
    }

    /// Returns whether all accepted work reached a terminal state.
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.slots.is_empty() && self.tail_build.is_none()
    }

    /// Applies a fully rejected provisional publication epoch.
    pub fn handle_provisional_outcomes(&mut self, index: usize, now: Duration) {
        if !self.slots[index].active.all_responded() {
            return;
        }
        let rejections = self.slots[index].active.rejections();
        if rejections.len() != self.publisher_count {
            return;
        }
        let error = Self::representative_rejection(&rejections);
        let exhausted = match &mut self.slots[index].state {
            SlotState::Provisional(budget) => {
                budget.record_rejection(error.clone());
                budget.exhausted(now)
            }
            SlotState::Committed => return,
        };
        if exhausted {
            let error = match &self.slots[index].state {
                SlotState::Provisional(budget) => budget.last_rejection(),
                SlotState::Committed => unreachable!("slot state checked above"),
            };
            self.fail_slot(index, error);
            return;
        }

        if rejections.iter().any(|reject| matches!(reject, PublishReject::NonceTooLow)) {
            self.slots[index].needs_nonce_sync = true;
        } else if rejections.iter().any(|reject| matches!(reject, PublishReject::FeeTooLow(_))) {
            let candidate = self.slots[index].replacement_candidate();
            self.slots[index].request_replacement(ReplacementReason::FeeBump, candidate);
        } else if rejections.iter().any(|reject| matches!(reject, PublishReject::NonceTooHigh)) {
            self.slots[index].active.schedule_retry(now, self.policy);
        } else {
            self.fail_slot(index, error);
        }
    }

    /// Updates retry and replacement policy for a committed slot.
    pub fn handle_committed_outcomes(&mut self, index: usize, now: Duration) {
        if self.slots[index].active.all_responded()
            && self.slots[index].active.fee_rejected()
            && now >= self.slots[index].replacement_ready_at
        {
            let candidate = self.slots[index].replacement_candidate();
            self.slots[index].request_replacement(ReplacementReason::FeeBump, candidate);
            return;
        }

        if self.slots[index].active.live_at.is_some() {
            if self.slots[index].active.all_responded() && self.slots[index].active.all_accepted() {
                self.slots[index].active.retry_at = None;
            } else {
                self.slots[index].active.schedule_retry(now, self.policy);
            }
            return;
        }

        if !self.slots[index].active.all_responded() {
            return;
        }
        let rejections = self.slots[index].active.rejections();
        if rejections.iter().any(|reject| matches!(reject, PublishReject::NonceTooLow)) {
            self.sweep_requested = true;
        }

        if self.slots[index].active.retries < self.policy.publish_max_retries {
            self.slots[index].active.schedule_retry(now, self.policy);
            return;
        }

        // A replacement that every backend rejected cleanly never became live.
        // Revert to the last potentially-live version rather than abandoning
        // the committed nonce.
        let error = Self::representative_rejection(&rejections);
        self.revert_to_fallback(index, error, now);
    }

    /// Reverts a collectively rejected replacement to its live predecessor.
    pub fn revert_to_fallback(&mut self, index: usize, error: TxManagerError, now: Duration) {
        let slot = &mut self.slots[index];
        let Some(mut fallback) = slot.fallback.take() else {
            slot.active.restart(now);
            slot.replacement_ready_at = now;
            self.revision = self.revision.saturating_add(1);
            return;
        };
        let rejected_kind = slot.active.kind;
        fallback.restart(now);
        slot.active = fallback;
        slot.replacement_request = None;
        slot.replacement_build = None;
        slot.replacement_ready_at = now;
        if matches!(rejected_kind, VersionKind::Cancel) {
            if let Some(result) = slot.cancel_result.take() {
                let _ = result.send(Err(error));
            }
            slot.cancel_candidate = None;
            slot.cancel_requested = false;
        }
        slot.notify_status();
        self.revision = self.revision.saturating_add(1);
    }

    /// Removes a nonce-safe provisional tail and resolves its waiters.
    pub fn fail_slot(&mut self, index: usize, error: TxManagerError) {
        debug_assert_eq!(
            index + 1,
            self.slots.len(),
            "only the provisional ledger tail may be recycled"
        );
        let slot = self.slots.remove(index).expect("slot index exists");
        if let Some(completion) = slot.completion {
            completion.finish(Err(error.clone()), false);
        }
        if let Some(cancel) = slot.cancel_result {
            let _ = cancel.send(Err(error));
        }
        self.revision = self.revision.saturating_add(1);
    }

    /// Resolves a consumed front slot from canonical sweep evidence.
    pub fn resolve_slot(&self, mut slot: PendingSlot, outcome: SweepOutcome) {
        let send_outcome = match outcome {
            SweepOutcome::Confirmed { kind: VersionKind::Cancel, .. } => {
                Err(TxManagerError::Cancelled)
            }
            SweepOutcome::Confirmed { receipt, .. } => Ok(*receipt),
            SweepOutcome::Superseded => Err(TxManagerError::Superseded),
        };
        if let Some(completion) = slot.completion.take() {
            let cancellation_confirmed = matches!(send_outcome, Err(TxManagerError::Cancelled));
            completion.finish(send_outcome, cancellation_confirmed);
        }
        if let Some(cancel) = slot.cancel_result.take() {
            let _ = cancel.send(Ok(()));
        }
    }

    /// Chooses a stable public error from one fully rejected backend round.
    pub fn representative_rejection(rejections: &[PublishReject]) -> TxManagerError {
        rejections
            .iter()
            .find(|reject| matches!(reject, PublishReject::FeeTooLow(_)))
            .or_else(|| {
                rejections.iter().find(|reject| matches!(reject, PublishReject::NonceTooLow))
            })
            .or_else(|| {
                rejections.iter().find(|reject| matches!(reject, PublishReject::NonceTooHigh))
            })
            .or_else(|| {
                rejections.iter().find(|reject| matches!(reject, PublishReject::AlreadyReserved))
            })
            .or_else(|| rejections.first())
            .map_or(TxManagerError::ChannelClosed, PublishReject::as_error)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::*;
    use crate::{SubmissionHandle, test_utils::StubReceipt};

    fn policy() -> PendingPolicy {
        PendingPolicy {
            publish_max_retries: 2,
            publish_retry_delay: Duration::from_secs(1),
            resubmission_timeout: Duration::from_secs(10),
            tx_not_in_mempool_timeout: Duration::from_secs(30),
        }
    }

    fn candidate(value: u64) -> TxCandidate {
        TxCandidate {
            to: Some(Address::with_last_byte(1)),
            value: U256::from(value),
            ..Default::default()
        }
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

    fn submit(queue: &mut PendingLedger, id: u64) -> SubmissionHandle {
        let (admission, handle) = PendingAdmission::new(SubmissionId::new(id), candidate(id));
        queue.submit(admission);
        handle
    }

    fn build_initial(queue: &mut PendingLedger, id: u64, nonce: u64, marker: u8, now: Duration) {
        assert!(matches!(
            queue.plan(now).as_slice(),
            [PendingWork::BuildInitial { submission_id, nonce: assigned, .. }]
                if *submission_id == SubmissionId::new(id) && *assigned == nonce
        ));
        queue.initial_built(SubmissionId::new(id), Ok(prepared(nonce, marker)), now);
    }

    fn publish(
        queue: &mut PendingLedger,
        publisher: usize,
        outcome: PublishOutcome,
        now: Duration,
    ) {
        let transaction = queue.publisher_snapshot().transactions.last().unwrap().clone();
        queue.published(
            PublisherEvent {
                publisher: PublisherId::new(publisher),
                submission_id: transaction.submission_id,
                version: transaction.version,
                epoch: transaction.epoch,
                kind: transaction.kind,
                tx_hash: transaction.prepared.tx_hash,
                outcome,
            },
            now,
        );
    }

    #[test]
    fn one_ambiguous_backend_commits_nonce_and_unblocks_successor() {
        let mut queue = PendingLedger::new(0, 2, policy());
        let first = submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);

        publish(&mut queue, 0, PublishOutcome::Ambiguous, Duration::ZERO);
        assert!(matches!(
            first.snapshot().status,
            SubmissionStatus::Pending { nonce: 0, version: 0 }
        ));

        submit(&mut queue, 2);
        assert!(matches!(
            queue.plan(Duration::ZERO).as_slice(),
            [PendingWork::BuildInitial { nonce: 1, .. }]
        ));
    }

    #[test]
    fn retry_epoch_preserves_backends_that_already_accepted_the_version() {
        let mut queue = PendingLedger::new(0, 2, policy());
        submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(&mut queue, 0, PublishOutcome::Accepted, Duration::ZERO);
        publish(&mut queue, 1, PublishOutcome::Ambiguous, Duration::ZERO);

        assert!(queue.plan(Duration::from_secs(1)).is_empty());
        assert_eq!(queue.publisher_snapshot().transactions[0].epoch, 1);
        publish(&mut queue, 1, PublishOutcome::Accepted, Duration::from_secs(1));

        assert!(queue.slots[0].active.all_accepted());
        assert!(queue.slots[0].active.retry_at.is_none());
    }

    #[tokio::test]
    async fn collective_deterministic_rejection_recycles_tail_nonce() {
        let mut queue = PendingLedger::new(0, 2, policy());
        let first = submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        let rejection = PublishOutcome::Rejected(PublishReject::Deterministic(
            TxManagerError::InsufficientFunds,
        ));

        publish(&mut queue, 0, rejection.clone(), Duration::ZERO);
        assert_eq!(queue.publisher_snapshot().transactions.len(), 1);
        publish(&mut queue, 1, rejection, Duration::ZERO);
        assert_eq!(first.wait().await.unwrap_err(), TxManagerError::InsufficientFunds);

        submit(&mut queue, 2);
        assert!(matches!(
            queue.plan(Duration::ZERO).as_slice(),
            [PendingWork::BuildInitial { nonce: 0, .. }]
        ));
    }

    #[test]
    fn fee_bump_replaces_shared_version_atomically() {
        let mut queue = PendingLedger::new(0, 2, policy());
        submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(&mut queue, 0, PublishOutcome::Accepted, Duration::ZERO);
        publish(&mut queue, 1, PublishOutcome::Accepted, Duration::ZERO);

        let work = queue.plan(Duration::from_secs(10));
        let [
            PendingWork::BuildReplacement {
                submission_id,
                base_version,
                reason: ReplacementReason::FeeBump,
                ..
            },
        ] = work.as_slice()
        else {
            panic!("live transaction should request a fee replacement")
        };
        queue.replacement_built(
            *submission_id,
            *base_version,
            ReplacementReason::FeeBump,
            Ok(prepared(0, 2)),
            Duration::from_secs(10),
        );

        let snapshot = queue.publisher_snapshot();
        assert_eq!(snapshot.transactions.len(), 1);
        assert_eq!(snapshot.transactions[0].version, VersionId::INITIAL.next());
        assert_eq!(snapshot.transactions[0].prepared.tx_hash, B256::with_last_byte(2));
    }

    #[test]
    fn failed_replacement_build_is_deferred_before_retrying() {
        let mut queue = PendingLedger::new(0, 1, policy());
        submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(&mut queue, 0, PublishOutcome::Accepted, Duration::ZERO);

        let work = queue.plan(Duration::from_secs(10));
        let [
            PendingWork::BuildReplacement {
                submission_id,
                base_version,
                reason: ReplacementReason::FeeBump,
                ..
            },
        ] = work.as_slice()
        else {
            panic!("live transaction should request a fee replacement")
        };
        queue.replacement_built(
            *submission_id,
            *base_version,
            ReplacementReason::FeeBump,
            Err(TxManagerError::Transport("unavailable".to_string())),
            Duration::from_secs(10),
        );

        assert!(queue.plan(Duration::from_secs(10)).is_empty());
        assert!(matches!(
            queue.plan(Duration::from_secs(20)).as_slice(),
            [PendingWork::BuildReplacement { reason: ReplacementReason::FeeBump, .. }]
        ));
    }

    #[test]
    fn clean_nonce_too_low_round_requests_chain_nonce_sync() {
        let mut queue = PendingLedger::new(0, 2, policy());
        submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(
            &mut queue,
            0,
            PublishOutcome::Rejected(PublishReject::NonceTooLow),
            Duration::ZERO,
        );
        publish(
            &mut queue,
            1,
            PublishOutcome::Rejected(PublishReject::NonceTooLow),
            Duration::ZERO,
        );

        assert!(matches!(
            queue.plan(Duration::ZERO).as_slice(),
            [PendingWork::SyncNonce { version: VersionId::INITIAL, .. }]
        ));
    }

    #[test]
    fn nonce_sync_keeps_old_signed_bytes_hidden_until_resigning_finishes() {
        let mut queue = PendingLedger::new(0, 1, policy());
        submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(
            &mut queue,
            0,
            PublishOutcome::Rejected(PublishReject::NonceTooLow),
            Duration::ZERO,
        );
        assert!(matches!(queue.plan(Duration::ZERO).as_slice(), [PendingWork::SyncNonce { .. }]));

        queue.nonce_synced(SubmissionId::new(1), VersionId::INITIAL, Ok(5), Duration::ZERO);
        let before = queue.publisher_snapshot();
        assert_eq!(before.transactions[0].nonce, 0);
        assert_eq!(before.transactions[0].prepared.nonce, 0);

        let work = queue.plan(Duration::ZERO);
        let [
            PendingWork::BuildReplacement {
                submission_id,
                base_version,
                nonce: 5,
                reason: ReplacementReason::Resign,
                ..
            },
        ] = work.as_slice()
        else {
            panic!("nonce synchronization should request re-signing")
        };
        queue.replacement_built(
            *submission_id,
            *base_version,
            ReplacementReason::Resign,
            Ok(prepared(5, 2)),
            Duration::ZERO,
        );

        let after = queue.publisher_snapshot();
        assert_eq!(after.transactions[0].nonce, 5);
        assert_eq!(after.transactions[0].prepared.nonce, 5);
    }

    #[test]
    fn fee_replacement_waits_for_every_backend_outcome() {
        let mut queue = PendingLedger::new(0, 3, policy());
        submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(&mut queue, 0, PublishOutcome::Accepted, Duration::ZERO);
        publish(
            &mut queue,
            1,
            PublishOutcome::Rejected(PublishReject::FeeTooLow(TxManagerError::Underpriced)),
            Duration::ZERO,
        );

        assert!(queue.plan(Duration::ZERO).is_empty());

        publish(&mut queue, 2, PublishOutcome::Accepted, Duration::ZERO);
        assert!(matches!(
            queue.plan(Duration::ZERO).as_slice(),
            [PendingWork::BuildReplacement { reason: ReplacementReason::FeeBump, .. }]
        ));
    }

    #[tokio::test]
    async fn cancellation_returns_when_any_backend_may_have_live_cancel_bytes() {
        let mut queue = PendingLedger::new(0, 2, policy());
        submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(&mut queue, 0, PublishOutcome::Accepted, Duration::ZERO);
        publish(&mut queue, 1, PublishOutcome::Accepted, Duration::ZERO);

        let (result_tx, result_rx) = oneshot::channel();
        queue.cancel(Address::with_last_byte(9), result_tx);
        let work = queue.plan(Duration::ZERO);
        let [
            PendingWork::BuildReplacement {
                submission_id,
                base_version,
                reason: ReplacementReason::Cancel,
                ..
            },
        ] = work.as_slice()
        else {
            panic!("cancellation should build a replacement")
        };
        queue.replacement_built(
            *submission_id,
            *base_version,
            ReplacementReason::Cancel,
            Ok(prepared(0, 2)),
            Duration::ZERO,
        );
        publish(&mut queue, 1, PublishOutcome::Ambiguous, Duration::ZERO);

        assert_eq!(result_rx.await.unwrap(), Ok(()));
        assert_eq!(queue.sweep_targets().len(), 1);
    }

    #[test]
    fn stale_sweep_snapshot_cannot_remove_a_slot() {
        let mut queue = PendingLedger::new(0, 1, policy());
        submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(&mut queue, 0, PublishOutcome::Accepted, Duration::ZERO);

        queue.apply_sweep(vec![SweepResolution {
            submission_id: SubmissionId::new(1),
            attempt_count: 0,
            outcome: SweepOutcome::Superseded,
        }]);
        assert_eq!(queue.sweep_targets().len(), 1);
    }

    #[tokio::test]
    async fn sweep_removes_and_resolves_only_the_confirmed_front_prefix() {
        let mut queue = PendingLedger::new(0, 1, policy());
        let first = submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        publish(&mut queue, 0, PublishOutcome::Accepted, Duration::ZERO);

        let second = submit(&mut queue, 2);
        build_initial(&mut queue, 2, 1, 2, Duration::ZERO);
        publish(&mut queue, 0, PublishOutcome::Accepted, Duration::ZERO);

        let receipt = StubReceipt::success();
        queue.apply_sweep(vec![SweepResolution {
            submission_id: SubmissionId::new(1),
            attempt_count: 1,
            outcome: SweepOutcome::Confirmed {
                kind: VersionKind::Original,
                receipt: Box::new(receipt.clone()),
            },
        }]);

        assert_eq!(first.wait().await.unwrap(), receipt);
        assert!(matches!(
            second.snapshot().status,
            SubmissionStatus::Pending { nonce: 1, version: 0 }
        ));
        let snapshot = queue.publisher_snapshot();
        assert_eq!(snapshot.transactions.len(), 1);
        assert_eq!(snapshot.transactions[0].nonce, 1);
    }

    #[tokio::test]
    async fn close_rejects_staged_work_but_keeps_signed_nonce_work() {
        let mut queue = PendingLedger::new(0, 1, policy());
        let signed = submit(&mut queue, 1);
        build_initial(&mut queue, 1, 0, 1, Duration::ZERO);
        let staged = submit(&mut queue, 2);

        queue.close();

        assert_eq!(staged.wait().await.unwrap_err(), TxManagerError::ChannelClosed);
        assert!(matches!(
            signed.snapshot().status,
            SubmissionStatus::Pending { nonce: 0, version: 0 }
        ));
        assert_eq!(queue.publisher_snapshot().transactions.len(), 1);
    }
}
