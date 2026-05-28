//! End-to-end integration tests using the mock backend.
//!
//! These tests require a running prover-service started with a mock backend
//! (e.g. `SP1_PROVER=mock`). The mock backend produces instant fake proofs --
//! no SP1 cluster, no S3, no real witness generation. The full pipeline is
//! exercised:
//!
//!   gRPC `ProveBlock` -> DB + outbox -> `OutboxProcessor` -> `ProverWorker`
//!   -> `MockBackend` (instant proof) -> `StatusPoller` -> `GetProof` returns receipt
//!

use std::time::{Duration, Instant};

mod common;

use base_prover_service::{
    GetProofRequest, GetProofResponse, ProofStatus, proof_result,
    prover_service_client::ProverServiceClient,
};
use common::{ProveBlockRequest, ProverServiceClientCompat};
use tonic::transport::Channel;
use uuid::Uuid;

const PROOF_TYPE_COMPRESSED: i32 = 3;
const PROOF_TYPE_SNARK_GROTH16: i32 = 4;

/// Polling configuration -- mock proofs are instant, but the outbox processor
/// and status poller run on intervals, so we need a small window.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const POLL_TIMEOUT: Duration = Duration::from_secs(120);

async fn connect() -> ProverServiceClient<Channel> {
    let addr =
        std::env::var("PROVER_GRPC_ADDR").unwrap_or_else(|_| "http://localhost:9000".to_string());

    ProverServiceClient::connect(addr)
        .await
        .expect("failed to connect to prover-service - is it running?")
}

fn proof_bytes(response: &GetProofResponse) -> Vec<u8> {
    match response.result.as_ref().and_then(|result| result.result.as_ref()) {
        Some(proof_result::Result::Compressed(result)) => result.proof.clone(),
        Some(proof_result::Result::SnarkGroth16(result)) => {
            result.proof.as_ref().map(|proof| proof.proof.clone()).unwrap_or_default()
        }
        Some(proof_result::Result::Tee(_)) | None => Vec::new(),
    }
}

/// Poll `GetProof` until the status is terminal (`SUCCEEDED` or `FAILED`).
/// Returns the final [`GetProofResponse`].
async fn poll_until_terminal(
    client: &mut ProverServiceClient<Channel>,
    session_id: &str,
    _receipt_type: Option<i32>,
) -> GetProofResponse {
    let start = Instant::now();
    loop {
        if start.elapsed() > POLL_TIMEOUT {
            panic!(
                "Timed out after {POLL_TIMEOUT:?} waiting for proof {session_id} to reach terminal state"
            );
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        let resp = client
            .get_proof(GetProofRequest { session_id: session_id.to_string() })
            .await
            .expect("GetProof should succeed");

        let inner = resp.into_inner();
        let status = ProofStatus::try_from(inner.status).unwrap_or(ProofStatus::Unspecified);

        println!(
            "  [{:.1}s] session={} status={:?} receipt_len={} error={:?}",
            start.elapsed().as_secs_f64(),
            session_id,
            status,
            proof_bytes(&inner).len(),
            inner.error_message,
        );

        match status {
            ProofStatus::Succeeded | ProofStatus::Failed => {
                return inner;
            }
            _ => {
                // Still in progress, keep polling
            }
        }
    }
}

// ============================================================
// COMPRESSED proof tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_compressed_proof_succeeds() {
    println!("\n=== test_compressed_proof_succeeds ===");
    let mut client = connect().await;

    let resp = client
        .prove_block(ProveBlockRequest {
            start_block_number: 1000,
            number_of_blocks_to_prove: 3,
            sequence_window: Some(50),
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: None,
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        })
        .await
        .expect("ProveBlock should succeed");

    let session_id = resp.into_inner().session_id;
    println!("  Submitted COMPRESSED proof: session_id={session_id}");
    Uuid::parse_str(&session_id).expect("session_id should be valid UUID");

    // Poll until SUCCEEDED
    let result = poll_until_terminal(&mut client, &session_id, None).await;
    let status = ProofStatus::try_from(result.status).unwrap();

    assert_eq!(status, ProofStatus::Succeeded, "COMPRESSED proof should succeed with mock backend");
    assert!(!proof_bytes(&result).is_empty(), "SUCCEEDED proof should have non-empty proof");
    assert!(result.error_message.is_none(), "SUCCEEDED proof should have no error_message");

    println!("  COMPRESSED proof succeeded: proof_len={}", proof_bytes(&result).len());
}

// NOTE: test_compressed_proof_receipt_is_valid_bincode is intentionally omitted.
// It requires `sp1_sdk::SP1ProofWithPublicValues` and `bincode`, which are not
// available in the base-base workspace.

