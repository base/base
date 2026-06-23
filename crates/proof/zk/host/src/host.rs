//! ZK host orchestration.

use std::{sync::Arc, time::Duration};

use base_proof_worker::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
    DEFAULT_JOB_DISCOVERY_POLL_INTERVAL, JobDiscovery, JobDiscoveryConfig, ProofSubmitter,
};
use base_prover_service_client::ProverWorkerProvider;
use tokio_util::sync::CancellationToken;

use crate::{ProofGenerator, ProofGeneratorHeartbeatConfig, ZkProofClaimType, ZkProver, ZkVm};

/// Settings for running a ZK host against the prover service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZkHostConfig {
    worker_id: String,
    proof_type: ZkProofClaimType,
    zk_vms: Vec<ZkVm>,
    job_discovery_poll_interval: Duration,
    job_discovery_lock_duration_seconds: u32,
    job_discovery_max_concurrent_jobs: usize,
    proof_generator_heartbeat: ProofGeneratorHeartbeatConfig,
}

impl ZkHostConfig {
    /// Creates a ZK host config with default timing settings.
    pub fn new(
        worker_id: impl Into<String>,
        proof_type: ZkProofClaimType,
        zk_vms: impl Into<Vec<ZkVm>>,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            proof_type,
            zk_vms: zk_vms.into(),
            job_discovery_poll_interval: DEFAULT_JOB_DISCOVERY_POLL_INTERVAL,
            job_discovery_lock_duration_seconds: DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS,
            job_discovery_max_concurrent_jobs: DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
            proof_generator_heartbeat: ProofGeneratorHeartbeatConfig::default(),
        }
    }

    /// Creates an SP1 ZK host config with default timing settings.
    pub fn sp1(worker_id: impl Into<String>, proof_type: ZkProofClaimType) -> Self {
        Self::new(worker_id, proof_type, vec![ZkVm::Sp1])
    }

    /// Returns the worker identifier used for prover-service claims.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Returns the proof type claimed by the worker.
    pub const fn proof_type(&self) -> ZkProofClaimType {
        self.proof_type
    }

    /// Returns the ZK VMs claimed by the worker.
    pub fn zk_vms(&self) -> &[ZkVm] {
        &self.zk_vms
    }

    /// Returns the heartbeat settings used while proofs are generated.
    pub const fn proof_generator_heartbeat(&self) -> ProofGeneratorHeartbeatConfig {
        self.proof_generator_heartbeat
    }

    /// Returns the configured delay after empty or failed discovery attempts.
    pub const fn job_discovery_poll_interval(&self) -> Duration {
        self.job_discovery_poll_interval
    }

    /// Returns the requested claim lock duration in seconds.
    pub const fn job_discovery_lock_duration_seconds(&self) -> u32 {
        self.job_discovery_lock_duration_seconds
    }

    /// Returns the maximum number of claimed proof jobs generated concurrently.
    pub const fn job_discovery_max_concurrent_jobs(&self) -> usize {
        self.job_discovery_max_concurrent_jobs
    }

    /// Sets the delay after empty or failed discovery attempts.
    #[must_use]
    pub const fn with_job_discovery_poll_interval(
        mut self,
        job_discovery_poll_interval: Duration,
    ) -> Self {
        self.job_discovery_poll_interval = job_discovery_poll_interval;
        self
    }

    /// Sets the requested claim lock duration in seconds.
    #[must_use]
    pub const fn with_job_discovery_lock_duration_seconds(
        mut self,
        job_discovery_lock_duration_seconds: u32,
    ) -> Self {
        self.job_discovery_lock_duration_seconds = job_discovery_lock_duration_seconds;
        self
    }

    /// Sets the maximum number of claimed proof jobs generated concurrently.
    #[must_use]
    pub const fn with_job_discovery_max_concurrent_jobs(
        mut self,
        job_discovery_max_concurrent_jobs: usize,
    ) -> Self {
        self.job_discovery_max_concurrent_jobs = job_discovery_max_concurrent_jobs;
        self
    }

    /// Sets the heartbeat settings used while proofs are generated.
    #[must_use]
    pub const fn with_proof_generator_heartbeat(
        mut self,
        proof_generator_heartbeat: ProofGeneratorHeartbeatConfig,
    ) -> Self {
        self.proof_generator_heartbeat = proof_generator_heartbeat;
        self
    }

    /// Builds the shared worker discovery config.
    pub fn job_discovery_config(&self) -> JobDiscoveryConfig {
        JobDiscoveryConfig::zk(self.worker_id.clone(), self.proof_type, self.zk_vms.clone())
            .with_poll_interval(self.job_discovery_poll_interval)
            .with_lock_duration_seconds(self.job_discovery_lock_duration_seconds)
            .with_max_concurrent_jobs(self.job_discovery_max_concurrent_jobs)
    }

    /// Converts this config into the shared worker discovery config.
    pub fn into_job_discovery_config(self) -> JobDiscoveryConfig {
        JobDiscoveryConfig::zk(self.worker_id, self.proof_type, self.zk_vms)
            .with_poll_interval(self.job_discovery_poll_interval)
            .with_lock_duration_seconds(self.job_discovery_lock_duration_seconds)
            .with_max_concurrent_jobs(self.job_discovery_max_concurrent_jobs)
    }
}

/// Runs ZK proof generation jobs claimed from the prover service.
#[derive(Debug)]
pub struct ZkHost<Client> {
    client: Client,
    prover: Arc<dyn ZkProver>,
    config: ZkHostConfig,
}

impl<Client> ZkHost<Client> {
    /// Creates a ZK host from a prover-service client and ZK prover backend.
    pub const fn new(client: Client, prover: Arc<dyn ZkProver>, config: ZkHostConfig) -> Self {
        Self { client, prover, config }
    }

    /// Returns the host config.
    pub const fn config(&self) -> &ZkHostConfig {
        &self.config
    }
}

impl<Client> ZkHost<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Runs the host until cancellation is requested.
    pub async fn run_until_cancelled(self, cancel: CancellationToken) {
        let Self { client, prover, config } = self;
        let submitter = ProofSubmitter::new(client.clone());
        let proof_generator =
            Arc::new(ProofGenerator::new(prover, submitter, config.proof_generator_heartbeat));
        let discovery =
            JobDiscovery::new(client, proof_generator, config.into_job_discovery_config());

        discovery.run_until_cancelled(cancel).await;
    }
}
