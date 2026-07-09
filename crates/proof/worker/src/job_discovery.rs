//! Job discovery loop for prover-service worker claims.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{GetNextProofRequest, ProofJob, ProofType, TeeKind, ZkVm};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{JoinError, JoinHandle, JoinSet},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::ClaimedProofJobHandler;

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

/// Future that generates a claimed proof job and starts proof submission.
pub type JobDiscoveryTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// ZK proof types claimed by every ZK host.
pub const ZK_PROOF_TYPES: [ProofType; 2] = [ProofType::Compressed, ProofType::SnarkGroth16];

/// Prover-service claim filter for a worker host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobClaimFilter {
    /// Claim TEE proof jobs for the configured TEE kinds.
    Tee {
        /// TEE kinds this worker can execute.
        tee_kinds: Vec<TeeKind>,
    },
    /// Claim ZK proof jobs for the configured virtual machines.
    Zk {
        /// ZK virtual machines this worker can execute.
        zk_vms: Vec<ZkVm>,
    },
}

impl JobClaimFilter {
    /// Creates a TEE claim filter.
    pub fn tee(tee_kinds: impl Into<Vec<TeeKind>>) -> Self {
        Self::Tee { tee_kinds: tee_kinds.into() }
    }

    /// Creates a ZK claim filter.
    pub fn zk(zk_vms: impl Into<Vec<ZkVm>>) -> Self {
        Self::Zk { zk_vms: zk_vms.into() }
    }

    /// Returns the prover-service proof types for this claim filter.
    pub const fn proof_types(&self) -> &'static [ProofType] {
        match self {
            Self::Tee { .. } => &[ProofType::Tee],
            Self::Zk { .. } => &ZK_PROOF_TYPES,
        }
    }

    /// Low-cardinality label used in discovery logs.
    pub const fn log_label(&self) -> &'static str {
        match self {
            Self::Tee { .. } => "tee",
            Self::Zk { .. } => "zk",
        }
    }

    /// Builds the worker claim requests for this filter.
    pub fn get_next_proof_requests(
        &self,
        worker_id: String,
        lock_duration_seconds: u32,
    ) -> impl Iterator<Item = GetNextProofRequest> {
        self.get_next_proof_requests_starting_at(worker_id, lock_duration_seconds, 0)
    }

    /// Builds the worker claim requests for this filter with a proof-type rotation offset.
    pub fn get_next_proof_requests_starting_at(
        &self,
        worker_id: String,
        lock_duration_seconds: u32,
        proof_type_offset: usize,
    ) -> impl Iterator<Item = GetNextProofRequest> {
        let requests = match self {
            Self::Tee { tee_kinds } => [
                Some(GetNextProofRequest {
                    worker_id,
                    proof_type: ProofType::Tee,
                    tee_kinds: tee_kinds.clone(),
                    zk_vms: Vec::new(),
                    lock_duration_seconds,
                }),
                None,
            ],
            Self::Zk { zk_vms } => {
                let zk_vms = zk_vms.clone();
                let proof_types = if proof_type_offset.is_multiple_of(ZK_PROOF_TYPES.len()) {
                    ZK_PROOF_TYPES
                } else {
                    [ProofType::SnarkGroth16, ProofType::Compressed]
                };
                let [first_proof_type, second_proof_type] = proof_types;

                [
                    Some(GetNextProofRequest {
                        worker_id: worker_id.clone(),
                        proof_type: first_proof_type,
                        tee_kinds: Vec::new(),
                        zk_vms: zk_vms.clone(),
                        lock_duration_seconds,
                    }),
                    Some(GetNextProofRequest {
                        worker_id,
                        proof_type: second_proof_type,
                        tee_kinds: Vec::new(),
                        zk_vms,
                        lock_duration_seconds,
                    }),
                ]
            }
        };

        requests.into_iter().flatten()
    }
}

/// Settings used by the prover-service job discovery loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobDiscoveryConfig {
    worker_id: String,
    claim_filter: JobClaimFilter,
    poll_interval: Duration,
    lock_duration_seconds: u32,
    max_concurrent_jobs: usize,
}

impl JobDiscoveryConfig {
    /// Creates a TEE job discovery config using default timings.
    pub fn tee(worker_id: impl Into<String>, tee_kinds: impl Into<Vec<TeeKind>>) -> Self {
        Self::new(worker_id, JobClaimFilter::tee(tee_kinds))
    }

