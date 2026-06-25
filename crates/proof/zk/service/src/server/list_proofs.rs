//! Implementation of the `ListProofs` gRPC endpoint.

use base_zk_client::{
    ListProofsRequest, ListProofsResponse, ProofJobStatus, ProofSummary,
    ProofType as ProtoProofType, get_proof_response,
};
use base_zk_db::{ProofRequestPage, ProofStatus, ProofType as DbProofType};
use tonic::{Request, Response, Status};
use tracing::debug;

use crate::{metrics, server::ProverServiceServer};

const MAX_LIMIT: u64 = 100;
const DEFAULT_LIMIT: u64 = 50;

impl ProverServiceServer {
    /// Returns a paginated list of proof summaries for the given filter.
    pub async fn list_proofs_impl(
        &self,
        request: Request<ListProofsRequest>,
    ) -> Result<Response<ListProofsResponse>, Status> {
        let start = std::time::Instant::now();
        let result = self.list_proofs_inner(request).await;

        let (success, status_code) = match &result {
            Ok(_) => (true, "OK"),
            Err(s) => (false, metrics::grpc_status_code_str(s.code())),
        };
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::inc_requests("ListProofs", success, status_code);
        metrics::record_response_latency("ListProofs", success, elapsed_ms);

        result
    }

    async fn list_proofs_inner(
        &self,
        request: Request<ListProofsRequest>,
    ) -> Result<Response<ListProofsResponse>, Status> {
        let req = request.into_inner();

        let limit = clamp_limit(req.limit);
        let page =
            ProofRequestPage::try_new(limit, req.offset).map_err(Status::invalid_argument)?;
        let status_filter = parse_status_filter(req.status_filter)?;

        debug!(
            limit = limit,
            offset = req.offset,
            status_filter = ?status_filter,
            "listing proofs"
        );

        let (proofs, total_count) = self
            .repo
            .list_with_offset(status_filter, page)
            .await
            .map_err(|e| Status::internal(format!("database error: {e}")))?;

        let summaries: Vec<ProofSummary> = proofs
            .into_iter()
            .map(|p| ProofSummary {
                id: p.id.to_string(),
                start_block_number: p.start_block_number.max(0) as u64,
                number_of_blocks_to_prove: p.number_of_blocks_to_prove.max(0) as u64,
                proof_type: proto_proof_type(p.proof_type).into(),
                status: proto_status(p.status).into(),
                created_at: p.created_at.to_rfc3339(),
                updated_at: p.updated_at.to_rfc3339(),
                completed_at: p.completed_at.map(|t| t.to_rfc3339()),
                error_message: p.error_message,
            })
            .collect();

        Ok(Response::new(ListProofsResponse { proofs: summaries, total_count }))
    }
}

const fn clamp_limit(limit: u64) -> u64 {
    match limit {
        0 => DEFAULT_LIMIT,
        n if n > MAX_LIMIT => MAX_LIMIT,
        n => n,
    }
}

fn parse_status_filter(status_filter: Option<i32>) -> Result<Option<ProofStatus>, Status> {
    match status_filter {
        None => Ok(None),
        Some(v) => {
            let proto_status = get_proof_response::Status::try_from(v).map_err(|_| {
                Status::invalid_argument(format!("invalid status_filter value: {v}"))
            })?;
            Ok(match proto_status {
                get_proof_response::Status::Unspecified => None,
                get_proof_response::Status::Created => Some(ProofStatus::Created),
                get_proof_response::Status::Pending => Some(ProofStatus::Pending),
                get_proof_response::Status::Running => Some(ProofStatus::Running),
                get_proof_response::Status::Succeeded => Some(ProofStatus::Succeeded),
                get_proof_response::Status::Failed => Some(ProofStatus::Failed),
            })
        }
    }
}

const fn proto_proof_type(proof_type: DbProofType) -> ProtoProofType {
    match proof_type {
        DbProofType::OpSuccinctSp1ClusterCompressed => ProtoProofType::Compressed,
        DbProofType::OpSuccinctSp1ClusterSnarkGroth16 => ProtoProofType::SnarkGroth16,
    }
}

const fn proto_status(status: ProofStatus) -> ProofJobStatus {
    match status {
        ProofStatus::Created => ProofJobStatus::Created,
        ProofStatus::Pending => ProofJobStatus::Pending,
        ProofStatus::Running => ProofJobStatus::Running,
        ProofStatus::Succeeded => ProofJobStatus::Succeeded,
        ProofStatus::Failed => ProofJobStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_status_maps_all_variants() {
        assert_eq!(proto_status(ProofStatus::Created), ProofJobStatus::Created);
        assert_eq!(proto_status(ProofStatus::Pending), ProofJobStatus::Pending);
        assert_eq!(proto_status(ProofStatus::Running), ProofJobStatus::Running);
        assert_eq!(proto_status(ProofStatus::Succeeded), ProofJobStatus::Succeeded);
        assert_eq!(proto_status(ProofStatus::Failed), ProofJobStatus::Failed);
    }

    #[test]
    fn proto_proof_type_maps_all_variants() {
        assert_eq!(
            proto_proof_type(DbProofType::OpSuccinctSp1ClusterCompressed),
            ProtoProofType::Compressed
        );
        assert_eq!(
            proto_proof_type(DbProofType::OpSuccinctSp1ClusterSnarkGroth16),
            ProtoProofType::SnarkGroth16
        );
    }

    #[test]
    fn clamp_limit_handles_default_max_and_passthrough() {
        assert_eq!(clamp_limit(0), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(500), MAX_LIMIT);
        assert_eq!(clamp_limit(25), 25);
    }

    #[test]
    fn proof_request_page_rejects_offset_overflow() {
        let err = ProofRequestPage::try_new(MAX_LIMIT, i64::MAX as u64 + 1).unwrap_err();
        assert_eq!(err, "offset exceeds maximum supported value");
    }

    #[test]
    fn proof_request_page_rejects_zero_limit() {
        let err = ProofRequestPage::try_new(0, 0).unwrap_err();
        assert_eq!(err, "limit must be greater than zero");
    }

    #[test]
    fn status_filter_maps_unset_unspecified_and_valid_values() {
        assert_eq!(parse_status_filter(None).unwrap(), None);
        assert_eq!(
            parse_status_filter(Some(get_proof_response::Status::Unspecified as i32)).unwrap(),
            None
        );

        for (proto, expected) in [
            (get_proof_response::Status::Created, ProofStatus::Created),
            (get_proof_response::Status::Pending, ProofStatus::Pending),
            (get_proof_response::Status::Running, ProofStatus::Running),
            (get_proof_response::Status::Succeeded, ProofStatus::Succeeded),
            (get_proof_response::Status::Failed, ProofStatus::Failed),
        ] {
            assert_eq!(parse_status_filter(Some(proto as i32)).unwrap(), Some(expected));
        }
    }

    #[test]
    fn status_filter_rejects_invalid_value() {
        let err = parse_status_filter(Some(999)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
