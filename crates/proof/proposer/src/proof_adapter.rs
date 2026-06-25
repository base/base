//! Adapters between proposer proof types and the shared prover-service protocol.

use alloy_primitives::B256;
use base_proof_primitives::{ProofRequest as PrimitiveProofRequest, Proposal};
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProofResult, ProofSessionId, ProveBlockRangeRequest, TeeKind,
    TeeProofRequest,
};

use crate::ProposerError;

/// Conversion helpers for proposer proof requests and results.
#[derive(Debug)]
pub struct ProposerProofAdapter;

impl ProposerProofAdapter {
    const SESSION_NAMESPACE: &'static [u8] = b"base/proposer/proof-session/v1";

    const TEE_SESSION_LABEL: &'static str = "tee/aws_nitro";

    /// Derives an idempotent TEE proof session ID from proof subtype and claimed root.
    pub fn tee_session_id_for_root(root: B256) -> String {
        ProofSessionId::derive(Self::SESSION_NAMESPACE, Self::TEE_SESSION_LABEL, root)
    }

    /// Builds a prover-service request for a TEE proposal proof.
    pub fn tee_prove_block_range_request(request: PrimitiveProofRequest) -> ProveBlockRangeRequest {
        let session_id = Self::tee_session_id_for_root(request.claimed_l2_output_root);
        Self::tee_prove_block_range_request_with_session_id(request, session_id)
    }

    /// Builds a prover-service request for a TEE proposal proof with a caller-supplied session id.
    pub const fn tee_prove_block_range_request_with_session_id(
        request: PrimitiveProofRequest,
        session_id: String,
    ) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id,
                request: ProofRequestKind::Tee(TeeProofRequest {
                    proof: request,
                    tee_kind: TeeKind::AwsNitro,
                }),
            },
        }
    }

    /// Converts a prover-service TEE proof result into proposal parts.
    pub fn tee_proof_result(
        result: ProofResult,
    ) -> Result<(Proposal, Vec<Proposal>), ProposerError> {
        let result = match result {
            ProofResult::Tee(result) => result,
            ProofResult::Compressed(_) => {
                return Err(ProposerError::Prover(
                    "expected TEE proof result, got Compressed".into(),
                ));
            }
            ProofResult::SnarkGroth16(_) => {
                return Err(ProposerError::Prover(
                    "expected TEE proof result, got SnarkGroth16".into(),
                ));
            }
        };
        if result.tee_kind != TeeKind::AwsNitro {
            return Err(ProposerError::Prover(format!(
                "expected TEE proof result from AwsNitro, got {:?}",
                result.tee_kind
            )));
        }

        Ok((result.aggregate_proposal, result.proposals))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use base_proof_primitives::Proposal;
    use base_prover_service_protocol::{
        ProofRequestKind, ProofResult, SnarkGroth16ProofResult, TeeKind, TeeProofResult,
        ZkProofResult, ZkVm,
    };

    use super::ProposerProofAdapter;
    use crate::ProposerError;

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
    fn tee_prove_block_range_request_wraps_primitive_request() {
        let root = B256::repeat_byte(0xaa);
        let request = test_request(root);
        let expected_session_id = ProposerProofAdapter::tee_session_id_for_root(root);

        let wrapped = ProposerProofAdapter::tee_prove_block_range_request(request.clone());

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
    fn tee_proof_result_converts_to_proposal_parts() {
        let aggregate = test_proposal(600);
        let proposal = test_proposal(300);
        let result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: aggregate.clone(),
            proposals: vec![proposal.clone()],
            tee_kind: TeeKind::AwsNitro,
        });

        let converted = ProposerProofAdapter::tee_proof_result(result).unwrap();

        assert_eq!(converted, (aggregate, vec![proposal]));
    }

    #[test]
    fn tee_proof_result_reports_wrong_result_variant() {
        for (result, expected) in [
            (
                ProofResult::Compressed(ZkProofResult {
                    zk_vm: ZkVm::Sp1,
                    proof: Bytes::from(vec![]),
                    execution_stats: None,
                }),
                "expected TEE proof result, got Compressed",
            ),
            (
                ProofResult::SnarkGroth16(SnarkGroth16ProofResult {
                    proof: ZkProofResult {
                        zk_vm: ZkVm::Sp1,
                        proof: Bytes::from(vec![]),
                        execution_stats: None,
                    },
                }),
                "expected TEE proof result, got SnarkGroth16",
            ),
        ] {
            let err = ProposerProofAdapter::tee_proof_result(result).unwrap_err();
            let ProposerError::Prover(message) = err else {
                panic!("unexpected error: {err:?}");
            };

            assert_eq!(message, expected);
        }
    }
}
