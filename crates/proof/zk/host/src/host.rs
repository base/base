//! ZK host orchestration.

use std::{collections::HashMap, sync::Arc, time::Duration};

use base_proof_worker::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
    DEFAULT_JOB_DISCOVERY_POLL_INTERVAL, JobDiscovery, JobDiscoveryConfig, ProofSubmitter,
};
use base_prover_service_client::ProverWorkerProvider;
use tokio_util::sync::CancellationToken;

use crate::{ProofGenerator, ProofGeneratorHeartbeatConfig, ZkBackend, ZkProver, ZkVm};

/// Settings for running a ZK host against the prover service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZkHostConfig {
    worker_id: String,
    zk_vms: Vec<ZkVm>,
    job_discovery_poll_interval: Duration,
    job_discovery_lock_duration_seconds: u32,
    job_discovery_max_concurrent_jobs: usize,
    proof_generator_heartbeat: ProofGeneratorHeartbeatConfig,
}

impl ZkHostConfig {
    /// Creates a ZK host config with default timing settings.
    pub fn new(worker_id: impl Into<String>, zk_vms: impl Into<Vec<ZkVm>>) -> Self {
        Self {
            worker_id: worker_id.into(),
            zk_vms: zk_vms.into(),
            job_discovery_poll_interval: DEFAULT_JOB_DISCOVERY_POLL_INTERVAL,
            job_discovery_lock_duration_seconds: DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS,
            job_discovery_max_concurrent_jobs: DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
            proof_generator_heartbeat: ProofGeneratorHeartbeatConfig::default(),
        }
    }

    /// Creates an SP1 ZK host config with default timing settings.
    pub fn sp1(worker_id: impl Into<String>) -> Self {
        Self::new(worker_id, vec![ZkVm::Sp1])
    }

    /// Returns the worker identifier used for prover-service claims.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
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
}

/// Runs ZK proof generation jobs claimed from the prover service.
#[derive(Debug)]
pub struct ZkHost<Client> {
    client: Client,
    provers: HashMap<ZkBackend, Arc<dyn ZkProver>>,
    config: ZkHostConfig,
}

impl<Client> ZkHost<Client> {
    /// Creates a ZK host from a prover-service client and ZK prover backends.
    pub fn new(
        client: Client,
        provers: HashMap<ZkBackend, Arc<dyn ZkProver>>,
        config: ZkHostConfig,
    ) -> Self {
        assert!(!provers.is_empty(), "ZK host requires at least one prover backend");
        Self { client, provers, config }
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
        let Self { client, provers, config } = self;
        let mut zk_backends = provers.keys().copied().collect::<Vec<_>>();
        zk_backends.sort_unstable_by_key(|backend| backend.as_str());
        let discovery_config = JobDiscoveryConfig::zk(config.worker_id, config.zk_vms, zk_backends)
            .with_poll_interval(config.job_discovery_poll_interval)
            .with_lock_duration_seconds(config.job_discovery_lock_duration_seconds)
            .with_max_concurrent_jobs(config.job_discovery_max_concurrent_jobs);
        let submitter = ProofSubmitter::new(client.clone());
        let proof_generator =
            Arc::new(ProofGenerator::new(provers, submitter, config.proof_generator_heartbeat));
        let discovery = JobDiscovery::new(client, proof_generator, discovery_config);

        discovery.run_until_cancelled(cancel).await;
    }
}
