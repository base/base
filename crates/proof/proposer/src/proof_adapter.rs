//! Adapters between proposer proof types and the shared prover-service protocol.

use base_proof_primitives::{
    ProofRequest as PrimitiveProofRequest, ProofResult as PrimitiveProofResult,
};
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProofResult, ProveBlockRangeRequest, TeeKind, TeeProofRequest,
};
use uuid::Uuid;

use crate::ProposerError;

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
    pub fn tee_session_id(request: &PrimitiveProofRequest, tee_kind: TeeKind) -> String {
        let mut name = Vec::with_capacity(
            Self::SESSION_NAMESPACE.len()
                + Self::tee_session_label(tee_kind).len()
                + request.claimed_l2_output_root.len(),
        );
        name.extend_from_slice(Self::SESSION_NAMESPACE);
        name.extend_from_slice(Self::tee_session_label(tee_kind).as_bytes());
        name.extend_from_slice(request.claimed_l2_output_root.as_slice());

        Uuid::new_v5(&Uuid::NAMESPACE_OID, &name).to_string()
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
    pub fn tee_proof_result(result: ProofResult) -> Result<PrimitiveProofResult, ProposerError> {
        match result {
            ProofResult::Tee(result) => Ok(PrimitiveProofResult::Tee {
                aggregate_proposal: result.aggregate_proposal,
                proposals: result.proposals,
            }),
            other => {
                Err(ProposerError::Prover(format!("expected TEE proof result, got {other:?}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use base_proof_primitives::Proposal;
    use base_prover_service_protocol::{ProofRequestKind, ProofResult, TeeKind, TeeProofResult};

    use super::ProposerProofAdapter;

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

        let converted = ProposerProofAdapter::tee_proof_result(result).unwrap();

        assert_eq!(
            converted,
            base_proof_primitives::ProofResult::Tee {
                aggregate_proposal: aggregate,
                proposals: vec![proposal]
            }
        );
    }
}
