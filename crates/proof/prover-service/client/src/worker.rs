//! Client for prover worker JSON-RPC methods.

use async_trait::async_trait;
use backon::Retryable;
use base_prover_service_protocol::{
    GetNextProofRequest, GetNextProofResponse, HeartbeatRequest, HeartbeatResponse,
    ProverWorkerApiClient, WorkerSubmitProofRequest, WorkerSubmitProofResponse,
};
use base_retry::RetryConfig;
use jsonrpsee::http_client::HttpClient;
use tracing::{debug, warn};

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
///
/// Idempotent worker operations are wrapped in a `backon` exponential backoff that retries
/// transient JSON-RPC failures (per [`ProverServiceClientError::is_retryable`]). Retry
/// behavior is controlled by the [`RetryConfig`] passed at construction time, defaulting
/// to [`RetryConfig::default`].
#[derive(Clone, Debug)]
pub struct ProverWorkerClient {
    inner: HttpClient,
    retry: RetryConfig,
}

impl ProverWorkerClient {
    /// Create a worker client from an existing JSON-RPC HTTP client. Idempotent worker retries
    /// use [`RetryConfig::default`]; call [`Self::with_retry_config`] to override.
    pub fn new(inner: HttpClient) -> Self {
        Self { inner, retry: RetryConfig::default() }
    }

    /// Connect a worker client using the provided configuration.
    pub fn connect(
        config: &ProverServiceClientConfig,
    ) -> Result<Self, ProverServiceClientBuildError> {
        Ok(Self::new(config.build_http_client()?).with_retry_config(config.retry_config()))
    }

