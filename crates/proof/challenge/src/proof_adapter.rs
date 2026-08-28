//! Adapters between challenger proof types and the shared prover-service protocol.

use alloy_primitives::{Address, B256, Bytes};
use base_proof_primitives::{ProofEncoder, ProofRequest as PrimitiveProofRequest};
use base_proof_submission::SnarkReceiptEncoder;
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProofResult, ProofSessionId, ProveBlockRangeRequest,
    SnarkPlonkProofRequest, TeeKind, TeeProofRequest,
};
use eyre::{Result, WrapErr, bail};

/// Conversion helpers for challenger proof requests and dispute proof bytes.
#[derive(Debug)]
pub struct ChallengerProofAdapter;

impl ChallengerProofAdapter {
    /// Namespace used to derive challenger proof session IDs.
    const SESSION_NAMESPACE: &'static [u8] = b"base/challenger/proof-session/v2";

    /// Derives an idempotent challenger SNARK proof session ID.
    pub fn snark_plonk_session_id(game_address: Address, invalid_index: u64) -> String {
        let invalid_index = invalid_index.to_be_bytes();
        ProofSessionId::derive_from_components(
            Self::SESSION_NAMESPACE,
            "zk/sp1/snark_plonk",
            &[game_address.as_slice(), &invalid_index],
        )
    }

    /// Derives an idempotent challenger TEE proof session ID.
    pub fn tee_session_id(game_address: Address, invalid_index: u64) -> String {
        let invalid_index = invalid_index.to_be_bytes();
        ProofSessionId::derive_from_components(
            Self::SESSION_NAMESPACE,
            "tee/aws_nitro",
            &[game_address.as_slice(), &invalid_index],
        )
    }

    /// Builds a prover-service request for a challenger SNARK proof.
    pub fn snark_plonk_prove_block_range_request(
        game_address: Address,
        invalid_index: u64,
        request: SnarkPlonkProofRequest,
    ) -> ProveBlockRangeRequest {
        let session_id = Self::snark_plonk_session_id(game_address, invalid_index);
        ProveBlockRangeRequest {
            proof: ProofRequest { session_id, request: ProofRequestKind::SnarkPlonk(request) },
            retry_failed: true,
        }
    }

