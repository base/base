//! Claimed proof job handling contract.

use async_trait::async_trait;
use base_prover_service_protocol::ProofJob;

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
