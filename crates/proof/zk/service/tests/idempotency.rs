//! Integration tests for `SubmitProof` `session_id` idempotency.
//!
//! These tests require a running prover-service.
//! Set `PROVER_GRPC_ADDR` to override the default address.

use base_zk_client::{
    ProofRequest, SubmitProofRequest, ZkProofRequest, prover_service_client::ProverServiceClient,
};
use tonic::transport::Channel;
use uuid::Uuid;

async fn connect() -> ProverServiceClient<Channel> {
    let addr =
        std::env::var("PROVER_GRPC_ADDR").unwrap_or_else(|_| "http://localhost:9000".to_string());

    ProverServiceClient::connect(addr)
        .await
        .expect("failed to connect to prover-service - is it running?")
}

fn compressed_request(session_id: Option<String>, start_block_number: u64) -> SubmitProofRequest {
    SubmitProofRequest {
        proof: Some(ProofRequest::compressed(
            session_id,
            ZkProofRequest {
                start_block_number,
                number_of_blocks_to_prove: 1,
                sequence_window: None,
                prover_address: None,
                l1_head: None,
                intermediate_root_interval: None,
            },
        )),
    }
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test idempotency`"]
async fn submit_proof_without_session_id_returns_uuid() {
    let mut client = connect().await;

    let resp = client
        .submit_proof(compressed_request(None, 100))
        .await
        .expect("SubmitProof should succeed without session_id");

    let session_id = resp.into_inner().session_id;
    Uuid::parse_str(&session_id).expect("session_id should be a valid UUID");
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test idempotency`"]
async fn submit_proof_with_session_id_uses_provided_id() {
    let mut client = connect().await;
    let session_id = "550e8400-e29b-41d4-a716-446655440000".to_string();

    let resp = client
        .submit_proof(compressed_request(Some(session_id.clone()), 200))
        .await
        .expect("SubmitProof should succeed with session_id");

    assert_eq!(resp.into_inner().session_id, session_id);
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test idempotency`"]
async fn submit_proof_duplicate_session_id_is_idempotent() {
    let mut client = connect().await;
    let session_id = "661f9a00-bbbb-4444-cccc-000000000001".to_string();

    let resp1 = client
        .submit_proof(compressed_request(Some(session_id.clone()), 300))
        .await
        .expect("first call should succeed");

    let resp2 = client
        .submit_proof(compressed_request(Some(session_id.clone()), 300))
        .await
        .expect("duplicate call should succeed (idempotent)");

    assert_eq!(
        resp1.into_inner().session_id,
        resp2.into_inner().session_id,
        "duplicate session_id should return the same session_id"
    );
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test idempotency`"]
async fn submit_proof_invalid_session_id_returns_error() {
    let mut client = connect().await;

    let err = client
        .submit_proof(compressed_request(Some("not-a-uuid".to_string()), 400))
        .await
        .expect_err("should fail with invalid session_id");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("session_id"),
        "error should mention session_id, got: {}",
        err.message()
    );
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test idempotency`"]
async fn submit_proof_missing_request_variant_returns_error() {
    let mut client = connect().await;

    let err = client
        .submit_proof(SubmitProofRequest { proof: Some(ProofRequest::default()) })
        .await
        .expect_err("should fail with missing request variant");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
