//! Client for prover worker JSON-RPC methods.

use async_trait::async_trait;
use base_prover_service_protocol::{
    ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest, CompleteProofJobResponse,
    FailProofJobRequest, FailProofJobResponse, GetProofJobRequest, GetProofJobResponse,
    HeartbeatProofJobRequest, HeartbeatProofJobResponse, ProverWorkerApiClient,
};
use jsonrpsee::http_client::HttpClient;
use tracing::debug;

use crate::{ProverServiceClientConfig, ProverServiceClientError};

/// Abstraction over prover worker JSON-RPC methods.
///
/// The canonical implementation is [`ProverWorkerClient`]. The trait lets
/// worker services depend on a mockable interface without exposing requester
/// helpers such as submit-and-wait flows.
#[async_trait]
pub trait ProverWorkerProvider: Send + Sync {
    /// Return a worker-owned proof job by session id.
    async fn get_proof_job(
        &self,
        request: GetProofJobRequest,
    ) -> Result<GetProofJobResponse, ProverServiceClientError>;

    /// Claim the next eligible queued proof job.
    async fn claim_proof_job(
        &self,
        request: ClaimProofJobRequest,
    ) -> Result<ClaimProofJobResponse, ProverServiceClientError>;

    /// Extend a proof job lease.
    async fn heartbeat_proof_job(
        &self,
        request: HeartbeatProofJobRequest,
    ) -> Result<HeartbeatProofJobResponse, ProverServiceClientError>;

    /// Complete a leased proof job.
    async fn complete_proof_job(
        &self,
        request: CompleteProofJobRequest,
    ) -> Result<CompleteProofJobResponse, ProverServiceClientError>;

    /// Fail a leased proof job.
    async fn fail_proof_job(
        &self,
        request: FailProofJobRequest,
    ) -> Result<FailProofJobResponse, ProverServiceClientError>;
}

/// JSON-RPC client for prover worker methods.
#[derive(Clone, Debug)]
pub struct ProverWorkerClient {
    inner: HttpClient,
}

impl ProverWorkerClient {
    /// Create a worker client from an existing JSON-RPC HTTP client.
    pub const fn new(inner: HttpClient) -> Self {
        Self { inner }
    }

    /// Connect a worker client using the provided configuration.
    pub fn connect(config: &ProverServiceClientConfig) -> Result<Self, ProverServiceClientError> {
        Ok(Self::new(config.build_http_client()?))
    }

    /// Return the underlying JSON-RPC HTTP client.
    pub const fn inner(&self) -> &HttpClient {
        &self.inner
    }

    /// Return a worker-owned proof job by session id.
    pub async fn get_proof_job(
        &self,
        request: GetProofJobRequest,
    ) -> Result<GetProofJobResponse, ProverServiceClientError> {
        debug!(session_id = %request.session_id, "fetching proof job");
        Ok(self.inner.get_proof_job(request).await?)
    }

    /// Claim the next eligible queued proof job.
    pub async fn claim_proof_job(
        &self,
        request: ClaimProofJobRequest,
    ) -> Result<ClaimProofJobResponse, ProverServiceClientError> {
        debug!(
            worker_id = %request.worker_id,
            proof_type = ?request.proof_type,
            lease_duration_seconds = request.lease_duration_seconds,
            tee_kinds = ?request.tee_kinds,
            zk_vms = ?request.zk_vms,
            "claiming proof job"
        );
        Ok(self.inner.claim_proof_job(request).await?)
    }

    /// Extend a proof job lease.
    pub async fn heartbeat_proof_job(
        &self,
        request: HeartbeatProofJobRequest,
    ) -> Result<HeartbeatProofJobResponse, ProverServiceClientError> {
        debug!(
            session_id = %request.session_id,
            worker_id = %request.worker_id,
            lease_id = %request.lease_id,
            lease_duration_seconds = request.lease_duration_seconds,
            "heartbeating proof job"
        );
        Ok(self.inner.heartbeat_proof_job(request).await?)
    }

    /// Complete a leased proof job.
    pub async fn complete_proof_job(
        &self,
        request: CompleteProofJobRequest,
    ) -> Result<CompleteProofJobResponse, ProverServiceClientError> {
        debug!(
            session_id = %request.session_id,
            worker_id = %request.worker_id,
            lease_id = %request.lease_id,
            "completing proof job"
        );
        Ok(self.inner.complete_proof_job(request).await?)
    }

