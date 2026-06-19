//! Adapters between proposer proof types and the shared prover-service protocol.

use std::{fmt, sync::Arc};

use alloy_primitives::B256;
use base_proof_primitives::{
    ProofRequest as PrimitiveProofRequest, ProofResult as PrimitiveProofResult,
};
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProofResult, ProofSessionId, ProveBlockRangeRequest, TeeKind,
    TeeProofRequest,
};
use tracing::debug;

use crate::ProposerError;

/// Proof request accepted by prover service for asynchronous collection.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DispatchedProof {
    /// Deterministic proof session identifier accepted by prover service.
    pub session_id: String,
}

/// Async proof dispatcher backed by the prover-service requester API.
#[derive(Clone)]
pub struct ProofRequesterDispatcher {
    requester: Arc<dyn ProofRequesterProvider>,
    tee_kind: TeeKind,
}

impl ProofRequesterDispatcher {
    /// Creates a dispatcher for AWS Nitro TEE proofs.
    pub fn aws_nitro(requester: Arc<dyn ProofRequesterProvider>) -> Self {
        Self { requester, tee_kind: TeeKind::AwsNitro }
    }

    /// Creates a dispatcher for the given TEE implementation.
    pub const fn new(requester: Arc<dyn ProofRequesterProvider>, tee_kind: TeeKind) -> Self {
        Self { requester, tee_kind }
    }

    /// Returns the TEE implementation used by this dispatcher.
    pub const fn tee_kind(&self) -> TeeKind {
        self.tee_kind
    }

    /// Submits a TEE proof request to prover service without waiting for completion.
    pub async fn dispatch_tee(
        &self,
        request: PrimitiveProofRequest,
    ) -> Result<DispatchedProof, ProposerError> {
        let request = ProposerProofAdapter::tee_prove_block_range_request(request, self.tee_kind);
        self.dispatch_prepared(request).await
    }

    /// Submits a TEE proof request under an explicit session id.
    ///
    /// Used for discard retries. The normal proposer session id is keyed only
    /// by claimed output root so restarts can rediscover in-flight work, but a
    /// discarded `Succeeded` session cannot be requeued by replaying that same
    /// id. A retry-specific id lets the proposer obtain a genuinely fresh TEE
    /// proof for the same output root.
    pub async fn dispatch_tee_with_session_id(
        &self,
        request: PrimitiveProofRequest,
        session_id: String,
    ) -> Result<DispatchedProof, ProposerError> {
        let request = ProposerProofAdapter::tee_prove_block_range_request_with_session_id(
            request,
            self.tee_kind,
            session_id,
        );
        self.dispatch_prepared(request).await
    }

    async fn dispatch_prepared(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<DispatchedProof, ProposerError> {
        let session_id = request.proof.session_id.clone();
        let response = match self.requester.prove_block_range(request).await {
            Ok(response) => response,
            Err(e) if e.is_l1_head_conflict_for_session(&session_id) => {
                debug!(
                    session_id = %session_id,
                    tee_kind = ?self.tee_kind,
                    "prover-service already has this TEE proof session with a different l1_head"
                );
                return Ok(DispatchedProof { session_id });
            }
            Err(e) => return Err(ProposerError::Prover(e.to_string())),
        };
        debug!(
            session_id = %response.session_id,
            tee_kind = ?self.tee_kind,
            "dispatched TEE proof request"
        );
        Ok(DispatchedProof { session_id: response.session_id })
    }
}

impl fmt::Debug for ProofRequesterDispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProofRequesterDispatcher")
            .field("tee_kind", &self.tee_kind)
            .finish_non_exhaustive()
    }
}

/// Conversion helpers for proposer proof requests and results.
#[derive(Debug)]
pub struct ProposerProofAdapter;

impl ProposerProofAdapter {
    /// Namespace used to derive proposer proof session IDs.
    pub const SESSION_NAMESPACE: &'static [u8] = b"base/proposer/proof-session/v1";

