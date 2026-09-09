//! Proof generation orchestration for claimed Nitro worker jobs.

use std::{future::Future, sync::Arc};

use async_trait::async_trait;
use base_proof_primitives::ProofRequest as NitroProofRequest;
use base_proof_worker::{
    ClaimedProofJobHandler, ClaimedProofJobMetadata, ClaimedProofJobMetadataError, ProofSubmitter,
    ProofTaskController, WorkerHeartbeat,
};
pub use base_proof_worker::{
    DEFAULT_WORKER_HEARTBEAT_INTERVAL as DEFAULT_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
    DEFAULT_WORKER_HEARTBEAT_LOCK_DURATION_SECONDS as DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_WORKER_MAX_CONSECUTIVE_HEARTBEAT_FAILURES as DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES,
    MIN_WORKER_HEARTBEAT_INTERVAL as MIN_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
    WorkerHeartbeatConfig as ProofGeneratorHeartbeatConfig,
};
use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{ProofJob, ProofRequestKind, TeeKind};
use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::{
    NitroEnclavePool, NitroEnclavePoolError, ProofSubmitterRequest, ProofSubmitterRequestError,
};

/// Claimed prover-service job data needed to generate and submit a Nitro proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofGeneratorRequest {
    /// Common worker claim metadata.
    pub claim: ClaimedProofJobMetadata,
    /// Server-issued claim lease expiry from the claim / latest heartbeat.
    pub lock_expires_at: Option<DateTime<Utc>>,
    /// Primitive Nitro proof request.
    pub proof: NitroProofRequest,
}

impl TryFrom<ProofJob> for ProofGeneratorRequest {
    type Error = ProofGeneratorError;

    fn try_from(job: ProofJob) -> Result<Self, Self::Error> {
        let claim = ClaimedProofJobMetadata::from_job(&job)?;
        let session_id = claim.session_id.clone();
        let lock_expires_at = job.lock_expires_at;

        let ProofRequestKind::Tee(tee) = job.request.request else {
            return Err(ProofGeneratorError::UnsupportedProofRequest { session_id });
        };
        let TeeKind::AwsNitro = tee.tee_kind;

        Ok(Self { claim, lock_expires_at, proof: tee.proof })
    }
}

/// Orchestrates Nitro witness generation, enclave proving, and async proof submission.
#[derive(Debug)]
pub struct ProofGenerator<Client> {
    pool: Arc<NitroEnclavePool>,
    submitter: ProofSubmitter<Client>,
    tasks: ProofTaskController,
    heartbeat: ProofGeneratorHeartbeatConfig,
}

impl<Client> ProofGenerator<Client> {
    /// Create a proof generator with its own submission cancellation token.
    pub fn new(
        pool: Arc<NitroEnclavePool>,
        submitter: ProofSubmitter<Client>,
        heartbeat: ProofGeneratorHeartbeatConfig,
    ) -> Self {
        Self { pool, submitter, tasks: ProofTaskController::new(), heartbeat }
    }

    /// Limits how many proof submission tasks may run at once.
    #[must_use]
    pub fn with_max_pending_submissions(mut self, max_pending: usize) -> Self {
        let tasks = self.tasks;
        self.tasks = tasks.with_max_pending_submissions(max_pending);
        self
    }
}

