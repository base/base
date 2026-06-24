//! Client for proof requester JSON-RPC methods.

use async_trait::async_trait;
use backon::Retryable;
use base_prover_service_protocol::{
    DeleteProofRequest, GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
    ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiClient,
};
use base_retry::RetryConfig;
use jsonrpsee::http_client::HttpClient;
use tracing::{debug, warn};

use crate::{ProverServiceClientBuildError, ProverServiceClientConfig, ProverServiceClientError};

/// Abstraction over proof requester JSON-RPC methods.
///
/// The canonical implementation is [`ProofRequesterClient`]. The trait lets
/// proposer, challenger, and CLI components depend on a mockable interface
/// that exposes only the requester surface — worker-only methods such as
/// `claim`, `heartbeat`, `complete`, and `fail` are intentionally absent.
#[async_trait]
pub trait ProofRequesterProvider: Send + Sync {
    /// Submit a request to prove a block range.
    async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<ProveBlockRangeResponse, ProverServiceClientError>;

    /// Return proof status and result data for a submitted proof request.
    async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ProverServiceClientError>;

    /// Delete a completed proof request so the same session id can be retried.
    async fn delete_proof_request(
        &self,
        request: DeleteProofRequest,
    ) -> Result<(), ProverServiceClientError>;

    /// List submitted proof requests.
    async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProverServiceClientError>;
}

/// JSON-RPC client for proof requester methods.
///
/// Each requester operation is wrapped in a `backon` exponential backoff that retries
/// transient JSON-RPC failures (per [`ProverServiceClientError::is_retryable`]). Retry
/// behavior is controlled by the [`RetryConfig`] passed at construction time, defaulting
/// to [`RetryConfig::default`].
#[derive(Clone, Debug)]
pub struct ProofRequesterClient {
    inner: HttpClient,
    retry: RetryConfig,
}

impl ProofRequesterClient {
    /// Create a requester client from an existing JSON-RPC HTTP client. Retries use
    /// [`RetryConfig::default`]; call [`Self::with_retry_config`] to override.
    pub fn new(inner: HttpClient) -> Self {
        Self { inner, retry: RetryConfig::default() }
    }

    /// Connect a requester client using the provided configuration. The retry
    /// configuration is taken from [`ProverServiceClientConfig::retry_config`].
    pub fn connect(
        config: &ProverServiceClientConfig,
    ) -> Result<Self, ProverServiceClientBuildError> {
        Ok(Self::new(config.build_http_client()?).with_retry_config(config.retry_config()))
    }

    /// Override the retry configuration applied to requester operations.
    #[must_use]
    pub const fn with_retry_config(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Return the underlying JSON-RPC HTTP client.
    pub const fn inner(&self) -> &HttpClient {
        &self.inner
    }

    /// Return the retry configuration applied to requester operations.
    pub const fn retry_config(&self) -> RetryConfig {
        self.retry
    }

    /// Submit a prove-block-range proof request.
    ///
    /// Because `session_id` is required by the protocol, retries are safe to issue
    /// without enqueueing duplicate proofs under different session IDs.
    pub async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
        debug!(
            session_id = %request.proof.session_id,
            "proving block range"
        );

        (|| {
            let request = request.clone();

            async move { Ok(self.inner.prove_block_range(request).await?) }
        })
        .retry(self.retry.to_backoff_builder())
        .when(ProverServiceClientError::is_retryable)
        .notify(|error, delay| {
            warn!(
                session_id = %request.proof.session_id,
                backoff_ms = delay.as_millis(),
                error = %error,
                "prove block range failed; retrying"
            );
        })
        .await
    }

    /// Return proof status and result data for a submitted proof request.
    pub async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ProverServiceClientError> {
        debug!(session_id = %request.session_id, "fetching proof");
        (|| {
            let request = request.clone();

            async move { Ok(self.inner.get_proof(request).await?) }
        })
        .retry(self.retry.to_backoff_builder())
        .when(ProverServiceClientError::is_retryable)
        .notify(|error, delay| {
            warn!(
                session_id = %request.session_id,
                backoff_ms = delay.as_millis(),
                error = %error,
                "get proof failed; retrying"
            );
        })
        .await
    }