    /// Returns the session-ID proof subtype label for a TEE implementation.
    pub const fn tee_session_label(tee_kind: TeeKind) -> &'static str {
        match tee_kind {
            TeeKind::AwsNitro => "tee/aws_nitro",
        }
    }

    /// Derives an idempotent TEE proof session ID from proof subtype and claimed root.
    ///
    /// This intentionally follows the consolidation-plan derivation of
    /// `proof type + root`. Other request fields are excluded so redeploys or
    /// retries for the same proof identity re-use the same prover-service session.
    pub fn tee_session_id(request: &PrimitiveProofRequest, tee_kind: TeeKind) -> String {
        Self::tee_session_id_for_root(request.claimed_l2_output_root, tee_kind)
    }

    /// Derives an idempotent TEE proof session ID from proof subtype and claimed root.
    pub fn tee_session_id_for_root(root: B256, tee_kind: TeeKind) -> String {
        ProofSessionId::derive(Self::SESSION_NAMESPACE, Self::tee_session_label(tee_kind), root)
    }

    /// Derives a TEE proof retry session id for a discarded proof.
    pub fn tee_discard_retry_session_id(
        request: &PrimitiveProofRequest,
        tee_kind: TeeKind,
        attempt: u32,
    ) -> String {
        let label = Self::tee_session_label(tee_kind);
        let l1_head_number = request.l1_head_number.to_be_bytes();
        let attempt = attempt.to_be_bytes();
        ProofSessionId::derive_from_components(
            Self::SESSION_NAMESPACE,
            label,
            &[request.claimed_l2_output_root.as_slice(), &l1_head_number, &attempt],
        )
    }

    /// Builds a prover-service request for a TEE proposal proof.
    pub fn tee_prove_block_range_request(
        request: PrimitiveProofRequest,
        tee_kind: TeeKind,
    ) -> ProveBlockRangeRequest {
        let session_id = Self::tee_session_id(&request, tee_kind);
        Self::tee_prove_block_range_request_with_session_id(request, tee_kind, session_id)
    }

    /// Builds a prover-service request for a TEE proposal proof with a caller-supplied session id.
    pub const fn tee_prove_block_range_request_with_session_id(
        request: PrimitiveProofRequest,
        tee_kind: TeeKind,
        session_id: String,
    ) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id,
                request: ProofRequestKind::Tee(TeeProofRequest { proof: request, tee_kind }),
            },
        }
    }

    /// Converts a prover-service TEE proof result into the proposer proof result type.
    pub fn tee_proof_result(
        result: ProofResult,
        expected_tee_kind: TeeKind,
    ) -> Result<PrimitiveProofResult, ProposerError> {
        match result {
            ProofResult::Tee(result) => {
                let actual_tee_kind = result.tee_kind;
                if actual_tee_kind != expected_tee_kind {
                    return Err(ProposerError::Prover(format!(
                        "expected TEE proof result from {expected_tee_kind:?}, got {actual_tee_kind:?}"
                    )));
                }

                Ok(PrimitiveProofResult::Tee {
                    aggregate_proposal: result.aggregate_proposal,
                    proposals: result.proposals,
                })
            }
            ProofResult::Compressed(_) => {
                Err(ProposerError::Prover("expected TEE proof result, got Compressed".to_owned()))
            }
            ProofResult::SnarkGroth16(_) => {
                Err(ProposerError::Prover("expected TEE proof result, got SnarkGroth16".to_owned()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_primitives::{Address, B256, Bytes};
    use async_trait::async_trait;
    use base_proof_primitives::Proposal;
    use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
    use base_prover_service_protocol::{
        GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse, ProofRequestKind,
        ProofResult, ProveBlockRangeRequest, ProveBlockRangeResponse, TeeKind, TeeProofResult,
    };
    use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorObjectOwned};

    use super::{ProofRequesterDispatcher, ProposerProofAdapter};

    #[derive(Debug, Default)]
    struct MockProofRequester {
        prove_requests: Mutex<Vec<ProveBlockRangeRequest>>,
        get_requests: Mutex<Vec<GetProofRequest>>,
    }

    #[async_trait]
    impl ProofRequesterProvider for MockProofRequester {
        async fn prove_block_range(
            &self,
            request: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            let session_id = request.proof.session_id.clone();
            self.prove_requests.lock().unwrap().push(request);
            Ok(ProveBlockRangeResponse { session_id })
        }

        async fn get_proof(
            &self,
            _request: GetProofRequest,
        ) -> Result<GetProofResponse, ProverServiceClientError> {
            unimplemented!("tests do not poll proofs")
        }

        async fn list_proofs(
            &self,
            _request: ListProofsRequest,
        ) -> Result<ListProofsResponse, ProverServiceClientError> {
            unimplemented!("tests do not list proofs")
        }
    }

    #[derive(Debug)]
    struct L1HeadConflictRequester;

    #[async_trait]
    impl ProofRequesterProvider for L1HeadConflictRequester {
        async fn prove_block_range(
            &self,
            request: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            let session_id = request.proof.session_id;
            Err(ProverServiceClientError::from(JsonRpcClientError::Call(ErrorObjectOwned::owned(
                ProverServiceClientError::ERROR_FAILED_PRECONDITION,
                format!("session_id {session_id} already exists with a different l1_head"),
                None::<()>,
            ))))
        }

        async fn get_proof(
            &self,
            _request: GetProofRequest,
        ) -> Result<GetProofResponse, ProverServiceClientError> {
            unimplemented!("tests do not poll proofs")
        }

        async fn list_proofs(
            &self,
            _request: ListProofsRequest,
        ) -> Result<ListProofsResponse, ProverServiceClientError> {
            unimplemented!("tests do not list proofs")
        }
    }

    fn test_request(root: B256) -> base_proof_primitives::ProofRequest {
        base_proof_primitives::ProofRequest {
            l1_head: B256::repeat_byte(0x01),
            agreed_l2_head_hash: B256::repeat_byte(0x02),
            agreed_l2_output_root: B256::repeat_byte(0x03),
            claimed_l2_output_root: root,
            claimed_l2_block_number: 600,
            proposer: Address::repeat_byte(0x04),
            intermediate_block_interval: 300,
            l1_head_number: 1200,
            image_hash: B256::repeat_byte(0x05),
        }
    }

    fn test_proposal(block_number: u64) -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(block_number as u8),
            signature: Bytes::from(vec![0xab; 65]),
            l1_origin_hash: B256::repeat_byte(0x06),
            l1_origin_number: 100 + block_number,
            l2_block_number: block_number,
            prev_output_root: B256::repeat_byte(0x07),
            config_hash: B256::repeat_byte(0x08),
        }
    }

    #[test]
    fn tee_session_id_is_stable_for_same_root() {
        let request = test_request(B256::repeat_byte(0xaa));

        assert_eq!(
            ProposerProofAdapter::tee_session_id(&request, TeeKind::AwsNitro),
            ProposerProofAdapter::tee_session_id(&request, TeeKind::AwsNitro)
        );
    }

    #[test]
    fn tee_session_id_changes_for_different_roots() {
        let first = test_request(B256::repeat_byte(0xaa));
        let second = test_request(B256::repeat_byte(0xbb));

        assert_ne!(
            ProposerProofAdapter::tee_session_id(&first, TeeKind::AwsNitro),
            ProposerProofAdapter::tee_session_id(&second, TeeKind::AwsNitro)
        );
    }

    #[test]
    fn tee_session_id_ignores_non_root_request_fields() {
        let root = B256::repeat_byte(0xaa);
        let first = test_request(root);
        let mut second = test_request(root);
        second.l1_head = B256::repeat_byte(0x10);
        second.agreed_l2_head_hash = B256::repeat_byte(0x11);
        second.agreed_l2_output_root = B256::repeat_byte(0x12);
        second.claimed_l2_block_number = 1200;
        second.proposer = Address::repeat_byte(0x13);
        second.intermediate_block_interval = 150;
        second.l1_head_number = 2400;
        second.image_hash = B256::repeat_byte(0x14);

        assert_eq!(
            ProposerProofAdapter::tee_session_id(&first, TeeKind::AwsNitro),
            ProposerProofAdapter::tee_session_id(&second, TeeKind::AwsNitro)
        );
    }

    #[test]
    fn tee_discard_retry_session_id_differs_from_root_session() {
        let request = test_request(B256::repeat_byte(0xaa));

        assert_ne!(
            ProposerProofAdapter::tee_discard_retry_session_id(&request, TeeKind::AwsNitro, 1),
            ProposerProofAdapter::tee_session_id(&request, TeeKind::AwsNitro),
        );
    }

    #[test]
    fn tee_discard_retry_session_id_changes_by_attempt() {
        let request = test_request(B256::repeat_byte(0xaa));

        assert_ne!(
            ProposerProofAdapter::tee_discard_retry_session_id(&request, TeeKind::AwsNitro, 1),
            ProposerProofAdapter::tee_discard_retry_session_id(&request, TeeKind::AwsNitro, 2),
        );
    }

    #[test]
    fn tee_discard_retry_session_id_changes_by_l1_head_number() {
        let first = test_request(B256::repeat_byte(0xaa));
        let mut second = first.clone();
        second.l1_head_number += 1;

        assert_ne!(
            ProposerProofAdapter::tee_discard_retry_session_id(&first, TeeKind::AwsNitro, 1),
            ProposerProofAdapter::tee_discard_retry_session_id(&second, TeeKind::AwsNitro, 1),
        );
    }

    #[test]
    fn tee_prove_block_range_request_wraps_primitive_request() {
        let request = test_request(B256::repeat_byte(0xaa));
        let expected_session_id = ProposerProofAdapter::tee_session_id(&request, TeeKind::AwsNitro);

        let wrapped =
            ProposerProofAdapter::tee_prove_block_range_request(request.clone(), TeeKind::AwsNitro);

        assert_eq!(wrapped.proof.session_id, expected_session_id);
        match wrapped.proof.request {
            ProofRequestKind::Tee(tee) => {
                assert_eq!(tee.proof, request);
                assert_eq!(tee.tee_kind, TeeKind::AwsNitro);
            }
            other => panic!("unexpected proof request kind: {other:?}"),
        }
    }

    #[test]
    fn tee_proof_result_converts_to_primitive_result() {
        let aggregate = test_proposal(600);
        let proposal = test_proposal(300);
        let result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: aggregate.clone(),
            proposals: vec![proposal.clone()],
            tee_kind: TeeKind::AwsNitro,
        });

        let converted = ProposerProofAdapter::tee_proof_result(result, TeeKind::AwsNitro).unwrap();

        assert_eq!(
            converted,
            base_proof_primitives::ProofResult::Tee {
                aggregate_proposal: aggregate,
                proposals: vec![proposal]
            }
        );
    }

    #[tokio::test]
    async fn proof_requester_dispatcher_submits_without_polling() {
        let requester = std::sync::Arc::new(MockProofRequester::default());
        let dispatcher = ProofRequesterDispatcher::aws_nitro(
            std::sync::Arc::clone(&requester) as std::sync::Arc<dyn ProofRequesterProvider>
        );
        let request = test_request(B256::repeat_byte(0xaa));
        let expected_session_id = ProposerProofAdapter::tee_session_id(&request, TeeKind::AwsNitro);

        let dispatched = dispatcher.dispatch_tee(request).await.unwrap();

        assert_eq!(dispatched.session_id, expected_session_id);
        assert_eq!(requester.prove_requests.lock().unwrap().len(), 1);
        assert!(requester.get_requests.lock().unwrap().is_empty());
    }

    /// Restart/idempotency: dispatching the same proof request twice — e.g. after
    /// a proposer restart re-discovers the same target — yields the same
    /// deterministic session id. The prover service is expected to dedupe the
    /// underlying session by id, but the proposer surfaces the same id either
    /// way so that a subsequent `get_proof` call lands on the existing session.
    #[tokio::test]
    async fn proof_requester_dispatcher_is_idempotent_for_same_request() {
        let requester = std::sync::Arc::new(MockProofRequester::default());
        let dispatcher = ProofRequesterDispatcher::aws_nitro(
            std::sync::Arc::clone(&requester) as std::sync::Arc<dyn ProofRequesterProvider>
        );
        let request = test_request(B256::repeat_byte(0xaa));
        let expected_session_id = ProposerProofAdapter::tee_session_id(&request, TeeKind::AwsNitro);

        let first = dispatcher.dispatch_tee(request.clone()).await.unwrap();
        let second = dispatcher.dispatch_tee(request).await.unwrap();

        assert_eq!(first.session_id, expected_session_id);
        assert_eq!(second.session_id, expected_session_id);
        let prove_requests = requester.prove_requests.lock().unwrap();
        assert_eq!(prove_requests.len(), 2);
        // Both calls carry the same session id and identical TEE proof payload.
        assert_eq!(prove_requests[0].proof.session_id, expected_session_id);
        assert_eq!(prove_requests[1].proof.session_id, expected_session_id);
    }

    #[tokio::test]
    async fn proof_requester_dispatcher_accepts_existing_l1_head_conflict() {
        let dispatcher =
            ProofRequesterDispatcher::aws_nitro(std::sync::Arc::new(L1HeadConflictRequester));
        let request = test_request(B256::repeat_byte(0xaa));
        let expected_session_id = ProposerProofAdapter::tee_session_id(&request, TeeKind::AwsNitro);

        let dispatched = dispatcher.dispatch_tee(request).await.unwrap();

        assert_eq!(dispatched.session_id, expected_session_id);
    }
}
