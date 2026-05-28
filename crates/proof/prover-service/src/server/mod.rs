//! gRPC server implementation for the prover service.

use std::fmt;

use crate::{
    ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest, CompleteProofJobResponse,
    FailProofJobRequest, FailProofJobResponse, GetProofJobRequest, GetProofJobResponse,
    GetProofRequest, GetProofResponse, HeartbeatProofJobRequest, HeartbeatProofJobResponse,
    ListProofsRequest, ListProofsResponse, SubmitProofRequest, SubmitProofResponse,
    prover_service_server::ProverService,
};
use base_prover_service_db::ProofRequestRepo;
use tonic::{Request, Response, Status};

use crate::proof_request_manager::ProofRequestManager;

mod get_proof;
mod list_proofs;
mod submit_proof;

/// gRPC server implementing the `ProverService` trait.
#[derive(Clone)]
pub struct ProverServiceServer {
    repo: ProofRequestRepo,
    manager: ProofRequestManager,
    /// Shared `retry_count` cap with [`crate::worker::StatusPoller`] (same as `retry_or_fail_stuck_request`).
    max_proof_retries: i32,
}

impl fmt::Debug for ProverServiceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverServiceServer")
            .field("max_proof_retries", &self.max_proof_retries)
            .finish_non_exhaustive()
    }
}

impl ProverServiceServer {
    /// Create a new prover service server.
    pub const fn new(
        repo: ProofRequestRepo,
        manager: ProofRequestManager,
        max_proof_retries: i32,
    ) -> Self {
        Self { repo, manager, max_proof_retries }
    }
}

#[tonic::async_trait]
impl ProverService for ProverServiceServer {
    async fn submit_proof(
        &self,
        request: Request<SubmitProofRequest>,
    ) -> Result<Response<SubmitProofResponse>, Status> {
        self.submit_proof_impl(request).await
    }

    async fn get_proof(
        &self,
        request: Request<GetProofRequest>,
    ) -> std::result::Result<tonic::Response<GetProofResponse>, Status> {
        self.get_proof_impl(request).await
    }

    async fn list_proofs(
        &self,
        request: Request<ListProofsRequest>,
    ) -> Result<Response<ListProofsResponse>, Status> {
        self.list_proofs_impl(request).await
    }

    async fn get_proof_job(
        &self,
        _request: Request<GetProofJobRequest>,
    ) -> Result<Response<GetProofJobResponse>, Status> {
        Err(Status::unimplemented("proof job leasing is not supported by this service"))
    }

    async fn claim_proof_job(
        &self,
        _request: Request<ClaimProofJobRequest>,
    ) -> Result<Response<ClaimProofJobResponse>, Status> {
        Err(Status::unimplemented("proof job leasing is not supported by this service"))
    }

    async fn heartbeat_proof_job(
        &self,
        _request: Request<HeartbeatProofJobRequest>,
    ) -> Result<Response<HeartbeatProofJobResponse>, Status> {
        Err(Status::unimplemented("proof job leasing is not supported by this service"))
    }

    async fn complete_proof_job(
        &self,
        _request: Request<CompleteProofJobRequest>,
    ) -> Result<Response<CompleteProofJobResponse>, Status> {
        Err(Status::unimplemented("proof job leasing is not supported by this service"))
    }

    async fn fail_proof_job(
        &self,
        _request: Request<FailProofJobRequest>,
    ) -> Result<Response<FailProofJobResponse>, Status> {
        Err(Status::unimplemented("proof job leasing is not supported by this service"))
    }
}
