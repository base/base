//! Proof generation orchestration for claimed Nitro worker jobs.

use std::{future::Future, sync::Arc, time::Duration};

use base_proof_primitives::ProofRequest as NitroProofRequest;
use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    HeartbeatRequest, ProofJob, ProofRequestKind, TeeKind, WorkerSubmitProofResponse,
};
use thiserror::Error;
use tokio::{task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    NitroEnclavePool, NitroEnclavePoolError, ProofSubmitter, ProofSubmitterError,
    ProofSubmitterRequest,
};

/// Minimum proof-generation heartbeat interval.
pub const MIN_PROOF_GENERATOR_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(1);

/// Default interval between worker API heartbeats while an enclave proof is being generated.
pub const DEFAULT_PROOF_GENERATOR_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Default lock duration requested by proof-generation heartbeats.
///
/// A value of zero asks the prover service to use its server-side default.
pub const DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS: u32 = 0;

/// Heartbeat settings used while the enclave pool is generating a proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofGeneratorHeartbeatConfig {
    /// Delay between heartbeat attempts.
    pub interval: Duration,
    /// Requested lock duration in seconds. Zero uses the server default.
    pub lock_duration_seconds: u32,
}

impl ProofGeneratorHeartbeatConfig {
    /// Creates a proof-generation heartbeat config.
    pub const fn new(interval: Duration, lock_duration_seconds: u32) -> Self {
        Self { interval, lock_duration_seconds }
    }

    /// Returns the configured interval clamped to the minimum allowed delay.
    pub fn normalized_interval(&self) -> Duration {
        self.interval.max(MIN_PROOF_GENERATOR_HEARTBEAT_INTERVAL)
    }
}

impl Default for ProofGeneratorHeartbeatConfig {
    fn default() -> Self {
        Self::new(
            DEFAULT_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
            DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
        )
    }
}

/// Claimed prover-service job data needed to generate and submit a Nitro proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofGeneratorRequest {
    /// Proof session identifier.
    pub session_id: String,
    /// Server-issued lock identifier for this worker claim.
    pub lock_id: String,
    /// Worker identifier that owns the claim.
    pub worker_id: String,
    /// Primitive Nitro proof request.
    pub proof: NitroProofRequest,
}

impl TryFrom<ProofJob> for ProofGeneratorRequest {
    type Error = ProofGeneratorError;

    fn try_from(job: ProofJob) -> Result<Self, Self::Error> {
        let session_id = job.session_id;
        let lock_id = job
            .lock_id
            .ok_or_else(|| ProofGeneratorError::MissingLockId { session_id: session_id.clone() })?;
        let worker_id = job.worker_id.ok_or_else(|| ProofGeneratorError::MissingWorkerId {
            session_id: session_id.clone(),
        })?;

        let ProofRequestKind::Tee(tee) = job.request.request else {
            return Err(ProofGeneratorError::UnsupportedProofRequest { session_id });
        };
        let TeeKind::AwsNitro = tee.tee_kind;

        Ok(Self { session_id, lock_id, worker_id, proof: tee.proof })
    }
}

/// Handle for a proof submission task spawned after successful proof generation.
#[derive(Debug)]
pub struct ProofGeneratorTask {
    /// Proof session identifier.
    pub session_id: String,
    /// Server-issued lock identifier for this worker claim.
    pub lock_id: String,
    /// Worker identifier that owns the claim.
    pub worker_id: String,
    /// Spawned proof submission task.
    pub submit_handle: JoinHandle<Result<WorkerSubmitProofResponse, ProofSubmitterError>>,
}

/// Orchestrates Nitro witness generation, enclave proving, and async proof submission.
#[derive(Debug)]
pub struct ProofGenerator<Client> {
    pool: Arc<NitroEnclavePool>,
    submitter: ProofSubmitter<Client>,
    submission_cancel: CancellationToken,
    heartbeat: ProofGeneratorHeartbeatConfig,
}

impl<Client> ProofGenerator<Client> {
    /// Create a proof generator with its own submission cancellation token.
    pub fn new(
        pool: Arc<NitroEnclavePool>,
        submitter: ProofSubmitter<Client>,
        heartbeat: ProofGeneratorHeartbeatConfig,
    ) -> Self {
        Self { pool, submitter, submission_cancel: CancellationToken::new(), heartbeat }
    }

    /// Use a caller-provided cancellation token for spawned submission tasks.
    pub fn with_submission_cancel(mut self, submission_cancel: CancellationToken) -> Self {
        self.submission_cancel = submission_cancel;
        self
    }

    /// Returns the Nitro enclave pool.
    pub fn pool(&self) -> Arc<NitroEnclavePool> {
        Arc::clone(&self.pool)
    }

