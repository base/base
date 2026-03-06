use alloy_primitives::B256;
use base_proof_primitives::{ProofBundle, ProofClaim, ProofEvidence, ProofResult};

use crate::{ProofTransport, test_utils::NativeTransport};

fn test_bundle() -> ProofBundle {
    ProofBundle { request: Default::default(), preimages: vec![] }
}

fn test_result() -> ProofResult {
    ProofResult {
        claim: ProofClaim { l2_block_number: 42, output_root: B256::ZERO, l1_head: B256::ZERO },
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
        assert_eq!(result.claim.l2_block_number, 42);
    }
}
