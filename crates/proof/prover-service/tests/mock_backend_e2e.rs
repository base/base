//! End-to-end integration tests using the mock backend.
//!
//! These tests require a running prover-service started with a mock backend
//! (e.g. `SP1_PROVER=mock`). The mock backend produces instant fake proofs; no
//! SP1 cluster, no S3, no real witness generation.

use std::time::{Duration, Instant};

mod common;

use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ProofResult, ProofStatus, ProverRequesterApiClient,
};
use common::{ProveBlockRequest, connect, prove_block};
use jsonrpsee::http_client::HttpClient;
use uuid::Uuid;

const PROOF_TYPE_COMPRESSED: i32 = 3;
const PROOF_TYPE_SNARK_GROTH16: i32 = 4;

/// Polling configuration -- mock proofs are instant, but worker/status polling
/// runs on intervals, so we need a small window.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const POLL_TIMEOUT: Duration = Duration::from_secs(120);

fn proof_bytes(response: &GetProofResponse) -> Vec<u8> {
    match response.result.as_ref() {
        Some(ProofResult::Compressed(result)) => result.proof.to_vec(),
        Some(ProofResult::SnarkGroth16(result)) => result.proof.proof.to_vec(),
        Some(ProofResult::Tee(_)) | None => Vec::new(),
    }
}

