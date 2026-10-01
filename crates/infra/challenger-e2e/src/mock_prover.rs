//! In-memory stand-in for the prover-service, so a run never touches the
//! shared prover or its SP1 cluster.
//!
//! The challenger under test and the driver's own staging step both talk to
//! the prover-service over JSON-RPC and only ever call `proveBlockRange` and
//! `getProof`. This serves exactly those, on loopback inside the pod, with the
//! request-level semantics of the real service: a session ID is an idempotency
//! key, re-sending it with a different request is rejected, and a failed
//! session is only requeued when the caller allows it.
//!
//! What it returns is a well-formed SP1 PLONK receipt with no real proof in it.
//! That is only sound because the fork's verifiers are replaced by
//! [`crate::mock_verifier`] for the same run; against a real verifier every
//! dispute built from it would revert `InvalidProof()`.
//!
//! TEE requests are refused with a non-retryable error. The challenger treats
//! that as "TEE unavailable" and falls back to ZK straight away, which is the
//! branch every zeronet run takes today: the throwaway challenger key is not a
//! registered TEE proposer, so its TEE disputes always revert. Serving TEE
//! results is a follow-up that also needs per-scenario control over which
//! branch the challenger takes.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy_primitives::{Bytes, hex};
use base_proof_submission::test_utils::SnarkReceiptFixture;
use base_prover_service_protocol::{
    DeleteProofRequest, DeleteProofsByTeeSignerRequest, GetProofRequest, GetProofResponse,
    ListProofsRequest, ListProofsResponse, ProofRequestKind, ProofResult, ProofStatus,
    ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiServer,
    SnarkPlonkProofResult, ZkProofResult, ZkVm,
};
use eyre::{Context, Result};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle},
    types::{ErrorObjectOwned, error::ErrorCode},
};
use tracing::info;
use url::Url;

/// `NOT_FOUND`, as the real prover-service and its client use it.
const ERROR_NOT_FOUND: i32 = -32004;

/// Leading four bytes of the mock receipt's PLONK verifying-key hash.
///
/// A real SP1 gateway routes on these; the mock verifier ignores them. ASCII
/// `MOCK`, so a proof that leaked onto a real chain is recognisable at a glance.
const MOCK_VKEY_PREFIX: [u8; 4] = *b"MOCK";

/// Body of the mock PLONK proof. Must be non-empty: the challenger's receipt
/// decoder rejects an empty proof as a mock.
const MOCK_PROOF: &[u8] = b"challenger-e2e-mock-proof";

/// One proof session, as the real service would store it.
#[derive(Debug, Clone)]
struct Session {
    request: ProofRequestKind,
    submitted_at: Instant,
}

/// Shared state behind the RPC handlers.
#[derive(Debug)]
struct MockProverService {
    sessions: Mutex<HashMap<String, Session>>,
    proving_time: Duration,
}