    /// Builds a prover-service request for a challenger TEE proof.
    pub fn tee_prove_block_range_request(
        game_address: Address,
        invalid_index: u64,
        request: PrimitiveProofRequest,
    ) -> ProveBlockRangeRequest {
        let session_id = Self::tee_session_id(game_address, invalid_index);
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

    /// Converts a prover-service SNARK result into bytes accepted by `submit_dispute`.
    pub fn snark_plonk_dispute_proof_bytes(result: ProofResult) -> Result<Bytes> {
        let receipt_bytes = match result {
            ProofResult::SnarkPlonk(result) => result.proof.proof,
            ProofResult::Compressed(_) => {
                bail!("expected SNARK_PLONK proof result, got Compressed")
            }
            ProofResult::Tee(_) => {
                bail!("expected SNARK_PLONK proof result, got Tee")
            }
        };

        SnarkReceiptEncoder::encode_onchain_zk_proof(&receipt_bytes)
            .wrap_err("failed to encode SP1 PLONK receipt into dispute proof bytes")
    }

    /// Converts a prover-service TEE result into bytes accepted by `submit_dispute`.
    pub fn tee_dispute_proof_bytes(result: ProofResult, expected_root: B256) -> Result<Bytes> {
        let aggregate_proposal = match result {
            ProofResult::Tee(result) => result.aggregate_proposal,
            ProofResult::Compressed(_) => {
                bail!("expected TEE proof result, got Compressed")
            }
            ProofResult::SnarkPlonk(_) => {
                bail!("expected TEE proof result, got SnarkPlonk")
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
    use base_proof_primitives::{PROOF_TYPE_TEE, PROOF_TYPE_ZK, ProofRequest, Proposal};
    use base_proof_submission::test_utils::SnarkReceiptFixture;
    use base_prover_service_protocol::{
        ProofRequestKind, ProofResult, SnarkPlonkProofRequest, SnarkPlonkProofResult, TeeKind,
        TeeProofRequest, TeeProofResult, ZkBackend, ZkProofRequest, ZkProofResult, ZkVm,
    };

    use super::ChallengerProofAdapter;

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
            schedule_id: B256::repeat_byte(0x09),
        }
    }

    #[test]
    fn challenger_session_ids_are_stable_and_type_separated() {
        let game_address = Address::repeat_byte(0xaa);
        let invalid_index = 1;

        assert_eq!(
            ChallengerProofAdapter::snark_plonk_session_id(game_address, invalid_index),
            ChallengerProofAdapter::snark_plonk_session_id(game_address, invalid_index)
        );
        assert_ne!(
            ChallengerProofAdapter::snark_plonk_session_id(game_address, invalid_index),
            ChallengerProofAdapter::tee_session_id(game_address, invalid_index)
        );
    }

    #[test]
    fn challenger_session_ids_separate_game_address_and_invalid_index() {
        let game_address = Address::repeat_byte(0xaa);

        assert_ne!(
            ChallengerProofAdapter::snark_plonk_session_id(game_address, 1),
            ChallengerProofAdapter::snark_plonk_session_id(game_address, 2)
        );
        assert_ne!(
            ChallengerProofAdapter::snark_plonk_session_id(game_address, 1),
            ChallengerProofAdapter::snark_plonk_session_id(Address::repeat_byte(0xbb), 1)
        );
    }

    #[test]
    fn snark_plonk_prove_block_range_request_converts_zk_request() {
        let game_address = Address::repeat_byte(0xaa);
        let invalid_index = 1;
        let session_id =
            ChallengerProofAdapter::snark_plonk_session_id(game_address, invalid_index);
        let prover_address = Address::repeat_byte(0x11);
        let l1_head = B256::repeat_byte(0x22);
        let proof = ZkProofRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 300,
            sequence_window: Some(10),
            l1_head: Some(l1_head),
            intermediate_root_interval: Some(150),
            schedule_l2_block_number: None,
            zk_artifact_hash: None,
            zk_vm: ZkVm::Sp1,
            zk_backend: ZkBackend::Cluster,
        };
        let request = SnarkPlonkProofRequest { proof, prover_address };

        let wrapped = ChallengerProofAdapter::snark_plonk_prove_block_range_request(
            game_address,
            invalid_index,
            request.clone(),
        );

        assert_eq!(wrapped.proof.session_id, session_id);
        assert_eq!(wrapped.proof.request, ProofRequestKind::SnarkPlonk(request));
    }

    #[test]
    fn tee_prove_block_range_request_wraps_primitive_request() {
        let root = B256::repeat_byte(0xaa);
        let game_address = Address::repeat_byte(0xaa);
        let invalid_index = 1;
        let request = ProofRequest {
            l1_head: B256::repeat_byte(0x01),
            agreed_l2_head_hash: B256::repeat_byte(0x02),
            agreed_l2_output_root: B256::repeat_byte(0x03),
            claimed_l2_output_root: root,
            claimed_l2_block_number: 600,
            proposer: Address::repeat_byte(0x04),
            intermediate_block_interval: 300,
            l1_head_number: 1200,
            image_hash: alloy_primitives::B256::ZERO,
            schedule_l2_block_number: None,
        };
        let session_id = ChallengerProofAdapter::tee_session_id(game_address, invalid_index);

        let wrapped = ChallengerProofAdapter::tee_prove_block_range_request(
            game_address,
            invalid_index,
            request.clone(),
        );

        assert_eq!(wrapped.proof.session_id, session_id);
        assert_eq!(
            wrapped.proof.request,
            ProofRequestKind::Tee(TeeProofRequest { proof: request, tee_kind: TeeKind::AwsNitro })
        );
    }

    #[test]
    fn snark_plonk_dispute_proof_bytes_decodes_receipt_to_onchain_seal() {
        let encoded = SnarkReceiptFixture::plonk_receipt_bytes([0x5a, 0x09, 0x3a, 0x2f], "abcd");
        let result = ProofResult::SnarkPlonk(SnarkPlonkProofResult {
            proof: ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: Bytes::from(encoded),
                execution_stats: None,
            },
        });

        let proof_bytes = ChallengerProofAdapter::snark_plonk_dispute_proof_bytes(result).unwrap();
        assert_eq!(proof_bytes.as_ref(), &[PROOF_TYPE_ZK, 0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);
    }

    #[test]
    fn tee_dispute_proof_bytes_encodes_signature() {
        let root = B256::repeat_byte(0xaa);
        let result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: test_proposal(root),
            proposals: Vec::new(),
            tee_kind: TeeKind::AwsNitro,
            tee_signer: Address::repeat_byte(0x11),
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
            tee_signer: Address::repeat_byte(0x11),
        });

        let err = ChallengerProofAdapter::tee_dispute_proof_bytes(result, B256::repeat_byte(0xbb))
            .expect_err("root mismatch should be rejected");

        assert!(err.to_string().contains("unexpected output root"));
    }
}
