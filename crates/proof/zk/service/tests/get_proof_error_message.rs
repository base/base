//! Integration test for `GetProof` `error_message` field.
//!
//! Submits a `ProveBlock` request that will fail (`number_of_blocks_to_prove=0`),
//! polls `GetProof` until `FAILED`, and verifies the `error_message` field is populated.
//!
//! Requires a running prover-service.

use std::time::{Duration, Instant};

use base_zk_client::{
    GetProofRequest, ProveBlockRequest, get_proof_response,
    prover_service_client::ProverServiceClient,
};
use tonic::transport::Channel;

const PROOF_TYPE_COMPRESSED: i32 = 3;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const POLL_TIMEOUT: Duration = Duration::from_secs(120);

async fn connect() -> ProverServiceClient<Channel> {
    let addr =
        std::env::var("PROVER_GRPC_ADDR").unwrap_or_else(|_| "http://localhost:9000".to_string());

    ProverServiceClient::connect(addr)
        .await
        .expect("failed to connect to prover-service - is it running?")
}

#[tokio::test]
#[ignore = "requires a running prover-service (set PROVER_GRPC_ADDR); run with `cargo nextest run --run-ignored all -p base-zk-service --test get_proof_error_message`"]
async fn get_proof_failed_returns_error_message() {
    let mut client = connect().await;

    // Submit a proof request that will fail (0 blocks is invalid)
    let resp = client
        .prove_block(ProveBlockRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 0,
            sequence_window: None,
            proof_type: PROOF_TYPE_COMPRESSED,
            session_id: None,
            prover_address: None,
            l1_head: None,
        })
        .await
        .expect("ProveBlock should accept the request");

    let session_id = resp.into_inner().session_id;
    println!("Submitted proof request: {session_id}");

    // Poll until FAILED
    let start = Instant::now();
    loop {
        if start.elapsed() > POLL_TIMEOUT {
            panic!("Timed out after {POLL_TIMEOUT:?} waiting for proof to fail",);
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        let resp = client
            .get_proof(GetProofRequest { session_id: session_id.clone(), receipt_type: None })
            .await
            .expect("GetProof should succeed");

        let inner = resp.into_inner();
        let status = get_proof_response::Status::try_from(inner.status)
            .unwrap_or(get_proof_response::Status::Unspecified);

        println!(
            "Poll [{:.0}s]: status={:?}, error_message={:?}",
            start.elapsed().as_secs_f64(),
            status,
            inner.error_message,
        );

        match status {
            get_proof_response::Status::Failed => {
                assert!(
                    inner.error_message.is_some(),
                    "error_message should be Some when status is FAILED"
                );
                assert!(
                    !inner.error_message.as_ref().unwrap().is_empty(),
                    "error_message should not be empty when status is FAILED"
                );
                println!("error_message: {}", inner.error_message.unwrap());
                return;
            }
            get_proof_response::Status::Succeeded => {
                panic!("Expected proof to fail, but it succeeded");
            }
            _ => {
                // Still in progress, continue polling
            }
        }
    }
}
