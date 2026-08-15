//! Claimed proof job handling contract.

use async_trait::async_trait;
use base_prover_service_protocol::ProofJob;
use thiserror::Error;

/// Stable claim identifiers attached to a worker-owned proof job.
///
/// This intentionally excludes lease metadata such as `lock_expires_at`, which changes after
/// heartbeat responses and should be read from the latest [`ProofJob`] snapshot instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedProofJobMetadata {
    /// Proof session identifier.
    pub session_id: String,
    /// Server-issued lock identifier for this worker claim.
    pub lock_id: String,
    /// Worker identifier that owns the claim.
    pub worker_id: String,
}

impl ClaimedProofJobMetadata {
    /// Extracts common claim metadata from a claimed proof job.
    #[must_use = "extraction may fail if lock_id or worker_id is missing"]
    pub fn from_job(job: &ProofJob) -> Result<Self, ClaimedProofJobMetadataError> {
        Self::try_from(job)
    }
}

impl TryFrom<&ProofJob> for ClaimedProofJobMetadata {
    type Error = ClaimedProofJobMetadataError;

    fn try_from(job: &ProofJob) -> Result<Self, Self::Error> {
        let session_id = job.session_id.clone();
        let lock_id = job.lock_id.clone().ok_or_else(|| {
            ClaimedProofJobMetadataError::MissingLockId { session_id: session_id.clone() }
        })?;
        let worker_id = job.worker_id.clone().ok_or_else(|| {
            ClaimedProofJobMetadataError::MissingWorkerId { session_id: session_id.clone() }
        })?;

        Ok(Self { session_id, lock_id, worker_id })
    }
}

/// Errors raised while extracting common claim metadata from a proof job.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ClaimedProofJobMetadataError {
    /// Claimed proof job did not include a lock identifier.
    #[error("proof job {session_id} is missing lock_id")]
    MissingLockId {
        /// Proof session identifier.
        session_id: String,
    },
    /// Claimed proof job did not include a worker identifier.
    #[error("proof job {session_id} is missing worker_id")]
    MissingWorkerId {
        /// Proof session identifier.
        session_id: String,
    },
}

/// Backend-specific claimed-job handler used by worker host loops.
#[async_trait]
pub trait ClaimedProofJobHandler: Send + Sync + 'static {
    /// Error returned while handling a claimed proof job.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Returns whether this worker should attempt to claim a job now.
    async fn ready_to_claim(&self, _worker_id: &str) -> bool {
        true
    }

    /// Handles a claimed proof job.
    async fn handle_claimed_job(&self, job: ProofJob) -> Result<(), Self::Error>;

    /// Signals backend-specific spawned work to stop during shutdown.
    fn shutdown(&self) {}
}
