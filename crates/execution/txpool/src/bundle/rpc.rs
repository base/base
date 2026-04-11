use alloy_eips::Decodable2718;
use alloy_primitives::{Bytes, TxHash};
use base_alloy_consensus::BaseTransactionSigned;
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_primitives_traits::SignedTransaction;
use reth_transaction_pool::TransactionPool;
use tracing::debug;

use crate::{
    BasePooledTransaction,
    transaction::{MAX_BUNDLE_ADVANCE_BLOCKS, MAX_BUNDLE_ADVANCE_MILLIS},
};

/// `eth_sendBundle` RPC request.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendBundleRequest {
    /// Signed transaction(s) to include. Currently limited to exactly one.
    pub txs: Vec<Bytes>,
    /// Target block number. Must be at most [`MAX_BUNDLE_ADVANCE_BLOCKS`] ahead.
    #[serde(default)]
    pub block_number: Option<u64>,
    /// Minimum inclusion timestamp in milliseconds since Unix epoch.
    #[serde(default)]
    pub min_timestamp: Option<u64>,
    /// Maximum inclusion timestamp in milliseconds since Unix epoch.
    #[serde(default)]
    pub max_timestamp: Option<u64>,
    /// Not supported — must be empty.
    #[serde(default)]
    pub reverting_tx_hashes: Option<Vec<TxHash>>,
    /// Not supported — must be `None`.
    #[serde(default)]
    pub replacement_uuid: Option<String>,
    /// Not supported — must be empty.
    #[serde(default)]
    pub builders: Option<Vec<String>>,
}

#[rpc(server, namespace = "eth")]
pub trait SendBundleApi {
    /// Accepts a minimal bundle containing a single transaction.
    #[method(name = "sendBundle")]
    async fn send_bundle(&self, bundle: SendBundleRequest) -> RpcResult<TxHash>;
}

