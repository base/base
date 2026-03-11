use std::fmt;

use base_zk_client::{
    GetProofRequest, GetProofResponse, ProveBlockRequest, ProveBlockResponse,
    prover_service_server::ProverService,
};
use base_zk_db::ProofRequestRepo;
use tonic::{Request, Response, Status};

mod get_proof;
mod prove_block;

/// gRPC server implementing the `ProverService` trait.
#[derive(Clone)]
pub struct ProverServiceServer {
    repo: ProofRequestRepo,
}

impl fmt::Debug for ProverServiceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverServiceServer").finish_non_exhaustive()
    }
}

impl ProverServiceServer {
    /// Create a new prover service server.
    pub const fn new(repo: ProofRequestRepo) -> Self {
        Self { repo }
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
}