    /// Delete a completed proof request so the same session id can be retried.
    pub async fn delete_proof_request(
        &self,
        request: DeleteProofRequest,
    ) -> Result<(), ProverServiceClientError> {
        debug!(session_id = %request.session_id, "deleting proof");
        (|| {
            let request = request.clone();

            async move { Ok(self.inner.delete_proof_request(request).await?) }
        })
        .retry(self.retry.to_backoff_builder())
        .when(ProverServiceClientError::is_retryable)
        .notify(|error, delay| {
            warn!(
                session_id = %request.session_id,
                backoff_ms = delay.as_millis(),
                error = %error,
                "delete proof failed; retrying"
            );
        })
        .await
    }

    /// List submitted proof requests.
    pub async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProverServiceClientError> {
        debug!(
            offset = request.offset,
            limit = request.limit,
            status_filter = ?request.status_filter,
            "listing proofs"
        );
        (|| async move { Ok(self.inner.list_proofs(request).await?) })
            .retry(self.retry.to_backoff_builder())
            .when(ProverServiceClientError::is_retryable)
            .notify(|error, delay| {
                warn!(
                    offset = request.offset,
                    limit = request.limit,
                    status_filter = ?request.status_filter,
                    backoff_ms = delay.as_millis(),
                    error = %error,
                    "list proofs failed; retrying"
                );
            })
            .await
    }
}

