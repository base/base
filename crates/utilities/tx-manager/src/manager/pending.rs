//! Ordered in-memory ledger for staged submissions and signed nonce slots.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use alloy_primitives::{Address, Bytes, U256};
use tokio::sync::oneshot;

mod slot;
pub use slot::{
    CancelRequest, NonceFetch, NonceSlot, ReplacementState, SignedVersion, SlotEffects, SlotPlan,
    SlotState,
};

mod work;
pub use work::{
    PendingPolicy, PendingWork, PublishedAttempt, RejectionVerdict, ReplacementReason,
    StagedSubmission, VersionId, VersionKind,
};

use super::{
    build::PreparedTx,
    publisher::{PublisherEvent, PublisherSnapshot},
    sweep::{SweepResolution, SweepTarget},
};
use crate::{SubmissionCompletion, SubmissionId, TxCandidate, TxManagerError, TxManagerResult};

/// Ordered source of truth for staged submissions and signed nonces.
#[derive(Debug)]
pub struct PendingLedger {
    /// Next nonce available for provisional assignment.
    next_nonce: u64,
    /// Caller submissions awaiting transaction preparation.
    staged: VecDeque<StagedSubmission>,
    /// Signed nonce slots ordered from oldest to newest.
    slots: VecDeque<NonceSlot>,
    /// Staged submission currently owned by a builder worker.
    preparing_submission: Option<SubmissionId>,
    /// Whether publication handling requested prompt canonical resolution.
    sweep_requested: bool,
    /// Monotonic revision of the publisher-visible ledger.
    revision: u64,
    /// Timing policy applied uniformly to nonce slots.
    policy: PendingPolicy,
    /// Number of symmetric publication workers.
    publisher_count: usize,
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
            preparing_submission: None,
            sweep_requested: false,
            revision: 1,
            policy,
            publisher_count,
        }
    }

    /// Appends a staged submission in caller order.
    pub fn submit(&mut self, staged: StagedSubmission) {
        self.staged.push_back(staged);
    }

    /// Requests cancellation of the oldest committed slot.
    ///
    /// If no local slot exists, a self-transfer is staged at the latest nonce
    /// so a reservation left by a previous process can be replaced.
    pub fn cancel(&mut self, sender: Address, result: oneshot::Sender<TxManagerResult<()>>) {
        if let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_committed()) {
            if let Err(result) = slot.request_cancel(sender, result) {
                let _ = result.send(Err(TxManagerError::CancellationInProgress));
            }
            return;
        }

        if !self.slots.is_empty() || !self.staged.is_empty() || self.preparing_submission.is_some()
        {
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

    /// Selects transaction preparation, nonce reads, and publication passes.
    pub fn plan(&mut self, now: Duration) -> Vec<PendingWork> {
        let mut work = Vec::new();

        // Prepare only the queue head. A successor receives a nonce after every
        // earlier slot is committed and can no longer be recycled.
        if self.preparing_submission.is_none()
            && self.slots.iter().all(|slot| !slot.is_provisional())
            && let Some(staged) = self.staged.front()
        {
            if self.next_nonce == u64::MAX {
                let staged = self.staged.pop_front().expect("front exists");
                staged.completion.finish(Err(TxManagerError::NonceOverflow), false);
            } else {
                self.preparing_submission = Some(staged.id);
                work.push(PendingWork::PrepareTx {
                    submission_id: staged.id,
                    nonce: self.next_nonce,
                    candidate: staged.candidate.clone(),
                });
            }
        }

        let mut index = 0;
        while index < self.slots.len() {
            let plan = self.slots[index].plan(now, self.policy);
            if let Some(action) = plan.work {
                work.push(action);
            }
            if let Some(error) = plan.failed {
                self.remove_failed(index, error);
                continue;
            }
            if plan.snapshot_changed {
                self.revision = self.revision.saturating_add(1);
            }
            index += 1;
        }

        work
    }

    /// Records a freshly prepared first transaction, creating its nonce slot.
    pub fn tx_prepared(
        &mut self,
        submission_id: SubmissionId,
        result: TxManagerResult<PreparedTx>,
        now: Duration,
    ) {
        if self.preparing_submission != Some(submission_id) {
            return;
        }
        self.preparing_submission = None;

        let Some(index) = self.staged.iter().position(|staged| staged.id == submission_id) else {
            return;
        };
        let staged = self.staged.remove(index).expect("position exists");
        match result {
            Ok(prepared) => {
                self.slots.push_back(NonceSlot::new(
                    staged,
                    prepared,
                    self.publisher_count,
                    self.policy,
                    now,
                ));
                self.revision = self.revision.saturating_add(1);
            }
            Err(error) => staged.completion.finish(Err(error), false),
        }
    }

    /// Records a prepared replacement if its baseline is still current.
    pub fn replacement_tx_prepared(
        &mut self,
        submission_id: SubmissionId,
        base_version: VersionId,
        reason: ReplacementReason,
        result: TxManagerResult<PreparedTx>,
        now: Duration,
    ) {
        let Some(index) = self.slot_index(submission_id) else {
            return;
        };
        let effects =
            self.slots[index].replacement_prepared(base_version, reason, result, now, self.policy);
        self.apply_effects(index, effects);
    }

    /// Applies one backend publication result.
    pub fn published(&mut self, event: PublisherEvent, now: Duration) {
        let Some(index) = self.slot_index(event.submission_id) else {
            return;
        };
        let effects = self.slots[index].record_publish(&event, now);
        self.apply_effects(index, effects);
    }

    /// Applies an account nonce fetched for the active version of a slot.
    pub fn account_nonce_fetched(
        &mut self,
        submission_id: SubmissionId,
        version: VersionId,
        result: TxManagerResult<u64>,
        now: Duration,
    ) {
        let Some(index) = self.slot_index(submission_id) else {
            return;
        };
        if self.slots[index].active_version() != version {
            return;
        }
        let effects = self.slots[index].record_account_nonce(result, now);
        self.apply_effects(index, effects);
    }

    /// Returns the committed prefix inspected by the next canonical sweep.
    pub fn sweep_targets(&self) -> Vec<SweepTarget> {
        self.slots
            .iter()
            .take_while(|slot| slot.is_committed())
            .map(NonceSlot::sweep_target)
            .collect()
    }

    /// Removes and resolves the exact confirmed prefix returned by the sweeper.
    pub fn apply_sweep(&mut self, resolutions: Vec<SweepResolution>) {
        for resolution in resolutions {
            let Some(front) = self.slots.front() else {
                break;
            };
            if front.submission_id() != resolution.submission_id
                || front.attempt_count() != resolution.attempt_count
            {
                break;
            }

            let slot = self.slots.pop_front().expect("front exists");
            slot.resolve(resolution.outcome);
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
        let transactions =
            self.slots.iter().map(NonceSlot::publisher_tx).collect::<Vec<_>>().into();
        PublisherSnapshot { revision: self.revision, transactions }
    }

    /// Stops admission while retaining signed slots until canonical resolution.
    pub fn close(&mut self) {
        while let Some(staged) = self.staged.pop_front() {
            staged.completion.finish(Err(TxManagerError::ChannelClosed), false);
        }
        self.preparing_submission = None;
    }

    /// Resolves every waiter when the owning runtime is shutting down.
    pub fn abort(&mut self) {
        while let Some(staged) = self.staged.pop_front() {
            staged.completion.finish(Err(TxManagerError::ChannelClosed), false);
        }
        self.preparing_submission = None;
        while let Some(slot) = self.slots.pop_front() {
            slot.fail(TxManagerError::ChannelClosed);
        }
    }

    /// Returns whether all accepted work reached a terminal state.
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.slots.is_empty() && self.preparing_submission.is_none()
    }

    /// Returns the position of a logical submission in the nonce ledger.
    pub fn slot_index(&self, submission_id: SubmissionId) -> Option<usize> {
        self.slots.iter().position(|slot| slot.submission_id() == submission_id)
    }

    /// Applies effects that require ownership of the ordered ledger.
    pub fn apply_effects(&mut self, index: usize, effects: SlotEffects) {
        if let Some(next_nonce) = effects.next_nonce_at_least {
            self.next_nonce = self.next_nonce.max(next_nonce);
        }
        if effects.sweep_requested {
            self.sweep_requested = true;
        }
        if let Some(error) = effects.failed {
            self.remove_failed(index, error);
            return;
        }
        if effects.snapshot_changed {
            self.revision = self.revision.saturating_add(1);
        }
    }

    /// Removes a safely recyclable provisional slot and resolves its waiters.
    pub fn remove_failed(&mut self, index: usize, error: TxManagerError) {
        debug_assert_eq!(
            index + 1,
            self.slots.len(),
            "only the provisional ledger tail may be recycled"
        );
        let slot = self.slots.remove(index).expect("slot index exists");
        slot.fail(error);
        self.revision = self.revision.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256};

    use super::*;
    use crate::{
        SubmissionHandle, SubmissionStatus,
        manager::{PublishOutcome, PublishReject, PublisherEvent, PublisherId, SweepOutcome},
        test_utils::StubReceipt,
    };

    fn policy() -> PendingPolicy {
        PendingPolicy {
            publish_retry_delay: Duration::from_secs(1),
            resubmission_timeout: Duration::from_secs(10),
            tx_not_in_mempool_timeout: Some(Duration::from_secs(30)),
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

    fn submit(ledger: &mut PendingLedger, id: u64) -> SubmissionHandle {
        let (staged, handle) = StagedSubmission::new(SubmissionId::new(id), candidate(id));
        ledger.submit(staged);
        handle
    }

    fn prepare_tx(ledger: &mut PendingLedger, id: u64, nonce: u64, marker: u8, now: Duration) {
        assert!(matches!(
            ledger.plan(now).as_slice(),
            [PendingWork::PrepareTx { submission_id, nonce: assigned, .. }]
                if *submission_id == SubmissionId::new(id) && *assigned == nonce
        ));
        ledger.tx_prepared(SubmissionId::new(id), Ok(prepared(nonce, marker)), now);
    }

    fn publish(
        ledger: &mut PendingLedger,
        publisher: usize,
        outcome: PublishOutcome,
        now: Duration,
    ) {
        let transaction = ledger.publisher_snapshot().transactions.last().unwrap().clone();
        ledger.published(
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
    fn ambiguous_publication_commits_nonce_and_unblocks_successor() {
        let mut ledger = PendingLedger::new(0, 2, policy());
        let first = submit(&mut ledger, 1);
        prepare_tx(&mut ledger, 1, 0, 1, Duration::ZERO);

        publish(&mut ledger, 0, PublishOutcome::Ambiguous, Duration::ZERO);
        assert!(matches!(
            first.snapshot().status,
            SubmissionStatus::Pending { nonce: 0, version: 0 }
        ));

        submit(&mut ledger, 2);
        assert!(matches!(
            ledger.plan(Duration::ZERO).as_slice(),
            [PendingWork::PrepareTx { nonce: 1, .. }]
        ));
    }

    #[test]
    fn retry_pass_preserves_backend_acceptance() {
        let mut ledger = PendingLedger::new(0, 2, policy());
        submit(&mut ledger, 1);
        prepare_tx(&mut ledger, 1, 0, 1, Duration::ZERO);
        publish(&mut ledger, 0, PublishOutcome::Accepted, Duration::ZERO);
        publish(&mut ledger, 1, PublishOutcome::Ambiguous, Duration::ZERO);

        assert!(ledger.plan(Duration::from_secs(1)).is_empty());
        assert_eq!(ledger.publisher_snapshot().transactions[0].epoch, 1);
        publish(&mut ledger, 1, PublishOutcome::Accepted, Duration::from_secs(1));
        assert!(ledger.slots[0].all_accepted());
    }

    #[tokio::test]
    async fn deterministic_rejection_recycles_provisional_nonce() {
        let mut ledger = PendingLedger::new(0, 2, policy());
        let first = submit(&mut ledger, 1);
        prepare_tx(&mut ledger, 1, 0, 1, Duration::ZERO);
        let rejection = PublishOutcome::Rejected(PublishReject::Deterministic(
            TxManagerError::InsufficientFunds,
        ));

        publish(&mut ledger, 0, rejection.clone(), Duration::ZERO);
        assert_eq!(ledger.publisher_snapshot().transactions.len(), 1);
        publish(&mut ledger, 1, rejection, Duration::ZERO);
        assert_eq!(first.wait().await.unwrap_err(), TxManagerError::InsufficientFunds);

        submit(&mut ledger, 2);
        assert!(matches!(
            ledger.plan(Duration::ZERO).as_slice(),
            [PendingWork::PrepareTx { nonce: 0, .. }]
        ));
    }

    #[test]
    fn account_nonce_is_hidden_until_resigning_finishes() {
        let mut ledger = PendingLedger::new(0, 1, policy());
        submit(&mut ledger, 1);
        prepare_tx(&mut ledger, 1, 0, 1, Duration::ZERO);
        publish(
            &mut ledger,
            0,
            PublishOutcome::Rejected(PublishReject::NonceTooLow),
            Duration::ZERO,
        );
        assert!(matches!(
            ledger.plan(Duration::ZERO).as_slice(),
            [PendingWork::FetchAccountNonce { .. }]
        ));

        ledger.account_nonce_fetched(
            SubmissionId::new(1),
            VersionId::INITIAL,
            Ok(5),
            Duration::ZERO,
        );
        assert_eq!(ledger.publisher_snapshot().transactions[0].nonce, 0);

        let work = ledger.plan(Duration::ZERO);
        let [
            PendingWork::PrepareReplacementTx {
                submission_id,
                base_version,
                nonce: 5,
                reason: ReplacementReason::Resign,
                ..
            },
        ] = work.as_slice()
        else {
            panic!("nonce refresh should request re-signing")
        };
        ledger.replacement_tx_prepared(
            *submission_id,
            *base_version,
            ReplacementReason::Resign,
            Ok(prepared(5, 2)),
            Duration::ZERO,
        );
        assert_eq!(ledger.publisher_snapshot().transactions[0].nonce, 5);
    }

    #[tokio::test]
    async fn sweep_removes_only_the_confirmed_front_prefix() {
        let mut ledger = PendingLedger::new(0, 1, policy());
        let first = submit(&mut ledger, 1);
        prepare_tx(&mut ledger, 1, 0, 1, Duration::ZERO);
        publish(&mut ledger, 0, PublishOutcome::Accepted, Duration::ZERO);

        let second = submit(&mut ledger, 2);
        prepare_tx(&mut ledger, 2, 1, 2, Duration::ZERO);
        publish(&mut ledger, 0, PublishOutcome::Accepted, Duration::ZERO);

        let receipt = StubReceipt::success();
        ledger.apply_sweep(vec![SweepResolution {
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
        assert_eq!(ledger.publisher_snapshot().transactions.len(), 1);
    }

    #[tokio::test]
    async fn cancellation_returns_when_cancel_bytes_may_be_live() {
        let mut ledger = PendingLedger::new(0, 1, policy());
        submit(&mut ledger, 1);
        prepare_tx(&mut ledger, 1, 0, 1, Duration::ZERO);
        publish(&mut ledger, 0, PublishOutcome::Accepted, Duration::ZERO);

        let (result_tx, result_rx) = oneshot::channel();
        ledger.cancel(Address::with_last_byte(9), result_tx);
        let work = ledger.plan(Duration::ZERO);
        let [
            PendingWork::PrepareReplacementTx {
                submission_id,
                base_version,
                reason: ReplacementReason::Cancel,
                ..
            },
        ] = work.as_slice()
        else {
            panic!("cancellation should prepare a replacement transaction")
        };
        ledger.replacement_tx_prepared(
            *submission_id,
            *base_version,
            ReplacementReason::Cancel,
            Ok(prepared(0, 2)),
            Duration::ZERO,
        );
        publish(&mut ledger, 0, PublishOutcome::Ambiguous, Duration::ZERO);

        assert_eq!(result_rx.await.unwrap(), Ok(()));
        assert_eq!(ledger.sweep_targets().len(), 1);
    }

    #[test]
    fn stale_sweep_snapshot_cannot_remove_a_slot() {
        let mut ledger = PendingLedger::new(0, 1, policy());
        submit(&mut ledger, 1);
        prepare_tx(&mut ledger, 1, 0, 1, Duration::ZERO);
        publish(&mut ledger, 0, PublishOutcome::Accepted, Duration::ZERO);

        ledger.apply_sweep(vec![SweepResolution {
            submission_id: SubmissionId::new(1),
            attempt_count: 0,
            outcome: SweepOutcome::Superseded,
        }]);

        assert_eq!(ledger.sweep_targets().len(), 1);
    }

    #[tokio::test]
    async fn close_rejects_staged_work_but_keeps_signed_slots() {
        let mut ledger = PendingLedger::new(0, 1, policy());
        let signed = submit(&mut ledger, 1);
        prepare_tx(&mut ledger, 1, 0, 1, Duration::ZERO);
        let staged = submit(&mut ledger, 2);

        ledger.close();

        assert_eq!(staged.wait().await.unwrap_err(), TxManagerError::ChannelClosed);
        assert!(matches!(
            signed.snapshot().status,
            SubmissionStatus::Pending { nonce: 0, version: 0 }
        ));
        assert_eq!(ledger.publisher_snapshot().transactions.len(), 1);
    }
}