impl MockProverService {
    fn invalid_params(message: impl Into<String>) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), message.into(), None::<()>)
    }

    fn not_found(message: impl Into<String>) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(ERROR_NOT_FOUND, message.into(), None::<()>)
    }

    fn unsupported(method: &str) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(
            ErrorCode::MethodNotFound.code(),
            format!("{method} is not served by the challenger E2E mock prover"),
            None::<()>,
        )
    }

    /// The proof bytes every successful session returns.
    fn mock_snark_result() -> ProofResult {
        let encoded_proof = hex::encode(MOCK_PROOF);
        ProofResult::SnarkPlonk(SnarkPlonkProofResult {
            proof: ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: Bytes::from(SnarkReceiptFixture::plonk_receipt_bytes(
                    MOCK_VKEY_PREFIX,
                    &encoded_proof,
                )),
                execution_stats: None,
            },
        })
    }

    fn prove(&self, request: ProveBlockRangeRequest) -> RpcResult<ProveBlockRangeResponse> {
        let session_id = request.proof.session_id;
        let kind = request.proof.request;
        if matches!(kind, ProofRequestKind::Tee(_)) {
            info!(session_id = %session_id, "mock prover refused a TEE proof request");
            return Err(Self::invalid_params(
                "TEE proofs are not served by the challenger E2E mock prover",
            ));
        }

        let mut sessions = self.sessions.lock().expect("mock prover state poisoned");
        if let Some(existing) = sessions.get(&session_id) {
            // Idempotent on an identical request, as the real service is.
            if existing.request == kind {
                return Ok(ProveBlockRangeResponse { session_id });
            }
            return Err(Self::invalid_params(format!(
                "session_id {session_id} is already bound to a different request"
            )));
        }

        info!(
            session_id = %session_id,
            request = ?kind,
            proving_time = ?self.proving_time,
            "mock prover accepted a proof request"
        );
        sessions
            .insert(session_id.clone(), Session { request: kind, submitted_at: Instant::now() });
        Ok(ProveBlockRangeResponse { session_id })
    }

    fn status(&self, session_id: &str) -> RpcResult<GetProofResponse> {
        let sessions = self.sessions.lock().expect("mock prover state poisoned");
        let session = sessions
            .get(session_id)
            .ok_or_else(|| Self::not_found(format!("session_id {session_id} not found")))?;

        // Reported as running for a while so the callers' polling loops are
        // exercised rather than satisfied on the first look.
        if session.submitted_at.elapsed() < self.proving_time {
            return Ok(GetProofResponse {
                status: ProofStatus::Running,
                error_message: None,
                result: None,
            });
        }
        Ok(GetProofResponse {
            status: ProofStatus::Succeeded,
            error_message: None,
            result: Some(Self::mock_snark_result()),
        })
    }
}

/// RPC adapter. Kept separate so the state above stays plain, synchronous code.
#[derive(Debug, Clone)]
struct MockProverRpc(Arc<MockProverService>);

#[async_trait]
impl ProverRequesterApiServer for MockProverRpc {
    async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> RpcResult<ProveBlockRangeResponse> {
        self.0.prove(request)
    }

    async fn get_proof(&self, request: GetProofRequest) -> RpcResult<GetProofResponse> {
        self.0.status(&request.session_id)
    }

    async fn delete_proof_request(&self, request: DeleteProofRequest) -> RpcResult<()> {
        self.0.sessions.lock().expect("mock prover state poisoned").remove(&request.session_id);
        Ok(())
    }

    async fn delete_proofs_by_tee_signer(
        &self,
        _request: DeleteProofsByTeeSignerRequest,
    ) -> RpcResult<u64> {
        // No TEE proof is ever stored, so there is never anything to delete.
        Ok(0)
    }

    async fn list_proofs(&self, _request: ListProofsRequest) -> RpcResult<ListProofsResponse> {
        Err(MockProverService::unsupported("listProofs"))
    }
}

/// A running mock prover. Stops when dropped.
#[derive(Debug)]
pub struct MockProver {
    url: Url,
    handle: ServerHandle,
}