#[async_trait]
impl ProofRequesterProvider for ProofRequesterClient {
    async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
        Self::prove_block_range(self, request).await
    }

    async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ProverServiceClientError> {
        Self::get_proof(self, request).await
    }

    async fn delete_proof_request(
        &self,
        request: DeleteProofRequest,
    ) -> Result<(), ProverServiceClientError> {
        Self::delete_proof_request(self, request).await
    }

    async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProverServiceClientError> {
        Self::list_proofs(self, request).await
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

    use async_trait::async_trait;
    use base_prover_service_protocol::{
        DeleteProofRequest, GetProofRequest, GetProofResponse, ListProofsRequest,
        ListProofsResponse, ProofRequest, ProofRequestKind, ProofResult, ProofStatus, ProofSummary,
        ProofType, ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiServer,
        ZkProofRequest, ZkProofResult, ZkVm,
    };
    use base_retry::RetryConfig;
    use chrono::Utc;
    use jsonrpsee::{
        core::{RpcResult, client::Error as JsonRpcClientError},
        http_client::HttpClientBuilder,
        server::{Server, ServerHandle},
        types::{ErrorObjectOwned, error::ErrorCode},
    };

    use super::{ProofRequesterClient, ProofRequesterProvider};
    use crate::ProverServiceClientError;

    /// Outcome script for a single requester call when the test wants to drive
    /// retry behavior. The server returns the head of the queue per call.
    #[derive(Clone, Copy, Debug)]
    enum ScriptedOutcome {
        Retryable,
        Fatal,
        Success,
    }

    #[derive(Clone, Debug)]
    struct MockRequesterApi {
        state: Arc<Mutex<MockRequesterState>>,
        reject_get_proof: bool,
        prove_script: Arc<Mutex<VecDeque<ScriptedOutcome>>>,
        get_script: Arc<Mutex<VecDeque<ScriptedOutcome>>>,
        delete_script: Arc<Mutex<VecDeque<ScriptedOutcome>>>,
        prove_calls: Arc<AtomicU32>,
        get_calls: Arc<AtomicU32>,
        delete_calls: Arc<AtomicU32>,
    }

    #[derive(Debug, Default)]
    struct MockRequesterState {
        prove_request: Option<ProveBlockRangeRequest>,
        get_request: Option<GetProofRequest>,
        delete_request: Option<DeleteProofRequest>,
        list_request: Option<ListProofsRequest>,
    }

    #[derive(Debug)]
    struct RunningRequesterServer {
        client: ProofRequesterClient,
        handle: ServerHandle,
    }

    impl MockRequesterApi {
        fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockRequesterState::default())),
                reject_get_proof: false,
                prove_script: Arc::new(Mutex::new(VecDeque::new())),
                get_script: Arc::new(Mutex::new(VecDeque::new())),
                delete_script: Arc::new(Mutex::new(VecDeque::new())),
                prove_calls: Arc::new(AtomicU32::new(0)),
                get_calls: Arc::new(AtomicU32::new(0)),
                delete_calls: Arc::new(AtomicU32::new(0)),
            }
        }

        fn rejecting_get_proof() -> Self {
            let mut api = Self::new();
            api.reject_get_proof = true;
            api
        }

        fn queue_prove_outcomes<I: IntoIterator<Item = ScriptedOutcome>>(&self, outcomes: I) {
            self.prove_script.lock().expect("script lock").extend(outcomes);
        }

        fn queue_get_outcomes<I: IntoIterator<Item = ScriptedOutcome>>(&self, outcomes: I) {
            self.get_script.lock().expect("script lock").extend(outcomes);
        }

        fn queue_delete_outcomes<I: IntoIterator<Item = ScriptedOutcome>>(&self, outcomes: I) {
            self.delete_script.lock().expect("script lock").extend(outcomes);
        }

        fn prove_calls(&self) -> u32 {
            self.prove_calls.load(Ordering::SeqCst)
        }

        fn get_calls(&self) -> u32 {
            self.get_calls.load(Ordering::SeqCst)
        }

        fn delete_calls(&self) -> u32 {
            self.delete_calls.load(Ordering::SeqCst)
        }
    }

    impl RunningRequesterServer {
        async fn spawn(api: MockRequesterApi) -> Self {
            Self::spawn_with_retry(api, RetryConfig::default()).await
        }

        async fn spawn_with_retry(api: MockRequesterApi, retry: RetryConfig) -> Self {
            let addr: SocketAddr = "127.0.0.1:0".parse().expect("test address should parse");
            let server = Server::builder().build(addr).await.expect("server should bind");
            let local_addr = server.local_addr().expect("server should have local address");
            let handle = server.start(api.into_rpc());
            let endpoint = format!("http://{local_addr}");
            let inner = HttpClientBuilder::default().build(endpoint).expect("client should build");

            Self { client: ProofRequesterClient::new(inner).with_retry_config(retry), handle }
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

    #[async_trait]
    impl ProverRequesterApiServer for MockRequesterApi {
        async fn prove_block_range(
            &self,
            request: ProveBlockRangeRequest,
        ) -> RpcResult<ProveBlockRangeResponse> {
            self.prove_calls.fetch_add(1, Ordering::SeqCst);
            self.state.lock().expect("state lock should not be poisoned").prove_request =
                Some(request.clone());

            let scripted = self.prove_script.lock().expect("script lock").pop_front();
            match scripted {
                Some(ScriptedOutcome::Retryable) => {
                    Err(unavailable_error("scripted prove_block_range retryable failure"))
                }
                Some(ScriptedOutcome::Fatal) => {
                    Err(invalid_params_error("scripted prove_block_range fatal failure"))
                }
                None | Some(ScriptedOutcome::Success) => {
                    Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
                }
            }
        }

        async fn get_proof(&self, request: GetProofRequest) -> RpcResult<GetProofResponse> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            self.state.lock().expect("state lock should not be poisoned").get_request =
                Some(request.clone());

            let scripted = self.get_script.lock().expect("script lock").pop_front();
            if let Some(ScriptedOutcome::Fatal) = scripted {
                return Err(invalid_params_error(format!(
                    "session_id {} is invalid",
                    request.session_id
                )));
            }

            if self.reject_get_proof || matches!(scripted, Some(ScriptedOutcome::Retryable)) {
                return Err(unavailable_error(format!(
                    "session_id {} is temporarily unavailable",
                    request.session_id
                )));
            }

            Ok(GetProofResponse {
                status: ProofStatus::Succeeded,
                error_message: None,
                result: Some(ProofResult::Compressed(ZkProofResult {
                    zk_vm: ZkVm::Sp1,
                    proof: vec![0xab, 0xcd].into(),
                    execution_stats: None,
                })),
            })
        }

        async fn delete_proof_request(&self, request: DeleteProofRequest) -> RpcResult<()> {
            self.delete_calls.fetch_add(1, Ordering::SeqCst);
            self.state.lock().expect("state lock should not be poisoned").delete_request =
                Some(request);

            let scripted = self.delete_script.lock().expect("script lock").pop_front();
            match scripted {
                Some(ScriptedOutcome::Retryable) => {
                    Err(unavailable_error("scripted delete_proof_request retryable failure"))
                }
                Some(ScriptedOutcome::Fatal) => {
                    Err(invalid_params_error("scripted delete_proof_request fatal failure"))
                }
                None | Some(ScriptedOutcome::Success) => Ok(()),
            }
        }

        async fn list_proofs(&self, request: ListProofsRequest) -> RpcResult<ListProofsResponse> {
            self.state.lock().expect("state lock should not be poisoned").list_request =
                Some(request);

            let summary = ProofSummary {
                session_id: "session-list-1".to_owned(),
                proof_type: ProofType::Compressed,
                status: ProofStatus::Succeeded,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                completed_at: Some(Utc::now()),
                error_message: None,
                tee_kind: None,
                zk_vm: Some(ZkVm::Sp1),
            };
            Ok(ListProofsResponse { proofs: vec![summary], total_count: 1 })
        }
    }

    fn sample_prove_request(session_id: &str) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: session_id.to_owned(),
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number: 100,
                    number_of_blocks_to_prove: 5,
                    sequence_window: None,
                    l1_head: None,
                    intermediate_root_interval: None,
                    zk_vm: ZkVm::Sp1,
                }),
            },
        }
    }

    #[tokio::test]
    async fn requester_methods_round_trip_requests_and_responses() {
        let api = MockRequesterApi::new();
        let server = RunningRequesterServer::spawn(api.clone()).await;
        let provider: &dyn ProofRequesterProvider = &server.client;

        let prove_request = sample_prove_request("session-prove");
        let prove_response = provider
            .prove_block_range(prove_request.clone())
            .await
            .expect("prove_block_range should succeed");
        assert_eq!(prove_response.session_id, "session-prove");

        let get_request = GetProofRequest { session_id: "session-get".to_owned() };
        let get_response =
            provider.get_proof(get_request.clone()).await.expect("get_proof should succeed");
        assert_eq!(get_response.status, ProofStatus::Succeeded);
        match get_response.result.expect("get response should include a result") {
            ProofResult::Compressed(zk) => {
                assert_eq!(zk.zk_vm, ZkVm::Sp1);
                assert_eq!(zk.proof.as_ref(), &[0xab, 0xcd]);
            }
            other => panic!("unexpected proof result variant: {other:?}"),
        }

        let delete_request = DeleteProofRequest { session_id: "session-get".to_owned() };
        provider
            .delete_proof_request(delete_request.clone())
            .await
            .expect("delete_proof_request should succeed");

        let list_request =
            ListProofsRequest { offset: 7, limit: 25, status_filter: Some(ProofStatus::Succeeded) };
        let list_response =
            provider.list_proofs(list_request).await.expect("list_proofs should succeed");
        assert_eq!(list_response.total_count, 1);
        assert_eq!(list_response.proofs.len(), 1);
        assert_eq!(list_response.proofs[0].session_id, "session-list-1");

        {
            let state = api.state.lock().expect("state lock should not be poisoned");
            assert_eq!(state.prove_request.as_ref(), Some(&prove_request));
            assert_eq!(state.get_request.as_ref(), Some(&get_request));
            assert_eq!(state.delete_request.as_ref(), Some(&delete_request));
            assert_eq!(state.list_request, Some(list_request));
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn requester_rpc_errors_preserve_call_context_and_retryability() {
        let api = MockRequesterApi::rejecting_get_proof();
        // Use a single explicit retry with 1ms delays so the test exercises the real
        // retry code path quickly. Call count is not asserted; this test only verifies
        // the final error variant, code, and retryability classification.
        let server = RunningRequesterServer::spawn_with_retry(
            api,
            RetryConfig::new(1, Duration::from_millis(1), Duration::from_millis(1)),
        )
        .await;
        let provider: &dyn ProofRequesterProvider = &server.client;

        let err = provider
            .get_proof(GetProofRequest { session_id: "session-error".to_owned() })
            .await
            .expect_err("get_proof should be rejected");

        assert!(err.is_retryable());

        match err {
            ProverServiceClientError::RpcTransport(JsonRpcClientError::Call(call)) => {
                assert_eq!(call.code(), ProverServiceClientError::ERROR_UNAVAILABLE);
                assert!(call.message().contains("session-error"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn requester_retries_retryable_prove_block_range_until_success() {
        let api = MockRequesterApi::new();
        api.queue_prove_outcomes([ScriptedOutcome::Retryable, ScriptedOutcome::Success]);
        let api_clone = api.clone();
        let server = RunningRequesterServer::spawn_with_retry(api, fast_retry_config()).await;

        let response = server
            .client
            .prove_block_range(sample_prove_request("session-retry"))
            .await
            .expect("prove_block_range should succeed after retry");

        assert_eq!(response.session_id, "session-retry");
        assert_eq!(api_clone.prove_calls(), 2);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn requester_propagates_final_error_when_retries_exhausted() {
        let config = fast_retry_config();
        let api = MockRequesterApi::new();
        // backon's `with_max_times(n)` allows `n` retries on top of the initial call,
        // so an exhausted run performs `max_attempts + 1` total calls.
        let total_calls = config.max_attempts.expect("fast retry config should be bounded") + 1;
        api.queue_prove_outcomes((0..total_calls).map(|_| ScriptedOutcome::Retryable));
        let api_clone = api.clone();
        let server = RunningRequesterServer::spawn_with_retry(api, config).await;

        let err = server
            .client
            .prove_block_range(sample_prove_request("session-exhaust"))
            .await
            .expect_err("retries should be exhausted");

        assert!(err.is_retryable());
        assert_eq!(api_clone.prove_calls(), total_calls);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn requester_does_not_retry_fatal_get_proof() {
        let api = MockRequesterApi::new();
        api.queue_get_outcomes([ScriptedOutcome::Fatal]);
        let api_clone = api.clone();
        let server = RunningRequesterServer::spawn_with_retry(api, fast_retry_config()).await;

        let err = server
            .client
            .get_proof(GetProofRequest { session_id: "session-fatal".to_owned() })
            .await
            .expect_err("fatal error should not be retried");

        assert!(!err.is_retryable());
        assert_eq!(api_clone.get_calls(), 1);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn requester_retries_retryable_delete_until_success() {
        let api = MockRequesterApi::new();
        api.queue_delete_outcomes([ScriptedOutcome::Retryable, ScriptedOutcome::Success]);
        let api_clone = api.clone();
        let server = RunningRequesterServer::spawn_with_retry(api, fast_retry_config()).await;

        server
            .client
            .delete_proof_request(DeleteProofRequest { session_id: "session-delete".to_owned() })
            .await
            .expect("delete_proof_request should succeed after retry");

        assert_eq!(api_clone.delete_calls(), 2);

        server.shutdown().await;
    }
}