impl<Client> ProofGenerator<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Generate a proof for a claimed worker job and spawn proof submission.
    #[tracing::instrument(
        name = "nitro.generate_and_submit",
        skip_all,
        fields(session_id, worker_id, l2_block)
    )]
    pub async fn generate_and_submit(&self, job: ProofJob) -> Result<(), ProofGeneratorError> {
        let request = ProofGeneratorRequest::try_from(job)?;
        let l2_block = request.proof.claimed_l2_block_number;
        tracing::Span::current()
            .record("session_id", tracing::field::display(&request.claim.session_id))
            .record("worker_id", tracing::field::display(&request.claim.worker_id))
            .record("l2_block", l2_block);

        info!(
            session_id = %request.claim.session_id,
            lock_id = %request.claim.lock_id,
            worker_id = %request.claim.worker_id,
            l2_block,
            "starting nitro proof generation"
        );

        let (proof, permit) = self
            .with_heartbeat_while_generating(&request, async {
                let proof = self
                    .pool
                    .prove(request.proof.clone())
                    .instrument(info_span!(
                        "nitro.prove",
                        session_id = %request.claim.session_id,
                        l2_block,
                    ))
                    .await?;
                let permit = self.tasks.acquire_submission_permit().await;
                Ok((proof, permit))
            })
            .await
            .inspect_err(|error| match error {
                ProofGeneratorError::Generate { source, .. } => {
                    warn!(
                        session_id = %request.claim.session_id,
                        lock_id = %request.claim.lock_id,
                        worker_id = %request.claim.worker_id,
                        l2_block,
                        error = %source,
                        "nitro proof generation failed"
                    );
                }
                ProofGeneratorError::Heartbeat { source, .. } => {
                    warn!(
                        session_id = %request.claim.session_id,
                        lock_id = %request.claim.lock_id,
                        worker_id = %request.claim.worker_id,
                        l2_block,
                        error = %source,
                        "aborting nitro proof generation due to heartbeat failure"
                    );
                }
                _ => {}
            })?;

        let submit_request = ProofSubmitterRequest::from_tee_proof(
            request.claim.session_id.clone(),
            request.claim.lock_id.clone(),
            request.claim.worker_id.clone(),
            proof,
        )
        .map_err(|source| ProofGeneratorError::BuildSubmission {
            session_id: request.claim.session_id.clone(),
            source,
        })?;

        self.tasks.spawn_submission_with_permit(&self.submitter, submit_request, permit).await;

        info!(
            session_id = %request.claim.session_id,
            lock_id = %request.claim.lock_id,
            worker_id = %request.claim.worker_id,
            "nitro proof generated; proof submitter task spawned"
        );

        Ok(())
    }

    async fn with_heartbeat_while_generating<Output, Generate>(
        &self,
        request: &ProofGeneratorRequest,
        generate: Generate,
    ) -> Result<Output, ProofGeneratorError>
    where
        Generate: Future<Output = Result<Output, NitroEnclavePoolError>>,
    {
        let heartbeat_cancel = CancellationToken::new();
        let _heartbeat_cancel_guard = heartbeat_cancel.clone().drop_guard();
        // Heartbeats run on a dedicated blocking thread so a blocking `pool.prove`
        // future cannot starve lease renewals on the shared async runtime.
        let mut heartbeat_failure =
            self.spawn_heartbeat_until_failure(request.clone(), heartbeat_cancel.clone());
        tokio::pin!(generate);

        tokio::select! {
            biased;
            result = &mut heartbeat_failure => {
                let source = result
                    .unwrap_or_else(|error| {
                        warn!(error = %error, "proof heartbeat task stopped unexpectedly");
                        Some(Self::stopped_heartbeat_error())
                    })
                    .unwrap_or_else(Self::stopped_heartbeat_error);
                match generate.await {
                    Ok(_) => {
                        info!(
                            session_id = %request.claim.session_id,
                            lock_id = %request.claim.lock_id,
                            worker_id = %request.claim.worker_id,
                            l2_block = request.proof.claimed_l2_block_number,
                            "discarding nitro proof generated after heartbeat failure"
                        );
                    }
                    Err(error) => {
                        warn!(
                            session_id = %request.claim.session_id,
                            lock_id = %request.claim.lock_id,
                            worker_id = %request.claim.worker_id,
                            error = %error,
                            "nitro proof generation finished with error after heartbeat failure"
                        );
                    }
                }

                Err(ProofGeneratorError::Heartbeat {
                    session_id: request.claim.session_id.clone(),
                    source,
                })
            },
            result = &mut generate => {
                heartbeat_cancel.cancel();
                if let Some(source) = heartbeat_failure
                    .await
                    .unwrap_or_else(|error| {
                        warn!(error = %error, "proof heartbeat task stopped unexpectedly");
                        Some(Self::stopped_heartbeat_error())
                    })
                {
                    return Err(ProofGeneratorError::Heartbeat {
                        session_id: request.claim.session_id.clone(),
                        source,
                    });
                }

                result.map_err(|source| ProofGeneratorError::Generate {
                    session_id: request.claim.session_id.clone(),
                    source,
                })
            }
        }
    }

    fn spawn_heartbeat_until_failure(
        &self,
        request: ProofGeneratorRequest,
        cancel: CancellationToken,
    ) -> JoinHandle<Option<ProverServiceClientError>> {
        let submitter = self.submitter.clone();
        let heartbeat_config = self.heartbeat;

        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build proof heartbeat runtime");

            runtime.block_on(async {
                tokio::select! {
                    biased;
                    source = WorkerHeartbeat::until_failure(
                        &submitter,
                        &request.claim,
                        heartbeat_config,
                        request.lock_expires_at,
                    )
                    .instrument(span) => Some(source),
                    () = cancel.cancelled() => None,
                }
            })
        })
    }

    fn stopped_heartbeat_error() -> ProverServiceClientError {
        ProverServiceClientError::MissingResult(
            "proof generator heartbeat thread stopped unexpectedly".to_owned(),
        )
    }
}

