//! Implementation of the `ListProofs` JSON-RPC endpoint.

use base_prover_service_db::{
    ApiProofType as DbApiProofType, ProofRequestPage, ProofStatus as DbProofStatus,
    TeeKind as DbTeeKind, ZkVmKind as DbZkVmKind,
};
use base_prover_service_protocol::{
    ListProofsRequest, ListProofsResponse, ProofStatus, ProofSummary, ProofType, TeeKind, ZkVm,
};
use jsonrpsee::core::RpcResult;
use tracing::debug;

use crate::server::{ProverServiceServer, internal, invalid_argument, record_rpc_result};

const MAX_LIMIT: u64 = 1000;
const DEFAULT_LIMIT: u64 = 50;

impl ProverServiceServer {
    /// Returns a paginated list of proof summaries for the given filter.
    pub async fn list_proofs_impl(
        &self,
        request: ListProofsRequest,
    ) -> RpcResult<ListProofsResponse> {
        let start = std::time::Instant::now();
        let result = self.list_proofs_inner(request).await;
        record_rpc_result("ListProofs", start, &result);

        result
    }

    async fn list_proofs_inner(&self, req: ListProofsRequest) -> RpcResult<ListProofsResponse> {
        let limit = parse_limit(req.limit)?;
        let page = ProofRequestPage::try_new(limit, req.offset).map_err(invalid_argument)?;
        let status_filter = parse_status_filter(req.status_filter);

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
            .map_err(|e| internal(format!("database error: {e}")))?;

        let summaries: Vec<ProofSummary> = proofs
            .into_iter()
            .map(|p| ProofSummary {
                session_id: p.session_id,
                proof_type: api_proof_type(p.api_proof_type),
                status: api_status(p.status),
                created_at: p.created_at,
                updated_at: p.updated_at,
                completed_at: p.completed_at,
                error_message: p.error_message,
                tee_kind: p.tee_kind.map(api_tee_kind),
                zk_vm: p.zk_vm.map(api_zk_vm),
            })
            .collect();

        Ok(ListProofsResponse { proofs: summaries, total_count })
    }
}

fn parse_limit(limit: u32) -> RpcResult<u64> {
    let limit = u64::from(limit);
    match limit {
        0 => Ok(DEFAULT_LIMIT),
        n if n > MAX_LIMIT => {
            Err(invalid_argument(format!("limit must be less than or equal to {MAX_LIMIT}")))
        }
        n => Ok(n),
    }
}

fn parse_status_filter(status_filter: Option<ProofStatus>) -> Vec<DbProofStatus> {
    match status_filter {
        None => Vec::new(),
        Some(ProofStatus::Queued) => vec![DbProofStatus::Created, DbProofStatus::Pending],
        Some(ProofStatus::Running) => vec![DbProofStatus::Running],
        Some(ProofStatus::Succeeded) => vec![DbProofStatus::Succeeded],
        Some(ProofStatus::Failed) => vec![DbProofStatus::Failed],
    }
}

const fn api_proof_type(proof_type: DbApiProofType) -> ProofType {
    match proof_type {
        DbApiProofType::Compressed => ProofType::Compressed,
        DbApiProofType::SnarkGroth16 => ProofType::SnarkGroth16,
        DbApiProofType::Tee => ProofType::Tee,
    }
}

const fn api_zk_vm(zk_vm: DbZkVmKind) -> ZkVm {
    match zk_vm {
        DbZkVmKind::Sp1 => ZkVm::Sp1,
    }
}

const fn api_tee_kind(tee_kind: DbTeeKind) -> TeeKind {
    match tee_kind {
        DbTeeKind::AwsNitro => TeeKind::AwsNitro,
    }
}

const fn api_status(status: DbProofStatus) -> ProofStatus {
    match status {
        DbProofStatus::Created | DbProofStatus::Pending => ProofStatus::Queued,
        DbProofStatus::Running => ProofStatus::Running,
        DbProofStatus::Succeeded => ProofStatus::Succeeded,
        DbProofStatus::Failed => ProofStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_status_maps_all_variants() {
        assert_eq!(api_status(DbProofStatus::Created), ProofStatus::Queued);
        assert_eq!(api_status(DbProofStatus::Pending), ProofStatus::Queued);
        assert_eq!(api_status(DbProofStatus::Running), ProofStatus::Running);
        assert_eq!(api_status(DbProofStatus::Succeeded), ProofStatus::Succeeded);
        assert_eq!(api_status(DbProofStatus::Failed), ProofStatus::Failed);
    }

    #[test]
    fn api_proof_type_maps_all_variants() {
        assert_eq!(api_proof_type(DbApiProofType::Compressed), ProofType::Compressed);
        assert_eq!(api_proof_type(DbApiProofType::SnarkGroth16), ProofType::SnarkGroth16);
        assert_eq!(api_proof_type(DbApiProofType::Tee), ProofType::Tee);
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
        assert_eq!(err.code(), jsonrpsee::types::error::INVALID_PARAMS_CODE);
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
    fn status_filter_maps_unset_and_valid_values() {
        assert_eq!(parse_status_filter(None), Vec::<DbProofStatus>::new());

        for (api, expected) in [
            (ProofStatus::Queued, vec![DbProofStatus::Created, DbProofStatus::Pending]),
            (ProofStatus::Running, vec![DbProofStatus::Running]),
            (ProofStatus::Succeeded, vec![DbProofStatus::Succeeded]),
            (ProofStatus::Failed, vec![DbProofStatus::Failed]),
        ] {
            assert_eq!(parse_status_filter(Some(api)), expected);
        }
    }
}
