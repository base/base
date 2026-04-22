//! Integration tests for `ProveBlock` `session_id` idempotency.
//!
//! These tests require a running prover-service.
//! Set `PROVER_GRPC_ADDR` to override the default address.

use base_zk_client::{ProveBlockRequest, prover_service_client::ProverServiceClient};
use tonic::transport::Channel;
use uuid::Uuid;

const PROOF_TYPE_COMPRESSED: i32 = 3;

async fn connect() -> ProverServiceClient<Channel> {
    let addr =
        std::env::var("PROVER_GRPC_ADDR").unwrap_or_else(|_| "http://localhost:9000".to_string());

    ProverServiceClient::connect(addr)
        .await
        .expect("failed to connect to prover-service - is it running?")
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test idempotency`"]
async fn prove_block_without_session_id_returns_uuid() {
    let mut client = connect().await;

    let resp = client
        .prove_block(ProveBlockRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: None,
            prover_address: None,
            l1_head: None,
        })
        .await
        .expect("ProveBlock should succeed without session_id");

    let session_id = resp.into_inner().session_id;
    Uuid::parse_str(&session_id).expect("session_id should be a valid UUID");
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test idempotency`"]
async fn prove_block_with_session_id_uses_provided_id() {
    let mut client = connect().await;
    let session_id = "550e8400-e29b-41d4-a716-446655440000".to_string();

    let resp = client
        .prove_block(ProveBlockRequest {
            start_block_number: 200,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some(session_id.clone()),
            prover_address: None,
            l1_head: None,
        })
        .await
        .expect("ProveBlock should succeed with session_id");

    assert_eq!(resp.into_inner().session_id, session_id);
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test idempotency`"]
async fn prove_block_duplicate_session_id_is_idempotent() {
    let mut client = connect().await;
    let session_id = "661f9a00-bbbb-4444-cccc-000000000001".to_string();

    let resp1 = client
        .prove_block(ProveBlockRequest {
            start_block_number: 300,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some(session_id.clone()),
            prover_address: None,
            l1_head: None,
        })
        .await
        .expect("first call should succeed");

    let resp2 = client
        .prove_block(ProveBlockRequest {
            start_block_number: 300,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some(session_id.clone()),
            prover_address: None,
            l1_head: None,
        })
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
async fn prove_block_invalid_session_id_returns_error() {
    let mut client = connect().await;

    let err = client
        .prove_block(ProveBlockRequest {
            start_block_number: 400,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some("not-a-uuid".to_string()),
            prover_address: None,
            l1_head: None,
        })
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
async fn prove_block_invalid_proof_type_returns_error() {
    let mut client = connect().await;

    let err = client
        .prove_block(ProveBlockRequest {
            start_block_number: 500,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: 99,
            session_id: None,
            prover_address: None,
            l1_head: None,
        })
        .await
        .expect_err("should fail with invalid proof_type");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