#[async_trait]
impl<Client> ClaimedProofJobHandler for ProofGenerator<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    type Error = ProofGeneratorError;

    async fn ready_to_claim(&self, _worker_id: &str) -> bool {
        let Some(checker) = self.pool.registration_checker() else {
            return true;
        };

        match checker.select_all_valid_enclaves().await {
            Ok(valid) => {
                debug!(
                    valid_signer_count = valid.len(),
                    "registration gate passed for nitro job claim"
                );
                true
            }
            Err(error) => {
                warn!(
                    error = %error,
                    "registration gate not ready; skipping nitro job claim"
                );
                false
            }
        }
    }

    async fn handle_claimed_job(&self, job: ProofJob) -> Result<(), Self::Error> {
        Self::generate_and_submit(self, job).await
    }

    fn shutdown(&self) {
        self.tasks.cancel_submissions();
    }

    async fn join_shutdown(&self) {
        self.tasks.drain_submissions().await;
    }
}

/// Errors raised while generating and dispatching Nitro proof submissions.
#[derive(Debug, Error)]
pub enum ProofGeneratorError {
    /// Claim metadata was missing from the proof job.
    #[error(transparent)]
    Metadata(#[from] ClaimedProofJobMetadataError),
    /// Claimed proof job is not a TEE proof request.
    #[error("proof job {session_id} is not an AWS Nitro TEE proof request")]
    UnsupportedProofRequest {
        /// Proof session identifier.
        session_id: String,
    },
    /// Witness generation or enclave proving failed.
    #[error("proof generation failed for job {session_id}: {source}")]
    Generate {
        /// Proof session identifier.
        session_id: String,
        /// Underlying proof generation error.
        #[source]
        source: NitroEnclavePoolError,
    },
    /// Worker API heartbeat failed while the proof was being generated.
    #[error("heartbeat failed while generating proof for job {session_id}: {source}")]
    Heartbeat {
        /// Proof session identifier.
        session_id: String,
        /// Underlying worker API error.
        #[source]
        source: ProverServiceClientError,
    },
    /// The generated proof could not be converted into a worker submission request.
    #[error("failed to build proof submission for job {session_id}: {source}")]
    BuildSubmission {
        /// Proof session identifier.
        session_id: String,
        /// Underlying proof submission request error.
        #[source]
        source: ProofSubmitterRequestError,
    },
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Duration};

    use alloy_genesis::ChainConfig;
    use async_trait::async_trait;
    use base_common_genesis::RollupConfig;
    use base_proof_host::ProverConfig;
    use base_proof_tee_nitro_enclave::Server as EnclaveServer;
    use base_proof_worker::ProofSubmitter;
    use base_prover_service_client::ProverServiceClientError;
    use base_prover_service_protocol::{
        GetNextProofRequest, GetNextProofResponse, GetProofSessionRequest, GetProofSessionResponse,
        HeartbeatRequest, HeartbeatResponse, ProofJobStatus, ProofRequest,
        RecordProofSessionRequest, RecordProofSessionResponse, TeeKind, TeeProofRequest,
        WorkerSubmitProofRequest, WorkerSubmitProofResponse,
    };
    use chrono::Utc;
    use tokio::time::sleep;

    use super::*;
    use crate::{NitroTransport, RegistrationChecker, test_utils::MockRegistry};

    const TEST_SESSION_ID: &str = "session-1";
    const TEST_LOCK_ID: &str = "lock-1";
    const TEST_WORKER_ID: &str = "worker-1";
    const TEST_HEARTBEAT_LOCK_DURATION_SECONDS: u32 = 123;

