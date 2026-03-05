use alloy_primitives::B256;
use base_proof_primitives::{ProofClaim, ProofEvidence, ProofResult, WitnessBundle};

use crate::{NativeTransport, TransportError, WitnessTransport};

fn test_bundle() -> WitnessBundle {
    WitnessBundle { preimages: vec![] }
}

fn test_result() -> ProofResult {
    ProofResult {
        claim: ProofClaim { l2_block_number: 42, output_root: B256::ZERO, l1_head: B256::ZERO },
        evidence: ProofEvidence::Tee { attestation_doc: vec![1, 2, 3], signature: vec![4, 5, 6] },
    }
}

#[tokio::test]
async fn roundtrip() {
    let (transport, backend) = NativeTransport::channel(1);
    let bundle = test_bundle();
    let result = test_result();

    transport.send_witness(&bundle).await.unwrap();

    let received_bundle = backend.recv_witness().await.unwrap();
    assert_eq!(received_bundle, bundle);

    backend.send_result(result.clone()).await.unwrap();

    let received_result = transport.recv_result().await.unwrap();
    assert_eq!(received_result, result);
}

#[tokio::test]
async fn recv_after_sender_dropped() {
    let (transport, backend) = NativeTransport::channel(1);

    drop(backend);

    let err = transport.recv_result().await.unwrap_err();
    assert!(matches!(err, TransportError::ChannelClosed));
}

#[tokio::test]
async fn send_after_receiver_dropped() {
    let (transport, backend) = NativeTransport::channel(1);
    let bundle = test_bundle();

    drop(backend);

    let err = transport.send_witness(&bundle).await.unwrap_err();
    assert!(matches!(err, TransportError::Send(_)));
}

#[tokio::test]
async fn backend_recv_after_transport_dropped() {
    let (transport, backend) = NativeTransport::channel(1);

    drop(transport);

    let err = backend.recv_witness().await.unwrap_err();
    assert!(matches!(err, TransportError::ChannelClosed));
}

#[tokio::test]
async fn backend_send_after_transport_dropped() {
    let (transport, backend) = NativeTransport::channel(1);
    let result = test_result();

    drop(transport);

    let err = backend.send_result(result).await.unwrap_err();
    assert!(matches!(err, TransportError::Send(_)));
}

#[tokio::test]
async fn multiple_bundles() {
    let (transport, backend) = NativeTransport::channel(8);

    for i in 0..5u64 {
        let bundle = WitnessBundle { preimages: vec![] };
        transport.send_witness(&bundle).await.unwrap();

        let _ = backend.recv_witness().await.unwrap();

        let result = ProofResult {
            claim: ProofClaim { l2_block_number: i, output_root: B256::ZERO, l1_head: B256::ZERO },
            evidence: ProofEvidence::Tee { attestation_doc: vec![], signature: vec![] },
        };
        backend.send_result(result).await.unwrap();

        let received = transport.recv_result().await.unwrap();
        assert_eq!(received.claim.l2_block_number, i);
    }
}
