# `base-zk-client`

ZK proof gRPC client implementation.

This crate provides a gRPC client for requesting ZK proofs from an external
proving service. It implements a two-step async proving flow: `prove_block` to
initiate a proof job (returns a session ID) and `get_proof` to poll for results.

## Example

```ignore
use std::time::Duration;
use url::Url;
use base_zk_client::{
    ZkProofClient, ZkProofClientConfig, ZkProofProvider,
    ProveBlockRequest, ProofType,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ZkProofClientConfig {
        endpoint: Url::parse("http://127.0.0.1:50051")?,
        connect_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(30),
    };
    let client = ZkProofClient::new(&config)?;

    let request = ProveBlockRequest {
        start_block_number: 42,
        number_of_blocks_to_prove: 1,
        sequence_window: None,
        proof_type: ProofType::KailuaBentoStark.into(),
    };

    let response = client.prove_block(request).await?;
    println!("Session ID: {}", response.session_id);

    Ok(())
}
```

## Error Handling

All fallible operations return [`ZkProofError`]. Use `is_retryable()` to
decide whether to retry a failed call:

```ignore
use base_zk_client::ZkProofError;

fn handle_error(err: &ZkProofError) {
    if err.is_retryable() {
        // Connection failures and transient gRPC codes
        // (UNAVAILABLE, DEADLINE_EXCEEDED, RESOURCE_EXHAUSTED, ABORTED)
        // are safe to retry with backoff.
    } else {
        // InvalidUrl and permanent gRPC failures should not be retried.
    }
}
```

## Testability

The `ZkProofProvider` trait allows consumers to mock the client for testing:

```ignore
use async_trait::async_trait;
use base_zk_client::{
    ZkProofProvider, ZkProofError,
    ProveBlockRequest, ProveBlockResponse,
    GetProofRequest, GetProofResponse, get_proof_response,
};

struct MockProvider;

#[async_trait]
impl ZkProofProvider for MockProvider {
    async fn prove_block(
        &self,
        request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        Ok(ProveBlockResponse {
            session_id: "mock-session".into(),
        })
    }

    async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ZkProofError> {
        Ok(GetProofResponse {
            status: get_proof_response::Status::Succeeded.into(),
            receipt: vec![1, 2, 3],
        })
    }
}
```
