//! Worker heartbeat configuration and delivery loop.

use std::time::Duration;

use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::HeartbeatRequest;
use chrono::{DateTime, Utc};
use tokio::time::{Instant, sleep, timeout};
use tracing::warn;

use crate::{ClaimedProofJobMetadata, ProofSubmitter};

/// Minimum proof-generation heartbeat interval.
pub const MIN_WORKER_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(1);

/// Default interval between worker API heartbeats while a proof is being generated.
pub const DEFAULT_WORKER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Default lock duration requested by proof-generation heartbeats.
///
/// A value of zero asks the prover service to use its server-side default.
pub const DEFAULT_WORKER_HEARTBEAT_LOCK_DURATION_SECONDS: u32 = 0;

/// Assumed server default lock when hosts request `0` and no `lock_expires_at` is available.
pub const ASSUMED_DEFAULT_WORKER_LOCK_DURATION_SECONDS: u32 = 300;

/// Default maximum consecutive retryable heartbeat failures before aborting generation.
pub const DEFAULT_WORKER_MAX_CONSECUTIVE_HEARTBEAT_FAILURES: u32 = 5;

/// Heartbeat settings used while a worker is generating a proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerHeartbeatConfig {
    /// Delay between heartbeat attempts.
    pub interval: Duration,
    /// Requested lock duration in seconds. Zero uses the server default.
    pub lock_duration_seconds: u32,
    /// Maximum consecutive retryable heartbeat failures before aborting proof generation.
    pub max_consecutive_failures: u32,
}

impl WorkerHeartbeatConfig {
    /// Creates a heartbeat config.
    pub const fn new(interval: Duration, lock_duration_seconds: u32) -> Self {
        Self::with_max_consecutive_failures(
            interval,
            lock_duration_seconds,
            DEFAULT_WORKER_MAX_CONSECUTIVE_HEARTBEAT_FAILURES,
        )
    }

    /// Creates a heartbeat config with an explicit retryable failure limit.
    pub const fn with_max_consecutive_failures(
        interval: Duration,
        lock_duration_seconds: u32,
        max_consecutive_failures: u32,
    ) -> Self {
        Self { interval, lock_duration_seconds, max_consecutive_failures }
    }

    /// Returns the configured interval clamped to the minimum allowed delay.
    pub fn normalized_interval(&self) -> Duration {
        self.interval.max(MIN_WORKER_HEARTBEAT_INTERVAL)
    }

    /// Returns the configured retryable failure limit clamped to at least one.
    pub const fn normalized_max_consecutive_failures(&self) -> u32 {
        if self.max_consecutive_failures == 0 { 1 } else { self.max_consecutive_failures }
    }

    /// Fallback lock duration when the server did not provide `lock_expires_at`.
    pub const fn effective_lock_duration(&self) -> Duration {
        let seconds = if self.lock_duration_seconds == 0 {
            ASSUMED_DEFAULT_WORKER_LOCK_DURATION_SECONDS
        } else {
            self.lock_duration_seconds
        };
        Duration::from_secs(seconds as u64)
    }
}

impl Default for WorkerHeartbeatConfig {
    fn default() -> Self {
        Self::new(DEFAULT_WORKER_HEARTBEAT_INTERVAL, DEFAULT_WORKER_HEARTBEAT_LOCK_DURATION_SECONDS)
    }
}

/// Worker heartbeat delivery.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerHeartbeat;