    /// Returns the proof submitter.
    pub const fn submitter(&self) -> &ProofSubmitter<Client> {
        &self.submitter
    }

    /// Returns the cancellation token used for spawned submission tasks.
    pub fn submission_cancel(&self) -> CancellationToken {
        self.submission_cancel.clone()
    }

    /// Returns the heartbeat settings used while proofs are generated.
    pub const fn heartbeat_config(&self) -> ProofGeneratorHeartbeatConfig {
        self.heartbeat
    }
}

impl<Client> ProofGenerator<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Generate a proof for a claimed worker job and spawn proof submission.
    pub async fn generate_and_submit(
        &self,
        job: ProofJob,
    ) -> Result<ProofGeneratorTask, ProofGeneratorError> {
        let request = ProofGeneratorRequest::try_from(job)?;

        info!(
            session_id = %request.session_id,
            lock_id = %request.lock_id,
            worker_id = %request.worker_id,
            l2_block = request.proof.claimed_l2_block_number,
            "starting nitro proof generation"
        );

        let l2_block = request.proof.claimed_l2_block_number;
        let proof = match self
            .with_heartbeat_while_generating(&request, self.pool.prove(request.proof.clone()))
            .await
        {
            Ok(proof) => proof,
            Err(ProofGeneratorError::Generate { session_id, source }) => {
                warn!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    l2_block,
                    error = %source,
                    "nitro proof generation failed"
                );

                return Err(ProofGeneratorError::Generate { session_id, source });
            }
            Err(ProofGeneratorError::Heartbeat { session_id, source }) => {
                warn!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    l2_block,
                    error = %source,
                    "aborting nitro proof generation due to heartbeat failure"
                );

                return Err(ProofGeneratorError::Heartbeat { session_id, source });
            }
            Err(
                source @ (ProofGeneratorError::MissingLockId { .. }
                | ProofGeneratorError::MissingWorkerId { .. }
                | ProofGeneratorError::UnsupportedProofRequest { .. }
                | ProofGeneratorError::BuildSubmission { .. }),
            ) => {
                unreachable!(
                    "with_heartbeat_while_generating returned an impossible error: {source}"
                );
            }
        };

        let submit_request = ProofSubmitterRequest::from_tee_proof(
            request.session_id.clone(),
            request.lock_id.clone(),
            request.worker_id.clone(),
            proof,
        )
        .map_err(|source| ProofGeneratorError::BuildSubmission {
            session_id: request.session_id.clone(),
            source,
        })?;

        let submit_handle =
            self.submitter.spawn_submit(submit_request, self.submission_cancel.clone());

        info!(
            session_id = %request.session_id,
            lock_id = %request.lock_id,
            worker_id = %request.worker_id,
            "nitro proof generated; proof submitter task spawned"
        );

        Ok(ProofGeneratorTask {
            session_id: request.session_id,
            lock_id: request.lock_id,
            worker_id: request.worker_id,
            submit_handle,
        })
    }

    async fn with_heartbeat_while_generating<Output, Generate>(
        &self,
        request: &ProofGeneratorRequest,
        generate: Generate,
    ) -> Result<Output, ProofGeneratorError>
    where
        Generate: Future<Output = Result<Output, NitroEnclavePoolError>>,
    {
        let heartbeat = self.heartbeat_until_failure(request);
        tokio::pin!(generate);
        tokio::pin!(heartbeat);

        tokio::select! {
            biased;
            result = &mut generate => result.map_err(|source| ProofGeneratorError::Generate {
                session_id: request.session_id.clone(),
                source,
            }),
            source = &mut heartbeat => {
                match generate.await {
                    Ok(_) => {
                        info!(
                            session_id = %request.session_id,
                            lock_id = %request.lock_id,
                            worker_id = %request.worker_id,
                            l2_block = request.proof.claimed_l2_block_number,
                            "discarding nitro proof generated after heartbeat failure"
                        );
                    }
                    Err(error) => {
                        warn!(
                            session_id = %request.session_id,
                            lock_id = %request.lock_id,
                            worker_id = %request.worker_id,
                            error = %error,
                            "nitro proof generation finished with error after heartbeat failure"
                        );
                    }
                }

                Err(ProofGeneratorError::Heartbeat {
                    session_id: request.session_id.clone(),
                    source,
                })
            },
        }
    }

    async fn heartbeat_until_failure(
        &self,
        request: &ProofGeneratorRequest,
    ) -> ProverServiceClientError {
        loop {
            sleep(self.heartbeat.normalized_interval()).await;

            let heartbeat = HeartbeatRequest {
                session_id: request.session_id.clone(),
                lock_id: request.lock_id.clone(),
                worker_id: request.worker_id.clone(),
                lock_duration_seconds: self.heartbeat.lock_duration_seconds,
            };

            match self.submitter.heartbeat(heartbeat).await {
                Ok(response) => {
                    debug!(
                        session_id = %request.session_id,
                        lock_id = %request.lock_id,
                        worker_id = %request.worker_id,
                        lock_expires_at = ?response.job.lock_expires_at,
                        "proof job heartbeat accepted"
                    );
                }
                Err(error) => {
                    warn!(
                        session_id = %request.session_id,
                        lock_id = %request.lock_id,
                        worker_id = %request.worker_id,
                        error = %error,
                        "proof job heartbeat failed"
                    );
                    return error;
                }
            }
        }
    }
}

