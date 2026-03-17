//! Prover client abstraction for communicating with the prover server.

use async_trait::async_trait;
use base_proof_primitives::{ProofRequest, ProofResult, ProverApiClient};
use jsonrpsee::http_client::HttpClient;

use crate::error::ProposerError;

/// Trait abstracting the prover server RPC client for testability.
#[async_trait]
pub trait ProverClient: Send + Sync {
    /// Sends a proof request to the prover server and returns the result.
    async fn prove(&self, request: ProofRequest) -> Result<ProofResult, ProposerError>;
}

/// JSON-RPC client for the prover server.
#[derive(Debug)]
pub struct RpcProverClient {
    client: HttpClient,
}

impl RpcProverClient {
    /// Creates a new [`RpcProverClient`] from the given HTTP client.
    pub const fn new(client: HttpClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ProverClient for RpcProverClient {
    async fn prove(&self, request: ProofRequest) -> Result<ProofResult, ProposerError> {
        ProverApiClient::prove(&self.client, request)
            .await
            .map_err(|e| ProposerError::Prover(e.to_string()))
    }
}
