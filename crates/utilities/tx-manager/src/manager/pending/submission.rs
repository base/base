//! Submission lifecycle channels and staged admission values.

use tokio::sync::{oneshot, watch};

use super::types::VersionKind;
use crate::{
    SubmissionHandle, SubmissionId, SubmissionResult, SubmissionSnapshot, SubmissionStatus,
    TxCandidate, TxManagerError, TxManagerResult,
};

/// Publishes lifecycle snapshots for one transaction submission.
#[derive(Debug)]
pub struct SubmissionTracker {
    /// Lifecycle snapshot sender retained until the submission resolves.
    status: watch::Sender<SubmissionSnapshot>,
}

impl SubmissionTracker {
    /// Creates paired manager and caller handles for a new submission.
    pub fn channel(id: SubmissionId) -> (Self, SubmissionHandle) {
        let (status_tx, status_rx) = watch::channel(SubmissionSnapshot::staged(id));
        (Self { status: status_tx }, SubmissionHandle::new(status_rx))
    }

    /// Publishes a non-terminal lifecycle transition.
    pub fn update(&self, status: SubmissionStatus) {
        self.status.send_modify(|snapshot| snapshot.status = status);
    }

    /// Stores the terminal outcome.
    pub fn finish(self, outcome: SubmissionResult) {
        self.status.send_modify(|snapshot| {
            snapshot.status = SubmissionStatus::Resolved(Box::new(outcome));
        });
    }
}

/// Completion contract for ordinary submissions and explicit cancellation requests.
#[derive(Debug)]
pub enum SubmissionCompletion {
    /// Ordinary transaction with result and status observation.
    Transaction(SubmissionTracker),
    /// Cancellation caller waiting for cancel bytes to become potentially live.
    Cancel(oneshot::Sender<TxManagerResult<()>>),
}

impl SubmissionCompletion {
    /// Resolves a completion using transaction or cancellation semantics.
    pub fn finish(self, outcome: SubmissionResult, cancellation_confirmed: bool) {
        match self {
            Self::Transaction(tracker) => tracker.finish(outcome),
            Self::Cancel(result) => {
                let response = if cancellation_confirmed || outcome.is_ok() {
                    Ok(())
                } else {
                    Err(outcome.expect_err("non-success outcome contains error"))
                };
                let _ = result.send(response);
            }
        }
    }
}

/// Candidate waiting for construction in FIFO caller order.
#[derive(Debug)]
pub struct StagedSubmission {
    /// Stable identity allocated before coordinator enqueue.
    pub id: SubmissionId,
    /// Unsigned transaction intent awaiting nonce assignment.
    pub candidate: TxCandidate,
    /// Caller contract to resolve when this submission terminates.
    pub completion: SubmissionCompletion,
    /// Purpose assigned to the first signed version.
    pub kind: VersionKind,
}

/// Fully constructed admission command created before coordinator enqueue.
#[derive(Debug)]
pub struct PendingAdmission {
    /// Fully tracked staged value transferred atomically to the coordinator.
    pub staged: StagedSubmission,
}

impl PendingAdmission {
    /// Creates an admission and lifecycle handle without awaiting the coordinator.
    pub fn new(id: SubmissionId, candidate: TxCandidate) -> (Self, SubmissionHandle) {
        let (tracker, handle) = SubmissionTracker::channel(id);
        (
            Self {
                staged: StagedSubmission {
                    id,
                    candidate,
                    completion: SubmissionCompletion::Transaction(tracker),
                    kind: VersionKind::Original,
                },
            },
            handle,
        )
    }

    /// Resolves an admission that could not reach the coordinator.
    pub fn reject(self, error: TxManagerError) {
        self.staged.completion.finish(Err(error), false);
    }
}