impl MockProver {
    /// Starts the mock on an ephemeral loopback port.
    ///
    /// Loopback is enough: the challenger runs as a sidecar in the same pod and
    /// so shares the network namespace.
    pub async fn start(proving_time: Duration) -> Result<Self> {
        let server = Server::builder()
            .build(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .context("failed to bind the mock prover")?;
        let address = server.local_addr().context("mock prover has no local address")?;
        let service =
            Arc::new(MockProverService { sessions: Mutex::new(HashMap::new()), proving_time });
        let handle = server.start(MockProverRpc(service).into_rpc());
        let url = Url::parse(&format!("http://{address}")).context("mock prover URL")?;
        info!(url = %url, proving_time = ?proving_time, "started the mock prover");
        Ok(Self { url, handle })
    }

    /// Endpoint to hand to the challenger and to the driver's staging step.
    pub const fn url(&self) -> &Url {
        &self.url
    }
}

impl Drop for MockProver {
    fn drop(&mut self) {
        // Already-stopped is the only error, and it means there is nothing to do.
        let _ = self.handle.stop();
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use base_proof_primitives::ProofRequest as TeeRequest;
    use base_proof_submission::SnarkReceiptEncoder;
    use base_prover_service_protocol::{
        ProofRequest, SnarkPlonkProofRequest, TeeKind, TeeProofRequest, ZkBackend, ZkProofRequest,
    };

    use super::*;

    fn service(proving_time: Duration) -> MockProverService {
        MockProverService { sessions: Mutex::new(HashMap::new()), proving_time }
    }

    fn snark(session_id: &str, start_block_number: u64) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: session_id.to_owned(),
                request: ProofRequestKind::SnarkPlonk(SnarkPlonkProofRequest {
                    proof: ZkProofRequest {
                        start_block_number,
                        number_of_blocks_to_prove: 30,
                        sequence_window: None,
                        l1_head: Some(B256::repeat_byte(0x11)),
                        intermediate_root_interval: Some(30),
                        schedule_l2_block_number: Some(start_block_number + 600),
                        zk_vm: ZkVm::Sp1,
                        zk_backend: ZkBackend::Cluster,
                    },
                    prover_address: Address::repeat_byte(0x22),
                }),
            },
            retry_failed: true,
        }
    }

    #[test]
    fn a_session_is_running_then_succeeds_with_a_decodable_receipt() {
        let mock = service(Duration::from_millis(50));
        mock.prove(snark("s", 100)).expect("accepted");

        assert_eq!(mock.status("s").expect("known").status, ProofStatus::Running);
        std::thread::sleep(Duration::from_millis(60));
        let done = mock.status("s").expect("known");
        assert_eq!(done.status, ProofStatus::Succeeded);

        // The bytes must make it through the exact encoder both the challenger
        // and the driver's staging step use, or every dispute dies client-side.
        let Some(ProofResult::SnarkPlonk(result)) = done.result else {
            panic!("expected a SNARK result");
        };
        let onchain = SnarkReceiptEncoder::encode_onchain_zk_proof(&result.proof.proof)
            .expect("mock receipt must encode like a real one");
        assert_eq!(onchain[0], 1, "ZK proof type");
        assert_eq!(&onchain[1..5], b"MOCK");
    }

    #[test]
    fn session_ids_are_idempotency_keys() {
        let mock = service(Duration::ZERO);
        mock.prove(snark("s", 100)).expect("first");
        mock.prove(snark("s", 100)).expect("identical resend is idempotent");
        let conflict = mock.prove(snark("s", 101)).expect_err("different request must conflict");
        assert_eq!(conflict.code(), ErrorCode::InvalidParams.code());
    }

    #[test]
    fn unknown_sessions_are_not_found() {
        let error = service(Duration::ZERO).status("missing").expect_err("unknown");
        assert_eq!(error.code(), ERROR_NOT_FOUND);
    }

    /// Non-retryable on purpose: the client retries only UNAVAILABLE and
    /// INTERNAL, so this reaches the challenger's ZK fallback immediately.
    #[test]
    fn tee_requests_are_refused_without_a_retryable_code() {
        let request = ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: "tee".to_owned(),
                request: ProofRequestKind::Tee(TeeProofRequest {
                    proof: TeeRequest {
                        l1_head: B256::repeat_byte(1),
                        agreed_l2_head_hash: B256::repeat_byte(2),
                        agreed_l2_output_root: B256::repeat_byte(3),
                        claimed_l2_output_root: B256::repeat_byte(4),
                        claimed_l2_block_number: 600,
                        proposer: Address::repeat_byte(5),
                        intermediate_block_interval: 30,
                        l1_head_number: 1200,
                        schedule_l2_block_number: None,
                    },
                    tee_kind: TeeKind::AwsNitro,
                }),
            },
            retry_failed: true,
        };
        let error = service(Duration::ZERO).prove(request).expect_err("TEE refused");
        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
    }

    /// End to end over HTTP, through the same client crate the challenger uses.
    #[tokio::test]
    async fn serves_the_real_client_over_loopback() {
        use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};

        let mock = MockProver::start(Duration::ZERO).await.expect("start");
        let client =
            ProofRequesterClient::connect(&ProverServiceClientConfig::new(mock.url().as_str()))
                .expect("client");

        let accepted = client.prove_block_range(snark("http", 100)).await.expect("prove");
        assert_eq!(accepted.session_id, "http");
        let proof =
            client.get_proof(GetProofRequest { session_id: "http".to_owned() }).await.expect("get");
        assert_eq!(proof.status, ProofStatus::Succeeded);
    }
}
