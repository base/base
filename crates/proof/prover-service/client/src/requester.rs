//! Client for proof requester JSON-RPC methods.

use async_trait::async_trait;
use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
    ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiClient,
};
use jsonrpsee::http_client::HttpClient;
use tracing::debug;

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

    /// List submitted proof requests.
    async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProverServiceClientError>;
}

/// JSON-RPC client for proof requester methods.
#[derive(Clone, Debug)]
pub struct ProofRequesterClient {
    inner: HttpClient,
}

impl ProofRequesterClient {
    /// Create a requester client from an existing JSON-RPC HTTP client.
    pub const fn new(inner: HttpClient) -> Self {
        Self { inner }
    }

    /// Connect a requester client using the provided configuration.
    pub fn connect(
        config: &ProverServiceClientConfig,
    ) -> Result<Self, ProverServiceClientBuildError> {
        Ok(Self::new(config.build_http_client()?))
    }

    /// Return the underlying JSON-RPC HTTP client.
    pub const fn inner(&self) -> &HttpClient {
        &self.inner
    }

    /// Submit a prove-block-range proof request.
    pub async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
        debug!(
            session_id = ?request.proof.session_id,
            "proving block range"
        );
        Ok(self.inner.prove_block_range(request).await?)
    }

    /// Return proof status and result data for a submitted proof request.
    pub async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ProverServiceClientError> {
        debug!(session_id = %request.session_id, "fetching proof");
        Ok(self.inner.get_proof(request).await?)
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
        Ok(self.inner.list_proofs(request).await?)
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
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use base_prover_service_protocol::{
        GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse, ProofRequest,
        ProofRequestKind, ProofResult, ProofStatus, ProofSummary, ProofType,
        ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiServer, ZkProofRequest,
        ZkProofResult, ZkVm,
    };
    use chrono::Utc;
    use jsonrpsee::{
        core::{RpcResult, client::Error as JsonRpcClientError},
        http_client::HttpClientBuilder,
        server::{Server, ServerHandle},
        types::ErrorObjectOwned,
    };

    use super::{ProofRequesterClient, ProofRequesterProvider};
    use crate::ProverServiceClientError;

    #[derive(Clone, Debug)]
    struct MockRequesterApi {
        state: Arc<Mutex<MockRequesterState>>,
        reject_get_proof: bool,
    }

    #[derive(Debug, Default)]
    struct MockRequesterState {
        prove_request: Option<ProveBlockRangeRequest>,
        get_request: Option<GetProofRequest>,
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
            }
        }

        fn rejecting_get_proof() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockRequesterState::default())),
                reject_get_proof: true,
            }
        }
    }

    impl RunningRequesterServer {
        async fn spawn(api: MockRequesterApi) -> Self {
            let addr: SocketAddr = "127.0.0.1:0".parse().expect("test address should parse");
            let server = Server::builder().build(addr).await.expect("server should bind");
            let local_addr = server.local_addr().expect("server should have local address");
            let handle = server.start(api.into_rpc());
            let endpoint = format!("http://{local_addr}");
            let inner = HttpClientBuilder::default().build(endpoint).expect("client should build");

            Self { client: ProofRequesterClient::new(inner), handle }
        }

        async fn shutdown(self) {
            self.handle.stop().expect("server should stop");
            self.handle.stopped().await;
        }
    }

    #[async_trait]
    impl ProverRequesterApiServer for MockRequesterApi {
        async fn prove_block_range(
            &self,
            request: ProveBlockRangeRequest,
        ) -> RpcResult<ProveBlockRangeResponse> {
            self.state.lock().expect("state lock should not be poisoned").prove_request =
                Some(request.clone());

            let session_id = request.proof.session_id.expect("test request should set session_id");
            Ok(ProveBlockRangeResponse { session_id })
        }

        async fn get_proof(&self, request: GetProofRequest) -> RpcResult<GetProofResponse> {
            self.state.lock().expect("state lock should not be poisoned").get_request =
                Some(request.clone());

            if self.reject_get_proof {
                return Err(ErrorObjectOwned::owned(
                    ProverServiceClientError::ERROR_UNAVAILABLE,
                    format!("session_id {} is temporarily unavailable", request.session_id),
                    None::<()>,
                ));
            }

            Ok(GetProofResponse {
                status: ProofStatus::Succeeded,
                error_message: None,
                result: Some(ProofResult::Compressed(ZkProofResult {
                    zk_vm: ZkVm::Sp1,
                    proof: vec![0xab, 0xcd].into(),
                })),
            })
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
                session_id: Some(session_id.to_owned()),
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
            assert_eq!(state.list_request, Some(list_request));
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn requester_rpc_errors_preserve_call_context_and_retryability() {
        let api = MockRequesterApi::rejecting_get_proof();
        let server = RunningRequesterServer::spawn(api).await;
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
}
