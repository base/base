//! Client for prover worker JSON-RPC methods.

use base_prover_service_protocol::{
    ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest, CompleteProofJobResponse,
    FailProofJobRequest, FailProofJobResponse, GetProofJobRequest, GetProofJobResponse,
    HeartbeatProofJobRequest, HeartbeatProofJobResponse, ProverWorkerApiClient,
};
use jsonrpsee::http_client::HttpClient;
use tracing::debug;

use crate::{ProverServiceClientConfig, ProverServiceClientError};

/// JSON-RPC client for prover worker methods.
#[derive(Clone, Debug)]
pub struct ProverWorkerClient {
    inner: HttpClient,
}

impl ProverWorkerClient {
    /// Create a worker client from an existing JSON-RPC HTTP client.
    pub const fn new(inner: HttpClient) -> Self {
        Self { inner }
    }

    /// Connect a worker client using the provided configuration.
    pub fn connect(config: &ProverServiceClientConfig) -> Result<Self, ProverServiceClientError> {
        Ok(Self::new(config.build_http_client()?))
    }

    /// Return the underlying JSON-RPC HTTP client.
    pub const fn inner(&self) -> &HttpClient {
        &self.inner
    }

    /// Return a worker-owned proof job by session id.
    pub async fn get_proof_job(
        &self,
        request: GetProofJobRequest,
    ) -> Result<GetProofJobResponse, ProverServiceClientError> {
        debug!("fetching proof job");
        Ok(self.inner.get_proof_job(request).await?)
    }

    /// Claim the next eligible queued proof job.
    pub async fn claim_proof_job(
        &self,
        request: ClaimProofJobRequest,
    ) -> Result<ClaimProofJobResponse, ProverServiceClientError> {
        debug!("claiming proof job");
        Ok(self.inner.claim_proof_job(request).await?)
    }

    /// Extend a proof job lease.
    pub async fn heartbeat_proof_job(
        &self,
        request: HeartbeatProofJobRequest,
    ) -> Result<HeartbeatProofJobResponse, ProverServiceClientError> {
        debug!("heartbeating proof job");
        Ok(self.inner.heartbeat_proof_job(request).await?)
    }

    /// Complete a leased proof job.
    pub async fn complete_proof_job(
        &self,
        request: CompleteProofJobRequest,
    ) -> Result<CompleteProofJobResponse, ProverServiceClientError> {
        debug!("completing proof job");
        Ok(self.inner.complete_proof_job(request).await?)
    }

    /// Fail a leased proof job.
    pub async fn fail_proof_job(
        &self,
        request: FailProofJobRequest,
    ) -> Result<FailProofJobResponse, ProverServiceClientError> {
        debug!("failing proof job");
        Ok(self.inner.fail_proof_job(request).await?)
    }
}
