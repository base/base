use std::time::Duration;

use base_prover_service_db::{FailExpiredProofJobs, ProofJob, ProofRequestRepo, RetryOutcome};
use tokio::time::sleep;
use tracing::{Instrument, error, info, warn};

use crate::{metrics, proof_request_manager::ProofRequestManager};

/// Server-side worker queue tuning shared by worker claims and the expired-claim reaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerQueueConfig {
    /// Reclaim budget: an expired claim is failed once `attempt >= reclaim_attempts`.
    pub reclaim_attempts: u32,
    /// Maximum expired claims to fail per poll tick.
    pub reaper_batch_size: u32,
}

impl WorkerQueueConfig {
    /// Default worker queue tuning.
    pub const DEFAULT: Self = Self { reclaim_attempts: 5, reaper_batch_size: 100 };
}

impl Default for WorkerQueueConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

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
    worker_queue: WorkerQueueConfig,
    expired_claim_error_message: String,
}

impl StatusPoller {
    /// Creates a status poller (`poll_interval_secs=<secs>`, `stuck_timeout_mins=<mins>`,
    /// `max_proof_retries=<n>`) with the given worker queue tuning.
    pub fn new(
        repo: ProofRequestRepo,
        manager: ProofRequestManager,
        poll_interval_secs: u64,
        stuck_timeout_mins: i32,
        max_proof_retries: i32,
        worker_queue: WorkerQueueConfig,
    ) -> Self {
        let expired_claim_error_message = format!(
            "Worker claim expired after exhausting {} attempts",
            worker_queue.reclaim_attempts
        );

        Self {
            repo,
            manager,
            poll_interval_secs,
            stuck_timeout_mins,
            max_proof_retries,
            worker_queue,
            expired_claim_error_message,
        }
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

    async fn poll_once(&self) -> anyhow::Result<()> {
        let running_requests = self.repo.get_running_proof_requests().await?;

        if !running_requests.is_empty() {
            info!(count = running_requests.len(), "Polling status for RUNNING proof requests");

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

        let stuck_requests = self.repo.get_stuck_requests(self.stuck_timeout_mins).await?;

        if !stuck_requests.is_empty() {
            info!(
                count = stuck_requests.len(),
                stuck_timeout_mins = self.stuck_timeout_mins,
                "Found stuck proof requests"
            );

            for request in stuck_requests {
                let proof_type_label =
                    request.proof_type.map(metrics::proof_type_label).unwrap_or("unknown");

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
                            "Retrying stuck request"
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

        self.reap_expired_claims().await;

        Ok(())
    }

    /// Fail claimed jobs whose lock expired after exhausting the reclaim budget.
    async fn reap_expired_claims(&self) {
        let result = self
            .repo
            .fail_expired_proof_jobs(FailExpiredProofJobs {
                max_attempts: self.worker_queue.reclaim_attempts,
                batch_size: self.worker_queue.reaper_batch_size,
                error_message: &self.expired_claim_error_message,
            })
            .await;

        match result {
            Ok(failed) if !failed.is_empty() => {
                warn!(count = failed.len(), "Failed expired worker claims past reclaim budget");
                Self::record_reaped_jobs("expired_exhausted", &failed);
            }
            Ok(_) => {}
            Err(e) => error!(error = %e, "Failed to reap expired worker claims"),
        }
    }

    /// Emit terminal-failure metrics for a batch of reaped jobs.
    fn record_reaped_jobs(reason: &str, jobs: &[ProofJob]) {
        for job in jobs {
            let proof_type = metrics::api_proof_type_label(job.api_proof_type);
            metrics::inc_worker_jobs_failed(reason, proof_type);
            metrics::inc_proof_requests_completed("failed", proof_type);
        }
    }
}
