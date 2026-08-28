//! Adapters between proposer proof types and the shared prover-service protocol.

use alloy_primitives::B256;
use base_proof_primitives::ProofRequest as PrimitiveProofRequest;
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProofResult, ProofSessionId, ProveBlockRangeRequest, TeeKind,
    TeeProofRequest, TeeProofResult,
};

use crate::ProposerError;

/// Conversion helpers for proposer proof requests and results.
#[derive(Debug)]
pub struct ProposerProofAdapter;

impl ProposerProofAdapter {
    const SESSION_NAMESPACE: &'static [u8] = b"base/proposer/proof-session/v2";

    const TEE_SESSION_LABEL: &'static str = "tee/aws_nitro";

    /// Derives an idempotent TEE proof session ID from the image hash and claimed root.
    pub fn tee_session_id_for_root(image_hash: B256, root: B256) -> String {
        ProofSessionId::derive_from_components(
            Self::SESSION_NAMESPACE,
            Self::TEE_SESSION_LABEL,
            &[image_hash.as_slice(), root.as_slice()],
        )
    }

    /// Builds a prover-service request for a TEE proposal proof.
    pub fn tee_prove_block_range_request(request: PrimitiveProofRequest) -> ProveBlockRangeRequest {
        let session_id =
            Self::tee_session_id_for_root(request.image_hash, request.claimed_l2_output_root);
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
            retry_failed: true,
        }
    }

    /// Converts a prover-service TEE proof result into proposal parts.
    pub fn tee_proof_result(result: ProofResult) -> Result<TeeProofResult, ProposerError> {
        let result = match result {
            ProofResult::Tee(result) => result,
            ProofResult::Compressed(_) => {
                return Err(ProposerError::Prover(
                    "expected TEE proof result, got Compressed".into(),
                ));
            }
            ProofResult::SnarkPlonk(_) => {
                return Err(ProposerError::Prover(
                    "expected TEE proof result, got SnarkPlonk".into(),
                ));
            }
        };
        if result.tee_kind != TeeKind::AwsNitro {
            return Err(ProposerError::Prover(format!(
                "expected TEE proof result from AwsNitro, got {:?}",
                result.tee_kind
            )));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use base_prover_service_protocol::{
        ProofRequestKind, ProofResult, SnarkPlonkProofResult, TeeKind, TeeProofResult,
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
            image_hash: B256::repeat_byte(0x06),
            schedule_l2_block_number: None,
        }
    }

    #[test]
    fn tee_prove_block_range_request_wraps_primitive_request() {
        let root = B256::repeat_byte(0xaa);
        let request = test_request(root);
        let expected_session_id =
            ProposerProofAdapter::tee_session_id_for_root(request.image_hash, root);

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
    fn tee_session_id_changes_with_image_hash() {
        let root = B256::repeat_byte(0xaa);
        assert_ne!(
            ProposerProofAdapter::tee_session_id_for_root(B256::repeat_byte(1), root),
            ProposerProofAdapter::tee_session_id_for_root(B256::repeat_byte(2), root),
        );
    }

    #[test]
    fn tee_proof_result_converts_to_proposal_parts() {
        let aggregate = test_proposal(600);
        let proposal = test_proposal(300);
        let result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: aggregate.clone(),
            proposals: vec![proposal.clone()],
            tee_kind: TeeKind::AwsNitro,
            tee_signer: Address::repeat_byte(0x11),
        });

        let converted = ProposerProofAdapter::tee_proof_result(result).unwrap();

        assert_eq!(converted.aggregate_proposal, aggregate);
        assert_eq!(converted.proposals, vec![proposal]);
        assert_eq!(converted.tee_signer, Address::repeat_byte(0x11));
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
                ProofResult::SnarkPlonk(SnarkPlonkProofResult {
                    proof: ZkProofResult {
                        zk_vm: ZkVm::Sp1,
                        proof: Bytes::from(vec![]),
                        execution_stats: None,
                    },
                }),
                "expected TEE proof result, got SnarkPlonk",
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
