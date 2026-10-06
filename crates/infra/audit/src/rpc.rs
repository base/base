//! RPC server for the audit archiver.
//! Serves Postgres-backed transaction event queries.

use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::error::ErrorObjectOwned};
use jsonrpsee_types::error::ErrorCode;
use tracing::error;

use crate::{
    DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT, PgTransactionEventSink, RejectedTransactionEventQuery,
    TransactionEventRecord,
};

/// RPC trait for the audit archiver.
#[rpc(server, namespace = "base")]
pub trait AuditArchiverApi {
    /// Returns Postgres-backed transaction event history for one transaction hash.
    #[method(name = "getTransactionEventsByHash")]
    async fn get_transaction_events_by_hash(
        &self,
        tx_hash: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>>;

    /// Returns Postgres-backed transaction/block event history for one block number.
    #[method(name = "getTransactionEventsByBlockNumber")]
    async fn get_transaction_events_by_block_number(
        &self,
        block_number: u64,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>>;

    /// Returns Postgres-backed transaction/block event history for one block hash.
    #[method(name = "getTransactionEventsByBlockHash")]
    async fn get_transaction_events_by_block_hash(
        &self,
        block_hash: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>>;

    /// Returns Postgres-backed transaction event history for one bundle UUID or hash.
    #[method(name = "getTransactionEventsByBundle")]
    async fn get_transaction_events_by_bundle(
        &self,
        bundle_key: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>>;

    /// Returns rejected transaction events by block/time range.
    #[method(name = "getRejectedTransactionEvents")]
    async fn get_rejected_transaction_events(
        &self,
        query: RejectedTransactionEventQuery,
    ) -> RpcResult<Vec<TransactionEventRecord>>;
}

/// RPC handler for audit archiver requests.
#[derive(Debug)]
pub struct AuditArchiverRpc {
    transaction_events: PgTransactionEventSink,
}

impl AuditArchiverRpc {
    /// Creates an RPC handler backed by the transaction event Postgres store.
    pub const fn new(transaction_events: PgTransactionEventSink) -> Self {
        Self { transaction_events }
    }
}

#[async_trait::async_trait]
impl AuditArchiverApiServer for AuditArchiverRpc {
    async fn get_transaction_events_by_hash(
        &self,
        tx_hash: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_events
            .events_by_transaction_hash(
                &tx_hash,
                limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT),
            )
            .await
            .map_err(internal_rpc_error)
    }

    async fn get_transaction_events_by_block_number(
        &self,
        block_number: u64,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_events
            .events_by_block_number(
                block_number,
                limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT),
            )
            .await
            .map_err(internal_rpc_error)
    }

    async fn get_transaction_events_by_block_hash(
        &self,
        block_hash: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_events
            .events_by_block_hash(
                &block_hash,
                limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT),
            )
            .await
            .map_err(internal_rpc_error)
    }

    async fn get_transaction_events_by_bundle(
        &self,
        bundle_key: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_events
            .events_by_bundle(&bundle_key, limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT))
            .await
            .map_err(internal_rpc_error)
    }

    async fn get_rejected_transaction_events(
        &self,
        query: RejectedTransactionEventQuery,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_events.rejected_transaction_events(query).await.map_err(internal_rpc_error)
    }
}

fn internal_rpc_error(error: anyhow::Error) -> ErrorObjectOwned {
    error!(error = %error, "transaction event query failed");
    ErrorObjectOwned::owned(
        ErrorCode::InternalError.code(),
        "internal server error".to_string(),
        None::<()>,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retired_write_rpcs_do_not_acknowledge_events() -> anyhow::Result<()> {
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/audit_test")?;
        let rpc = AuditArchiverRpc::new(PgTransactionEventSink::new(pool)).into_rpc();

        for method in ["base_persistBatchedBundleEvent", "base_persistRejectedTransactionBatch"] {
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": [[]],
                "id": 1,
            })
            .to_string();
            let (response, _subscriptions) = rpc.raw_json_request(&request, 1).await?;
            let response: serde_json::Value = serde_json::from_str(response.get())?;
            assert_eq!(response["error"]["code"], -32601, "{method} must fail explicitly");
            assert!(response.get("result").is_none(), "{method} must not claim persistence");
        }
        Ok(())
    }
}
