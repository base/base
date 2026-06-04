//! Adapters between proposer proof types and the shared prover-service protocol.

use std::{fmt, sync::Arc, time::Duration};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_proof_primitives::{
    ProofRequest as PrimitiveProofRequest, ProofResult as PrimitiveProofResult, ProverClient,
};
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{
    GetProofRequest, ProofRequest, ProofRequestKind, ProofResult, ProofSessionId, ProofStatus,
    ProveBlockRangeRequest, TeeKind, TeeProofRequest,
};
use tracing::debug;

use crate::ProposerError;

/// [`ProverClient`] implementation backed by the prover-service requester API.
#[derive(Clone)]
pub struct ProofRequesterProver {
    requester: Arc<dyn ProofRequesterProvider>,
    tee_kind: TeeKind,
    poll_interval: Duration,
    max_wait: Duration,
}

impl ProofRequesterProver {
    /// Creates a prover adapter for AWS Nitro TEE proofs.
    pub fn aws_nitro(
        requester: Arc<dyn ProofRequesterProvider>,
        poll_interval: Duration,
        max_wait: Duration,
    ) -> Self {
        Self { requester, tee_kind: TeeKind::AwsNitro, poll_interval, max_wait }
    }

    /// Creates a prover adapter for the given TEE implementation.
    pub const fn new(
        requester: Arc<dyn ProofRequesterProvider>,
        tee_kind: TeeKind,
        poll_interval: Duration,
        max_wait: Duration,
    ) -> Self {
        Self { requester, tee_kind, poll_interval, max_wait }
    }
}

