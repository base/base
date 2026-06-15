//! Worker heartbeat configuration and delivery loop.

use std::time::Duration;

use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::HeartbeatRequest;
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::ClaimedProofJobMetadata;

/// Minimum proof-generation heartbeat interval.
pub const MIN_WORKER_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(1);

/// Default interval between worker API heartbeats while a proof is being generated.
pub const DEFAULT_WORKER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Default lock duration requested by proof-generation heartbeats.
///
/// A value of zero asks the prover service to use its server-side default.
pub const DEFAULT_WORKER_HEARTBEAT_LOCK_DURATION_SECONDS: u32 = 0;

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
    /// Sends heartbeats until a non-recoverable heartbeat failure occurs.
    ///
    /// Retryable errors may already have exhausted the client's per-call retry budget.
    pub async fn until_failure<Client>(
        client: &Client,
        claim: &ClaimedProofJobMetadata,
        config: WorkerHeartbeatConfig,
    ) -> ProverServiceClientError
    where
        Client: ProverWorkerProvider,
    {
        let max_consecutive_failures = config.normalized_max_consecutive_failures();
        let mut consecutive_failures = 0;
        let heartbeat = HeartbeatRequest {
            session_id: claim.session_id.clone(),
            lock_id: claim.lock_id.clone(),
            worker_id: claim.worker_id.clone(),
            lock_duration_seconds: config.lock_duration_seconds,
        };

        loop {
            sleep(config.normalized_interval()).await;

            match client.heartbeat(heartbeat.clone()).await {
                Ok(response) => {
                    consecutive_failures = 0;

                    debug!(
                        session_id = %claim.session_id,
                        lock_id = %claim.lock_id,
                        worker_id = %claim.worker_id,
                        lock_expires_at = ?response.job.lock_expires_at,
                        "proof job heartbeat accepted"
                    );
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

                    warn!(
                        session_id = %claim.session_id,
                        lock_id = %claim.lock_id,
                        worker_id = %claim.worker_id,
                        consecutive_failures,
                        max_consecutive_failures,
                        error = %error,
                        "proof job heartbeat failed; retrying on next interval"
                    );
                }
                Err(error) => {
                    warn!(
                        session_id = %claim.session_id,
                        lock_id = %claim.lock_id,
                        worker_id = %claim.worker_id,
                        error = %error,
                        "proof job heartbeat failed permanently"
                    );
                    return error;
                }
            }
        }
    }
}
