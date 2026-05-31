//! Adapters between challenger proof types and the shared prover-service protocol.

use alloy_primitives::{B256, Bytes};
use base_proof_primitives::{PROOF_TYPE_ZK, ProofEncoder, ProofRequest as PrimitiveProofRequest};
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProofResult, ProofSessionId, ProveBlockRangeRequest,
    SnarkGroth16ProofRequest, TeeKind, TeeProofRequest,
};
use eyre::{Result, WrapErr, bail};

/// Conversion helpers for challenger proof requests and dispute proof bytes.
#[derive(Debug)]
pub struct ChallengerProofAdapter;

impl ChallengerProofAdapter {
    /// Namespace used to derive challenger proof session IDs.
    pub const SESSION_NAMESPACE: &'static [u8] = b"base/challenger/proof-session/v1";

    /// Returns the session-ID proof subtype label for challenger SNARK proofs.
    pub const fn snark_groth16_session_label() -> &'static str {
        "zk/sp1/snark_groth16"
    }

    /// Returns the session-ID proof subtype label for a TEE implementation.
    pub const fn tee_session_label(tee_kind: TeeKind) -> &'static str {
        match tee_kind {
            TeeKind::AwsNitro => "tee/aws_nitro",
        }
    }

    /// Derives an idempotent challenger SNARK proof session ID.
    pub fn snark_groth16_session_id(disputed_root: B256) -> String {
        ProofSessionId::derive(
            Self::SESSION_NAMESPACE,
            Self::snark_groth16_session_label(),
            disputed_root,
        )
    }

    /// Derives an idempotent challenger TEE proof session ID.
    pub fn tee_session_id(disputed_root: B256, tee_kind: TeeKind) -> String {
        ProofSessionId::derive(
            Self::SESSION_NAMESPACE,
            Self::tee_session_label(tee_kind),
            disputed_root,
        )
    }

    /// Builds a prover-service request for a challenger SNARK proof.
    pub fn snark_groth16_prove_block_range_request(
        disputed_root: B256,
        request: SnarkGroth16ProofRequest,
    ) -> ProveBlockRangeRequest {
        let session_id = Self::snark_groth16_session_id(disputed_root);
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: Some(session_id),
                request: ProofRequestKind::SnarkGroth16(request),
            },
        }
    }

    /// Builds a prover-service request for a challenger TEE proof.
    pub fn tee_prove_block_range_request(
        request: PrimitiveProofRequest,
        tee_kind: TeeKind,
    ) -> ProveBlockRangeRequest {
        let session_id = Self::tee_session_id(request.claimed_l2_output_root, tee_kind);
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: Some(session_id),
                request: ProofRequestKind::Tee(TeeProofRequest { proof: request, tee_kind }),
            },
        }
    }

    /// Converts a prover-service SNARK result into bytes accepted by `submit_dispute`.
    pub fn snark_groth16_dispute_proof_bytes(result: ProofResult) -> Result<Bytes> {
        let proof = match result {
            ProofResult::SnarkGroth16(result) => result.proof.proof,
            ProofResult::Compressed(_) => {
                bail!("expected SNARK_GROTH16 proof result, got Compressed")
            }
            ProofResult::Tee(_) => {
                bail!("expected SNARK_GROTH16 proof result, got Tee")
            }
        };

        let mut raw = Vec::with_capacity(1 + proof.len());
        raw.push(PROOF_TYPE_ZK);
        raw.extend_from_slice(proof.as_ref());
        Ok(Bytes::from(raw))
    }

    /// Converts a prover-service TEE result into bytes accepted by `submit_dispute`.
    pub fn tee_dispute_proof_bytes(result: ProofResult, expected_root: B256) -> Result<Bytes> {
        let aggregate_proposal = match result {
            ProofResult::Tee(result) => result.aggregate_proposal,
            ProofResult::Compressed(_) => {
                bail!("expected TEE proof result, got Compressed")
            }
            ProofResult::SnarkGroth16(_) => {
                bail!("expected TEE proof result, got SnarkGroth16")
            }
        };

        if aggregate_proposal.output_root != expected_root {
            bail!(
                "TEE computed unexpected output root: expected {expected_root}, got {}",
                aggregate_proposal.output_root
            );
        }

        ProofEncoder::encode_dispute_proof_bytes(&aggregate_proposal.signature)
            .wrap_err("TEE proof encoding failed")
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use base_proof_primitives::{PROOF_TYPE_TEE, Proposal};
    use base_prover_service_protocol::{
        ProofRequestKind, ProofResult, SnarkGroth16ProofRequest, SnarkGroth16ProofResult, TeeKind,
        TeeProofResult, ZkProofRequest, ZkProofResult, ZkVm,
    };

    use super::ChallengerProofAdapter;

    fn test_primitive_request(root: B256) -> base_proof_primitives::ProofRequest {
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

    fn test_proposal(root: B256) -> Proposal {
        let mut signature = vec![0xab; 65];
        signature[64] = 0;
        Proposal {
            output_root: root,
            signature: Bytes::from(signature),
            l1_origin_hash: B256::repeat_byte(0x06),
            l1_origin_number: 1200,
            l2_block_number: 600,
            prev_output_root: B256::repeat_byte(0x07),
            config_hash: B256::repeat_byte(0x08),
        }
    }

    #[test]
    fn challenger_session_ids_are_stable_and_type_separated() {
        let root = B256::repeat_byte(0xaa);

        assert_eq!(
            ChallengerProofAdapter::snark_groth16_session_id(root),
            ChallengerProofAdapter::snark_groth16_session_id(root)
        );
        assert_ne!(
            ChallengerProofAdapter::snark_groth16_session_id(root),
            ChallengerProofAdapter::tee_session_id(root, TeeKind::AwsNitro)
        );
    }

    #[test]
    fn snark_groth16_prove_block_range_request_converts_zk_request() {
        let root = B256::repeat_byte(0xaa);
        let session_id = ChallengerProofAdapter::snark_groth16_session_id(root);
        let prover_address = Address::repeat_byte(0x11);
        let l1_head = B256::repeat_byte(0x22);
        let proof = ZkProofRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 300,
            sequence_window: Some(10),
            l1_head: Some(l1_head),
            intermediate_root_interval: Some(150),
            zk_vm: ZkVm::Sp1,
        };
        let request = SnarkGroth16ProofRequest { proof, prover_address };

        let wrapped =
            ChallengerProofAdapter::snark_groth16_prove_block_range_request(root, request);

        assert_eq!(wrapped.proof.session_id.as_deref(), Some(session_id.as_str()));
        match wrapped.proof.request {
            ProofRequestKind::SnarkGroth16(snark) => {
                assert_eq!(snark.prover_address, prover_address);
                assert_eq!(snark.proof.start_block_number, 100);
                assert_eq!(snark.proof.number_of_blocks_to_prove, 300);
                assert_eq!(snark.proof.sequence_window, Some(10));
                assert_eq!(snark.proof.l1_head, Some(l1_head));
                assert_eq!(snark.proof.intermediate_root_interval, Some(150));
                assert_eq!(snark.proof.zk_vm, ZkVm::Sp1);
            }
            other => panic!("unexpected proof request kind: {other:?}"),
        }
    }

    #[test]
    fn tee_prove_block_range_request_wraps_primitive_request() {
        let root = B256::repeat_byte(0xaa);
        let request = test_primitive_request(root);
        let session_id = ChallengerProofAdapter::tee_session_id(root, TeeKind::AwsNitro);

        let wrapped = ChallengerProofAdapter::tee_prove_block_range_request(
            request.clone(),
            TeeKind::AwsNitro,
        );

        assert_eq!(wrapped.proof.session_id.as_deref(), Some(session_id.as_str()));
        match wrapped.proof.request {
            ProofRequestKind::Tee(tee) => {
                assert_eq!(tee.proof, request);
                assert_eq!(tee.tee_kind, TeeKind::AwsNitro);
            }
            other => panic!("unexpected proof request kind: {other:?}"),
        }
    }

    #[test]
    fn snark_groth16_dispute_proof_bytes_prefixes_zk_type() {
        let result = ProofResult::SnarkGroth16(SnarkGroth16ProofResult {
            proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: Bytes::from_static(&[0xab, 0xcd]) },
        });

        let proof_bytes =
            ChallengerProofAdapter::snark_groth16_dispute_proof_bytes(result).unwrap();

        assert_eq!(proof_bytes.as_ref(), &[1, 0xab, 0xcd]);
    }

    #[test]
    fn tee_dispute_proof_bytes_encodes_signature() {
        let root = B256::repeat_byte(0xaa);
        let result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: test_proposal(root),
            proposals: Vec::new(),
            tee_kind: TeeKind::AwsNitro,
        });

        let proof_bytes = ChallengerProofAdapter::tee_dispute_proof_bytes(result, root).unwrap();

        assert_eq!(proof_bytes[0], PROOF_TYPE_TEE);
        assert_eq!(proof_bytes.len(), 66);
    }

    #[test]
    fn tee_dispute_proof_bytes_rejects_unexpected_root() {
        let result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: test_proposal(B256::repeat_byte(0xaa)),
            proposals: Vec::new(),
            tee_kind: TeeKind::AwsNitro,
        });

        let err = ChallengerProofAdapter::tee_dispute_proof_bytes(result, B256::repeat_byte(0xbb))
            .expect_err("root mismatch should be rejected");

        assert!(err.to_string().contains("unexpected output root"));
    }
}