    /// Fail a leased proof job.
    pub async fn fail_proof_job(
        &self,
        request: FailProofJobRequest,
    ) -> Result<FailProofJobResponse, ProverServiceClientError> {
        debug!(
            session_id = %request.session_id,
            worker_id = %request.worker_id,
            lease_id = %request.lease_id,
            retryable = request.retryable,
            "failing proof job"
        );
        Ok(self.inner.fail_proof_job(request).await?)
    }
}

#[async_trait]
impl ProverWorkerProvider for ProverWorkerClient {
    async fn get_proof_job(
        &self,
        request: GetProofJobRequest,
    ) -> Result<GetProofJobResponse, ProverServiceClientError> {
        Self::get_proof_job(self, request).await
    }

    async fn claim_proof_job(
        &self,
        request: ClaimProofJobRequest,
    ) -> Result<ClaimProofJobResponse, ProverServiceClientError> {
        Self::claim_proof_job(self, request).await
    }

    async fn heartbeat_proof_job(
        &self,
        request: HeartbeatProofJobRequest,
    ) -> Result<HeartbeatProofJobResponse, ProverServiceClientError> {
        Self::heartbeat_proof_job(self, request).await
    }

    async fn complete_proof_job(
        &self,
        request: CompleteProofJobRequest,
    ) -> Result<CompleteProofJobResponse, ProverServiceClientError> {
        Self::complete_proof_job(self, request).await
    }

