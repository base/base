use alloy_primitives::{B256, Bytes, U256};
use base_proof_primitives::{ProofBundle, ProofClaim, ProofEvidence, ProofResult, Proposal};

use crate::{ProofTransport, test_utils::NativeTransport};

fn test_proposal() -> Proposal {
    Proposal {
        output_root: B256::ZERO,
        signature: Bytes::new(),
        l1_origin_hash: B256::ZERO,
        l1_origin_number: U256::from(100),
        l2_block_number: U256::from(42),
        prev_output_root: B256::ZERO,
        config_hash: B256::ZERO,
    }
}

fn test_bundle() -> ProofBundle {
    ProofBundle { request: Default::default(), preimages: vec![] }
}

fn test_result() -> ProofResult {
    ProofResult {
        claim: ProofClaim { aggregate_proposal: test_proposal(), proposals: vec![test_proposal()] },
        evidence: ProofEvidence::Tee { attestation_doc: vec![1, 2, 3], signature: vec![4, 5, 6] },
    }
}

#[tokio::test]
async fn roundtrip() {
    let expected = test_result();
    let transport = NativeTransport::new(|_| test_result());

    let result = transport.prove(&test_bundle()).await.unwrap();
    assert_eq!(result, expected);
}

#[tokio::test]
async fn handler_receives_bundle() {
    let transport = NativeTransport::new(|bundle| {
        assert!(bundle.preimages.is_empty());
        test_result()
    });

    transport.prove(&test_bundle()).await.unwrap();
}

#[tokio::test]
async fn multiple_proves() {
    let transport = NativeTransport::new(|_| test_result());

    for _ in 0..5 {
        let result = transport.prove(&test_bundle()).await.unwrap();
        assert_eq!(result.claim.aggregate_proposal.l2_block_number, U256::from(42));
    }
}
