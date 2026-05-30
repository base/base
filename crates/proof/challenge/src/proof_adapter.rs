//! Adapters between challenger proof types and the shared prover-service protocol.

use alloy_primitives::{Address, B256, Bytes};
use base_proof_primitives::{PROOF_TYPE_ZK, ProofEncoder, ProofRequest as PrimitiveProofRequest};
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProofResult, ProveBlockRangeRequest, SnarkGroth16ProofRequest,
    TeeKind, TeeProofRequest, ZkProofRequest, ZkVm,
};
use base_zk_client::{ProofType as ZkServiceProofType, ProveBlockRequest};
use eyre::{Result, WrapErr, bail, eyre};
use uuid::Uuid;

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

    /// Derives an idempotent proof session ID from proof subtype and disputed root.
    pub fn session_id(proof_subtype: &str, disputed_root: B256) -> String {
        let mut name = Vec::with_capacity(
            Self::SESSION_NAMESPACE.len() + proof_subtype.len() + disputed_root.len(),
        );
        name.extend_from_slice(Self::SESSION_NAMESPACE);
        name.extend_from_slice(proof_subtype.as_bytes());
        name.extend_from_slice(disputed_root.as_slice());

        Uuid::new_v5(&Uuid::NAMESPACE_OID, &name).to_string()
    }

    /// Derives an idempotent challenger SNARK proof session ID.
    pub fn snark_groth16_session_id(disputed_root: B256) -> String {
        Self::session_id(Self::snark_groth16_session_label(), disputed_root)
    }

    /// Derives an idempotent challenger TEE proof session ID.
    pub fn tee_session_id(disputed_root: B256, tee_kind: TeeKind) -> String {
        Self::session_id(Self::tee_session_label(tee_kind), disputed_root)
    }

    /// Builds a prover-service request for a challenger SNARK proof.
    pub fn snark_groth16_prove_block_range_request(
        request: ProveBlockRequest,
    ) -> Result<ProveBlockRangeRequest> {
        let expected_proof_type: i32 = ZkServiceProofType::SnarkGroth16.into();
        if request.proof_type != expected_proof_type {
            bail!(
                "expected SNARK_GROTH16 proof_type {}, got {}",
                expected_proof_type,
                request.proof_type
            );
        }

        let l1_head = request
            .l1_head
            .as_deref()
            .map(str::parse::<B256>)
            .transpose()
            .wrap_err("l1_head must be a 0x-prefixed 32-byte hash")?;
        let prover_address = request
            .prover_address
            .as_deref()
            .ok_or_else(|| eyre!("prover_address is required for SNARK_GROTH16 proofs"))?
            .parse::<Address>()
            .wrap_err("prover_address must be a valid Ethereum address")?;
        let proof = ZkProofRequest {
            start_block_number: request.start_block_number,
            number_of_blocks_to_prove: request.number_of_blocks_to_prove,
            sequence_window: request.sequence_window,
            l1_head,
            intermediate_root_interval: request.intermediate_root_interval,
            zk_vm: ZkVm::Sp1,
        };

        Ok(ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: request.session_id,
                request: ProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
                    proof,
                    prover_address,
                }),
            },
        })
    }

    /// Builds a prover-service request for a challenger TEE proof.
    pub const fn tee_prove_block_range_request(
        session_id: String,
        request: PrimitiveProofRequest,
        tee_kind: TeeKind,
    ) -> ProveBlockRangeRequest {
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
            other => bail!("expected SNARK_GROTH16 proof result, got {other:?}"),
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
            other => bail!("expected TEE proof result, got {other:?}"),
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
        ProofRequestKind, ProofResult, SnarkGroth16ProofResult, TeeKind, TeeProofResult,
        ZkProofResult, ZkVm,
    };
    use base_zk_client::{ProofType as ZkServiceProofType, ProveBlockRequest};

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
        let session_id = "session-zk".to_owned();
        let prover_address = Address::repeat_byte(0x11);
        let l1_head = B256::repeat_byte(0x22);
        let request = ProveBlockRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 300,
            sequence_window: Some(10),
            proof_type: ZkServiceProofType::SnarkGroth16.into(),
            session_id: Some(session_id.clone()),
            prover_address: Some(format!("{prover_address:#x}")),
            l1_head: Some(format!("{l1_head:#x}")),
            intermediate_root_interval: Some(150),
        };

        let wrapped =
            ChallengerProofAdapter::snark_groth16_prove_block_range_request(request).unwrap();

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
    fn snark_groth16_prove_block_range_request_rejects_non_snark_type() {
        let request = ProveBlockRequest {
            proof_type: ZkServiceProofType::Compressed.into(),
            prover_address: Some(format!("{:#x}", Address::repeat_byte(0x11))),
            ..Default::default()
        };

        let err = ChallengerProofAdapter::snark_groth16_prove_block_range_request(request)
            .expect_err("compressed proof type should be rejected");

        assert!(err.to_string().contains("SNARK_GROTH16"));
    }

    #[test]
    fn tee_prove_block_range_request_wraps_primitive_request() {
        let root = B256::repeat_byte(0xaa);
        let request = test_primitive_request(root);
        let session_id = ChallengerProofAdapter::tee_session_id(root, TeeKind::AwsNitro);

        let wrapped = ChallengerProofAdapter::tee_prove_block_range_request(
            session_id.clone(),
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