    /// Override the retry configuration applied to idempotent worker operations.
    #[must_use]
    pub const fn with_retry_config(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Return the underlying JSON-RPC HTTP client.
    pub const fn inner(&self) -> &HttpClient {
        &self.inner
    }

    /// Return the retry configuration applied to idempotent worker operations.
    pub const fn retry_config(&self) -> RetryConfig {
        self.retry
    }

    /// Atomically claim the next available proof job for this worker.
    ///
    /// This call is issued exactly once because it mutates server-side job lock state and has no
    /// idempotency key. Retrying after a lost response could claim and lock another job.
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
        (|| {
            let request = request.clone();

            async move { Ok(self.inner.heartbeat(request).await?) }
        })
        .retry(self.retry.to_backoff_builder())
        .when(ProverServiceClientError::is_retryable)
        .notify(|error, delay| {
            warn!(
                session_id = %request.session_id,
                lock_id = %request.lock_id,
                worker_id = %request.worker_id,
                lock_duration_seconds = request.lock_duration_seconds,
                backoff_ms = delay.as_millis(),
                error = %error,
                "heartbeat failed; retrying"
            );
        })
        .await
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
        (|| {
            let request = request.clone();

            async move { Ok(self.inner.submit_proof(request).await?) }
        })
        .retry(self.retry.to_backoff_builder())
        .when(ProverServiceClientError::is_retryable)
        .notify(|error, delay| {
            warn!(
                session_id = %request.session_id,
                lock_id = %request.lock_id,
                worker_id = %request.worker_id,
                backoff_ms = delay.as_millis(),
                error = %error,
                "submit proof failed; retrying"
            );
        })
        .await
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
        collections::VecDeque,
        net::SocketAddr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, Ordering},
        },
        time::Duration,
    };

    use base_prover_service_protocol::{
        ProofJob, ProofJobStatus, ProofRequest, ProofRequestKind, ProofResult, ProofType,
        ProverWorkerApiServer, ZkProofRequest, ZkProofResult, ZkVm,
    };
    use base_retry::RetryConfig;
    use chrono::Utc;
    use jsonrpsee::{
        core::{RpcResult, async_trait, client::Error as JsonRpcClientError},
        http_client::HttpClientBuilder,
        server::{Server, ServerHandle},
        types::{ErrorObjectOwned, error::ErrorCode},
    };

    use super::{
        GetNextProofRequest, GetNextProofResponse, HeartbeatRequest, HeartbeatResponse,
        ProverWorkerClient, ProverWorkerProvider, WorkerSubmitProofRequest,
        WorkerSubmitProofResponse,
    };
    use crate::ProverServiceClientError;

    #[derive(Debug)]
    enum ScriptedOutcome {
        Retryable,
        Fatal,
        Success,
    }

    #[derive(Clone, Debug)]
    struct MockWorkerApi {
        state: Arc<Mutex<MockWorkerState>>,
        reject_heartbeat: bool,
        get_next_script: Arc<Mutex<VecDeque<ScriptedOutcome>>>,
        heartbeat_script: Arc<Mutex<VecDeque<ScriptedOutcome>>>,
        submit_script: Arc<Mutex<VecDeque<ScriptedOutcome>>>,
        get_next_calls: Arc<AtomicU32>,
        heartbeat_calls: Arc<AtomicU32>,
        submit_calls: Arc<AtomicU32>,
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
                get_next_script: Arc::new(Mutex::new(VecDeque::new())),
                heartbeat_script: Arc::new(Mutex::new(VecDeque::new())),
                submit_script: Arc::new(Mutex::new(VecDeque::new())),
                get_next_calls: Arc::new(AtomicU32::new(0)),
                heartbeat_calls: Arc::new(AtomicU32::new(0)),
                submit_calls: Arc::new(AtomicU32::new(0)),
            }
        }

        fn rejecting_heartbeat() -> Self {
            let mut api = Self::new();
            api.reject_heartbeat = true;
            api
        }

        fn queue_get_next_outcomes<I: IntoIterator<Item = ScriptedOutcome>>(&self, outcomes: I) {
            self.get_next_script.lock().expect("script lock").extend(outcomes);
        }

        fn queue_heartbeat_outcomes<I: IntoIterator<Item = ScriptedOutcome>>(&self, outcomes: I) {
            self.heartbeat_script.lock().expect("script lock").extend(outcomes);
        }

        fn queue_submit_outcomes<I: IntoIterator<Item = ScriptedOutcome>>(&self, outcomes: I) {
            self.submit_script.lock().expect("script lock").extend(outcomes);
        }

        fn get_next_calls(&self) -> u32 {
            self.get_next_calls.load(Ordering::SeqCst)
        }

        fn heartbeat_calls(&self) -> u32 {
            self.heartbeat_calls.load(Ordering::SeqCst)
        }

        fn submit_calls(&self) -> u32 {
            self.submit_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ProverWorkerApiServer for MockWorkerApi {
        async fn get_next_proof(
            &self,
            request: GetNextProofRequest,
        ) -> RpcResult<GetNextProofResponse> {
            self.get_next_calls.fetch_add(1, Ordering::SeqCst);
            self.state.lock().expect("state lock should not be poisoned").get_next_request =
                Some(request.clone());

            match self.get_next_script.lock().expect("script lock").pop_front() {
                Some(ScriptedOutcome::Retryable) => {
                    return Err(unavailable_error("scripted get_next_proof retryable failure"));
                }
                Some(ScriptedOutcome::Fatal) => {
                    return Err(invalid_params_error("scripted get_next_proof fatal failure"));
                }
                None | Some(ScriptedOutcome::Success) => {}
            }

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
            self.heartbeat_calls.fetch_add(1, Ordering::SeqCst);
            self.state.lock().expect("state lock should not be poisoned").heartbeat_request =
                Some(request.clone());

            match self.heartbeat_script.lock().expect("script lock").pop_front() {
                Some(ScriptedOutcome::Retryable) => {
                    return Err(unavailable_error("scripted heartbeat retryable failure"));
                }
                Some(ScriptedOutcome::Fatal) => {
                    return Err(invalid_params_error("scripted heartbeat fatal failure"));
                }
                None | Some(ScriptedOutcome::Success) => {}
            }

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
            self.submit_calls.fetch_add(1, Ordering::SeqCst);
            self.state.lock().expect("state lock should not be poisoned").submit_request =
                Some(request.clone());

            match self.submit_script.lock().expect("script lock").pop_front() {
                Some(ScriptedOutcome::Retryable) => {
                    return Err(unavailable_error("scripted submit_proof retryable failure"));
                }
                Some(ScriptedOutcome::Fatal) => {
                    return Err(invalid_params_error("scripted submit_proof fatal failure"));
                }
                None | Some(ScriptedOutcome::Success) => {}
            }

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
            Self::spawn_with_retry(api, RetryConfig::default()).await
        }

        async fn spawn_with_retry(api: MockWorkerApi, retry: RetryConfig) -> Self {
            let addr: SocketAddr = "127.0.0.1:0".parse().expect("test address should parse");
            let server = Server::builder().build(addr).await.expect("server should bind");
            let local_addr = server.local_addr().expect("server should have local address");
            let handle = server.start(api.into_rpc());
            let endpoint = format!("http://{local_addr}");
            let inner = HttpClientBuilder::default().build(endpoint).expect("client should build");

            Self { client: ProverWorkerClient::new(inner).with_retry_config(retry), handle }
        }

        async fn shutdown(self) {
            self.handle.stop().expect("server should stop");
            self.handle.stopped().await;
        }
    }

    fn unavailable_error(message: impl Into<String>) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(
            ProverServiceClientError::ERROR_UNAVAILABLE,
            message.into(),
            None::<()>,
        )
    }

    fn invalid_params_error(message: impl Into<String>) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), message.into(), None::<()>)
    }

    fn fast_retry_config() -> RetryConfig {
        RetryConfig::new(3, Duration::from_millis(1), Duration::from_millis(1))
    }

    fn sample_get_next_request(worker_id: &str) -> GetNextProofRequest {
        GetNextProofRequest {
            worker_id: worker_id.to_owned(),
            proof_type: ProofType::Compressed,
            tee_kinds: Vec::new(),
            zk_vms: vec![ZkVm::Sp1],
            lock_duration_seconds: 60,
        }
    }

    fn sample_heartbeat_request(session_id: &str) -> HeartbeatRequest {
        HeartbeatRequest {
            session_id: session_id.to_owned(),
            lock_id: "lock-heartbeat".to_owned(),
            worker_id: "worker-heartbeat".to_owned(),
            lock_duration_seconds: 30,
        }
    }

    fn sample_submit_request(session_id: &str) -> WorkerSubmitProofRequest {
        WorkerSubmitProofRequest {
            session_id: session_id.to_owned(),
            lock_id: "lock-submit".to_owned(),
            worker_id: "worker-submit".to_owned(),
            result: proof_result(),
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

    #[tokio::test]
    async fn worker_does_not_retry_retryable_get_next_proof() {
        let api = MockWorkerApi::new();
        api.queue_get_next_outcomes([ScriptedOutcome::Retryable, ScriptedOutcome::Success]);
        let api_clone = api.clone();
        let server = RunningWorkerServer::spawn_with_retry(api, fast_retry_config()).await;

        let err = server
            .client
            .get_next_proof(sample_get_next_request("worker-retry"))
            .await
            .expect_err("get_next_proof retryable error should not be retried");

        assert!(err.is_retryable());
        assert_eq!(api_clone.get_next_calls(), 1);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn worker_retries_retryable_heartbeat_until_success() {
        let api = MockWorkerApi::new();
        api.queue_heartbeat_outcomes([ScriptedOutcome::Retryable, ScriptedOutcome::Success]);
        let api_clone = api.clone();
        let server = RunningWorkerServer::spawn_with_retry(api, fast_retry_config()).await;

        let response = server
            .client
            .heartbeat(sample_heartbeat_request("session-heartbeat-retry"))
            .await
            .expect("heartbeat should succeed after retry");

        assert_eq!(response.job.session_id, "session-heartbeat-retry");
        assert_eq!(api_clone.heartbeat_calls(), 2);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn worker_retries_retryable_submit_proof_until_success() {
        let api = MockWorkerApi::new();
        api.queue_submit_outcomes([ScriptedOutcome::Retryable, ScriptedOutcome::Success]);
        let api_clone = api.clone();
        let server = RunningWorkerServer::spawn_with_retry(api, fast_retry_config()).await;

        let response = server
            .client
            .submit_proof(sample_submit_request("session-submit-retry"))
            .await
            .expect("submit_proof should succeed after retry");

        assert_eq!(response.job.session_id, "session-submit-retry");
        assert_eq!(api_clone.submit_calls(), 2);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn worker_propagates_final_error_when_retries_exhausted() {
        let config = fast_retry_config();
        let api = MockWorkerApi::new();
        let total_calls = config.max_attempts.expect("fast retry config should be bounded") + 1;
        api.queue_submit_outcomes((0..total_calls).map(|_| ScriptedOutcome::Retryable));
        let api_clone = api.clone();
        let server = RunningWorkerServer::spawn_with_retry(api, config).await;

        let err = server
            .client
            .submit_proof(sample_submit_request("session-exhaust"))
            .await
            .expect_err("retries should be exhausted");

        assert!(err.is_retryable());
        assert_eq!(api_clone.submit_calls(), total_calls);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn worker_does_not_retry_fatal_heartbeat() {
        let api = MockWorkerApi::new();
        api.queue_heartbeat_outcomes([ScriptedOutcome::Fatal]);
        let api_clone = api.clone();
        let server = RunningWorkerServer::spawn_with_retry(api, fast_retry_config()).await;

        let err = server
            .client
            .heartbeat(sample_heartbeat_request("session-fatal"))
            .await
            .expect_err("fatal error should not be retried");

        assert!(!err.is_retryable());
        assert_eq!(api_clone.heartbeat_calls(), 1);

        server.shutdown().await;
    }
}
