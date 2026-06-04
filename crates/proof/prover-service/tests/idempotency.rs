//! Integration tests for `ProveBlock` `session_id` idempotency.
//!
//! These tests require a running prover-service.
//! Set `PROVER_RPC_ADDR` to override the default address.

mod common;

use common::{ProveBlockRequest, connect, prove_block};
use uuid::Uuid;

const PROOF_TYPE_COMPRESSED: i32 = 3;

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_without_session_id_returns_uuid() {
    let client = connect();

    let resp = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: None,
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect("ProveBlock should succeed without session_id");

    Uuid::parse_str(&resp.session_id).expect("session_id should be a valid UUID");
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_with_session_id_uses_provided_id() {
    let client = connect();
    let session_id = "550e8400-e29b-41d4-a716-446655440000".to_string();

    let resp = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 200,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some(session_id.clone()),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect("ProveBlock should succeed with session_id");

    assert_eq!(resp.session_id, session_id);
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_duplicate_session_id_is_idempotent() {
    let client = connect();
    let session_id = "661f9a00-bbbb-4444-cccc-000000000001".to_string();

    let resp1 = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 300,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some(session_id.clone()),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect("first call should succeed");

    let resp2 = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 300,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some(session_id.clone()),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect("duplicate call should succeed (idempotent)");

    assert_eq!(
        resp1.session_id, resp2.session_id,
        "duplicate session_id should return the same session_id"
    );
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_invalid_session_id_returns_error() {
    let client = connect();

    let err = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 400,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some("not-a-uuid".to_string()),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect_err("should fail with invalid session_id");

    assert!(err.to_string().contains("session_id"), "error should mention session_id, got: {err}");
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_invalid_proof_type_returns_error() {
    let client = connect();

    let err = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 500,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: 99,
            session_id: None,
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect_err("should fail with invalid proof_type");

    assert!(
        err.to_string().contains("invalid proof_type"),
        "error should mention invalid proof_type, got: {err}"
    );
}
