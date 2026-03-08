//! ZK proof gRPC client.

use std::time::Duration;

use async_trait::async_trait;
use tonic::transport::{Channel, Endpoint};
use tracing::debug;
use url::Url;

use crate::{
    ProofJobStatus,
    error::ZkProofError,
    proto::{
        GetProofRequest, GetProofResponse, ProveBlockRequest, ProveBlockResponse,
        prover_service_client::ProverServiceClient,
    },
};

/// Default timeout for establishing the initial gRPC connection.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout for individual gRPC requests. Proving can be long-running,
/// so this is set conservatively high; callers should override via their own
/// timeout/retry layer when tighter deadlines are needed.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

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
    /// Create a new ZK proof client for the given endpoint.
    ///
    /// The underlying gRPC channel is created lazily — no TCP connection is
    /// established until the first RPC call. A 10-second connection timeout
    /// and a 300-second request timeout are applied by default. Production
    /// deployments should use `https://` endpoints to ensure proof data is
    /// encrypted in transit.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The URL of the gRPC proving service
    ///   (e.g., `http://127.0.0.1:50051`)
    ///
    /// # Errors
    ///
    /// Returns [`ZkProofError::InvalidUrl`] if the URL cannot be parsed as a
    /// valid gRPC endpoint.
    pub fn new(endpoint: &Url) -> Result<Self, ZkProofError> {
        let endpoint_str = endpoint.as_str();

        let channel = Endpoint::from_shared(endpoint_str.to_owned())
            .map_err(|e| ZkProofError::InvalidUrl(format!("{endpoint}: {e}")))?
            .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .connect_lazy();

        debug!(endpoint = %endpoint, "ZK client created");

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
        let response = self.inner.clone().prove_block(request).await?.into_inner();
        let status =
            ProofJobStatus::try_from(response.status).unwrap_or(ProofJobStatus::Unspecified);
        debug!(session_id = %response.session_id, status = ?status, "proof job initiated");
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
        let status =
            ProofJobStatus::try_from(response.status).unwrap_or(ProofJobStatus::Unspecified);
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
