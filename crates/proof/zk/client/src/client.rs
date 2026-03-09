//! ZK proof gRPC client.

use std::time::Duration;

use async_trait::async_trait;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, warn};
use url::Url;

use crate::{
    error::ZkProofError,
    proto::{
        GetProofRequest, GetProofResponse, ProofJobStatus, ProveBlockRequest, ProveBlockResponse,
        prover_service_client::ProverServiceClient,
    },
};

/// Configuration for creating a [`ZkProofClient`].
#[derive(Debug, Clone)]
pub struct ZkProofClientConfig {
    /// The URL of the gRPC proving service (e.g., `http://127.0.0.1:50051`).
    pub endpoint: Url,
    /// Timeout for establishing the initial gRPC connection.
    pub connect_timeout: Duration,
    /// Timeout for individual gRPC requests.
    pub request_timeout: Duration,
}

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
///
/// The client wraps a [`tonic`] gRPC channel which is internally
/// reference-counted, so cloning a `ZkProofClient` is cheap and all clones
/// share the same underlying HTTP/2 connection.
#[derive(Debug, Clone)]
pub struct ZkProofClient {
    inner: ProverServiceClient<Channel>,
}

impl ZkProofClient {
    /// Create a new ZK proof client from the given configuration.
    ///
    /// The underlying gRPC channel is created lazily — no TCP connection is
    /// established until the first RPC call. Production deployments should
    /// use `https://` endpoints to ensure proof data is encrypted in transit.
    ///
    /// # Errors
    ///
    /// Returns [`ZkProofError::InvalidUrl`] if the URL cannot be parsed as a
    /// valid gRPC endpoint.
    pub fn new(config: &ZkProofClientConfig) -> Result<Self, ZkProofError> {
        let endpoint_str = config.endpoint.as_str();

        let channel = Endpoint::from_shared(endpoint_str.to_owned())
            .map_err(|e| ZkProofError::InvalidUrl(format!("{}: {e}", config.endpoint)))?
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .connect_lazy();

        debug!(endpoint = %config.endpoint, "ZK client created");

        Ok(Self { inner: ProverServiceClient::new(channel) })
    }

    /// Initiate a proof job for a given block range.
    ///
    /// # Errors
    ///
    /// Returns [`ZkProofError::GrpcStatus`] if the server returns a non-OK
    /// status.
    pub async fn prove_block(
        &self,
        request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        let start_block = request.start_block_number;
        let num_blocks = request.number_of_blocks_to_prove;
        let response = self.inner.clone().prove_block(request).await?.into_inner();
        debug!(
            start_block,
            num_blocks,
            session_id = %response.session_id,
            "proof job initiated",
        );
        Ok(response)
    }

    /// Poll for the result of a previously initiated proof job.
    ///
    /// # Errors
    ///
    /// Returns [`ZkProofError::GrpcStatus`] if the server returns a non-OK
    /// status.
    pub async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ZkProofError> {
        let session_id = request.session_id.clone();
        let response = self.inner.clone().get_proof(request).await?.into_inner();
        let status = ProofJobStatus::try_from(response.status).unwrap_or_else(|_| {
            warn!(raw_status = response.status, "unknown proof status value");
            ProofJobStatus::Unspecified
        });
        debug!(session_id = %session_id, status = ?status, "proof status polled");
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
