use std::{sync::Arc, time::Duration};

use base_prover_service_db::{FailExpiredProofJobs, ProofJob, ProofRequestRepo, RetryOutcome};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::{metrics, metrics::PendingArtifactGauge};

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

/// Background worker that maintains the worker queue.
///
/// The poller detects stale queued requests and expired worker claims, then
/// retries or fails them according to the configured retry/reclaim budgets.
#[derive(Debug, Clone)]
pub struct StatusPoller {
    repo: ProofRequestRepo,
    poll_interval_secs: u64,
    stuck_timeout_mins: i32,
    max_proof_retries: i32,
    worker_queue: WorkerQueueConfig,
    expired_claim_error_message: String,
    pending_artifact_gauge: Arc<PendingArtifactGauge>,
}

impl StatusPoller {
    /// Creates a status poller (`poll_interval_secs=<secs>`, `stuck_timeout_mins=<mins>`,
    /// `max_proof_retries=<n>`) with the given worker queue tuning.
    pub fn new(
        repo: ProofRequestRepo,
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
            poll_interval_secs,
            stuck_timeout_mins,
            max_proof_retries,
            worker_queue,
            expired_claim_error_message,
            pending_artifact_gauge: Arc::default(),
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
        // Published before the reaper runs so a queue that never drains — the only
        // external symptom of an artifact-hash mismatch — is always visible, even if
        // the stuck-request sweep below fails.
        match self.repo.pending_depth_by_artifact().await {
            Ok(depths) => self.pending_artifact_gauge.record(&depths),
            Err(e) => error!(error = %e, "Failed to read pending queue depth by artifact"),
        }

        let stuck_requests = self.repo.get_stuck_requests(self.stuck_timeout_mins).await?;

        if !stuck_requests.is_empty() {
            info!(
                count = stuck_requests.len(),
                stuck_timeout_mins = self.stuck_timeout_mins,
                "Found stuck proof requests"
            );

            for request in stuck_requests {
                let proof_type_label = metrics::api_proof_type_label(request.api_proof_type);

                let error_msg = format!(
                    "Request stuck in {} state for {}+ minutes",
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
                    Ok(RetryOutcome::PermanentlyFailed(job)) => {
                        error!(
                            proof_request_id = %request.id,
                            retry_count = request.retry_count,
                            "Permanently failing stuck request — max retries exceeded"
                        );
                        metrics::inc_stuck_requests(proof_type_label);
                        metrics::record_terminal_proof_job(metrics::PROOF_STATUS_FAILED, &job);
                    }
                    Ok(RetryOutcome::Skipped) => {
                        warn!(
                            proof_request_id = %request.id,
                            "Stuck request no longer eligible for retry"
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
            metrics::record_terminal_proof_job(metrics::PROOF_STATUS_FAILED, job);
        }
    }
}
