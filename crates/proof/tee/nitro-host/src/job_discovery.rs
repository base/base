//! Job discovery loop for prover-service worker claims.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{GetNextProofRequest, ProofJob, ProofType, TeeKind};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{JoinError, JoinHandle, JoinSet},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::ProofGenerator;

/// Minimum delay used by the discovery loop when no job is available or an error occurs.
pub const MIN_JOB_DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Default delay used by the discovery loop when no job is available or an error occurs.
pub const DEFAULT_JOB_DISCOVERY_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default lock duration requested when claiming a proof job.
///
/// A value of zero asks the prover service to use its server-side default.
pub const DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS: u32 = 0;

/// Default maximum number of claimed proof jobs being generated concurrently.
pub const DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS: usize = 1;

/// Default worker identifier used by generic nitro worker configs.
pub const DEFAULT_JOB_DISCOVERY_WORKER_ID: &str = "nitro-host";

/// Future that generates a claimed proof job and starts proof submission.
pub type JobDiscoveryTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Settings used by the prover-service job discovery loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobDiscoveryConfig {
    /// Worker identifier used when claiming prover-service jobs.
    pub worker_id: String,
    /// Delay after an empty or failed discovery attempt.
    pub poll_interval: Duration,
    /// Requested claim lock duration in seconds. Zero uses the server default.
    pub lock_duration_seconds: u32,
    /// Maximum number of claimed proof jobs being generated concurrently.
    ///
    /// This is worker-side backpressure for in-flight generation, not just claim RPCs.
    pub max_concurrent_jobs: usize,
}

impl JobDiscoveryConfig {
    /// Creates a job discovery config using default timings.
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            poll_interval: DEFAULT_JOB_DISCOVERY_POLL_INTERVAL,
            lock_duration_seconds: DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS,
            max_concurrent_jobs: DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
        }
    }

    /// Sets the delay after empty or failed discovery attempts.
    pub const fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Sets the requested claim lock duration in seconds.
    pub const fn with_lock_duration_seconds(mut self, lock_duration_seconds: u32) -> Self {
        self.lock_duration_seconds = lock_duration_seconds;
        self
    }

    /// Sets the maximum number of claimed proof jobs being generated concurrently.
    pub const fn with_max_concurrent_jobs(mut self, max_concurrent_jobs: usize) -> Self {
        self.max_concurrent_jobs = max_concurrent_jobs;
        self
    }

    /// Returns the configured poll interval clamped to the minimum allowed delay.
    pub fn normalized_poll_interval(&self) -> Duration {
        self.poll_interval.max(MIN_JOB_DISCOVERY_POLL_INTERVAL)
    }

    /// Returns the configured concurrent job limit clamped to at least one.
    pub const fn normalized_max_concurrent_jobs(&self) -> usize {
        if self.max_concurrent_jobs == 0 { 1 } else { self.max_concurrent_jobs }
    }

    /// Builds the worker claim request for this nitro host.
    pub fn get_next_proof_request(&self) -> GetNextProofRequest {
        GetNextProofRequest {
            worker_id: self.worker_id.clone(),
            proof_type: ProofType::Tee,
            tee_kinds: vec![TeeKind::AwsNitro],
            zk_vms: Vec::new(),
            lock_duration_seconds: self.lock_duration_seconds,
        }
    }
}

impl Default for JobDiscoveryConfig {
    fn default() -> Self {
        Self::new(DEFAULT_JOB_DISCOVERY_WORKER_ID)
    }
}

/// Polls the prover service for Nitro TEE proof jobs and prepares proof generation tasks.
#[derive(Debug)]
pub struct JobDiscovery<Client> {
    client: Client,
    proof_generator: Arc<ProofGenerator<Client>>,
    config: JobDiscoveryConfig,
    generator_permits: Arc<Semaphore>,
}

/// Outcome of one job discovery poll.
pub enum JobDiscoveryPollOutcome {
    /// No matching proof job was available.
    Empty,
    /// A proof job was claimed and its proof generator task is ready to spawn.
    Claimed {
        /// Future for the proof generator task.
        task: JobDiscoveryTask,
    },
}

