use base_zk_client::{ProveBlockRequest, ProveBlockResponse};
use base_zk_db::{CreateProofRequest, ProofType};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::server::ProverServiceServer;

impl ProverServiceServer {
    /// Enqueues a new proof request and returns the generated `session_id=<uuid>`.
    pub async fn prove_block_impl(
        &self,
        request: Request<ProveBlockRequest>,
    ) -> Result<Response<ProveBlockResponse>, Status> {
        let prove_block_request = request.into_inner();

        info!(
            start_block_number = prove_block_request.start_block_number,
            num_blocks_to_prove = prove_block_request.number_of_blocks_to_prove,
            proof_type = prove_block_request.proof_type,
            "Attempting to prove base block(s)",
        );

        let proof_type = ProofType::try_from(prove_block_request.proof_type)
            .map_err(|e| Status::invalid_argument(format!("Invalid proof_type: {e}")))?;

        if proof_type == ProofType::GenericZkvmClusterSnarkGroth16
            && prove_block_request.prover_address.is_none()
        {
            return Err(Status::invalid_argument(
                "prover_address is required for SNARK_GROTH16 proofs",
            ));
        }

        let db_request = CreateProofRequest {
            start_block_number: prove_block_request.start_block_number,
            number_of_blocks_to_prove: prove_block_request.number_of_blocks_to_prove,
            sequence_window: prove_block_request.sequence_window,
            proof_type,
            prover_address: prove_block_request.prover_address,
        };

        let proof_request_id = self
            .repo
            .create_with_outbox(db_request)
            .await
            .map_err(|e| Status::internal(format!("Database error: {e}")))?;

        info!(
            proof_request_id = %proof_request_id,
            "Created proof request and outbox entry"
        );

        let response = ProveBlockResponse { session_id: proof_request_id.to_string() };

        Ok(Response::new(response))
    }
}
