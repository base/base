//! Prover-service requester client helpers for the `basectl proofs` command group.

use std::time::Duration;

use alloy_primitives::B256;
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse, ProofRequest,
    ProofRequestKind, ProofSessionId, ProofStatus, ProveBlockRangeRequest, ZkBackend,
    ZkProofRequest, ZkVm,
};
use tokio::time::{Instant, sleep};
use tracing::{debug, info};
use url::Url;

use crate::errors::ProofsCommandError;

/// Parameters for a `basectl proofs finalize` compressed ZK proof request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofFinalizeRequest {
    /// First L2 block number to prove.
    pub start_block: u64,
    /// Number of consecutive L2 blocks to prove.
    pub num_blocks: u64,
    /// Explicit session ID override. When `None`, an idempotent session ID is
    /// derived from the network name and block range.
    pub session_id: Option<String>,
    /// Optional L1 head hash used for witness generation.
    pub l1_head: Option<B256>,
    /// Optional sequencing window.
    pub sequence_window: Option<u64>,
    /// Optional intermediate output root interval.
    pub intermediate_root_interval: Option<u64>,
}

impl ProofFinalizeRequest {
    /// Session ID namespace for proofs requested via basectl.
    const SESSION_NAMESPACE: &'static [u8] = b"basectl";

    /// Session ID proof subtype for compressed SP1 proofs.
    const SESSION_SUBTYPE: &'static str = "zk/sp1/compressed";

    /// Returns the effective session ID for `network`.
    ///
    /// Uses the explicit override when set; otherwise derives an idempotent
    /// `UUIDv5` from the network name and block range so re-running the same
    /// command resolves to the same prover-service session instead of
    /// enqueueing a duplicate proof.
    pub fn effective_session_id(&self, network: &str) -> String {
        self.session_id.clone().unwrap_or_else(|| {
            ProofSessionId::derive_from_components(
                Self::SESSION_NAMESPACE,
                Self::SESSION_SUBTYPE,
                &[
                    network.as_bytes(),
                    &self.start_block.to_be_bytes(),
                    &self.num_blocks.to_be_bytes(),
                ],
            )
        })
    }

    /// Builds the prover-service prove-block-range request for `network`, deriving
    /// its effective session ID with [`Self::effective_session_id`].
    pub fn to_prove_request(&self, network: &str) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: self.effective_session_id(network),
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number: self.start_block,
                    number_of_blocks_to_prove: self.num_blocks,
                    sequence_window: self.sequence_window,
                    l1_head: self.l1_head,
                    intermediate_root_interval: self.intermediate_root_interval,
                    zk_vm: ZkVm::Sp1,
                    zk_backend: ZkBackend::Cluster,
                }),
            },
        }
    }
}

/// Prover-service requester client used by the `basectl proofs` commands.
#[derive(Debug, Clone)]
pub struct ProofsClient {
    endpoint: Url,
    requester: ProofRequesterClient,
    poll_interval: Duration,
    max_wait: Duration,
}

impl ProofsClient {
    /// Connects a requester client to the prover-service `endpoint`.
    pub fn connect(endpoint: &Url) -> Result<Self, ProofsCommandError> {
        let config = ProverServiceClientConfig::new(endpoint.as_str());
        let requester = ProofRequesterClient::connect(&config).map_err(|error| {
            ProofsCommandError::BuildClient {
                endpoint: endpoint.to_string(),
                message: error.to_string(),
            }
        })?;
        Ok(Self {
            endpoint: endpoint.clone(),
            requester,
            poll_interval: config.poll_interval(),
            max_wait: config.max_wait(),
        })
    }

    /// Overrides the poll cadence used by [`Self::wait_for_completion`].
    #[cfg(test)]
    #[must_use]
    pub const fn with_wait_config(mut self, poll_interval: Duration, max_wait: Duration) -> Self {
        self.poll_interval = poll_interval;
        self.max_wait = max_wait;
        self
    }