impl std::fmt::Debug for JobDiscoveryPollOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.debug_tuple("Empty").finish(),
            Self::Claimed { .. } => f.debug_struct("Claimed").finish_non_exhaustive(),
        }
    }
}

impl<Client> JobDiscovery<Client> {
    /// Creates a job discovery component.
    pub fn new(
        client: Client,
        proof_generator: Arc<ProofGenerator<Client>>,
        config: JobDiscoveryConfig,
    ) -> Self {
        let generator_permits = Arc::new(Semaphore::new(config.normalized_max_concurrent_jobs()));

        Self { client, proof_generator, config, generator_permits }
    }

    /// Returns the discovery config.
    pub const fn config(&self) -> &JobDiscoveryConfig {
        &self.config
    }
}

impl<Client> JobDiscovery<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Runs the discovery loop until the cancellation token is cancelled.
    pub async fn run_until_cancelled(self, cancel: CancellationToken) {
        let mut proof_tasks = JoinSet::new();

        info!(
            worker_id = %self.config.worker_id,
            poll_interval_ms = self.config.normalized_poll_interval().as_millis(),
            lock_duration_seconds = self.config.lock_duration_seconds,
            "starting nitro job discovery"
        );

        loop {
            if cancel.is_cancelled() {
                break;
            }

            Self::drain_finished_proof_tasks(&mut proof_tasks);

            let result = tokio::select! {
                // Cancelling this branch can drop an in-flight claim RPC after the service has
                // accepted it. Leases expire server-side, so the job becomes claimable again when
                // the requested lock duration elapses.
                () = cancel.cancelled() => break,
                result = self.poll_once() => result,
            };

            match result {
                Ok(JobDiscoveryPollOutcome::Claimed { task }) => {
                    proof_tasks.spawn(task);
                }
                Ok(JobDiscoveryPollOutcome::Empty) => {
                    self.sleep_until_next_poll(&cancel).await;
                }
                Err(error) => {
                    warn!(
                        worker_id = %self.config.worker_id,
                        retryable = error.is_retryable(),
                        error = %error,
                        "nitro job discovery failed"
                    );
                    self.sleep_until_next_poll(&cancel).await;
                }
            }
        }

        self.proof_generator.submission_cancel().cancel();
        while let Some(result) = proof_tasks.join_next().await {
            Self::log_proof_task_join_result(result);
        }

        info!(worker_id = %self.config.worker_id, "nitro job discovery stopped");
    }

    /// Spawns the discovery loop as a Tokio task.
    pub fn spawn_until_cancelled(self, cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run_until_cancelled(cancel).await;
        })
    }

    /// Polls the prover service once, returning a proof generation task when a job is claimed.
    pub async fn poll_once(&self) -> Result<JobDiscoveryPollOutcome, ProverServiceClientError> {
        let permit = Arc::clone(&self.generator_permits)
            .acquire_owned()
            .await
            .expect("semaphore is not closed");
        let request = self.config.get_next_proof_request();
        let response = self.client.get_next_proof(request).await?;

        let Some(job) = response.job else {
            drop(permit);
            debug!(worker_id = %self.config.worker_id, "no nitro proof job available");
            return Ok(JobDiscoveryPollOutcome::Empty);
        };

        let task = self.proof_generator_task(job, permit);
        Ok(JobDiscoveryPollOutcome::Claimed { task })
    }

    /// Builds a proof generator task for a claimed prover-service job.
    pub fn proof_generator_task(
        &self,
        job: ProofJob,
        permit: OwnedSemaphorePermit,
    ) -> JobDiscoveryTask {
        let session_id = job.session_id.clone();
        let proof_generator = Arc::clone(&self.proof_generator);

        Box::pin(async move {
            // Keep this permit until generation hands off to the submitter. This prevents the
            // worker from over-claiming jobs that would only wait behind the enclave pool.
            let _permit = permit;
            match proof_generator.generate_and_submit(job).await {
                Ok(task) => {
                    drop(task);
                }
                Err(error) => {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "nitro proof generator task failed"
                    );
                }
            }
        })
    }

    /// Sleeps until the next discovery poll or cancellation, whichever happens first.
    pub async fn sleep_until_next_poll(&self, cancel: &CancellationToken) {
        let deadline = Instant::now() + self.config.normalized_poll_interval();

        tokio::select! {
            () = cancel.cancelled() => {}
            () = sleep_until(deadline) => {}
        }
    }

    fn log_proof_task_join_result(result: Result<(), JoinError>) {
        match result {
            Ok(()) => {}
            Err(error) => {
                warn!(error = %error, "nitro proof generator task join failed");
            }
        }
    }

    fn drain_finished_proof_tasks(proof_tasks: &mut JoinSet<()>) {
        while let Some(result) = proof_tasks.try_join_next() {
            Self::log_proof_task_join_result(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alloy_genesis::ChainConfig;
    use async_trait::async_trait;
    use base_common_genesis::RollupConfig;
    use base_proof_host::ProverConfig;
    use base_proof_tee_nitro_enclave::Server as EnclaveServer;
    use base_prover_service_protocol::{
        GetNextProofResponse, HeartbeatRequest, HeartbeatResponse, ProofJob, ProofJobStatus,
        ProofRequest, ProofRequestKind, WorkerSubmitProofRequest, WorkerSubmitProofResponse,
        ZkProofRequest, ZkVm,
    };
    use chrono::Utc;
    use tokio::time::timeout;

    use super::*;
    use crate::{NitroEnclavePool, NitroTransport, ProofGeneratorHeartbeatConfig, ProofSubmitter};

    #[derive(Clone, Debug)]
    struct MockWorkerClient {
        state: Arc<Mutex<MockWorkerState>>,
    }

    #[derive(Debug, Default)]
    struct MockWorkerState {
        get_next_requests: Vec<GetNextProofRequest>,
        next_job: Option<ProofJob>,
    }

    impl MockWorkerClient {
        fn new(next_job: Option<ProofJob>) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockWorkerState {
                    get_next_requests: Vec::new(),
                    next_job,
                })),
            }
        }

        fn get_next_requests(&self) -> Vec<GetNextProofRequest> {
            self.state
                .lock()
                .expect("mock worker state lock should not be poisoned")
                .get_next_requests
                .clone()
        }
    }

    #[async_trait]
    impl ProverWorkerProvider for MockWorkerClient {
        async fn get_next_proof(
            &self,
            request: GetNextProofRequest,
        ) -> Result<GetNextProofResponse, ProverServiceClientError> {
            let mut state =
                self.state.lock().expect("mock worker state lock should not be poisoned");
            state.get_next_requests.push(request);

            Ok(GetNextProofResponse { job: state.next_job.take() })
        }

        async fn heartbeat(
            &self,
            _request: HeartbeatRequest,
        ) -> Result<HeartbeatResponse, ProverServiceClientError> {
            panic!("heartbeat is not used by job discovery tests")
        }

        async fn submit_proof(
            &self,
            _request: WorkerSubmitProofRequest,
        ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError> {
            panic!("submit_proof is not used by job discovery tests")
        }
    }

    fn test_prover_config() -> ProverConfig {
        ProverConfig {
            l1_eth_url: "http://127.0.0.1:1".to_string(),
            l2_eth_url: "http://127.0.0.1:1".to_string(),
            l1_beacon_url: "http://127.0.0.1:1".to_string(),
            l2_chain_id: 0,
            rollup_config: RollupConfig::default(),
            l1_config: ChainConfig::default(),
            enable_experimental_witness_endpoint: false,
        }
    }

    fn test_generator(client: MockWorkerClient) -> Arc<ProofGenerator<MockWorkerClient>> {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let pool = NitroEnclavePool::new(test_prover_config(), transport);
        let submitter = ProofSubmitter::new(client);

        Arc::new(ProofGenerator::new(
            Arc::new(pool),
            submitter,
            ProofGeneratorHeartbeatConfig::default(),
        ))
    }

    fn compressed_job() -> ProofJob {
        let session_id = "session-1".to_string();
        let now = Utc::now();

        ProofJob {
            session_id: session_id.clone(),
            status: ProofJobStatus::Claimed,
            request: ProofRequest {
                session_id: Some(session_id),
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number: 1,
                    number_of_blocks_to_prove: 1,
                    sequence_window: None,
                    l1_head: None,
                    intermediate_root_interval: None,
                    zk_vm: ZkVm::Sp1,
                }),
            },
            attempt: 1,
            lock_id: Some("lock-1".to_string()),
            worker_id: Some("worker-1".to_string()),
            lock_expires_at: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            error_message: None,
        }
    }

    #[test]
    fn config_builds_nitro_tee_claim_request() {
        let config = JobDiscoveryConfig::new("worker-a")
            .with_poll_interval(Duration::ZERO)
            .with_lock_duration_seconds(30)
            .with_max_concurrent_jobs(0);

        let request = config.get_next_proof_request();

        assert_eq!(request.worker_id, "worker-a");
        assert_eq!(request.proof_type, ProofType::Tee);
        assert_eq!(request.tee_kinds, vec![TeeKind::AwsNitro]);
        assert!(request.zk_vms.is_empty());
        assert_eq!(request.lock_duration_seconds, 30);
        assert_eq!(config.normalized_poll_interval(), MIN_JOB_DISCOVERY_POLL_INTERVAL);
        assert_eq!(config.normalized_max_concurrent_jobs(), 1);
    }

    #[tokio::test]
    async fn poll_once_returns_empty_when_no_job_is_available() {
        let client = MockWorkerClient::new(None);
        let discovery = JobDiscovery::new(
            client.clone(),
            test_generator(client.clone()),
            JobDiscoveryConfig::new("worker-a").with_lock_duration_seconds(45),
        );

        let outcome = discovery.poll_once().await.expect("poll should succeed");

        assert!(matches!(outcome, JobDiscoveryPollOutcome::Empty));
        let requests = client.get_next_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].worker_id, "worker-a");
        assert_eq!(requests[0].proof_type, ProofType::Tee);
        assert_eq!(requests[0].tee_kinds, vec![TeeKind::AwsNitro]);
        assert_eq!(requests[0].lock_duration_seconds, 45);
    }

    #[tokio::test]
    async fn poll_once_spawns_proof_generator_when_job_is_available() {
        let client = MockWorkerClient::new(Some(compressed_job()));
        let discovery = JobDiscovery::new(
            client.clone(),
            test_generator(client.clone()),
            JobDiscoveryConfig::new("worker-a"),
        );

        let outcome = discovery.poll_once().await.expect("poll should succeed");

        let JobDiscoveryPollOutcome::Claimed { task } = outcome else {
            panic!("expected proof generator task to be returned");
        };
        timeout(Duration::from_secs(1), task).await.expect("proof generator task should finish");
        assert_eq!(client.get_next_requests().len(), 1);
    }

    #[tokio::test]
    async fn poll_once_waits_for_generator_permit_before_claiming_job() {
        let client = MockWorkerClient::new(None);
        let discovery = JobDiscovery::new(
            client.clone(),
            test_generator(client.clone()),
            JobDiscoveryConfig::new("worker-a").with_max_concurrent_jobs(1),
        );
        let permit = Arc::clone(&discovery.generator_permits)
            .try_acquire_owned()
            .expect("test should acquire the only permit");

        let blocked = timeout(Duration::from_millis(5), discovery.poll_once()).await;

        assert!(blocked.is_err(), "poll should wait while all generator permits are held");
        assert!(
            client.get_next_requests().is_empty(),
            "discovery must not claim another job without generator capacity"
        );

        drop(permit);

        let outcome = timeout(Duration::from_secs(1), discovery.poll_once())
            .await
            .expect("poll should proceed after permit is released")
            .expect("poll should succeed");

        assert!(matches!(outcome, JobDiscoveryPollOutcome::Empty));
        assert_eq!(client.get_next_requests().len(), 1);
    }
}
