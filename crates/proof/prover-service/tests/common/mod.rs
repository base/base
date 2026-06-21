//! Shared helpers for prover-service integration tests.

use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};

pub(crate) fn connect() -> HttpClient {
    let addr = std::env::var("PROVER_RPC_ADDR")
        .or_else(|_| std::env::var("PROVER_GRPC_ADDR"))
        .unwrap_or_else(|_| "http://localhost:9000".to_string());

    HttpClientBuilder::default()
        .build(addr)
        .expect("failed to connect to prover-service - is it running?")
}
