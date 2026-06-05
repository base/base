//! Integration test for `GetProof` `error_message` field.
//!
//! Submits a `ProveBlock` request that will fail (`number_of_blocks_to_prove=0`),
//! polls `GetProof` until `FAILED`, and verifies the `error_message` field is populated.
//!
//! Requires a running prover-service.

use std::time::{Duration, Instant};

mod common;

use base_prover_service_protocol::{GetProofRequest, ProofStatus, ProverRequesterApiClient};
use common::{ProveBlockRequest, connect, prove_block};
use uuid::Uuid;

const PROOF_TYPE_COMPRESSED: i32 = 3;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const POLL_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_RPC_ADDR); run with `cargo nextest run --run-ignored all -p base-prover-service --test get_proof_error_message`"]
async fn get_proof_failed_returns_error_message() {
    let client = connect();

    let resp = prove_block(
        &client,
        ProveBlockRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 0,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: Uuid::new_v4().to_string(),
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
        },
    )
    .await
    .expect("ProveBlock should accept the request");

    let session_id = resp.session_id;
    println!("Submitted proof request: {session_id}");

    let start = Instant::now();
    loop {
        if start.elapsed() > POLL_TIMEOUT {
            panic!("Timed out after {POLL_TIMEOUT:?} waiting for proof to fail",);
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        let resp = client
            .get_proof(GetProofRequest { session_id: session_id.clone() })
            .await
            .expect("GetProof should succeed");

        println!(
            "Poll [{:.0}s]: status={:?}, error_message={:?}",
            start.elapsed().as_secs_f64(),
            resp.status,
            resp.error_message,
        );

        match resp.status {
            ProofStatus::Failed => {
                assert!(
                    resp.error_message.is_some(),
                    "error_message should be Some when status is FAILED"
                );
                assert!(
                    !resp.error_message.as_ref().unwrap().is_empty(),
                    "error_message should not be empty when status is FAILED"
                );
                println!("error_message: {}", resp.error_message.unwrap());
                return;
            }
            ProofStatus::Succeeded => {
                panic!("Expected proof to fail, but it succeeded");
            }
            ProofStatus::Queued | ProofStatus::Running => {
                // Still in progress, continue polling.
            }
        }
    }
}
