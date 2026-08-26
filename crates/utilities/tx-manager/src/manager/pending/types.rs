//! Value types exchanged between the pending ledger and coordinator workers.

use std::time::Duration;

use alloy_primitives::B256;
use alloy_rpc_types_eth::TransactionReceipt;

use super::super::build::PreparedTx;
use crate::{SubmissionId, TxCandidate};

/// Monotonic identifier for signed variants within one nonce slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VersionId(pub u32);

impl VersionId {
    /// Identifier of the first signed transaction in a slot.
    pub const INITIAL: Self = Self(0);

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
    Bump,
    /// Self-transfer intended to clear the nonce slot.
    Cancel,
}

/// Minimal history retained for every version that may be live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedAttempt {
    /// Signed version that produced the hash.
    pub version: VersionId,
    /// Purpose of the signed version.
    pub kind: VersionKind,
    /// Canonical transaction hash.
    pub hash: B256,
}

/// Retry and timing settings consumed by the pending ledger.
#[derive(Debug, Clone, Copy)]
pub struct PendingPolicy {
    /// Number of fast retries after the first publication attempt.
    pub publish_max_retries: usize,
    /// Delay between fast publication attempts.
    pub publish_retry_delay: Duration,
    /// Delay before normal resubmission or fee replacement.
    pub resubmission_timeout: Duration,
    /// Clean-rejection deadline for initial publication. Zero disables it.
    pub tx_not_in_mempool_timeout: Duration,
}

/// Work selected by the pending ledger for execution outside the state machine.
#[derive(Debug, Clone)]
pub enum PendingWork {
    /// Build and sign the oldest staged submission.
    BuildInitial {
        /// Submission identifier.
        submission_id: SubmissionId,
        /// Nonce assigned provisionally to the submission.
        nonce: u64,
        /// Candidate to construct.
        candidate: TxCandidate,
    },
    /// Build and sign a replacement or re-signed version.
    BuildReplacement {
        /// Submission identifier.
        submission_id: SubmissionId,
        /// Version used as the build baseline.
        base_version: VersionId,
        /// Nonce to sign.
        nonce: u64,
        /// Candidate to construct.
        candidate: TxCandidate,
        /// Existing signed version used as the fee and sidecar baseline.
        base: PreparedTx,
        /// Why the new version is being built.
        reason: ReplacementReason,
    },
    /// Refresh the latest chain nonce after a provisional nonce was rejected.
    SyncNonce {
        /// Submission identifier.
        submission_id: SubmissionId,
        /// Version that observed `NonceTooLow`.
        version: VersionId,
    },
}

/// Reason for constructing another signed version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementReason {
    /// Increase fees for ordinary resubmission.
    FeeBump,
    /// Build a self-transfer cancellation.
    Cancel,
    /// Re-sign a cleanly rejected provisional transaction at a newer nonce.
    Resign,
}

impl ReplacementReason {
    /// Returns the purpose assigned to the resulting version.
    pub const fn version_kind(self, base: VersionKind) -> VersionKind {
        match self {
            Self::FeeBump if matches!(base, VersionKind::Cancel) => VersionKind::Cancel,
            Self::FeeBump => VersionKind::Bump,
            Self::Cancel => VersionKind::Cancel,
            Self::Resign => base,
        }
    }
}

/// Snapshot passed to the chain sweeper.
#[derive(Debug, Clone)]
pub struct SweepTarget {
    /// Submission identifier.
    pub submission_id: SubmissionId,
    /// Nonce represented by the slot.
    pub nonce: u64,
    /// Attempts known when the sweep started.
    pub attempts: Vec<PublishedAttempt>,
}

/// Chain-proven resolution for one sweep target.
#[derive(Debug, Clone)]
pub enum SweepOutcome {
    /// One locally published version was confirmed.
    Confirmed {
        /// Purpose of the confirmed version.
        kind: VersionKind,
        /// Confirmed transaction receipt.
        receipt: Box<TransactionReceipt>,
    },
    /// The nonce was consumed without a receipt for any known version.
    Superseded,
}

/// Versioned sweep result applied only if its attempt snapshot is still current.
#[derive(Debug, Clone)]
pub struct SweepResolution {
    /// Submission identifier.
    pub submission_id: SubmissionId,
    /// Number of attempts present in the worker snapshot.
    pub attempt_count: usize,
    /// Chain-proven outcome.
    pub outcome: SweepOutcome,
}