// ============================================================
// SNARK_GROTH16 proof tests (two-stage: STARK -> SNARK)
// ============================================================

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_snark_groth16_proof_succeeds() {
    println!("\n=== test_snark_groth16_proof_succeeds ===");
    let mut client = connect().await;

    let resp = client
        .prove_block(ProveBlockRequest {
            start_block_number: 3000,
            number_of_blocks_to_prove: 2,
            sequence_window: Some(100),
            proof_type: PROOF_TYPE_SNARK_GROTH16,
            session_id: None,
            prover_address: Some("0x1234567890abcdef1234567890abcdef12345678".to_string()),
            l1_head: None,
            intermediate_root_interval: None,
        })
        .await
        .expect("ProveBlock SNARK should succeed");

    let session_id = resp.into_inner().session_id;
    println!("  Submitted SNARK_GROTH16 proof: session_id={session_id}");

    // Poll until terminal
    let result = poll_until_terminal(&mut client, &session_id, None).await;
    let status = ProofStatus::try_from(result.status).unwrap();

    assert_eq!(
        status,
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
    let mut client = connect().await;

    let resp = client
        .prove_block(ProveBlockRequest {
            start_block_number: 4000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_SNARK_GROTH16,
            session_id: None,
            prover_address: Some("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string()),
            l1_head: None,
            intermediate_root_interval: None,
        })
        .await
        .unwrap();

    let session_id = resp.into_inner().session_id;

    // Wait for SUCCEEDED
    let _ = poll_until_terminal(&mut client, &session_id, None).await;

    // Fetch SNARK proof.
    let snark_resp = client
        .get_proof(GetProofRequest { session_id: session_id.clone() })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(ProofStatus::try_from(snark_resp.status).unwrap(), ProofStatus::Succeeded);
    assert!(
        !proof_bytes(&snark_resp).is_empty(),
        "SNARK proof should be available for SNARK_GROTH16 proof"
    );

    println!("  SNARK proof_len={}", proof_bytes(&snark_resp).len());

    // NOTE: Bincode deserialization of SP1ProofWithPublicValues is intentionally
    // omitted. The `sp1_sdk` and `bincode` dependencies are not available in the
    // base-base workspace.
}

// ============================================================
// Idempotency tests with mock backend
// ============================================================

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_idempotent_request_returns_same_session() {
    println!("\n=== test_idempotent_request_returns_same_session ===");
    let mut client = connect().await;

    let session_id = Uuid::new_v4().to_string();

    let resp1 = client
        .prove_block(ProveBlockRequest {
            start_block_number: 5000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some(session_id.clone()),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        })
        .await
        .expect("first call should succeed");

    let resp2 = client
        .prove_block(ProveBlockRequest {
            start_block_number: 5000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Some(session_id.clone()),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        })
        .await
        .expect("duplicate call should succeed (idempotent)");

    assert_eq!(
        resp1.into_inner().session_id,
        resp2.into_inner().session_id,
        "idempotent request should return same session_id"
    );

    // Wait for it to complete
    let result = poll_until_terminal(&mut client, &session_id, None).await;
    assert_eq!(ProofStatus::try_from(result.status).unwrap(), ProofStatus::Succeeded);
    println!("  Idempotent request completed successfully");
}

// ============================================================
// Multiple concurrent proofs
// ============================================================

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_multiple_concurrent_compressed_proofs() {
    println!("\n=== test_multiple_concurrent_compressed_proofs ===");
    let mut client = connect().await;

    // Submit 3 proofs concurrently
    let mut session_ids = Vec::new();
    for i in 0..3 {
        let resp = client
            .prove_block(ProveBlockRequest {
                start_block_number: 6000 + i * 10,
                number_of_blocks_to_prove: 1,
                sequence_window: None,
                proof_type: PROOF_TYPE_COMPRESSED,
                session_id: None,
                prover_address: None,
                l1_head: None,
                intermediate_root_interval: None,
            })
            .await
            .expect("ProveBlock should succeed");

        let sid = resp.into_inner().session_id;
        println!("  Submitted proof {i}: session_id={sid}");
        session_ids.push(sid);
    }

    // Poll all until terminal
    for (i, session_id) in session_ids.iter().enumerate() {
        let result = poll_until_terminal(&mut client, session_id, None).await;
        let status = ProofStatus::try_from(result.status).unwrap();
        assert_eq!(status, ProofStatus::Succeeded, "proof {i} should succeed");
        println!("  Proof {} succeeded: proof_len={}", i, proof_bytes(&result).len());
    }
}

// ============================================================
// Validation tests (should work the same with mock backend)
// ============================================================

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_invalid_proof_type_rejected() {
    println!("\n=== test_invalid_proof_type_rejected ===");
    let mut client = connect().await;

    let err = client
        .prove_block(ProveBlockRequest {
            start_block_number: 7000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: 99,
            session_id: None,
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        })
        .await
        .expect_err("invalid proof_type should be rejected");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    println!("  Correctly rejected invalid proof_type: {}", err.message());
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_snark_without_prover_address_rejected() {
    println!("\n=== test_snark_without_prover_address_rejected ===");
    let mut client = connect().await;

    let err = client
        .prove_block(ProveBlockRequest {
            start_block_number: 8000,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: PROOF_TYPE_SNARK_GROTH16,
            session_id: None,
            prover_address: None, // required for SNARK_GROTH16
            l1_head: None,
            intermediate_root_interval: None,
        })
        .await
        .expect_err("SNARK without prover_address should be rejected");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().to_lowercase().contains("prover_address"),
        "error should mention prover_address, got: {}",
        err.message()
    );
    println!("  Correctly rejected SNARK without prover_address: {}", err.message());
}

#[tokio::test]
#[ignore = "requires a running prover-service with a mock backend (SP1_PROVER=mock); run with `cargo nextest run --run-ignored all -p base-prover-service --test mock_backend_e2e`"]
async fn test_get_proof_nonexistent_session() {
    println!("\n=== test_get_proof_nonexistent_session ===");
    let mut client = connect().await;

    let fake_session = Uuid::new_v4().to_string();
    let err = client
        .get_proof(GetProofRequest { session_id: fake_session })
        .await
        .expect_err("nonexistent session should return error");

    assert_eq!(err.code(), tonic::Code::NotFound);
    println!("  Correctly returned NotFound for nonexistent session: {}", err.message());
}