    /// Returns the CLI label for a proof status.
    pub const fn status_label(status: ProofStatus) -> &'static str {
        match status {
            ProofStatus::Queued => "queued",
            ProofStatus::Running => "running",
            ProofStatus::Succeeded => "succeeded",
            ProofStatus::Failed => "failed",
        }
    }

    /// Submits a prove-block-range request and returns the accepted session ID.
    pub async fn submit(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<String, ProofsCommandError> {
        debug!(
            endpoint = %self.endpoint,
            session_id = %request.proof.session_id,
            "submitting prove-block-range request"
        );
        let response = self
            .requester
            .prove_block_range(request)
            .await
            .map_err(|error| self.rpc_error("prover_proveBlockRange", &error))?;
        info!(
            endpoint = %self.endpoint,
            session_id = %response.session_id,
            "prove-block-range request accepted"
        );
        Ok(response.session_id)
    }

    /// Returns proof status and result data for `session_id`.
    pub async fn proof_status(
        &self,
        session_id: &str,
    ) -> Result<GetProofResponse, ProofsCommandError> {
        self.requester
            .get_proof(GetProofRequest { session_id: session_id.to_string() })
            .await
            .map_err(|error| self.rpc_error("prover_getProof", &error))
    }

    /// Lists submitted proof requests.
    pub async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProofsCommandError> {
        self.requester
            .list_proofs(request)
            .await
            .map_err(|error| self.rpc_error("prover_listProofs", &error))
    }

    /// Polls `session_id` until it reaches a terminal status or the wait
    /// window elapses.
    pub async fn wait_for_completion(
        &self,
        session_id: &str,
    ) -> Result<GetProofResponse, ProofsCommandError> {
        let started = Instant::now();
        loop {
            let response = self.proof_status(session_id).await?;
            if matches!(response.status, ProofStatus::Succeeded | ProofStatus::Failed) {
                return Ok(response);
            }

            let waited = started.elapsed();
            debug!(
                endpoint = %self.endpoint,
                session_id = %session_id,
                status = Self::status_label(response.status),
                waited_secs = waited.as_secs(),
                "proof not complete; polling again"
            );
            if waited >= self.max_wait {
                return Err(ProofsCommandError::WaitTimeout {
                    session_id: session_id.to_string(),
                    waited,
                    last_status: Self::status_label(response.status).to_string(),
                });
            }
            sleep(self.poll_interval.min(self.max_wait - waited)).await;
        }
    }

    fn rpc_error(&self, method: &'static str, error: &dyn std::fmt::Display) -> ProofsCommandError {
        ProofsCommandError::Rpc {
            endpoint: self.endpoint.to_string(),
            method,
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        net::SocketAddr,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use base_prover_service_protocol::{
        DeleteProofRequest, GetProofRequest, GetProofResponse, ListProofsRequest,
        ListProofsResponse, ProofRequestKind, ProofStatus, ProveBlockRangeRequest,
        ProveBlockRangeResponse, ProverRequesterApiServer, ZkVm,
    };
    use jsonrpsee::{
        core::{RpcResult, async_trait},
        server::{Server, ServerHandle},
        types::{ErrorObjectOwned, error::ErrorCode},
    };
    use url::Url;

    use super::{ProofFinalizeRequest, ProofsClient};
    use crate::errors::ProofsCommandError;

    fn finalize_request() -> ProofFinalizeRequest {
        ProofFinalizeRequest {
            start_block: 100,
            num_blocks: 5,
            session_id: None,
            l1_head: None,
            sequence_window: None,
            intermediate_root_interval: None,
        }
    }

    #[test]
    fn session_id_is_deterministic_per_network_and_range() {
        let request = finalize_request();

        assert_eq!(
            request.effective_session_id("mainnet"),
            request.effective_session_id("mainnet")
        );
        assert_ne!(
            request.effective_session_id("mainnet"),
            request.effective_session_id("sepolia")
        );

        let other_range = ProofFinalizeRequest { start_block: 101, ..finalize_request() };
        assert_ne!(
            request.effective_session_id("mainnet"),
            other_range.effective_session_id("mainnet")
        );
    }

    #[test]
    fn explicit_session_id_overrides_derivation() {
        let request = ProofFinalizeRequest {
            session_id: Some("custom-session".to_string()),
            ..finalize_request()
        };

        assert_eq!(request.effective_session_id("mainnet"), "custom-session");
    }

    #[test]
    fn to_prove_request_maps_all_fields() {
        let request = ProofFinalizeRequest {
            start_block: 100,
            num_blocks: 5,
            session_id: Some("session-map".to_string()),
            l1_head: Some(alloy_primitives::B256::repeat_byte(0xaa)),
            sequence_window: Some(3600),
            intermediate_root_interval: Some(10),
        };

        let prove = request.to_prove_request("devnet");
        assert_eq!(prove.proof.session_id, "session-map");
        match prove.proof.request {
            ProofRequestKind::Compressed(zk) => {
                assert_eq!(zk.start_block_number, 100);
                assert_eq!(zk.number_of_blocks_to_prove, 5);
                assert_eq!(zk.sequence_window, Some(3600));
                assert_eq!(zk.l1_head, Some(alloy_primitives::B256::repeat_byte(0xaa)));
                assert_eq!(zk.intermediate_root_interval, Some(10));
                assert_eq!(zk.zk_vm, ZkVm::Sp1);
                assert_eq!(zk.zk_backend, super::ZkBackend::Cluster);
            }
            other => panic!("unexpected proof request kind: {other:?}"),
        }
    }

    /// Mock requester API that returns scripted `get_proof` statuses in order,
    /// repeating the final status once the script is exhausted.
    #[derive(Clone, Debug)]
    struct MockRequesterApi {
        statuses: Arc<Mutex<VecDeque<ProofStatus>>>,
        last_status: ProofStatus,
    }

    impl MockRequesterApi {
        fn scripted<I: IntoIterator<Item = ProofStatus>>(
            statuses: I,
            last_status: ProofStatus,
        ) -> Self {
            Self { statuses: Arc::new(Mutex::new(statuses.into_iter().collect())), last_status }
        }
    }

    #[async_trait]
    impl ProverRequesterApiServer for MockRequesterApi {
        async fn prove_block_range(
            &self,
            request: ProveBlockRangeRequest,
        ) -> RpcResult<ProveBlockRangeResponse> {
            Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
        }

        async fn get_proof(&self, _request: GetProofRequest) -> RpcResult<GetProofResponse> {
            let status = self
                .statuses
                .lock()
                .expect("status lock should not be poisoned")
                .pop_front()
                .unwrap_or(self.last_status);
            Ok(GetProofResponse { status, error_message: None, result: None })
        }

        async fn delete_proof_request(&self, _request: DeleteProofRequest) -> RpcResult<()> {
            Err(ErrorObjectOwned::owned(
                ErrorCode::MethodNotFound.code(),
                "not used by tests",
                None::<()>,
            ))
        }

        async fn list_proofs(&self, _request: ListProofsRequest) -> RpcResult<ListProofsResponse> {
            Ok(ListProofsResponse { proofs: vec![], total_count: 0 })
        }
    }

    async fn spawn_mock(api: MockRequesterApi) -> (ProofsClient, ServerHandle) {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("test address should parse");
        let server = Server::builder().build(addr).await.expect("server should bind");
        let local_addr = server.local_addr().expect("server should have local address");
        let handle = server.start(api.into_rpc());
        let endpoint =
            Url::parse(&format!("http://{local_addr}")).expect("endpoint URL should parse");
        let client = ProofsClient::connect(&endpoint)
            .expect("client should connect")
            .with_wait_config(Duration::from_millis(5), Duration::from_millis(200));
        (client, handle)
    }

    async fn shutdown(handle: ServerHandle) {
        handle.stop().expect("server should stop");
        handle.stopped().await;
    }

    #[tokio::test]
    async fn submit_returns_accepted_session_id() {
        let api = MockRequesterApi::scripted([], ProofStatus::Queued);
        let (client, handle) = spawn_mock(api).await;

        let request = finalize_request();
        let session_id =
            client.submit(request.to_prove_request("devnet")).await.expect("submit should succeed");

        assert_eq!(session_id, finalize_request().effective_session_id("devnet"));
        shutdown(handle).await;
    }

    #[tokio::test]
    async fn wait_polls_until_terminal_status() {
        let api = MockRequesterApi::scripted(
            [ProofStatus::Queued, ProofStatus::Running],
            ProofStatus::Succeeded,
        );
        let (client, handle) = spawn_mock(api).await;

        let response = client
            .wait_for_completion("session-wait")
            .await
            .expect("wait should reach terminal status");

        assert_eq!(response.status, ProofStatus::Succeeded);
        shutdown(handle).await;
    }

    #[tokio::test]
    async fn wait_polls_at_deadline_when_max_wait_is_shorter_than_poll_interval() {
        let api = MockRequesterApi::scripted([ProofStatus::Running], ProofStatus::Succeeded);
        let (client, handle) = spawn_mock(api).await;
        let client = client.with_wait_config(Duration::from_millis(100), Duration::from_millis(10));

        let response = tokio::time::timeout(
            Duration::from_millis(50),
            client.wait_for_completion("session-short-wait"),
        )
        .await
        .expect("wait should clamp the poll interval to the deadline")
        .expect("wait should make a final poll at the deadline");

        assert_eq!(response.status, ProofStatus::Succeeded);
        shutdown(handle).await;
    }

    #[tokio::test]
    async fn wait_times_out_on_non_terminal_status() {
        let api = MockRequesterApi::scripted([], ProofStatus::Running);
        let (client, handle) = spawn_mock(api).await;
        let client = client.with_wait_config(Duration::from_millis(5), Duration::from_millis(20));

        let err =
            client.wait_for_completion("session-timeout").await.expect_err("wait should time out");

        match err {
            ProofsCommandError::WaitTimeout { session_id, last_status, .. } => {
                assert_eq!(session_id, "session-timeout");
                assert_eq!(last_status, "running");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
        shutdown(handle).await;
    }
}
