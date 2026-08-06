//! Integration tests for `ProveBlockRange` `session_id` idempotency.
//!
//! These tests require a running prover-service.
//! Set `PROVER_RPC_ADDR` to override the default address.

mod common;

use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProveBlockRangeRequest, ProverRequesterApiClient, ZkBackend,
    ZkProofRequest, ZkVm,
};
use common::connect;
use uuid::Uuid;

fn compressed_request(session_id: &str, start_block_number: u64) -> ProveBlockRangeRequest {
    ProveBlockRangeRequest {
        proof: ProofRequest {
            session_id: session_id.to_string(),
            request: ProofRequestKind::Compressed(ZkProofRequest {
                start_block_number,
                number_of_blocks_to_prove: 1,
                sequence_window: None,
                l1_head: None,
                intermediate_root_interval: None,
                zk_vm: ZkVm::Sp1,
                zk_backend: ZkBackend::Cluster,
            }),
        },
    }
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_range_with_session_id_returns_uuid() {
    let client = connect();
    let session_id = Uuid::new_v4().to_string();

    let resp = client
        .prove_block_range(compressed_request(&session_id, 100))
        .await
        .expect("ProveBlockRange should succeed with session_id");

    assert_eq!(resp.session_id, session_id);
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_range_with_session_id_uses_provided_id() {
    let client = connect();
    let session_id = "550e8400-e29b-41d4-a716-446655440000".to_string();

    let resp = client
        .prove_block_range(compressed_request(&session_id, 200))
        .await
        .expect("ProveBlockRange should succeed with session_id");

    assert_eq!(resp.session_id, session_id);
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_range_duplicate_session_id_is_idempotent() {
    let client = connect();
    let session_id = "661f9a00-bbbb-4444-cccc-000000000001".to_string();

    let resp1 = client
        .prove_block_range(compressed_request(&session_id, 300))
        .await
        .expect("first call should succeed");

    let resp2 = client
        .prove_block_range(compressed_request(&session_id, 300))
        .await
        .expect("duplicate call should succeed (idempotent)");

    assert_eq!(
        resp1.session_id, resp2.session_id,
        "duplicate session_id should return the same session_id"
    );
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test idempotency`"]
async fn prove_block_range_empty_session_id_returns_error() {
    let client = connect();

    let err = client
        .prove_block_range(compressed_request("", 400))
        .await
        .expect_err("should fail with empty session_id");

    assert!(err.to_string().contains("session_id"), "error should mention session_id, got: {err}");
}