/// `eth_sendBundle` RPC handler backed by the transaction pool.
#[derive(Debug)]
pub struct SendBundleApiImpl<P> {
    pool: P,
    enabled: bool,
    /// The latest known block number, used to validate `blockNumber` in bundle
    /// requests. Callers must update this atomically (via [`Ordering::Release`]
    /// or stronger) each time a new canonical block is committed. Reads use
    /// [`Ordering::Relaxed`] because a slightly stale value is acceptable for
    /// validation bounds.
    ///
    /// [`Ordering::Release`]: std::sync::atomic::Ordering::Release
    /// [`Ordering::Relaxed`]: std::sync::atomic::Ordering::Relaxed
    current_block_number: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<P> SendBundleApiImpl<P> {
    /// Creates a new handler.
    pub const fn new(
        pool: P,
        enabled: bool,
        current_block_number: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self { pool, enabled, current_block_number }
    }
}

fn rpc_err(code: ErrorCode, msg: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(code.code(), msg.into(), None::<()>)
}

/// Validates the structural constraints on a [`SendBundleRequest`].
///
/// Returns `Ok(())` if all restrictions pass, or an RPC error describing the
/// first violated constraint.
fn validate_bundle_request(
    req: &SendBundleRequest,
    current_block: u64,
) -> Result<(), ErrorObjectOwned> {
    if req.txs.len() != 1 {
        return Err(rpc_err(
            ErrorCode::InvalidParams,
            format!("txs must contain exactly 1 transaction, got {}", req.txs.len()),
        ));
    }

    let now_ms = crate::transaction::unix_time_millis() as u64;

    if let Some(block_number) = req.block_number {
        if block_number < current_block {
            return Err(rpc_err(
                ErrorCode::InvalidParams,
                format!("blockNumber {block_number} is in the past (current {current_block})",),
            ));
        }
        let max_block = current_block + MAX_BUNDLE_ADVANCE_BLOCKS;
        if block_number > max_block {
            return Err(rpc_err(
                ErrorCode::InvalidParams,
                format!(
                    "blockNumber {block_number} is too far ahead (max {max_block}, current {current_block})",
                ),
            ));
        }
    }

    if let Some(min_ts) = req.min_timestamp {
        let max_allowed = now_ms + MAX_BUNDLE_ADVANCE_MILLIS;
        if min_ts > max_allowed {
            return Err(rpc_err(
                ErrorCode::InvalidParams,
                format!("minTimestamp {min_ts}ms is too far ahead (max {max_allowed}ms)"),
            ));
        }
    }

    if let Some(max_ts) = req.max_timestamp {
        if max_ts < now_ms {
            return Err(rpc_err(
                ErrorCode::InvalidParams,
                format!("maxTimestamp {max_ts}ms is in the past (now {now_ms}ms)"),
            ));
        }
        let max_allowed = now_ms + MAX_BUNDLE_ADVANCE_MILLIS;
        if max_ts > max_allowed {
            return Err(rpc_err(
                ErrorCode::InvalidParams,
                format!("maxTimestamp {max_ts}ms is too far ahead (max {max_allowed}ms)"),
            ));
        }
    }

    if let (Some(min_ts), Some(max_ts)) = (req.min_timestamp, req.max_timestamp)
        && min_ts > max_ts
    {
        return Err(rpc_err(
            ErrorCode::InvalidParams,
            format!("minTimestamp {min_ts}ms is after maxTimestamp {max_ts}ms"),
        ));
    }

    if req.reverting_tx_hashes.as_ref().is_some_and(|v| !v.is_empty()) {
        return Err(rpc_err(ErrorCode::InvalidParams, "revertingTxHashes is not supported"));
    }

    if req.replacement_uuid.is_some() {
        return Err(rpc_err(ErrorCode::InvalidParams, "replacementUuid is not supported"));
    }

    if req.builders.as_ref().is_some_and(|v| !v.is_empty()) {
        return Err(rpc_err(ErrorCode::InvalidParams, "builders is not supported"));
    }

    Ok(())
}

#[async_trait::async_trait]
impl<P> SendBundleApiServer for SendBundleApiImpl<P>
where
    P: TransactionPool<Transaction = BasePooledTransaction> + Send + Sync + 'static,
{
    async fn send_bundle(&self, bundle: SendBundleRequest) -> RpcResult<TxHash> {
        if !self.enabled {
            return Err(rpc_err(ErrorCode::MethodNotFound, "eth_sendBundle is not enabled"));
        }

        let current_block = self.current_block_number.load(std::sync::atomic::Ordering::Relaxed);
        validate_bundle_request(&bundle, current_block)?;

        let raw = &bundle.txs[0];
        let consensus_tx = BaseTransactionSigned::decode_2718(&mut raw.as_ref()).map_err(|e| {
            rpc_err(ErrorCode::InvalidParams, format!("failed to decode transaction: {e:?}"))
        })?;

        let recovered = consensus_tx.try_into_recovered().map_err(|e| {
            rpc_err(ErrorCode::InvalidParams, format!("failed to recover signer: {e:?}"))
        })?;

        let tx_hash = TxHash::from(*recovered.tx_hash());
        let encoded_len = raw.len();

        let pool_tx = BasePooledTransaction::new(recovered, encoded_len).with_bundle_metadata(
            bundle.block_number,
            bundle.min_timestamp,
            bundle.max_timestamp,
        );

        debug!(
            tx_hash = %tx_hash,
            target_block = ?bundle.block_number,
            min_timestamp = ?bundle.min_timestamp,
            max_timestamp = ?bundle.max_timestamp,
            "eth_sendBundle",
        );

        self.pool.add_external_transaction(pool_tx).await.map_err(|e| {
            rpc_err(ErrorCode::InternalError, format!("pool rejected transaction: {e}"))
        })?;

        Ok(tx_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_txs() {
        let req = SendBundleRequest {
            txs: vec![],
            block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("exactly 1 transaction"));
    }

    #[test]
    fn rejects_multiple_txs() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
            block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("exactly 1 transaction"));
    }

    #[test]
    fn rejects_block_number_too_far_ahead() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: Some(200),
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("too far ahead"));
    }

    #[test]
    fn accepts_block_number_within_range() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: Some(130),
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        assert!(validate_bundle_request(&req, 100).is_ok());
    }

    #[test]
    fn rejects_reverting_tx_hashes() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: Some(vec![TxHash::ZERO]),
            replacement_uuid: None,
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("revertingTxHashes"));
    }

    #[test]
    fn rejects_replacement_uuid() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: Some("abc".to_string()),
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("replacementUuid"));
    }

    #[test]
    fn rejects_builders() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: Some(vec!["builder1".to_string()]),
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("builders"));
    }

    #[test]
    fn rejects_block_number_in_the_past() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: Some(50),
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("in the past"));
    }

    #[test]
    fn rejects_max_timestamp_in_the_past() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: None,
            min_timestamp: None,
            max_timestamp: Some(1),
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("in the past"));
    }

    #[test]
    fn rejects_min_after_max_timestamp() {
        let now = crate::transaction::unix_time_millis() as u64;
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: None,
            min_timestamp: Some(now + 30_000),
            max_timestamp: Some(now + 10_000),
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert!(err.message().contains("after maxTimestamp"));
    }

    #[test]
    fn accepts_minimal_valid_request() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        assert!(validate_bundle_request(&req, 100).is_ok());
    }
}
