//! Client for proof requester JSON-RPC methods.

use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
    ProverRequesterApiClient, SubmitProofRequest, SubmitProofResponse,
};
use jsonrpsee::http_client::HttpClient;
use tracing::debug;

use crate::{ProverServiceClientConfig, ProverServiceClientError};

/// JSON-RPC client for proof requester methods.
#[derive(Clone, Debug)]
pub struct ProverRequesterClient {
    inner: HttpClient,
}

impl ProverRequesterClient {
    /// Create a requester client from an existing JSON-RPC HTTP client.
    pub const fn new(inner: HttpClient) -> Self {
        Self { inner }
    }

    /// Connect a requester client using the provided configuration.
    pub fn connect(config: &ProverServiceClientConfig) -> Result<Self, ProverServiceClientError> {
        Ok(Self::new(config.build_http_client()?))
    }

    /// Return the underlying JSON-RPC HTTP client.
    pub const fn inner(&self) -> &HttpClient {
        &self.inner
    }

    /// Submit a proof request.
    pub async fn submit_proof(
        &self,
        request: SubmitProofRequest,
    ) -> Result<SubmitProofResponse, ProverServiceClientError> {
        debug!(
            session_id = ?request.proof.session_id,
            "submitting proof request"
        );
        Ok(self.inner.submit_proof(request).await?)
    }

    /// Return proof status and result data for a submitted proof request.
    pub async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ProverServiceClientError> {
        debug!(session_id = %request.session_id, "fetching proof request");
        Ok(self.inner.get_proof(request).await?)
    }

    /// List submitted proof requests.
    pub async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProverServiceClientError> {
        debug!(
            offset = request.offset,
            limit = request.limit,
            status_filter = ?request.status_filter,
            "listing proof requests"
        );
        Ok(self.inner.list_proofs(request).await?)
    }
}
