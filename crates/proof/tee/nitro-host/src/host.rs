//! Nitro TEE host orchestration.

use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use base_proof_worker::{JobDiscovery, JobDiscoveryConfig, ProofSubmitter};
use base_prover_service_client::ProverWorkerProvider;
use base_prover_service_protocol::TeeKind;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::{NitroEnclavePool, ProofGenerator, ProofGeneratorHeartbeatConfig};

/// Runs Nitro TEE proof generation jobs claimed from the prover service.
#[derive(Debug)]
pub struct NitroHost<Client> {
    client: Client,
    pool: Arc<NitroEnclavePool>,
    discovery: JobDiscoveryConfig,
    heartbeat: ProofGeneratorHeartbeatConfig,
}

impl<Client> NitroHost<Client> {
    /// Creates a Nitro host that claims only AWS Nitro TEE jobs.
    pub fn new(
        client: Client,
        pool: Arc<NitroEnclavePool>,
        worker_id: impl Into<String>,
        heartbeat: ProofGeneratorHeartbeatConfig,
    ) -> Self {
        Self {
            client,
            pool,
            discovery: JobDiscoveryConfig::tee(worker_id, vec![TeeKind::AwsNitro]),
            heartbeat,
        }
    }

    /// Sets the delay after empty or failed discovery attempts.
    #[must_use]
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.discovery = self.discovery.with_poll_interval(poll_interval);
        self
    }

    /// Sets the requested claim lock duration in seconds.
    #[must_use]
    pub fn with_lock_duration_seconds(mut self, lock_duration_seconds: u32) -> Self {
        self.discovery = self.discovery.with_lock_duration_seconds(lock_duration_seconds);
        self
    }

    /// Sets the maximum number of claimed proof jobs being generated concurrently.
    #[must_use]
    pub fn with_max_concurrent_jobs(mut self, max_concurrent_jobs: usize) -> Self {
        self.discovery = self.discovery.with_max_concurrent_jobs(max_concurrent_jobs);
        self
    }
}

impl<Client> NitroHost<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Runs the host until cancellation is requested.
    pub async fn run_until_cancelled(self, cancel: CancellationToken) {
        let Some(artifact_hash) = self.read_artifact_hash(&cancel).await else {
            return;
        };
        let discovery_config = self.discovery.with_supported_artifact_hash(artifact_hash);
        let submitter = ProofSubmitter::new(self.client.clone());
        let proof_generator = Arc::new(
            ProofGenerator::new(self.pool, submitter, self.heartbeat)
                .with_max_pending_submissions(discovery_config.normalized_max_concurrent_jobs()),
        );
        let discovery = JobDiscovery::new(self.client, proof_generator, discovery_config);

        discovery.run_until_cancelled(cancel).await;
    }

    /// Reads the local enclave image hash, retrying until it succeeds or is cancelled.
    ///
    /// The host cannot claim any job without this hash, so a transient read failure
    /// must not be fatal: the enclave is frequently still booting when the host
    /// starts, and exiting here would take the worker out of the pool permanently
    /// rather than for the few seconds the enclave needs.
    ///
    /// Returns `None` only if cancellation was requested while retrying.
    async fn read_artifact_hash(&self, cancel: &CancellationToken) -> Option<B256> {
        let retry_delay = self.discovery.normalized_poll_interval();
        loop {
            match self.pool.tee_image_hash().await {
                Ok(hash) => return Some(hash),
                Err(error) => {
                    warn!(
                        error = %error,
                        retry_in_secs = retry_delay.as_secs(),
                        "failed to read local Nitro artifact hash, retrying"
                    );
                }
            }

            tokio::select! {
                () = cancel.cancelled() => {
                    error!("cancelled before the local Nitro artifact hash could be read");
                    return None;
                }
                () = tokio::time::sleep(retry_delay) => {}
            }
        }
    }
}