/// Errors raised while generating and dispatching Nitro proof submissions.
#[derive(Debug, Error)]
pub enum ProofGeneratorError {
    /// Claimed proof job did not include a lock identifier.
    #[error("proof job {session_id} is missing lock_id")]
    MissingLockId {
        /// Proof session identifier.
        session_id: String,
    },
    /// Claimed proof job did not include a worker identifier.
    #[error("proof job {session_id} is missing worker_id")]
    MissingWorkerId {
        /// Proof session identifier.
        session_id: String,
    },
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
        source: ProofSubmitterError,
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
    use base_prover_service_client::ProverServiceClientError;
    use base_prover_service_protocol::{
        GetNextProofRequest, GetNextProofResponse, HeartbeatRequest, HeartbeatResponse,
        ProofJobStatus, ProofRequest, TeeKind, TeeProofRequest, WorkerSubmitProofRequest,
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
        Retryable,
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
                Some(MockHeartbeatFailure::Retryable) => Err(ProverServiceClientError::Timeout(
                    "mock retryable heartbeat failure".to_owned(),
                )),
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
                    zk_vm: base_prover_service_protocol::ZkVm::Sp1,
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
            ProofGeneratorHeartbeatConfig::new(interval, TEST_HEARTBEAT_LOCK_DURATION_SECONDS),
        )
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

        assert!(matches!(err, ProofGeneratorError::MissingLockId { .. }));
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
    async fn heartbeat_runs_while_generation_is_in_progress() {
        let client = MockWorkerClient::default();
        let generator = generator_with_heartbeat_interval(client.clone(), Duration::from_millis(5));
        let request = claimed_tee_request();

        let err = generator
            .with_heartbeat_while_generating(&request, async {
                sleep(Duration::from_millis(20)).await;
                Err::<(), NitroEnclavePoolError>(NitroEnclavePoolError::Busy)
            })
            .await
            .unwrap_err();

        assert!(matches!(err, ProofGeneratorError::Generate { .. }));

        let heartbeats = client.heartbeats();
        assert!(heartbeats.len() >= 2);
        assert!(heartbeats.iter().all(|heartbeat| {
            heartbeat.session_id == TEST_SESSION_ID
                && heartbeat.lock_id == TEST_LOCK_ID
                && heartbeat.worker_id == TEST_WORKER_ID
                && heartbeat.lock_duration_seconds == TEST_HEARTBEAT_LOCK_DURATION_SECONDS
        }));
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
        assert!(client.heartbeats().is_empty());
    }

    #[tokio::test]
    async fn retryable_heartbeat_failure_aborts_generation() {
        let client = MockWorkerClient::with_heartbeat_failure(MockHeartbeatFailure::Retryable);
        let generator = generator_with_heartbeat_interval(client.clone(), Duration::from_millis(5));
        let request = claimed_tee_request();

        let err = generator
            .with_heartbeat_while_generating(&request, async {
                sleep(Duration::from_millis(20)).await;
                Err::<(), NitroEnclavePoolError>(NitroEnclavePoolError::Busy)
            })
            .await
            .unwrap_err();

        assert!(matches!(err, ProofGeneratorError::Heartbeat { .. }));
        assert_eq!(client.heartbeats().len(), 1);
    }

    #[tokio::test]
    async fn non_retryable_heartbeat_failure_aborts_generation() {
        let client = MockWorkerClient::with_heartbeat_failure(MockHeartbeatFailure::NonRetryable);
        let generator = generator_with_heartbeat_interval(client.clone(), Duration::from_millis(5));
        let request = claimed_tee_request();

        let err = generator
            .with_heartbeat_while_generating(&request, async {
                sleep(Duration::from_millis(25)).await;
                Err::<(), NitroEnclavePoolError>(NitroEnclavePoolError::Busy)
            })
            .await
            .unwrap_err();

        assert!(matches!(err, ProofGeneratorError::Heartbeat { .. }));
        assert_eq!(client.heartbeats().len(), 1);
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
