//! Implementation of the `ListProofs` JSON-RPC endpoint.

use base_prover_service_db::{ProofRequestPage, ProofStatus as DbProofStatus};
use base_prover_service_protocol::{ListProofsRequest, ListProofsResponse, ProofSummary};
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
        let status_filter =
            req.status_filter.map(DbProofStatus::matching_filter).unwrap_or_default();

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

        let summaries: Vec<ProofSummary> = proofs.into_iter().map(ProofSummary::from).collect();

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
