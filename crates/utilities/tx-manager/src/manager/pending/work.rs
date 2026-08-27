//! Shared vocabulary and worker actions used by the pending transaction state machine.

use std::time::Duration;

use alloy_primitives::B256;

use super::super::build::PreparedTx;
use crate::{
    SubmissionCompletion, SubmissionHandle, SubmissionId, SubmissionTracker, TxCandidate,
    TxManagerError,
};

/// A caller submission waiting for nonce assignment and transaction preparation.
#[derive(Debug)]
pub struct StagedSubmission {
    /// Stable identity allocated before coordinator enqueue.
    pub id: SubmissionId,
    /// Unsigned transaction intent awaiting nonce assignment.
    pub candidate: TxCandidate,
    /// Destination for the submission's terminal result.
    pub completion: SubmissionCompletion,
    /// Purpose assigned to the first signed version.
    pub kind: VersionKind,
}

impl StagedSubmission {
    /// Creates a staged submission and its caller-facing lifecycle handle.
    pub fn new(id: SubmissionId, candidate: TxCandidate) -> (Self, SubmissionHandle) {
        let (tracker, handle) = SubmissionTracker::channel(id);
        let staged = Self {
            id,
            candidate,
            completion: SubmissionCompletion::Transaction(tracker),
            kind: VersionKind::Original,
        };

        (staged, handle)
    }

    /// Resolves a submission that could not be handed to the coordinator.
    pub fn reject(self, error: TxManagerError) {
        self.completion.finish(Err(error), false);
    }
}

/// Monotonic identifier for signed transactions within one nonce slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VersionId(u32);

impl VersionId {
    /// Identifier of the first signed transaction in a slot.
    pub const INITIAL: Self = Self(0);

    /// Creates an identifier from its numeric representation.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the numeric version.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Returns the following version.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Purpose of a signed transaction version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionKind {
    /// Initial transaction for the logical submission.
    Original,
    /// Fee replacement of an earlier version.
    FeeBump,
    /// Self-transfer intended to clear the nonce slot.
    Cancel,
}

/// Minimal history retained for every transaction that may be live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedAttempt {
    /// Signed version that produced the hash.
    pub version: VersionId,
    /// Purpose of the signed version.
    pub kind: VersionKind,
    /// Canonical transaction hash.
    pub hash: B256,
}

/// Timing policy used by the pending transaction state machine.
#[derive(Debug, Clone, Copy)]
pub struct PendingPolicy {
    /// Delay between publication passes.
    pub publish_retry_delay: Duration,
    /// Delay before replacing an unconfirmed live transaction.
    pub resubmission_timeout: Duration,
    /// Maximum clean-rejection admission window. `None` disables the deadline.
    pub tx_not_in_mempool_timeout: Option<Duration>,
}

/// Work selected by the pending ledger for execution outside the state machine.
#[derive(Debug, Clone)]
pub enum PendingWork {
    /// Prepare the oldest staged submission's first transaction.
    PrepareTx {
        /// Submission identifier.
        submission_id: SubmissionId,
        /// Nonce assigned provisionally to the submission.
        nonce: u64,
        /// Candidate to prepare.
        candidate: TxCandidate,
    },
    /// Prepare a replacement transaction for an existing nonce.
    PrepareReplacementTx {
        /// Submission identifier.
        submission_id: SubmissionId,
        /// Version used as the replacement baseline.
        base_version: VersionId,
        /// Nonce to sign.
        nonce: u64,
        /// Candidate to prepare.
        candidate: TxCandidate,
        /// Existing transaction used as the fee and sidecar baseline.
        base: PreparedTx,
        /// Why the replacement is needed.
        reason: ReplacementReason,
    },
    /// Fetch the account nonce after a provisional transaction was rejected.
    FetchAccountNonce {
        /// Submission identifier.
        submission_id: SubmissionId,
    },
}

/// Reason for preparing another transaction for an existing submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementReason {
    /// Increase fees for ordinary resubmission.
    FeeBump,
    /// Prepare a self-transfer cancellation.
    Cancel,
    /// Re-sign a cleanly rejected transaction with a newer nonce.
    Resign,
}

impl ReplacementReason {
    /// Returns the purpose assigned to the replacement transaction.
    pub const fn version_kind(self, base: VersionKind) -> VersionKind {
        match self {
            Self::FeeBump if matches!(base, VersionKind::Cancel) => VersionKind::Cancel,
            Self::FeeBump => VersionKind::FeeBump,
            Self::Cancel => VersionKind::Cancel,
            Self::Resign => base,
        }
    }
}

/// Aggregate decision for a complete pass of definitive backend rejections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionVerdict {
    /// The account nonce must be refreshed before publishing again.
    NonceTooLow,
    /// Every backend reported a permanent error.
    Deterministic(TxManagerError),
    /// At least one backend requires higher fees and none reported a permanent error.
    FeeTooLow(TxManagerError),
    /// The same transaction may be attempted again in a later pass.
    Retry(TxManagerError),
}

impl RejectionVerdict {
    /// Returns the most useful public error represented by this verdict.
    pub fn error(&self) -> TxManagerError {
        match self {
            Self::NonceTooLow => TxManagerError::NonceTooLow,
            Self::Deterministic(error) | Self::FeeTooLow(error) | Self::Retry(error) => {
                error.clone()
            }
        }
    }
}
