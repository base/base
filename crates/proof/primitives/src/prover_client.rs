//! Mockable prover client trait and JSON-RPC implementation.

use async_trait::async_trait;
use jsonrpsee::http_client::HttpClient;

use crate::{ProofRequest, ProofResult, ProverApiClient};

/// Trait abstracting the prover server RPC client for testability.
#[async_trait]
pub trait ProverClient: Send + Sync + std::fmt::Debug {
    /// Sends a proof request to the prover server and returns the result.
    async fn prove(
        &self,
        request: ProofRequest,
    ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait]
impl ProverClient for HttpClient {
    async fn prove(
        &self,
        request: ProofRequest,
    ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ProverApiClient::prove(self, request).await?)
    }
}
