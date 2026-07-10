//! Adapters between proposer proof types and the shared prover-service protocol.

use alloy_primitives::B256;
use base_proof_primitives::ProofRequest as PrimitiveProofRequest;
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProofResult, ProofSessionId, ProveBlockRangeRequest, TeeKind,
    TeeProofRequest,
};

use crate::{ProposerError, TeeProof};

/// Conversion helpers for proposer proof requests and results.
#[derive(Debug)]
pub struct ProposerProofAdapter;

impl ProposerProofAdapter {
    const SESSION_NAMESPACE: &'static [u8] = b"base/proposer/proof-session/v1";

    /// Derives an idempotent TEE proof session ID from proof subtype and claimed root.
    pub fn tee_session_id_for_root(root: B256, tee_kind: TeeKind) -> String {
        let label = match tee_kind {
            TeeKind::AwsNitro => "tee/aws_nitro",
            TeeKind::IntelTdx => "tee/intel_tdx",
        };
        ProofSessionId::derive(Self::SESSION_NAMESPACE, label, root)
    }

    /// Builds a prover-service request for a TEE proposal proof.
    pub fn tee_prove_block_range_request(
        request: PrimitiveProofRequest,
        tee_kind: TeeKind,
    ) -> ProveBlockRangeRequest {
        let session_id = Self::tee_session_id_for_root(request.claimed_l2_output_root, tee_kind);
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id,
                request: ProofRequestKind::Tee(TeeProofRequest { proof: request, tee_kind }),
            },
        }
    }

    /// Converts a prover-service TEE proof result into a typed single-platform TEE proof.
    pub fn tee_proof(result: ProofResult, tee_kind: TeeKind) -> Result<TeeProof, ProposerError> {
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
        if result.tee_kind != tee_kind {
            return Err(ProposerError::Prover(format!(
                "expected TEE proof result from {tee_kind:?}, got {:?}",
                result.tee_kind,
            )));
        }

        Ok(TeeProof { aggregate_proposal: result.aggregate_proposal, proposals: result.proposals })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use base_prover_service_protocol::{
        ProofRequestKind, ProofResult, SnarkGroth16ProofResult, TeeKind, TeeProofResult,
        ZkProofResult, ZkVm,
    };

    use super::ProposerProofAdapter;
    use crate::{ProposerError, test_utils::test_proposal};

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

    #[test]
    fn tee_prove_block_range_request_wraps_primitive_request() {
        let root = B256::repeat_byte(0xaa);
        let request = test_request(root);
        let expected_session_id =
            ProposerProofAdapter::tee_session_id_for_root(root, TeeKind::IntelTdx);

        let wrapped =
            ProposerProofAdapter::tee_prove_block_range_request(request.clone(), TeeKind::IntelTdx);

        assert_eq!(wrapped.proof.session_id, expected_session_id);
        match wrapped.proof.request {
            ProofRequestKind::Tee(tee) => {
                assert_eq!(tee.proof, request);
                assert_eq!(tee.tee_kind, TeeKind::IntelTdx);
            }
            other => panic!("unexpected proof request kind: {other:?}"),
        }
    }

    #[test]
    fn tee_proof_converts_to_typed_proof() {
        let aggregate = test_proposal(600);
        let proposal = test_proposal(300);
        let result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: aggregate.clone(),
            proposals: vec![proposal.clone()],
            tee_kind: TeeKind::AwsNitro,
        });

        let proof = ProposerProofAdapter::tee_proof(result, TeeKind::AwsNitro).unwrap();

        assert_eq!(proof.aggregate_proposal, aggregate);
        assert_eq!(proof.proposals, vec![proposal]);
    }

    #[test]
    fn tee_proof_reports_wrong_result_variant() {
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
            let err = ProposerProofAdapter::tee_proof(result, TeeKind::AwsNitro).unwrap_err();
            let ProposerError::Prover(message) = err else {
                panic!("unexpected error: {err:?}");
            };

            assert_eq!(message, expected);
        }
    }
}