    #[derive(Clone, Debug, Default)]
    struct MockWorkerClient {
        state: Arc<Mutex<MockWorkerState>>,
    }

    #[derive(Debug, Default)]
    struct MockWorkerState {
        heartbeats: Vec<HeartbeatRequest>,
        heartbeat_failure: Option<MockHeartbeatFailure>,
        submissions: Vec<WorkerSubmitProofRequest>,
    }

    #[derive(Debug, Clone, Copy)]
    enum MockHeartbeatFailure {
        NonRetryable,
    }

    impl MockWorkerClient {
        fn with_heartbeat_failure(failure: MockHeartbeatFailure) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockWorkerState {
                    heartbeat_failure: Some(failure),
                    ..Default::default()
                })),
            }
        }

        fn heartbeats(&self) -> Vec<HeartbeatRequest> {
            self.state.lock().expect("mock state lock should not be poisoned").heartbeats.clone()
        }

        fn submissions(&self) -> Vec<WorkerSubmitProofRequest> {
            self.state.lock().expect("mock state lock should not be poisoned").submissions.clone()
        }
    }

    #[async_trait]
    impl ProverWorkerProvider for MockWorkerClient {
        async fn get_next_proof(
            &self,
            _request: GetNextProofRequest,
        ) -> Result<GetNextProofResponse, ProverServiceClientError> {
            panic!("get_next_proof is not used by proof generator tests")
        }

        async fn heartbeat(
            &self,
            request: HeartbeatRequest,
        ) -> Result<HeartbeatResponse, ProverServiceClientError> {
            let failure = {
                let mut state = self.state.lock().expect("mock state lock should not be poisoned");
                state.heartbeats.push(request.clone());
                state.heartbeat_failure
            };

            match failure {
                Some(MockHeartbeatFailure::NonRetryable) => {
                    Err(ProverServiceClientError::WorkerLeaseRejected {
                        message: "mock lease rejected".to_owned(),
                    })
                }
                None => Ok(HeartbeatResponse {
                    job: proof_job(
                        request.session_id,
                        ProofJobStatus::Claimed,
                        Some(request.lock_id),
                        Some(request.worker_id),
                        PrimitiveRequestKind::Tee,
                    ),
                }),
            }
        }

        async fn submit_proof(
            &self,
            request: WorkerSubmitProofRequest,
        ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError> {
            self.state
                .lock()
                .expect("mock state lock should not be poisoned")
                .submissions
                .push(request.clone());

            Ok(WorkerSubmitProofResponse {
                job: proof_job(
                    request.session_id,
                    ProofJobStatus::Succeeded,
                    Some(request.lock_id),
                    Some(request.worker_id),
                    PrimitiveRequestKind::Tee,
                ),
            })
        }

        async fn get_proof_session(
            &self,
            _request: GetProofSessionRequest,
        ) -> Result<GetProofSessionResponse, ProverServiceClientError> {
            panic!("get_proof_session is not used by proof generator tests")
        }

        async fn record_proof_session(
            &self,
            _request: RecordProofSessionRequest,
        ) -> Result<RecordProofSessionResponse, ProverServiceClientError> {
            panic!("record_proof_session is not used by proof generator tests")
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum PrimitiveRequestKind {
        Tee,
        Compressed,
    }

    fn primitive_request(block: u64) -> NitroProofRequest {
        NitroProofRequest { claimed_l2_block_number: block, ..Default::default() }
    }

    fn test_prover_config() -> ProverConfig {
        ProverConfig {
            l1_eth_url: "http://127.0.0.1:1".to_string(),
            l2_eth_url: "http://127.0.0.1:1".to_string(),
            l2_node_url: "http://127.0.0.1:1".to_string(),
            l1_beacon_url: "http://127.0.0.1:1".to_string(),
            l2_chain_id: 0,
            rollup_config: RollupConfig::default(),
            l1_config: ChainConfig::default(),
            enable_experimental_witness_endpoint: false,
        }
    }

    fn test_pool() -> NitroEnclavePool {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let checker = Arc::new(
            RegistrationChecker::new(vec![Arc::clone(&transport)], MockRegistry::new(false))
                .unwrap(),
        );

        NitroEnclavePool::new(test_prover_config(), Arc::clone(&transport))
            .with_registration_checker(checker)
            .unwrap()
    }

    fn test_pool_with_registry(registry: MockRegistry) -> NitroEnclavePool {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let checker =
            Arc::new(RegistrationChecker::new(vec![Arc::clone(&transport)], registry).unwrap());

        NitroEnclavePool::new(test_prover_config(), Arc::clone(&transport))
            .with_registration_checker(checker)
            .unwrap()
    }

    fn proof_job(
        session_id: impl Into<String>,
        status: ProofJobStatus,
        lock_id: Option<String>,
        worker_id: Option<String>,
        kind: PrimitiveRequestKind,
    ) -> ProofJob {
        let session_id = session_id.into();
        let now = Utc::now();
        let request = match kind {
            PrimitiveRequestKind::Tee => ProofRequestKind::Tee(TeeProofRequest {
                proof: primitive_request(42),
                tee_kind: TeeKind::AwsNitro,
            }),
            PrimitiveRequestKind::Compressed => {
                ProofRequestKind::Compressed(base_prover_service_protocol::ZkProofRequest {
                    start_block_number: 1,
                    number_of_blocks_to_prove: 1,
                    sequence_window: None,
                    l1_head: None,
                    intermediate_root_interval: None,
                    schedule_l2_block_number: None,
                    zk_vm: base_prover_service_protocol::ZkVm::Sp1,
                    zk_backend: base_prover_service_protocol::ZkBackend::Cluster,
                })
            }
        };

        ProofJob {
            session_id: session_id.clone(),
            status,
            request: ProofRequest { session_id, request },
            attempt: 1,
            lock_id,
            worker_id,
            lock_expires_at: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            error_message: None,
        }
    }

    fn claimed_tee_job() -> ProofJob {
        proof_job(
            TEST_SESSION_ID,
            ProofJobStatus::Claimed,
            Some(TEST_LOCK_ID.to_owned()),
            Some(TEST_WORKER_ID.to_owned()),
            PrimitiveRequestKind::Tee,
        )
    }

    fn claimed_tee_request() -> ProofGeneratorRequest {
        ProofGeneratorRequest::try_from(claimed_tee_job())
            .expect("claimed tee job should build generator request")
    }

    fn generator_with_heartbeat(
        client: MockWorkerClient,
        heartbeat: ProofGeneratorHeartbeatConfig,
    ) -> ProofGenerator<MockWorkerClient> {
        ProofGenerator::new(Arc::new(test_pool()), ProofSubmitter::new(client), heartbeat)
    }

    fn generator_with_heartbeat_interval(
        client: MockWorkerClient,
        interval: Duration,
    ) -> ProofGenerator<MockWorkerClient> {
        generator_with_heartbeat(
            client,
            ProofGeneratorHeartbeatConfig::with_max_consecutive_failures(
                interval,
                TEST_HEARTBEAT_LOCK_DURATION_SECONDS,
                1,
            ),
        )
    }

    async fn wait_for_heartbeats(client: &MockWorkerClient, count: usize) {
        for _ in 0..50 {
            if client.heartbeats().len() >= count {
                return;
            }

            sleep(Duration::from_millis(1)).await;
        }

        panic!("expected at least {count} heartbeat(s)");
    }

    #[test]
    fn request_requires_claim_metadata() {
        let job = proof_job(
            TEST_SESSION_ID,
            ProofJobStatus::Claimed,
            None,
            Some(TEST_WORKER_ID.to_owned()),
            PrimitiveRequestKind::Tee,
        );

        let err = ProofGeneratorRequest::try_from(job).unwrap_err();

        assert!(matches!(
            err,
            ProofGeneratorError::Metadata(ClaimedProofJobMetadataError::MissingLockId { .. })
        ));
    }

    #[test]
    fn request_rejects_non_tee_jobs() {
        let job = proof_job(
            TEST_SESSION_ID,
            ProofJobStatus::Claimed,
            Some(TEST_LOCK_ID.to_owned()),
            Some(TEST_WORKER_ID.to_owned()),
            PrimitiveRequestKind::Compressed,
        );

        let err = ProofGeneratorRequest::try_from(job).unwrap_err();

        assert!(matches!(err, ProofGeneratorError::UnsupportedProofRequest { .. }));
    }

    #[tokio::test]
    async fn ready_to_claim_is_false_when_registration_has_no_valid_signer() {
        let client = MockWorkerClient::default();
        let generator = ProofGenerator::new(
            Arc::new(test_pool_with_registry(MockRegistry::new(false))),
            ProofSubmitter::new(client),
            ProofGeneratorHeartbeatConfig::default(),
        );

        assert!(!generator.ready_to_claim(TEST_WORKER_ID).await);
    }

    #[tokio::test]
    async fn heartbeat_failure_wins_after_generation_poll_blocks_runtime_thread() {
        let client = MockWorkerClient::with_heartbeat_failure(MockHeartbeatFailure::NonRetryable);
        let generator = generator_with_heartbeat_interval(client.clone(), Duration::from_millis(5));
        let request = claimed_tee_request();

        let err = generator
            .with_heartbeat_while_generating(&request, async {
                std::thread::sleep(Duration::from_millis(50));
                Ok::<(), NitroEnclavePoolError>(())
            })
            .await
            .unwrap_err();

        assert!(matches!(err, ProofGeneratorError::Heartbeat { .. }));
        assert!(
            !client.heartbeats().is_empty(),
            "heartbeat task should run independently of the busy generation task"
        );
    }

    #[tokio::test]
    async fn heartbeat_stops_when_generation_future_is_aborted() {
        let client = MockWorkerClient::default();
        let generator = generator_with_heartbeat_interval(client.clone(), Duration::from_millis(5));
        let request = claimed_tee_request();

        let handle = tokio::spawn(async move {
            generator
                .with_heartbeat_while_generating(
                    &request,
                    std::future::pending::<Result<(), NitroEnclavePoolError>>(),
                )
                .await
        });

        wait_for_heartbeats(&client, 1).await;
        handle.abort();
        assert!(handle.await.expect_err("generation task should be aborted").is_cancelled());

        let heartbeat_count = client.heartbeats().len();
        sleep(Duration::from_millis(25)).await;
        assert_eq!(client.heartbeats().len(), heartbeat_count);
    }

    #[tokio::test]
    async fn short_generation_failure_does_not_heartbeat() {
        let client = MockWorkerClient::default();
        let generator =
            generator_with_heartbeat_interval(client.clone(), Duration::from_millis(50));
        let request = claimed_tee_request();

        let err = generator
            .with_heartbeat_while_generating(&request, async {
                tokio::task::yield_now().await;
                Err::<(), NitroEnclavePoolError>(NitroEnclavePoolError::Busy)
            })
            .await
            .unwrap_err();

        assert!(matches!(err, ProofGeneratorError::Generate { .. }));
        sleep(Duration::from_millis(75)).await;
        assert!(client.heartbeats().is_empty());
    }

    #[tokio::test]
    async fn heartbeat_failure_waits_for_in_flight_generation() {
        let client = MockWorkerClient::with_heartbeat_failure(MockHeartbeatFailure::NonRetryable);
        let generator = generator_with_heartbeat_interval(client, Duration::from_millis(5));
        let request = claimed_tee_request();
        let generation_finished = Arc::new(Mutex::new(false));
        let generation_finished_for_task = Arc::clone(&generation_finished);

        let err = generator
            .with_heartbeat_while_generating(&request, async move {
                sleep(Duration::from_millis(25)).await;
                *generation_finished_for_task
                    .lock()
                    .expect("generation completion flag should not be poisoned") = true;
                Ok::<(), NitroEnclavePoolError>(())
            })
            .await
            .unwrap_err();

        assert!(matches!(err, ProofGeneratorError::Heartbeat { .. }));
        assert!(
            *generation_finished.lock().expect("generation completion flag should not be poisoned"),
            "heartbeat failure must not return until in-flight generation finishes"
        );
    }

    #[tokio::test]
    async fn generate_failure_does_not_spawn_submitter() {
        let client = MockWorkerClient::default();
        let generator =
            generator_with_heartbeat(client.clone(), ProofGeneratorHeartbeatConfig::default());

        let err = generator.generate_and_submit(claimed_tee_job()).await.unwrap_err();

        assert!(matches!(err, ProofGeneratorError::Generate { .. }));
        assert!(client.submissions().is_empty());
    }
}
