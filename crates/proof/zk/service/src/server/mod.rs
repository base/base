//! gRPC server implementation for the prover service.

use std::fmt;

use base_zk_client::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse, ProveBlockRequest,
    ProveBlockResponse, prover_service_server::ProverService,
};
use base_zk_db::ProofRequestRepo;
use tonic::{Request, Response, Status};

use crate::proof_request_manager::ProofRequestManager;

mod get_proof;
mod list_proofs;
mod prove_block;

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
    async fn prove_block(
        &self,
        request: Request<ProveBlockRequest>,
    ) -> Result<Response<ProveBlockResponse>, Status> {
        self.prove_block_impl(request).await
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
}
