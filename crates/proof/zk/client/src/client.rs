//! ZK proof gRPC client.

use async_trait::async_trait;
use tonic::transport::{Channel, Endpoint};
use tracing::info;
use url::Url;

use crate::error::ZkProofError;
use crate::proto::{
    GetProofRequest, GetProofResponse, ProveBlockRequest, ProveBlockResponse,
    prover_service_client::ProverServiceClient,
};

/// Abstraction over a ZK proving service that supports a two-step async flow:
/// initiate a proof job with [`prove_block`](ZkProofProvider::prove_block) and
/// poll for results with [`get_proof`](ZkProofProvider::get_proof).
///
/// The canonical implementation is [`ZkProofClient`], but the trait allows
/// callers to swap in a mock for testing without needing a real gRPC server.
#[async_trait]
pub trait ZkProofProvider: Send + Sync {
    /// Initiate a proof job for a given block range.
    ///
    /// Returns a [`ProveBlockResponse`] containing a session ID that can be
    /// used to poll for the result via [`get_proof`](ZkProofProvider::get_proof).
    async fn prove_block(
        &self,
        request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError>;

    /// Poll for the result of a previously initiated proof job.
    ///
    /// The caller should poll until the returned [`GetProofResponse`] indicates
    /// a terminal status (completed or failed).
    async fn get_proof(&self, request: GetProofRequest) -> Result<GetProofResponse, ZkProofError>;
}

/// gRPC client for requesting ZK proofs from an external proving service.
#[derive(Debug, Clone)]
pub struct ZkProofClient {
    inner: ProverServiceClient<Channel>,
}

impl ZkProofClient {
    /// Connect to a ZK proving service at the given endpoint.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The URL of the gRPC proving service
    ///   (e.g., `http://127.0.0.1:50051`)
    ///
    /// # Errors
    ///
    /// Returns [`ZkProofError::InvalidUrl`] if the URL cannot be parsed as a
    /// valid gRPC endpoint, or [`ZkProofError::Connection`] if the channel
    /// cannot be established.
    pub async fn connect(endpoint: &Url) -> Result<Self, ZkProofError> {
        let endpoint_str = endpoint.as_str();

        let channel = Endpoint::from_shared(endpoint_str.to_owned())
            .map_err(|e| ZkProofError::InvalidUrl(e.to_string()))?
            .connect()
            .await
            .map_err(|e| ZkProofError::Connection(e.to_string()))?;

        info!(endpoint = %endpoint, "ZK client connected");

        Ok(Self { inner: ProverServiceClient::new(channel) })
    }

    /// Initiate a proof job for a given block range.
    ///
    /// # Errors
    ///
    /// Returns an error if the gRPC call fails.
    pub async fn prove_block(
        &self,
        request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        let response = self.inner.clone().prove_block(request).await?.into_inner();

        Ok(response)
    }

    /// Poll for the result of a previously initiated proof job.
    ///
    /// # Errors
    ///
    /// Returns an error if the gRPC call fails.
    pub async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ZkProofError> {
        let response = self.inner.clone().get_proof(request).await?.into_inner();

        Ok(response)
    }
}

#[async_trait]
impl ZkProofProvider for ZkProofClient {
    async fn prove_block(
        &self,
        request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        self.prove_block(request).await
    }

    async fn get_proof(&self, request: GetProofRequest) -> Result<GetProofResponse, ZkProofError> {
        self.get_proof(request).await
    }
}