/// Poll `GetProof` until the status is terminal (`SUCCEEDED` or `FAILED`).
async fn poll_until_terminal(client: &HttpClient, session_id: &str) -> GetProofResponse {
    let start = Instant::now();
    loop {
        if start.elapsed() > POLL_TIMEOUT {
            panic!(
                "Timed out after {POLL_TIMEOUT:?} waiting for proof {session_id} to reach terminal state"
            );
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        let response = client
            .get_proof(GetProofRequest { session_id: session_id.to_string() })
            .await
            .expect("GetProof should succeed");

        println!(
            "  [{:.1}s] session={} status={:?} receipt_len={} error={:?}",
            start.elapsed().as_secs_f64(),
            session_id,
            response.status,
            proof_bytes(&response).len(),
            response.error_message,
        );

        match response.status {
            ProofStatus::Succeeded | ProofStatus::Failed => return response,
            ProofStatus::Queued | ProofStatus::Running => {
                // Still in progress, keep polling.
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_compressed_proof_succeeds() {
    println!("\n=== test_compressed_proof_succeeds ===");
    let client = connect();

    let resp = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 1000,
            number_of_blocks_to_prove: 3,
            sequence_window: Some(50),
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Uuid::new_v4().to_string(),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect("ProveBlock should succeed");

    let session_id = resp.session_id;
    println!("  Submitted COMPRESSED proof: session_id={session_id}");
    Uuid::parse_str(&session_id).expect("session_id should be valid UUID");

    let result = poll_until_terminal(&client, &session_id).await;

    assert_eq!(
        result.status,
        ProofStatus::Succeeded,
        "COMPRESSED proof should succeed with mock backend"
    );
    assert!(!proof_bytes(&result).is_empty(), "SUCCEEDED proof should have non-empty proof");
    assert!(result.error_message.is_none(), "SUCCEEDED proof should have no error_message");

    println!("  COMPRESSED proof succeeded: proof_len={}", proof_bytes(&result).len());
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_snark_groth16_proof_succeeds() {
    println!("\n=== test_snark_groth16_proof_succeeds ===");
    let client = connect();

    let resp = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 3000,
            number_of_blocks_to_prove: 2,
            sequence_window: Some(100),
            proof_type: PROOF_TYPE_SNARK_GROTH16,
            session_id: Uuid::new_v4().to_string(),
            prover_address: Some("0x1234567890abcdef1234567890abcdef12345678".to_string()),
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect("ProveBlock SNARK should succeed");

    let session_id = resp.session_id;
    println!("  Submitted SNARK_GROTH16 proof: session_id={session_id}");

    let result = poll_until_terminal(&client, &session_id).await;

    assert_eq!(
        result.status,
        ProofStatus::Succeeded,
        "SNARK_GROTH16 proof should succeed with mock backend"
    );
    assert!(!proof_bytes(&result).is_empty(), "SUCCEEDED SNARK proof should have non-empty proof");

    println!("  SNARK_GROTH16 proof succeeded: proof_len={}", proof_bytes(&result).len());
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_snark_groth16_both_receipts_available() {
    println!("\n=== test_snark_groth16_both_receipts_available ===");
    let client = connect();

    let resp = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 4000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_SNARK_GROTH16,
            session_id: Uuid::new_v4().to_string(),
            prover_address: Some("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string()),
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .unwrap();

    let session_id = resp.session_id;
    let _ = poll_until_terminal(&client, &session_id).await;

    let snark_resp =
        client.get_proof(GetProofRequest { session_id: session_id.clone() }).await.unwrap();

    assert_eq!(snark_resp.status, ProofStatus::Succeeded);
    assert!(
        !proof_bytes(&snark_resp).is_empty(),
        "SNARK proof should be available for SNARK_GROTH16 proof"
    );

    println!("  SNARK proof_len={}", proof_bytes(&snark_resp).len());
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_idempotent_request_returns_same_session() {
    println!("\n=== test_idempotent_request_returns_same_session ===");
    let client = connect();

    let session_id = Uuid::new_v4().to_string();

    let resp1 = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 5000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: session_id.clone(),
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
            start_block_number: 5000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: session_id.clone(),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect("duplicate call should succeed (idempotent)");

    assert_eq!(
        resp1.session_id, resp2.session_id,
        "idempotent request should return same session_id"
    );

    let result = poll_until_terminal(&client, &session_id).await;
    assert_eq!(result.status, ProofStatus::Succeeded);
    println!("  Idempotent request completed successfully");
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_multiple_concurrent_compressed_proofs() {
    println!("\n=== test_multiple_concurrent_compressed_proofs ===");
    let client = connect();

    let mut session_ids = Vec::new();
    for i in 0..3 {
        let resp = prove_block(
            &client,
            ProveBlockRequest {
                start_block_number: 6000 + i * 10,
                number_of_blocks_to_prove: 1,
                sequence_window: None,
                proof_type: PROOF_TYPE_COMPRESSED,
                session_id: Uuid::new_v4().to_string(),
                prover_address: None,
                l1_head: None,
                intermediate_root_interval: None,
            },
        )
        .await
        .expect("ProveBlock should succeed");

        let sid = resp.session_id;
        println!("  Submitted proof {i}: session_id={sid}");
        session_ids.push(sid);
    }

    for (i, session_id) in session_ids.iter().enumerate() {
        let result = poll_until_terminal(&client, session_id).await;
        assert_eq!(result.status, ProofStatus::Succeeded, "proof {i} should succeed");
        println!("  Proof {} succeeded: proof_len={}", i, proof_bytes(&result).len());
    }
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_invalid_proof_type_rejected() {
    println!("\n=== test_invalid_proof_type_rejected ===");
    let client = connect();

    let err = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 7000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: 99,
            session_id: Uuid::new_v4().to_string(),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect_err("invalid proof_type should be rejected");

    assert!(
        err.to_string().contains("invalid proof_type"),
        "error should mention invalid proof_type, got: {err}"
    );
    println!("  Correctly rejected invalid proof_type: {err}");
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_snark_without_prover_address_rejected() {
    println!("\n=== test_snark_without_prover_address_rejected ===");
    let client = connect();

    let err = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 8000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_SNARK_GROTH16,
            session_id: Uuid::new_v4().to_string(),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect_err("SNARK without prover_address should be rejected");

    assert!(
        err.to_string().contains("prover_address"),
        "error should mention prover_address, got: {err}"
    );
    println!("  Correctly rejected SNARK without prover_address: {err}");
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_get_proof_nonexistent_session() {
    println!("\n=== test_get_proof_nonexistent_session ===");
    let client = connect();

    let fake_session = Uuid::new_v4().to_string();
    let err = client
        .get_proof(GetProofRequest { session_id: fake_session })
        .await
        .expect_err("nonexistent session should return error");

    assert!(
        err.to_string().contains("Proof request not found"),
        "error should mention missing proof request, got: {err}"
    );
    println!("  Correctly returned NotFound for nonexistent session: {err}");
}
