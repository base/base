//! Implementation of the `ListProofs` gRPC endpoint.

use crate::{
    ListProofsRequest, ListProofsResponse, ProofStatus as ProtoProofStatus, ProofSummary,
    ProofType as ProtoProofType, TeeKind, ZkVm,
};
use base_prover_service_db::{ProofRequestPage, ProofStatus, ProofType as DbProofType};
use chrono::{DateTime, Utc};
use prost_types::Timestamp;
use tonic::{Request, Response, Status};
use tracing::debug;

use crate::{metrics, server::ProverServiceServer};

const MAX_LIMIT: u64 = 1000;
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

        let limit = parse_limit(req.limit)?;
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
            .list_with_offset(&status_filter, page)
            .await
            .map_err(|e| Status::internal(format!("database error: {e}")))?;

        let summaries: Vec<ProofSummary> = proofs
            .into_iter()
            .map(|p| ProofSummary {
                session_id: p.id.to_string(),
                proof_type: proto_proof_type(p.proof_type).into(),
                status: proto_status(p.status).into(),
                created_at: Some(timestamp_from_datetime(p.created_at)),
                updated_at: Some(timestamp_from_datetime(p.updated_at)),
                completed_at: p.completed_at.map(timestamp_from_datetime),
                error_message: p.error_message,
                tee_kind: TeeKind::Unspecified.into(),
                zk_vm: ZkVm::Sp1.into(),
            })
            .collect();

        Ok(Response::new(ListProofsResponse { proofs: summaries, total_count }))
    }
}

fn parse_limit(limit: u32) -> Result<u64, Status> {
    let limit = u64::from(limit);
    match limit {
        0 => Ok(DEFAULT_LIMIT),
        n if n > MAX_LIMIT => Err(Status::invalid_argument(format!(
            "limit must be less than or equal to {MAX_LIMIT}"
        ))),
        n => Ok(n),
    }
}

fn parse_status_filter(status_filter: Option<i32>) -> Result<Vec<ProofStatus>, Status> {
    match status_filter {
        None => Ok(Vec::new()),
        Some(v) => {
            let proto_status = ProtoProofStatus::try_from(v).map_err(|_| {
                Status::invalid_argument(format!("invalid status_filter value: {v}"))
            })?;
            Ok(match proto_status {
                ProtoProofStatus::Unspecified => Vec::new(),
                ProtoProofStatus::Queued => vec![ProofStatus::Created, ProofStatus::Pending],
                ProtoProofStatus::Running => vec![ProofStatus::Running],
                ProtoProofStatus::Succeeded => vec![ProofStatus::Succeeded],
                ProtoProofStatus::Failed => vec![ProofStatus::Failed],
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

const fn proto_status(status: ProofStatus) -> ProtoProofStatus {
    match status {
        ProofStatus::Created | ProofStatus::Pending => ProtoProofStatus::Queued,
        ProofStatus::Running => ProtoProofStatus::Running,
        ProofStatus::Succeeded => ProtoProofStatus::Succeeded,
        ProofStatus::Failed => ProtoProofStatus::Failed,
    }
}

const fn timestamp_from_datetime(datetime: DateTime<Utc>) -> Timestamp {
    Timestamp { seconds: datetime.timestamp(), nanos: datetime.timestamp_subsec_nanos() as i32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_status_maps_all_variants() {
        assert_eq!(proto_status(ProofStatus::Created), ProtoProofStatus::Queued);
        assert_eq!(proto_status(ProofStatus::Pending), ProtoProofStatus::Queued);
        assert_eq!(proto_status(ProofStatus::Running), ProtoProofStatus::Running);
        assert_eq!(proto_status(ProofStatus::Succeeded), ProtoProofStatus::Succeeded);
        assert_eq!(proto_status(ProofStatus::Failed), ProtoProofStatus::Failed);
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
    fn parse_limit_handles_default_max_and_passthrough() {
        assert_eq!(parse_limit(0).unwrap(), DEFAULT_LIMIT);
        assert_eq!(parse_limit(500).unwrap(), 500);
        assert_eq!(parse_limit(MAX_LIMIT as u32).unwrap(), MAX_LIMIT);
        assert_eq!(parse_limit(25).unwrap(), 25);
    }

    #[test]
    fn parse_limit_rejects_values_above_max() {
        let err = parse_limit(MAX_LIMIT as u32 + 1).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
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
        assert_eq!(parse_status_filter(None).unwrap(), Vec::<ProofStatus>::new());
        assert_eq!(
            parse_status_filter(Some(ProtoProofStatus::Unspecified as i32)).unwrap(),
            Vec::<ProofStatus>::new()
        );

        for (proto, expected) in [
            (ProtoProofStatus::Queued, vec![ProofStatus::Created, ProofStatus::Pending]),
            (ProtoProofStatus::Running, vec![ProofStatus::Running]),
            (ProtoProofStatus::Succeeded, vec![ProofStatus::Succeeded]),
            (ProtoProofStatus::Failed, vec![ProofStatus::Failed]),
        ] {
            assert_eq!(parse_status_filter(Some(proto as i32)).unwrap(), expected);
        }
    }

    #[test]
    fn status_filter_rejects_invalid_value() {
        let err = parse_status_filter(Some(999)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
