//! Client for proof requester JSON-RPC methods.

use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
    ProverRequesterApiClient, SubmitProofRequest, SubmitProofResponse,
};
use jsonrpsee::{core::client::Error, http_client::HttpClient};
use tracing::debug;

use crate::ProverServiceClientConfig;

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
    pub fn connect(config: &ProverServiceClientConfig) -> Result<Self, Error> {
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
    ) -> Result<SubmitProofResponse, Error> {
        debug!("submitting proof request");
        self.inner.submit_proof(request).await
    }

    /// Return proof status and result data for a submitted proof request.
    pub async fn get_proof(&self, request: GetProofRequest) -> Result<GetProofResponse, Error> {
        debug!("fetching proof request");
        self.inner.get_proof(request).await
    }

    /// List submitted proof requests.
    pub async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, Error> {
        debug!("listing proof requests");
        self.inner.list_proofs(request).await
    }
}
