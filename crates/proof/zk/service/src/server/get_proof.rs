use base_zk_client::{GetProofRequest, GetProofResponse, ProofJobStatus, ReceiptType};
use base_zk_db::ProofStatus;
use tonic::{Request, Response, Status};
use tracing::info;
use uuid::Uuid;

use crate::server::ProverServiceServer;

/// Helper function to get the appropriate receipt based on requested type
fn get_receipt_by_type(
    proof_req: &base_zk_db::ProofRequest,
    requested_type: ReceiptType,
) -> Result<Vec<u8>, Status> {
    match requested_type {
        ReceiptType::Unspecified | ReceiptType::Stark => proof_req
            .stark_receipt
            .clone()
            .ok_or_else(|| Status::not_found("STARK receipt not available")),
        ReceiptType::Snark | ReceiptType::OnChainSnark => proof_req
            .snark_receipt
            .clone()
            .ok_or_else(|| Status::not_found("SNARK receipt not available")),
    }
}

impl ProverServiceServer {
    /// Returns current proof status and receipt bytes for `session_id=<uuid>`.
    pub async fn get_proof_impl(
        &self,
        request: Request<GetProofRequest>,
    ) -> std::result::Result<Response<GetProofResponse>, Status> {
        let get_proof_request = request.into_inner();

        // Parse UUID from request
        let proof_request_id = Uuid::parse_str(&get_proof_request.session_id)
            .map_err(|_| Status::invalid_argument("Invalid UUID"))?;

        // Determine requested receipt type (default to STARK)
        let requested_receipt_type = get_proof_request
            .receipt_type
            .and_then(|t| ReceiptType::try_from(t).ok())
            .unwrap_or(ReceiptType::Stark);

        info!(
            proof_request_id = %proof_request_id,
            receipt_type = ?requested_receipt_type,
            "Getting proof status"
        );

        // Get from database
        let proof_req = self
            .repo
            .get(proof_request_id)
            .await
            .map_err(|e| Status::internal(format!("Database error: {e}")))?
            .ok_or_else(|| Status::not_found("Proof request not found"))?;

        // Map database status to proto status
        let (proto_status, receipt_bytes, error_message) = match proof_req.status {
            ProofStatus::Created => (ProofJobStatus::Created, vec![], None),
            ProofStatus::Pending => (ProofJobStatus::Pending, vec![], None),
            // StatusPoller handles periodic syncing with the backend,
            // avoiding write amplification on every poll.
            ProofStatus::Running => (ProofJobStatus::Running, vec![], None),
            ProofStatus::Succeeded => {
                let receipt_buf = get_receipt_by_type(&proof_req, requested_receipt_type)?;
                (ProofJobStatus::Succeeded, receipt_buf, None)
            }
            ProofStatus::Failed => (ProofJobStatus::Failed, vec![], proof_req.error_message),
        };

        let response =
            GetProofResponse { status: proto_status.into(), receipt: receipt_bytes, error_message };

        Ok(Response::new(response))
    }
}
