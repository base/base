use alloy_primitives::{Bytes, U256};
use base_proof_preimage::{PreimageKey, PreimageKeyType};
use base_proof_primitives::{ProofClaim, ProofEvidence, ProofResult, Proposal};

use crate::{ProofTransport, test_utils::NativeTransport};

fn test_proposal() -> Proposal {
    Proposal {
        output_root: Default::default(),
        signature: Bytes::from(vec![0u8; 65]),
        l1_origin_hash: Default::default(),
        l1_origin_number: U256::from(100),
        l2_block_number: U256::from(42),
        prev_output_root: Default::default(),
        config_hash: Default::default(),
    }
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
    let transport = NativeTransport::new(|_| Ok(test_result()));

    let result = transport.prove(&[]).await.unwrap();
    assert_eq!(result, expected);
}

#[tokio::test]
async fn handler_receives_preimages() {
    let key = PreimageKey::new([1u8; 32], PreimageKeyType::Local);
    let transport = NativeTransport::new(move |preimages: &[(PreimageKey, Vec<u8>)]| {
        assert_eq!(preimages.len(), 1);
        assert_eq!(preimages[0].0, PreimageKey::new([1u8; 32], PreimageKeyType::Local));
        Ok(test_result())
    });

    transport.prove(&[(key, b"value".to_vec())]).await.unwrap();
}

#[tokio::test]
async fn multiple_proves() {
    let transport = NativeTransport::new(|_| Ok(test_result()));

    for _ in 0..5 {
        let result = transport.prove(&[]).await.unwrap();
        assert_eq!(result.claim.aggregate_proposal.l2_block_number, U256::from(42));
    }
}