    async fn fail_proof_job(
        &self,
        request: FailProofJobRequest,
    ) -> Result<FailProofJobResponse, ProverServiceClientError> {
        Self::fail_proof_job(self, request).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use base_prover_service_protocol::{
        ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest,
        CompleteProofJobResponse, FailProofJobRequest, FailProofJobResponse, GetProofJobRequest,
        GetProofJobResponse, HeartbeatProofJobRequest, HeartbeatProofJobResponse, ProofJob,
        ProofJobStatus, ProofRequest, ProofRequestKind, ProofResult, ProofType,
        ProverWorkerApiServer, ZkProofRequest, ZkProofResult, ZkVm,
    };
    use chrono::Utc;
    use jsonrpsee::{
        core::{RpcResult, client::Error as JsonRpcClientError},
        http_client::HttpClientBuilder,
        server::{Server, ServerHandle},
        types::ErrorObjectOwned,
    };

    use super::{ProverWorkerClient, ProverWorkerProvider};
    use crate::ProverServiceClientError;

    #[derive(Clone, Debug)]
    struct MockWorkerApi {
        state: Arc<Mutex<MockWorkerState>>,
        reject_heartbeat: bool,
    }

    #[derive(Debug, Default)]
    struct MockWorkerState {
        get_request: Option<GetProofJobRequest>,
        claim_request: Option<ClaimProofJobRequest>,
        heartbeat_request: Option<HeartbeatProofJobRequest>,
        complete_request: Option<CompleteProofJobRequest>,
        fail_request: Option<FailProofJobRequest>,
    }

    #[derive(Debug)]
    struct RunningWorkerServer {
        client: ProverWorkerClient,
        handle: ServerHandle,
    }

    impl MockWorkerApi {
        fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockWorkerState::default())),
                reject_heartbeat: false,
            }
        }

        fn rejecting_heartbeat() -> Self {
            Self { state: Arc::new(Mutex::new(MockWorkerState::default())), reject_heartbeat: true }
        }
    }

    impl RunningWorkerServer {
        async fn spawn(api: MockWorkerApi) -> Self {
            let addr: SocketAddr = "127.0.0.1:0".parse().expect("test address should parse");
            let server = Server::builder().build(addr).await.expect("server should bind");
            let local_addr = server.local_addr().expect("server should have local address");
            let handle = server.start(api.into_rpc());
            let endpoint = format!("http://{local_addr}");
            let inner = HttpClientBuilder::default().build(endpoint).expect("client should build");

            Self { client: ProverWorkerClient::new(inner), handle }
        }

        async fn shutdown(self) {
            self.handle.stop().expect("server should stop");
            self.handle.stopped().await;
        }
    }

    #[async_trait]
    impl ProverWorkerApiServer for MockWorkerApi {
        async fn get_proof_job(
            &self,
            request: GetProofJobRequest,
        ) -> RpcResult<GetProofJobResponse> {
            self.state.lock().expect("state lock should not be poisoned").get_request =
                Some(request.clone());

            Ok(GetProofJobResponse {
                job: Some(proof_job(
                    request.session_id,
                    ProofJobStatus::Leased,
                    Some("lease-get".to_owned()),
                    Some("worker-get".to_owned()),
                    None,
                )),
            })
        }

        async fn claim_proof_job(
            &self,
            request: ClaimProofJobRequest,
        ) -> RpcResult<ClaimProofJobResponse> {
            self.state.lock().expect("state lock should not be poisoned").claim_request =
                Some(request.clone());

            Ok(ClaimProofJobResponse {
                claimed: true,
                job: Some(proof_job(
                    "session-claim",
                    ProofJobStatus::Leased,
                    Some("lease-claim".to_owned()),
                    Some(request.worker_id),
                    None,
                )),
            })
        }

        async fn heartbeat_proof_job(
            &self,
            request: HeartbeatProofJobRequest,
        ) -> RpcResult<HeartbeatProofJobResponse> {
            self.state.lock().expect("state lock should not be poisoned").heartbeat_request =
                Some(request.clone());

            if self.reject_heartbeat {
                return Err(ErrorObjectOwned::owned(
                    ProverServiceClientError::ERROR_FAILED_PRECONDITION,
                    format!(
                        "lease {} is not owned by worker {}",
                        request.lease_id, request.worker_id
                    ),
                    None::<()>,
                ));
            }

            Ok(HeartbeatProofJobResponse {
                accepted: true,
                job: Some(proof_job(
                    request.session_id,
                    ProofJobStatus::Leased,
                    Some(request.lease_id),
                    Some(request.worker_id),
                    None,
                )),
            })
        }

        async fn complete_proof_job(
            &self,
            request: CompleteProofJobRequest,
        ) -> RpcResult<CompleteProofJobResponse> {
            self.state.lock().expect("state lock should not be poisoned").complete_request =
                Some(request.clone());

            Ok(CompleteProofJobResponse {
                job: proof_job(
                    request.session_id,
                    ProofJobStatus::Succeeded,
                    Some(request.lease_id),
                    Some(request.worker_id),
                    None,
                ),
            })
        }

        async fn fail_proof_job(
            &self,
            request: FailProofJobRequest,
        ) -> RpcResult<FailProofJobResponse> {
            self.state.lock().expect("state lock should not be poisoned").fail_request =
                Some(request.clone());

            Ok(FailProofJobResponse {
                job: proof_job(
                    request.session_id,
                    ProofJobStatus::Failed,
                    Some(request.lease_id),
                    Some(request.worker_id),
                    Some(request.error_message),
                ),
                will_retry: request.retryable,
            })
        }
    }

    fn proof_job(
        session_id: impl Into<String>,
        status: ProofJobStatus,
        lease_id: Option<String>,
        worker_id: Option<String>,
        error_message: Option<String>,
    ) -> ProofJob {
        ProofJob {
            session_id: session_id.into(),
            status,
            request: proof_request(),
            attempt: 2,
            lease_id,
            worker_id,
            lease_expires_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: matches!(status, ProofJobStatus::Succeeded | ProofJobStatus::Failed)
                .then(Utc::now),
            error_message,
        }
    }

    fn proof_request() -> ProofRequest {
        ProofRequest {
            session_id: Some("session-request".to_owned()),
            request: ProofRequestKind::Compressed(ZkProofRequest {
                start_block_number: 10,
                number_of_blocks_to_prove: 2,
                sequence_window: None,
                l1_head: None,
                intermediate_root_interval: None,
                zk_vm: ZkVm::Sp1,
            }),
        }
    }

    fn proof_result() -> ProofResult {
        ProofResult::Compressed(ZkProofResult { zk_vm: ZkVm::Sp1, proof: vec![1, 2, 3].into() })
    }

    #[tokio::test]
    async fn worker_methods_round_trip_requests_and_responses() {
        let api = MockWorkerApi::new();
        let server = RunningWorkerServer::spawn(api.clone()).await;

        let get_request = GetProofJobRequest { session_id: "session-get".to_owned() };
        let provider: &dyn ProverWorkerProvider = &server.client;
        let get_response = provider
            .get_proof_job(get_request.clone())
            .await
            .expect("get_proof_job should succeed");
        let get_job = get_response.job.expect("get response should include a job");
        assert_eq!(get_job.session_id, "session-get");
        assert_eq!(get_job.lease_id.as_deref(), Some("lease-get"));
        assert_eq!(get_job.worker_id.as_deref(), Some("worker-get"));

        let claim_request = ClaimProofJobRequest {
            proof_type: ProofType::Compressed,
            worker_id: "worker-claim".to_owned(),
            lease_duration_seconds: 60,
            tee_kinds: Vec::new(),
            zk_vms: vec![ZkVm::Sp1],
        };
        let claim_response = provider
            .claim_proof_job(claim_request.clone())
            .await
            .expect("claim_proof_job should succeed");
        let claim_job = claim_response.job.expect("claim response should include a job");
        assert!(claim_response.claimed);
        assert_eq!(claim_job.session_id, "session-claim");
        assert_eq!(claim_job.status, ProofJobStatus::Leased);
        assert_eq!(claim_job.lease_id.as_deref(), Some("lease-claim"));
        assert_eq!(claim_job.worker_id.as_deref(), Some("worker-claim"));

        let heartbeat_request = HeartbeatProofJobRequest {
            session_id: "session-heartbeat".to_owned(),
            lease_id: "lease-heartbeat".to_owned(),
            worker_id: "worker-heartbeat".to_owned(),
            lease_duration_seconds: 30,
        };
        let heartbeat_response = provider
            .heartbeat_proof_job(heartbeat_request.clone())
            .await
            .expect("heartbeat_proof_job should succeed");
        let heartbeat_job =
            heartbeat_response.job.expect("heartbeat response should include a job");
        assert!(heartbeat_response.accepted);
        assert_eq!(heartbeat_job.session_id, "session-heartbeat");
        assert_eq!(heartbeat_job.lease_id.as_deref(), Some("lease-heartbeat"));
        assert_eq!(heartbeat_job.worker_id.as_deref(), Some("worker-heartbeat"));

        let complete_request = CompleteProofJobRequest {
            session_id: "session-complete".to_owned(),
            lease_id: "lease-complete".to_owned(),
            worker_id: "worker-complete".to_owned(),
            result: proof_result(),
        };
        let complete_response = provider
            .complete_proof_job(complete_request.clone())
            .await
            .expect("complete_proof_job should succeed");
        assert_eq!(complete_response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(complete_response.job.lease_id.as_deref(), Some("lease-complete"));
        assert_eq!(complete_response.job.worker_id.as_deref(), Some("worker-complete"));

        let fail_request = FailProofJobRequest {
            session_id: "session-fail".to_owned(),
            lease_id: "lease-fail".to_owned(),
            worker_id: "worker-fail".to_owned(),
            error_message: "proof backend exited".to_owned(),
            retryable: true,
        };
        let fail_response = provider
            .fail_proof_job(fail_request.clone())
            .await
            .expect("fail_proof_job should succeed");
        assert!(fail_response.will_retry);
        assert_eq!(fail_response.job.status, ProofJobStatus::Failed);
        assert_eq!(fail_response.job.lease_id.as_deref(), Some("lease-fail"));
        assert_eq!(fail_response.job.worker_id.as_deref(), Some("worker-fail"));
        assert_eq!(fail_response.job.error_message.as_deref(), Some("proof backend exited"));

        {
            let state = api.state.lock().expect("state lock should not be poisoned");
            assert_eq!(state.get_request, Some(get_request));
            assert_eq!(state.claim_request, Some(claim_request));
            assert_eq!(state.heartbeat_request, Some(heartbeat_request));
            assert_eq!(state.complete_request, Some(complete_request));
            assert_eq!(state.fail_request, Some(fail_request));
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn worker_rpc_errors_preserve_rejection_context_and_retryability() {
        let api = MockWorkerApi::rejecting_heartbeat();
        let server = RunningWorkerServer::spawn(api).await;
        let provider: &dyn ProverWorkerProvider = &server.client;

        let err = provider
            .heartbeat_proof_job(HeartbeatProofJobRequest {
                session_id: "session-error".to_owned(),
                lease_id: "lease-error".to_owned(),
                worker_id: "worker-error".to_owned(),
                lease_duration_seconds: 30,
            })
            .await
            .expect_err("heartbeat should be rejected");

        assert!(!err.is_retryable());

        match err {
            ProverServiceClientError::RpcTransport(JsonRpcClientError::Call(call)) => {
                assert_eq!(call.code(), ProverServiceClientError::ERROR_FAILED_PRECONDITION);
                assert!(call.message().contains("lease-error"));
                assert!(call.message().contains("worker-error"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }

        server.shutdown().await;
    }
}
