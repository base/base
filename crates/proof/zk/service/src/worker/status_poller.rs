use std::time::Duration;

use base_zk_db::{ProofRequestRepo, RetryOutcome};
use tokio::time::sleep;
use tracing::{Instrument, error, info, warn};

use crate::{metrics, proof_request_manager::ProofRequestManager};

/// Background worker that polls proving backends for status updates
/// on RUNNING proof requests.
///
/// The poller runs in a loop, querying the database for all RUNNING proof requests,
/// checking their status with the proving backend, and updating the database when
/// jobs complete (SUCCEEDED) or fail (FAILED).
///
/// Additionally, it detects stuck requests (PENDING/RUNNING without active sessions)
/// and retries or fails them after a timeout to prevent orphaned jobs.
#[derive(Debug, Clone)]
pub struct StatusPoller {
    repo: ProofRequestRepo,
    manager: ProofRequestManager,
    poll_interval_secs: u64,
    stuck_timeout_mins: i32,
    max_proof_retries: i32,
}

impl StatusPoller {
    /// Creates a status poller (`poll_interval_secs=<secs>`, `stuck_timeout_mins=<mins>`,
    /// `max_proof_retries=<n>`).
    pub const fn new(
        repo: ProofRequestRepo,
        manager: ProofRequestManager,
        poll_interval_secs: u64,
        stuck_timeout_mins: i32,
        max_proof_retries: i32,
    ) -> Self {
        Self { repo, manager, poll_interval_secs, stuck_timeout_mins, max_proof_retries }
    }

    /// Run the status poller in a loop
    pub async fn run(&self) {
        info!(poll_interval_secs = self.poll_interval_secs, "Starting status poller");

        loop {
            if let Err(e) = self.poll_once().await {
                error!(error = %e, "Status poll failed");
            }

            sleep(Duration::from_secs(self.poll_interval_secs)).await;
        }
    }

    /// Poll once for all RUNNING proof requests and detect stuck requests
    async fn poll_once(&self) -> anyhow::Result<()> {
        // Get all RUNNING proof_requests
        let running_requests = self.repo.get_running_proof_requests().await?;

        if !running_requests.is_empty() {
            info!(count = running_requests.len(), "Polling status for RUNNING proof requests");

            // Process each RUNNING proof request.
            // Terminal metrics (proof_requests_completed, proof_request_duration_ms) are
            // emitted inside sync_and_update_proof_status, so they fire regardless of
            // whether the transition is triggered by this poller or by a GetProof RPC.
            for proof_request in &running_requests {
                let poll_span = tracing::info_span!(
                    "poll_proof_status",
                    proof_request_id = %proof_request.id,
                );
                if let Err(e) = self
                    .manager
                    .sync_and_update_proof_status(proof_request)
                    .instrument(poll_span)
                    .await
                {
                    error!(
                        proof_request_id = %proof_request.id,
                        error = %e,
                        "Failed to sync and update proof status"
                    );
                }
            }
        }

        // Check for stuck requests (PENDING without any sessions)
        let stuck_requests = self.repo.get_stuck_requests(self.stuck_timeout_mins).await?;

        if !stuck_requests.is_empty() {
            info!(
                count = stuck_requests.len(),
                stuck_timeout_mins = self.stuck_timeout_mins,
                "Found stuck proof requests"
            );

            for request in stuck_requests {
                let proof_type_label = metrics::proof_type_label(request.proof_type);

                let error_msg = format!(
                    "Request stuck in {} state without active session for {}+ minutes",
                    request.status, self.stuck_timeout_mins
                );

                match self
                    .repo
                    .retry_or_fail_stuck_request(request.id, self.max_proof_retries, &error_msg)
                    .await
                {
                    Ok(RetryOutcome::Retried) => {
                        info!(
                            proof_request_id = %request.id,
                            retry_count = request.retry_count + 1,
                            max_retries = self.max_proof_retries,
                            "Retrying stuck request — reset to CREATED with new outbox entry"
                        );
                        metrics::inc_retried_requests(proof_type_label);
                    }
                    Ok(RetryOutcome::PermanentlyFailed) => {
                        error!(
                            proof_request_id = %request.id,
                            retry_count = request.retry_count,
                            "Permanently failing stuck request — max retries exceeded"
                        );
                        metrics::inc_stuck_requests(proof_type_label);
                        metrics::inc_proof_requests_completed("failed", proof_type_label);
                    }
                    Ok(RetryOutcome::Skipped) => {
                        warn!(
                            proof_request_id = %request.id,
                            "Stuck request no longer PENDING — already claimed or transitioned"
                        );
                    }
                    Err(e) => {
                        error!(
                            proof_request_id = %request.id,
                            error = %e,
                            "Failed to retry/fail stuck request"
                        );
                    }
                }
            }
        }

        Ok(())
    }
}
