//! Client for prover worker JSON-RPC methods.

use async_trait::async_trait;
use base_prover_service_protocol::{
    GetNextProofRequest, GetNextProofResponse, HeartbeatRequest, HeartbeatResponse,
    ProverWorkerApiClient, WorkerSubmitProofRequest, WorkerSubmitProofResponse,
};
use jsonrpsee::http_client::HttpClient;
use tracing::debug;

use crate::{ProverServiceClientBuildError, ProverServiceClientConfig, ProverServiceClientError};

/// Abstraction over prover worker JSON-RPC methods.
///
/// The canonical implementation is [`ProverWorkerClient`]. The trait lets
/// worker services depend on a mockable interface without exposing requester
/// helpers such as submit-and-wait flows.
#[async_trait]
pub trait ProverWorkerProvider: Send + Sync {
    /// Atomically claim the next available proof job for this worker.
    async fn get_next_proof(
        &self,
        request: GetNextProofRequest,
    ) -> Result<GetNextProofResponse, ProverServiceClientError>;

    /// Extend a claimed proof job lock.
    async fn heartbeat(
        &self,
        request: HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ProverServiceClientError>;

    /// Submit a proof result for a proof job.
    async fn submit_proof(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError>;
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
    pub fn connect(
        config: &ProverServiceClientConfig,
    ) -> Result<Self, ProverServiceClientBuildError> {
        Ok(Self::new(config.build_http_client()?))
    }

    /// Return the underlying JSON-RPC HTTP client.
    pub const fn inner(&self) -> &HttpClient {
        &self.inner
    }

    /// Atomically claim the next available proof job for this worker.
    pub async fn get_next_proof(
        &self,
        request: GetNextProofRequest,
    ) -> Result<GetNextProofResponse, ProverServiceClientError> {
        debug!(
            worker_id = %request.worker_id,
            proof_type = ?request.proof_type,
            tee_kinds = ?request.tee_kinds,
            zk_vms = ?request.zk_vms,
            lock_duration_seconds = request.lock_duration_seconds,
            "claiming next proof job"
        );
        Ok(self.inner.get_next_proof(request).await?)
    }

    /// Extend a claimed proof job lock.
    pub async fn heartbeat(
        &self,
        request: HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ProverServiceClientError> {
        debug!(
            session_id = %request.session_id,
            lock_id = %request.lock_id,
            worker_id = %request.worker_id,
            lock_duration_seconds = request.lock_duration_seconds,
            "heartbeating proof job"
        );
        Ok(self.inner.heartbeat(request).await?)
    }

    /// Submit a proof result for a proof job.
    pub async fn submit_proof(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError> {
        debug!(
            session_id = %request.session_id,
            lock_id = %request.lock_id,
            worker_id = %request.worker_id,
            "submitting proof job result"
        );
        Ok(self.inner.submit_proof(request).await?)
    }
}

#[async_trait]
impl ProverWorkerProvider for ProverWorkerClient {
    async fn get_next_proof(
        &self,
        request: GetNextProofRequest,
    ) -> Result<GetNextProofResponse, ProverServiceClientError> {
        Self::get_next_proof(self, request).await
    }

    async fn heartbeat(
        &self,
        request: HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ProverServiceClientError> {
        Self::heartbeat(self, request).await
    }

    async fn submit_proof(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError> {
        Self::submit_proof(self, request).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use base_prover_service_protocol::{
        ProofJob, ProofJobStatus, ProofRequest, ProofRequestKind, ProofResult, ProofType,
        ProverWorkerApiServer, ZkProofRequest, ZkProofResult, ZkVm,
    };
    use chrono::Utc;
    use jsonrpsee::{
        core::{RpcResult, async_trait, client::Error as JsonRpcClientError},
        http_client::HttpClientBuilder,
        server::{Server, ServerHandle},
        types::ErrorObjectOwned,
    };

    use super::{
        GetNextProofRequest, GetNextProofResponse, HeartbeatRequest, HeartbeatResponse,
        ProverWorkerClient, ProverWorkerProvider, WorkerSubmitProofRequest,
        WorkerSubmitProofResponse,
    };
    use crate::ProverServiceClientError;

    #[derive(Clone, Debug)]
    struct MockWorkerApi {
        state: Arc<Mutex<MockWorkerState>>,
        reject_heartbeat: bool,
    }

    #[derive(Debug, Default)]
    struct MockWorkerState {
        get_next_request: Option<GetNextProofRequest>,
        heartbeat_request: Option<HeartbeatRequest>,
        submit_request: Option<WorkerSubmitProofRequest>,
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

    #[async_trait]
    impl ProverWorkerApiServer for MockWorkerApi {
        async fn get_next_proof(
            &self,
            request: GetNextProofRequest,
        ) -> RpcResult<GetNextProofResponse> {
            self.state.lock().expect("state lock should not be poisoned").get_next_request =
                Some(request.clone());
            let session_id = format!("session-for-{}", request.worker_id);

            Ok(GetNextProofResponse {
                job: Some(proof_job(
                    session_id,
                    ProofJobStatus::Claimed,
                    Some("lock-claim".to_owned()),
                    Some(request.worker_id),
                    None,
                )),
            })
        }

        async fn heartbeat(&self, request: HeartbeatRequest) -> RpcResult<HeartbeatResponse> {
            self.state.lock().expect("state lock should not be poisoned").heartbeat_request =
                Some(request.clone());

            if self.reject_heartbeat {
                return Err(ErrorObjectOwned::owned(
                    ProverServiceClientError::ERROR_FAILED_PRECONDITION,
                    format!(
                        "proof job lock {} is not claimed by worker {}",
                        request.lock_id, request.worker_id
                    ),
                    None::<()>,
                ));
            }

            Ok(HeartbeatResponse {
                job: proof_job(
                    request.session_id,
                    ProofJobStatus::Claimed,
                    Some(request.lock_id),
                    Some(request.worker_id),
                    None,
                ),
            })
        }

        async fn submit_proof(
            &self,
            request: WorkerSubmitProofRequest,
        ) -> RpcResult<WorkerSubmitProofResponse> {
            self.state.lock().expect("state lock should not be poisoned").submit_request =
                Some(request.clone());

            Ok(WorkerSubmitProofResponse {
                job: proof_job(
                    request.session_id,
                    ProofJobStatus::Succeeded,
                    Some(request.lock_id),
                    Some(request.worker_id),
                    None,
                ),
            })
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

    fn proof_job(
        session_id: impl Into<String>,
        status: ProofJobStatus,
        lock_id: Option<String>,
        worker_id: Option<String>,
        error_message: Option<String>,
    ) -> ProofJob {
        let session_id = session_id.into();

        ProofJob {
            request: proof_request(session_id.clone()),
            session_id,
            status,
            attempt: 2,
            lock_id,
            worker_id,
            lock_expires_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: matches!(status, ProofJobStatus::Succeeded | ProofJobStatus::Failed)
                .then(Utc::now),
            error_message,
        }
    }

    fn proof_request(session_id: impl Into<String>) -> ProofRequest {
        ProofRequest {
            session_id: Some(session_id.into()),
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

        let get_next_request = GetNextProofRequest {
            worker_id: "worker-claim".to_owned(),
            proof_type: ProofType::Compressed,
            tee_kinds: Vec::new(),
            zk_vms: vec![ZkVm::Sp1],
            lock_duration_seconds: 60,
        };
        let provider: &dyn ProverWorkerProvider = &server.client;
        let get_next_response = provider
            .get_next_proof(get_next_request.clone())
            .await
            .expect("get_next_proof should succeed");
        let claim_job = get_next_response.job.expect("get next response should include a job");
        assert_eq!(claim_job.session_id, "session-for-worker-claim");
        assert_eq!(claim_job.request.session_id.as_deref(), Some("session-for-worker-claim"));
        assert_eq!(claim_job.status, ProofJobStatus::Claimed);
        assert_eq!(claim_job.lock_id.as_deref(), Some("lock-claim"));
        assert_eq!(claim_job.worker_id.as_deref(), Some("worker-claim"));

        let heartbeat_request = HeartbeatRequest {
            session_id: "session-heartbeat".to_owned(),
            lock_id: "lock-heartbeat".to_owned(),
            worker_id: "worker-heartbeat".to_owned(),
            lock_duration_seconds: 30,
        };
        let heartbeat_response =
            provider.heartbeat(heartbeat_request.clone()).await.expect("heartbeat should succeed");
        assert_eq!(heartbeat_response.job.session_id, "session-heartbeat");
        assert_eq!(heartbeat_response.job.request.session_id.as_deref(), Some("session-heartbeat"));
        assert_eq!(heartbeat_response.job.lock_id.as_deref(), Some("lock-heartbeat"));
        assert_eq!(heartbeat_response.job.worker_id.as_deref(), Some("worker-heartbeat"));

        let submit_request = WorkerSubmitProofRequest {
            session_id: "session-submit".to_owned(),
            lock_id: "lock-submit".to_owned(),
            worker_id: "worker-submit".to_owned(),
            result: proof_result(),
        };
        let submit_response = provider
            .submit_proof(submit_request.clone())
            .await
            .expect("submit_proof should succeed");
        assert_eq!(submit_response.job.session_id, "session-submit");
        assert_eq!(submit_response.job.request.session_id.as_deref(), Some("session-submit"));
        assert_eq!(submit_response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(submit_response.job.lock_id.as_deref(), Some("lock-submit"));
        assert_eq!(submit_response.job.worker_id.as_deref(), Some("worker-submit"));

        {
            let state = api.state.lock().expect("state lock should not be poisoned");
            assert_eq!(state.get_next_request, Some(get_next_request));
            assert_eq!(state.heartbeat_request, Some(heartbeat_request));
            assert_eq!(state.submit_request, Some(submit_request));
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn worker_rpc_errors_preserve_rejection_context_and_retryability() {
        let api = MockWorkerApi::rejecting_heartbeat();
        let server = RunningWorkerServer::spawn(api).await;
        let provider: &dyn ProverWorkerProvider = &server.client;

        let err = provider
            .heartbeat(HeartbeatRequest {
                session_id: "session-error".to_owned(),
                lock_id: "lock-error".to_owned(),
                worker_id: "worker-error".to_owned(),
                lock_duration_seconds: 30,
            })
            .await
            .expect_err("heartbeat should be rejected");

        assert!(!err.is_retryable());

        match err {
            ProverServiceClientError::RpcTransport(JsonRpcClientError::Call(call)) => {
                assert_eq!(call.code(), ProverServiceClientError::ERROR_FAILED_PRECONDITION);
                assert!(call.message().contains("lock-error"));
                assert!(call.message().contains("worker-error"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }

        server.shutdown().await;
    }
}