    /// Creates a ZK job discovery config using default timings.
    pub fn zk(worker_id: impl Into<String>, zk_vms: impl Into<Vec<ZkVm>>) -> Self {
        Self::new(worker_id, JobClaimFilter::zk(zk_vms))
    }

    /// Creates a job discovery config using default timings.
    pub fn new(worker_id: impl Into<String>, claim_filter: JobClaimFilter) -> Self {
        Self {
            worker_id: worker_id.into(),
            claim_filter,
            poll_interval: DEFAULT_JOB_DISCOVERY_POLL_INTERVAL,
            lock_duration_seconds: DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS,
            max_concurrent_jobs: DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
        }
    }

    /// Returns the worker identifier used when claiming prover-service jobs.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Returns the claim filter for this worker host.
    pub const fn claim_filter(&self) -> &JobClaimFilter {
        &self.claim_filter
    }

    /// Returns the requested claim lock duration in seconds.
    pub const fn lock_duration_seconds(&self) -> u32 {
        self.lock_duration_seconds
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

    /// Builds the worker claim requests for this host.
    pub fn get_next_proof_requests(&self) -> impl Iterator<Item = GetNextProofRequest> {
        self.get_next_proof_requests_starting_at(0)
    }

    /// Builds the worker claim requests for this host with a proof-type rotation offset.
    pub fn get_next_proof_requests_starting_at(
        &self,
        proof_type_offset: usize,
    ) -> impl Iterator<Item = GetNextProofRequest> {
        self.claim_filter.get_next_proof_requests_starting_at(
            self.worker_id.clone(),
            self.lock_duration_seconds,
            proof_type_offset,
        )
    }
}

/// Polls the prover service for proof jobs and prepares proof generation tasks.
#[derive(Debug)]
pub struct JobDiscovery<Client, Generator> {
    client: Client,
    proof_generator: Arc<Generator>,
    config: JobDiscoveryConfig,
    generator_permits: Arc<Semaphore>,
    claim_offset: AtomicUsize,
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

impl<Client, Generator> JobDiscovery<Client, Generator> {
    /// Creates a job discovery component.
    pub fn new(
        client: Client,
        proof_generator: Arc<Generator>,
        config: JobDiscoveryConfig,
    ) -> Self {
        let generator_permits = Arc::new(Semaphore::new(config.normalized_max_concurrent_jobs()));

        Self {
            client,
            proof_generator,
            config,
            generator_permits,
            claim_offset: AtomicUsize::new(0),
        }
    }

    /// Returns the discovery config.
    pub const fn config(&self) -> &JobDiscoveryConfig {
        &self.config
    }
}

impl<Client, Generator> JobDiscovery<Client, Generator>
where
    Client: ProverWorkerProvider + 'static,
    Generator: ClaimedProofJobHandler,
{
    /// Runs the discovery loop until the cancellation token is cancelled.
    pub async fn run_until_cancelled(self, cancel: CancellationToken) {
        let mut proof_tasks = JoinSet::new();

        info!(
            worker_id = %self.config.worker_id,
            proof_types = ?self.config.claim_filter.proof_types(),
            poll_interval_ms = self.config.normalized_poll_interval().as_millis(),
            lock_duration_seconds = self.config.lock_duration_seconds,
            worker_kind = self.config.claim_filter.log_label(),
            "starting job discovery"
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
                result = self.claim_once() => result,
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
                        worker_kind = self.config.claim_filter.log_label(),
                        retryable = error.is_retryable(),
                        error = %error,
                        "job discovery failed"
                    );
                    self.sleep_until_next_poll(&cancel).await;
                }
            }
        }

        self.proof_generator.shutdown();
        while let Some(result) = proof_tasks.join_next().await {
            Self::log_proof_task_join_result(result);
        }