impl WorkerHeartbeat {
    /// Sends heartbeats until a non-recoverable failure or lease budget exhaustion.
    ///
    /// The lease budget is derived from `lock_expires_at` when present and refreshed from each
    /// successful heartbeat response. When absent, falls back to
    /// [`WorkerHeartbeatConfig::effective_lock_duration`].
    pub async fn until_failure<Client>(
        submitter: &ProofSubmitter<Client>,
        claim: &ClaimedProofJobMetadata,
        config: WorkerHeartbeatConfig,
        lock_expires_at: Option<DateTime<Utc>>,
    ) -> ProverServiceClientError
    where
        Client: ProverWorkerProvider,
    {
        let max_consecutive_failures = config.normalized_max_consecutive_failures();
        let mut consecutive_failures = 0;
        let mut deadline = Self::deadline_from_expiry(lock_expires_at)
            .unwrap_or_else(|| Instant::now() + config.effective_lock_duration());

        loop {
            // Check before sleeping so an already-expired claim aborts immediately.
            let Some(remaining_before_sleep) = deadline.checked_duration_since(Instant::now())
            else {
                warn!(
                    session_id = %claim.session_id,
                    lock_id = %claim.lock_id,
                    worker_id = %claim.worker_id,
                    "proof job heartbeat budget exceeded claimed lock expiry"
                );
                return Self::lease_budget_exceeded_error();
            };

            let interval = config.normalized_interval();
            let delay = if remaining_before_sleep > interval {
                interval
            } else {
                remaining_before_sleep
                    .checked_div(2)
                    .unwrap_or(MIN_WORKER_HEARTBEAT_INTERVAL)
                    .max(MIN_WORKER_HEARTBEAT_INTERVAL)
                    .min(remaining_before_sleep)
            };
            sleep(delay).await;

            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                warn!(
                    session_id = %claim.session_id,
                    lock_id = %claim.lock_id,
                    worker_id = %claim.worker_id,
                    "proof job heartbeat budget exceeded claimed lock expiry"
                );
                return Self::lease_budget_exceeded_error();
            };

            let heartbeat = HeartbeatRequest {
                session_id: claim.session_id.clone(),
                lock_id: claim.lock_id.clone(),
                worker_id: claim.worker_id.clone(),
                lock_duration_seconds: config.lock_duration_seconds,
            };

            let result = match timeout(remaining, submitter.heartbeat(heartbeat)).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    warn!(
                        session_id = %claim.session_id,
                        lock_id = %claim.lock_id,
                        worker_id = %claim.worker_id,
                        "proof job heartbeat attempt exceeded remaining lock expiry"
                    );
                    return Self::lease_budget_exceeded_error();
                }
            };

            match result {
                Ok(response) => {
                    consecutive_failures = 0;
                    if let Some(next_deadline) =
                        Self::deadline_from_expiry(response.job.lock_expires_at)
                    {
                        deadline = next_deadline;
                    }
                }
                Err(error) if error.is_retryable() => {
                    consecutive_failures += 1;

                    if consecutive_failures >= max_consecutive_failures {
                        warn!(
                            session_id = %claim.session_id,
                            lock_id = %claim.lock_id,
                            worker_id = %claim.worker_id,
                            consecutive_failures,
                            max_consecutive_failures,
                            error = %error,
                            "proof job heartbeat retryable failures exceeded limit"
                        );
                        return error;
                    }
                }
                Err(error) => return error,
            }
        }
    }

    fn deadline_from_expiry(lock_expires_at: Option<DateTime<Utc>>) -> Option<Instant> {
        let expires_at = lock_expires_at?;
        let remaining = (expires_at - Utc::now()).to_std().unwrap_or_default();
        Some(Instant::now() + remaining)
    }

    fn lease_budget_exceeded_error() -> ProverServiceClientError {
        ProverServiceClientError::LeaseBudgetExceeded(
            "heartbeat budget exceeded claimed lock expiry".to_owned(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };

    use async_trait::async_trait;
    use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
    use base_prover_service_protocol::{
        GetNextProofRequest, GetNextProofResponse, GetProofSessionRequest, GetProofSessionResponse,
        HeartbeatRequest, HeartbeatResponse, RecordProofSessionRequest, RecordProofSessionResponse,
        WorkerSubmitProofRequest, WorkerSubmitProofResponse,
    };
    use tokio::time::advance;

    use super::*;
    use crate::ProofSubmitter;

    #[derive(Clone, Debug)]
    struct HangingClient {
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl ProverWorkerProvider for HangingClient {
        async fn get_next_proof(
            &self,
            _request: GetNextProofRequest,
        ) -> Result<GetNextProofResponse, ProverServiceClientError> {
            unreachable!()
        }

        async fn heartbeat(
            &self,
            _request: HeartbeatRequest,
        ) -> Result<HeartbeatResponse, ProverServiceClientError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }

        async fn submit_proof(
            &self,
            _request: WorkerSubmitProofRequest,
        ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError> {
            unreachable!()
        }

        async fn get_proof_session(
            &self,
            _request: GetProofSessionRequest,
        ) -> Result<GetProofSessionResponse, ProverServiceClientError> {
            unreachable!()
        }

        async fn record_proof_session(
            &self,
            _request: RecordProofSessionRequest,
        ) -> Result<RecordProofSessionResponse, ProverServiceClientError> {
            unreachable!()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn aborts_when_heartbeat_exceeds_remaining_lease() {
        let calls = Arc::new(AtomicU32::new(0));
        let submitter = ProofSubmitter::new(HangingClient { calls: Arc::clone(&calls) });
        let config =
            WorkerHeartbeatConfig::with_max_consecutive_failures(Duration::from_secs(1), 2, 100);
        let claim = ClaimedProofJobMetadata {
            session_id: "session-1".to_owned(),
            lock_id: "lock-1".to_owned(),
            worker_id: "worker-1".to_owned(),
        };

        let failure = tokio::spawn({
            let submitter = submitter.clone();
            let claim = claim.clone();
            async move { WorkerHeartbeat::until_failure(&submitter, &claim, config, None).await }
        });

        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        advance(Duration::from_secs(1)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        advance(Duration::from_secs(1)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        let error = failure.await.expect("heartbeat task should finish");
        assert!(matches!(error, ProverServiceClientError::LeaseBudgetExceeded(_)));
    }
}