impl fmt::Debug for ProofRequesterProver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProofRequesterProver")
            .field("tee_kind", &self.tee_kind)
            .field("poll_interval", &self.poll_interval)
            .field("max_wait", &self.max_wait)
            .finish_non_exhaustive()
    }
}

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

    /// Submits a TEE proof request to prover service without waiting for completion.
    pub async fn dispatch_tee(
        &self,
        request: PrimitiveProofRequest,
    ) -> Result<DispatchedProof, ProposerError> {
        let request = ProposerProofAdapter::tee_prove_block_range_request(request, self.tee_kind);
        let response = self
            .requester
            .prove_block_range(request)
            .await
            .map_err(|e| ProposerError::Prover(e.to_string()))?;
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

#[async_trait]
impl ProverClient for ProofRequesterProver {
    async fn prove(
        &self,
        request: PrimitiveProofRequest,
    ) -> Result<PrimitiveProofResult, Box<dyn std::error::Error + Send + Sync>> {
        let request = ProposerProofAdapter::tee_prove_block_range_request(request, self.tee_kind);
        let session_id = self.requester.prove_block_range(request).await?.session_id;
        let started_at = tokio::time::Instant::now();

        loop {
            if started_at.elapsed() >= self.max_wait {
                return Err(Box::new(ProposerError::Prover(format!(
                    "timed out waiting for TEE proof session {session_id} after {:?}",
                    self.max_wait
                ))));
            }

            let response = self
                .requester
                .get_proof(GetProofRequest { session_id: session_id.clone() })
                .await?;

            match response.status {
                ProofStatus::Succeeded => {
                    let result = response.result.ok_or_else(|| {
                        ProposerError::Prover(format!(
                            "TEE proof session {session_id} succeeded without a result"
                        ))
                    })?;
                    return Ok(ProposerProofAdapter::tee_proof_result(result, self.tee_kind)?);
                }
                ProofStatus::Failed => {
                    let message = response.error_message.unwrap_or_else(|| {
                        format!("TEE proof session {session_id} failed without an error message")
                    });
                    return Err(Box::new(ProposerError::Prover(message)));
                }
                ProofStatus::Queued | ProofStatus::Running => {
                    debug!(
                        session_id = %session_id,
                        status = ?response.status,
                        elapsed = ?started_at.elapsed(),
                        "waiting for TEE proof"
                    );
                }
            }

            tokio::time::sleep(self.poll_interval).await;
        }
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

    /// Builds a prover-service request for a TEE proposal proof.
    pub fn tee_prove_block_range_request(
        request: PrimitiveProofRequest,
        tee_kind: TeeKind,
    ) -> ProveBlockRangeRequest {
        let session_id = Self::tee_session_id(&request, tee_kind);
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: Some(session_id),
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
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    use alloy_primitives::{Address, B256, Bytes};
    use async_trait::async_trait;
    use base_proof_primitives::{Proposal, ProverClient};
    use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
    use base_prover_service_protocol::{
        GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse, ProofRequestKind,
        ProofResult, ProofStatus, ProveBlockRangeRequest, ProveBlockRangeResponse, TeeKind,
        TeeProofResult,
    };

    use super::{ProofRequesterDispatcher, ProofRequesterProver, ProposerProofAdapter};

    #[derive(Debug)]
    struct MockProofRequester {
        prove_requests: Mutex<Vec<ProveBlockRangeRequest>>,
        get_requests: Mutex<Vec<GetProofRequest>>,
        proof_responses: Mutex<VecDeque<GetProofResponse>>,
    }

    impl MockProofRequester {
        fn new(proof_responses: Vec<GetProofResponse>) -> Self {
            Self {
                prove_requests: Mutex::new(Vec::new()),
                get_requests: Mutex::new(Vec::new()),
                proof_responses: Mutex::new(proof_responses.into()),
            }
        }
    }

    #[async_trait]
    impl ProofRequesterProvider for MockProofRequester {
        async fn prove_block_range(
            &self,
            request: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            let session_id =
                request.proof.session_id.clone().expect("request should set session_id");
            self.prove_requests.lock().unwrap().push(request);
            Ok(ProveBlockRangeResponse { session_id })
        }

        async fn get_proof(
            &self,
            request: GetProofRequest,
        ) -> Result<GetProofResponse, ProverServiceClientError> {
            self.get_requests.lock().unwrap().push(request);
            Ok(self.proof_responses.lock().unwrap().pop_front().expect("missing proof response"))
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
    fn tee_prove_block_range_request_wraps_primitive_request() {
        let request = test_request(B256::repeat_byte(0xaa));
        let expected_session_id = ProposerProofAdapter::tee_session_id(&request, TeeKind::AwsNitro);

        let wrapped =
            ProposerProofAdapter::tee_prove_block_range_request(request.clone(), TeeKind::AwsNitro);

        assert_eq!(wrapped.proof.session_id.as_deref(), Some(expected_session_id.as_str()));
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
    async fn proof_requester_prover_submits_and_polls_tee_result() {
        let aggregate = test_proposal(600);
        let proposal = test_proposal(300);
        let response = GetProofResponse {
            status: ProofStatus::Succeeded,
            error_message: None,
            result: Some(ProofResult::Tee(TeeProofResult {
                aggregate_proposal: aggregate.clone(),
                proposals: vec![proposal.clone()],
                tee_kind: TeeKind::AwsNitro,
            })),
        };
        let requester = std::sync::Arc::new(MockProofRequester::new(vec![response]));
        let prover = ProofRequesterProver::aws_nitro(
            std::sync::Arc::clone(&requester) as std::sync::Arc<dyn ProofRequesterProvider>,
            Duration::from_millis(1),
            Duration::from_secs(1),
        );

        let result = prover.prove(test_request(B256::repeat_byte(0xaa))).await.unwrap();

        assert_eq!(
            result,
            base_proof_primitives::ProofResult::Tee {
                aggregate_proposal: aggregate,
                proposals: vec![proposal]
            }
        );
        assert_eq!(requester.prove_requests.lock().unwrap().len(), 1);
        assert_eq!(requester.get_requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn proof_requester_prover_returns_failed_proof_error() {
        let response = GetProofResponse {
            status: ProofStatus::Failed,
            error_message: Some("boom".to_owned()),
            result: None,
        };
        let requester = std::sync::Arc::new(MockProofRequester::new(vec![response]));
        let prover = ProofRequesterProver::aws_nitro(
            requester,
            Duration::from_millis(1),
            Duration::from_secs(1),
        );

        let err = prover
            .prove(test_request(B256::repeat_byte(0xaa)))
            .await
            .expect_err("failed proof should return an error");

        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn proof_requester_dispatcher_submits_without_polling() {
        let requester = std::sync::Arc::new(MockProofRequester::new(Vec::new()));
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

    #[tokio::test]
    async fn proof_requester_prover_times_out_waiting_for_terminal_status() {
        let response =
            GetProofResponse { status: ProofStatus::Running, error_message: None, result: None };
        let requester = std::sync::Arc::new(MockProofRequester::new(vec![response]));
        let prover =
            ProofRequesterProver::aws_nitro(requester, Duration::from_millis(1), Duration::ZERO);

        let err = prover
            .prove(test_request(B256::repeat_byte(0xaa)))
            .await
            .expect_err("non-terminal proof should time out");

        assert!(err.to_string().contains("timed out waiting for TEE proof session"));
    }
}