        info!(
            worker_id = %self.config.worker_id,
            worker_kind = self.config.claim_filter.log_label(),
            "job discovery stopped"
        );
    }

    /// Spawns the discovery loop as a Tokio task.
    pub fn spawn_until_cancelled(self, cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run_until_cancelled(cancel).await;
        })
    }

    /// Waits for generator capacity, then claims at most one proof job.
    ///
    /// This can wait while all generator permits are in use; use a cancellable
    /// `select!` when shutdown must interrupt capacity waiting.
    pub async fn claim_once(&self) -> Result<JobDiscoveryPollOutcome, ProverServiceClientError> {
        let Ok(permit) = Arc::clone(&self.generator_permits).acquire_owned().await else {
            warn!(
                worker_id = %self.config.worker_id,
                worker_kind = self.config.claim_filter.log_label(),
                "job discovery permits closed"
            );
            return Ok(JobDiscoveryPollOutcome::Empty);
        };

        if !self.proof_generator.ready_to_claim(&self.config.worker_id).await {
            drop(permit);
            return Ok(JobDiscoveryPollOutcome::Empty);
        }

        let proof_type_offset = self.claim_offset.fetch_add(1, Ordering::Relaxed);
        for request in self.config.get_next_proof_requests_starting_at(proof_type_offset) {
            let proof_type = request.proof_type;
            let response = match self.client.get_next_proof(request).await {
                Ok(response) => response,
                Err(error) => {
                    warn!(
                        worker_id = %self.config.worker_id,
                        worker_kind = self.config.claim_filter.log_label(),
                        proof_type = ?proof_type,
                        retryable = error.is_retryable(),
                        error = %error,
                        "job discovery claim failed for proof type"
                    );
                    return Err(error);
                }
            };

            if let Some(job) = response.job {
                let task = self.proof_generator_task(job, permit);
                return Ok(JobDiscoveryPollOutcome::Claimed { task });
            }
        }

        drop(permit);
        debug!(
            worker_id = %self.config.worker_id,
            worker_kind = self.config.claim_filter.log_label(),
            "no proof job available"
        );
        Ok(JobDiscoveryPollOutcome::Empty)
    }

    /// Builds a proof generator task for a claimed prover-service job.
    pub fn proof_generator_task(
        &self,
        job: ProofJob,
        permit: OwnedSemaphorePermit,
    ) -> JobDiscoveryTask {
        let session_id = job.session_id.clone();
        let proof_generator = Arc::clone(&self.proof_generator);
        let worker_kind = self.config.claim_filter.log_label();

        Box::pin(async move {
            // Keep this permit until generation hands off to the submitter. This prevents the
            // worker from over-claiming jobs that would only wait behind the proof backend.
            let _permit = permit;
            if let Err(error) = proof_generator.handle_claimed_job(job).await {
                warn!(
                    session_id = %session_id,
                    worker_kind = worker_kind,
                    error = %error,
                    "proof generator task failed"
                );
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
                warn!(error = %error, "proof generator task join failed");
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

    use async_trait::async_trait;
    use base_prover_service_protocol::{
        GetNextProofResponse, GetProofSessionRequest, GetProofSessionResponse, HeartbeatRequest,
        HeartbeatResponse, ProofJob, ProofJobStatus, ProofRequest, ProofRequestKind,
        RecordProofSessionRequest, RecordProofSessionResponse, SnarkGroth16ProofRequest,
        WorkerSubmitProofRequest, WorkerSubmitProofResponse, ZkProofRequest,
    };
    use chrono::Utc;
    use tokio::time::timeout;

    use super::*;

    #[derive(Clone, Debug)]
    struct MockWorkerClient {
        state: Arc<Mutex<MockWorkerState>>,
    }

    #[derive(Debug, Default)]
    struct MockWorkerState {
        get_next_requests: Vec<GetNextProofRequest>,
        next_job: Option<ProofJob>,
        failed_claims: Vec<ProofType>,
    }

    impl MockWorkerClient {
        fn new(next_job: Option<ProofJob>) -> Self {
            Self::with_failed_claims(next_job, Vec::new())
        }

        fn with_failed_claims(next_job: Option<ProofJob>, failed_claims: Vec<ProofType>) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockWorkerState {
                    get_next_requests: Vec::new(),
                    next_job,
                    failed_claims,
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
            let proof_type = request.proof_type;
            let mut state =
                self.state.lock().expect("mock worker state lock should not be poisoned");
            state.get_next_requests.push(request);

            if state.failed_claims.contains(&proof_type) {
                return Err(ProverServiceClientError::Timeout(format!(
                    "simulated {proof_type:?} claim failure"
                )));
            }

            let job = state.next_job.as_ref().is_some_and(|job| job_proof_type(job) == proof_type);
            let job = if job { state.next_job.take() } else { None };

            Ok(GetNextProofResponse { job })
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

        async fn get_proof_session(
            &self,
            _request: GetProofSessionRequest,
        ) -> Result<GetProofSessionResponse, ProverServiceClientError> {
            panic!("get_proof_session is not used by job discovery tests")
        }

        async fn record_proof_session(
            &self,
            _request: RecordProofSessionRequest,
        ) -> Result<RecordProofSessionResponse, ProverServiceClientError> {
            panic!("record_proof_session is not used by job discovery tests")
        }
    }

    #[derive(Debug, Default)]
    struct MockGenerator {
        generated: Arc<Mutex<Vec<String>>>,
        can_claim: bool,
    }

    #[async_trait]
    impl ClaimedProofJobHandler for MockGenerator {
        type Error = std::convert::Infallible;

        async fn ready_to_claim(&self, _worker_id: &str) -> bool {
            self.can_claim
        }

        async fn handle_claimed_job(&self, job: ProofJob) -> Result<(), Self::Error> {
            self.generated
                .lock()
                .expect("generated jobs lock should not be poisoned")
                .push(job.session_id);
            Ok(())
        }
    }

    fn compressed_job() -> ProofJob {
        proof_job(ProofRequestKind::Compressed(zk_request()))
    }

    fn snark_job() -> ProofJob {
        proof_job(ProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
            proof: zk_request(),
            prover_address: Default::default(),
        }))
    }

    fn zk_request() -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 1,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
        }
    }

    fn proof_job(request: ProofRequestKind) -> ProofJob {
        let session_id = "session-1".to_string();
        let now = Utc::now();

        ProofJob {
            session_id: session_id.clone(),
            status: ProofJobStatus::Claimed,
            request: ProofRequest { session_id, request },
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

    fn job_proof_type(job: &ProofJob) -> ProofType {
        match job.request.request {
            ProofRequestKind::Compressed(_) => ProofType::Compressed,
            ProofRequestKind::SnarkGroth16(_) => ProofType::SnarkGroth16,
            ProofRequestKind::Tee(_) => ProofType::Tee,
        }
    }

    #[test]
    fn config_builds_zk_claim_requests() {
        let config = JobDiscoveryConfig::zk("worker-a", vec![ZkVm::Sp1])
            .with_lock_duration_seconds(30)
            .with_max_concurrent_jobs(0);

        let requests = config.get_next_proof_requests().collect::<Vec<_>>();

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].worker_id, "worker-a");
        assert_eq!(requests[0].proof_type, ProofType::Compressed);
        assert!(requests[0].tee_kinds.is_empty());
        assert_eq!(requests[0].zk_vms, vec![ZkVm::Sp1]);
        assert_eq!(requests[0].lock_duration_seconds, 30);
        assert_eq!(requests[1].worker_id, "worker-a");
        assert_eq!(requests[1].proof_type, ProofType::SnarkGroth16);
        assert!(requests[1].tee_kinds.is_empty());
        assert_eq!(requests[1].zk_vms, vec![ZkVm::Sp1]);
        assert_eq!(requests[1].lock_duration_seconds, 30);
        assert_eq!(config.normalized_max_concurrent_jobs(), 1);
    }

    #[test]
    fn config_builds_nitro_claim_request() {
        let config = JobDiscoveryConfig::tee("worker-a", vec![TeeKind::AwsNitro])
            .with_lock_duration_seconds(45);

        let requests = config.get_next_proof_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];

        assert_eq!(request.worker_id, "worker-a");
        assert_eq!(request.proof_type, ProofType::Tee);
        assert_eq!(request.tee_kinds, vec![TeeKind::AwsNitro]);
        assert!(request.zk_vms.is_empty());
        assert_eq!(request.lock_duration_seconds, 45);
    }

    #[tokio::test]
    async fn claim_once_returns_empty_when_no_job_is_available() {
        let client = MockWorkerClient::new(None);
        let generator = Arc::new(MockGenerator { can_claim: true, ..Default::default() });
        let discovery = JobDiscovery::new(
            client.clone(),
            generator,
            JobDiscoveryConfig::zk("worker-a", vec![ZkVm::Sp1]),
        );

        let outcome = discovery.claim_once().await.expect("claim should succeed");

        assert!(matches!(outcome, JobDiscoveryPollOutcome::Empty));
        let requests = client.get_next_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].proof_type, ProofType::Compressed);
        assert_eq!(requests[1].proof_type, ProofType::SnarkGroth16);
    }

    #[tokio::test]
    async fn claim_once_skips_claim_when_generator_is_not_ready() {
        let client = MockWorkerClient::new(Some(compressed_job()));
        let discovery = JobDiscovery::new(
            client.clone(),
            Arc::new(MockGenerator::default()),
            JobDiscoveryConfig::zk("worker-a", vec![ZkVm::Sp1]),
        );

        let outcome = discovery.claim_once().await.expect("claim should succeed");

        assert!(matches!(outcome, JobDiscoveryPollOutcome::Empty));
        assert!(client.get_next_requests().is_empty());
    }

    #[tokio::test]
    async fn claim_once_returns_empty_when_generator_permits_are_closed() {
        let client = MockWorkerClient::new(Some(compressed_job()));
        let discovery = JobDiscovery::new(
            client.clone(),
            Arc::new(MockGenerator { can_claim: true, ..Default::default() }),
            JobDiscoveryConfig::zk("worker-a", vec![ZkVm::Sp1]),
        );
        discovery.generator_permits.close();

        let outcome = discovery.claim_once().await.expect("claim should succeed");

        assert!(matches!(outcome, JobDiscoveryPollOutcome::Empty));
        assert!(client.get_next_requests().is_empty());
    }

    #[tokio::test]
    async fn claim_once_spawns_generator_task_when_job_is_available() {
        let client = MockWorkerClient::new(Some(compressed_job()));
        let generator = Arc::new(MockGenerator { can_claim: true, ..Default::default() });
        let generated = Arc::clone(&generator.generated);
        let discovery = JobDiscovery::new(
            client,
            generator,
            JobDiscoveryConfig::zk("worker-a", vec![ZkVm::Sp1]),
        );

        let outcome = discovery.claim_once().await.expect("claim should succeed");

        let JobDiscoveryPollOutcome::Claimed { task } = outcome else {
            panic!("expected proof generator task to be returned");
        };
        timeout(Duration::from_secs(1), task).await.expect("proof generator task should finish");
        assert_eq!(
            *generated.lock().expect("generated jobs lock should not be poisoned"),
            vec!["session-1".to_string()]
        );
    }

    #[tokio::test]
    async fn claim_once_rotates_zk_claim_order() {
        let client = MockWorkerClient::new(Some(snark_job()));
        let generator = Arc::new(MockGenerator { can_claim: true, ..Default::default() });
        let generated = Arc::clone(&generator.generated);
        let discovery = JobDiscovery::new(
            client.clone(),
            generator,
            JobDiscoveryConfig::zk("worker-a", vec![ZkVm::Sp1]),
        );
        discovery.claim_offset.store(1, Ordering::Relaxed);

        let outcome = discovery.claim_once().await.expect("claim should succeed");

        let JobDiscoveryPollOutcome::Claimed { task } = outcome else {
            panic!("expected proof generator task to be returned");
        };
        timeout(Duration::from_secs(1), task).await.expect("proof generator task should finish");
        let requests = client.get_next_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].proof_type, ProofType::SnarkGroth16);
        assert_eq!(
            *generated.lock().expect("generated jobs lock should not be poisoned"),
            vec!["session-1".to_string()]
        );
    }

    #[tokio::test]
    async fn claim_once_returns_error_after_claim_failure_without_trying_next_type() {
        let client =
            MockWorkerClient::with_failed_claims(Some(snark_job()), vec![ProofType::Compressed]);
        let generator = Arc::new(MockGenerator { can_claim: true, ..Default::default() });
        let generated = Arc::clone(&generator.generated);
        let discovery = JobDiscovery::new(
            client.clone(),
            generator,
            JobDiscoveryConfig::zk("worker-a", vec![ZkVm::Sp1]),
        );

        let error =
            discovery.claim_once().await.expect_err("claim should surface the ambiguous failure");

        assert!(error.to_string().contains("simulated Compressed claim failure"));
        let requests = client.get_next_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].proof_type, ProofType::Compressed);
        assert_eq!(
            *generated.lock().expect("generated jobs lock should not be poisoned"),
            Vec::<String>::new()
        );
    }
}
