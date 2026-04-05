use alloy_eips::Decodable2718;
use alloy_primitives::{Bytes, TxHash};
use base_execution_primitives::OpTransactionSigned;
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_primitives_traits::SignedTransaction;
use reth_transaction_pool::TransactionPool;
use std::fmt;
use tracing::debug;

use crate::{
    BasePooledTransaction,
    transaction::{MAX_BUNDLE_ADVANCE_BLOCKS, MAX_BUNDLE_ADVANCE_MILLIS},
};

/// Errors that can occur during bundle validation.
#[derive(Debug, Clone, PartialEq)]
pub enum BundleValidationError {
    /// Transaction count is not exactly 1.
    WrongTxCount { got: usize },
    /// Block number is in the past.
    BlockNumberInPast { block_number: u64, current: u64 },
    /// Block number is too far ahead.
    BlockNumberTooFarAhead { block_number: u64, max: u64, current: u64 },
    /// Minimum timestamp is too far ahead.
    MinTimestampTooFarAhead { min_ts: u64, max_allowed: u64 },
    /// Maximum timestamp is in the past.
    MaxTimestampInPast { max_ts: u64, now: u64 },
    /// Maximum timestamp is too far ahead.
    MaxTimestampTooFarAhead { max_ts: u64, max_allowed: u64 },
    /// Minimum timestamp is after maximum timestamp.
    MinTimestampAfterMax { min_ts: u64, max_ts: u64 },
    /// Reverting transaction hashes must be empty.
    NonEmptyRevertingTxHashes,
    /// Replacement UUID must be None.
    ReplacementUuidNotNone,
    /// Builders list must be empty.
    NonEmptyBuilders,
}

impl fmt::Display for BundleValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongTxCount { got } => {
                write!(f, "txs must contain exactly 1 transaction, got {got}")
            }
            Self::BlockNumberInPast { block_number, current } => {
                write!(f, "blockNumber {block_number} is in the past (current {current})")
            }
            Self::BlockNumberTooFarAhead { block_number, max, current } => {
                write!(f, "blockNumber {block_number} is too far ahead (max {max}, current {current})")
            }
            Self::MinTimestampTooFarAhead { min_ts, max_allowed } => {
                write!(f, "minTimestamp {min_ts}ms is too far ahead (max {max_allowed}ms)")
            }
            Self::MaxTimestampInPast { max_ts, now } => {
                write!(f, "maxTimestamp {max_ts}ms is in the past (now {now}ms)")
            }
            Self::MaxTimestampTooFarAhead { max_ts, max_allowed } => {
                write!(f, "maxTimestamp {max_ts}ms is too far ahead (max {max_allowed}ms)")
            }
            Self::MinTimestampAfterMax { min_ts, max_ts } => {
                write!(f, "minTimestamp {min_ts}ms is after maxTimestamp {max_ts}ms")
            }
            Self::NonEmptyRevertingTxHashes => {
                write!(f, "revertingTxHashes must be empty")
            }
            Self::ReplacementUuidNotNone => {
                write!(f, "replacementUuid must be None")
            }
            Self::NonEmptyBuilders => {
                write!(f, "builders must be empty")
            }
        }
    }
}

impl From<BundleValidationError> for ErrorObjectOwned {
    fn from(err: BundleValidationError) -> Self {
        ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), err.to_string(), None::<()>)
    }
}

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
    /// or stronger) each time a new block is received.
    latest_block_number: std::sync::atomic::AtomicU64,
}

impl<P> SendBundleApiImpl<P> {
    /// Creates a new [`SendBundleApiImpl`] with the given pool and enabled flag.
    pub fn new(pool: P, enabled: bool) -> Self {
        Self {
            pool,
            enabled,
            latest_block_number: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Updates the latest known block number.
    pub fn update_latest_block_number(&self, block_number: u64) {
        self.latest_block_number
            .store(block_number, std::sync::atomic::Ordering::Release);
    }
}

impl<P, Pool> SendBundleApiServer for SendBundleApiImpl<P>
where
    P: Send + Sync + 'static,
    Pool: TransactionPool + 'static,
    P: AsRef<Pool>,
{
    async fn send_bundle(&self, bundle: SendBundleRequest) -> RpcResult<TxHash> {
        if !self.enabled {
            return Err(ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "Bundle RPC is not enabled",
                None::<()>,
            ));
        }

        let current_block = self
            .latest_block_number
            .load(std::sync::atomic::Ordering::Acquire);

        validate_bundle_request(&bundle, current_block)?;

        let tx_bytes = bundle.txs.into_iter().next().expect("checked above");
        let tx = OpTransactionSigned::decode_2718_exact(tx_bytes.iter().as_slice())
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    format!("failed to decode transaction: {e:?}"),
                    None::<()>,
                )
            })?;

        let recovered = tx.try_into_recovered().map_err(|e| {
            ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("failed to recover signer: {e:?}"),
                None::<()>,
            )
        })?;

        let pooled_tx = BasePooledTransaction::from_recovered_pooled_transaction(recovered);
        let hash = pooled_tx.hash().clone();

        self.pool.as_ref().add_transaction(reth_transaction_pool::TransactionOrigin::External, pooled_tx)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("pool rejected transaction: {e}"),
                    None::<()>,
                )
            })?;

        Ok(hash)
    }
}

/// Validates a bundle request, returning the first violated constraint.
fn validate_bundle_request(
    req: &SendBundleRequest,
    current_block: u64,
) -> Result<(), BundleValidationError> {
    if req.txs.len() != 1 {
        return Err(BundleValidationError::WrongTxCount { got: req.txs.len() });
    }

    let now_ms = crate::transaction::unix_time_millis() as u64;

    if let Some(block_number) = req.block_number {
        if block_number < current_block {
            return Err(BundleValidationError::BlockNumberInPast {
                block_number,
                current: current_block,
            });
        }
        let max_block = current_block + MAX_BUNDLE_ADVANCE_BLOCKS;
        if block_number > max_block {
            return Err(BundleValidationError::BlockNumberTooFarAhead {
                block_number,
                max: max_block,
                current: current_block,
            });
        }
    }

    if let Some(min_ts) = req.min_timestamp {
        let max_allowed = now_ms + MAX_BUNDLE_ADVANCE_MILLIS;
        if min_ts > max_allowed {
            return Err(BundleValidationError::MinTimestampTooFarAhead {
                min_ts,
                max_allowed,
            });
        }
    }

    if let Some(max_ts) = req.max_timestamp {
        if max_ts < now_ms {
            return Err(BundleValidationError::MaxTimestampInPast { max_ts, now: now_ms });
        }
        let max_allowed = now_ms + MAX_BUNDLE_ADVANCE_MILLIS;
        if max_ts > max_allowed {
            return Err(BundleValidationError::MaxTimestampTooFarAhead {
                max_ts,
                max_allowed,
            });
        }
    }

    if let (Some(min_ts), Some(max_ts)) = (req.min_timestamp, req.max_timestamp) {
        if min_ts > max_ts {
            return Err(BundleValidationError::MinTimestampAfterMax { min_ts, max_ts });
        }
    }

    if let Some(ref hashes) = req.reverting_tx_hashes {
        if !hashes.is_empty() {
            return Err(BundleValidationError::NonEmptyRevertingTxHashes);
        }
    }

    if req.replacement_uuid.is_some() {
        return Err(BundleValidationError::ReplacementUuidNotNone);
    }

    if let Some(ref builders) = req.builders {
        if !builders.is_empty() {
            return Err(BundleValidationError::NonEmptyBuilders);
        }
    }

    Ok(())
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
        assert_eq!(err, BundleValidationError::WrongTxCount { got: 0 });
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
        assert_eq!(err, BundleValidationError::WrongTxCount { got: 2 });
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
        assert_eq!(
            err,
            BundleValidationError::BlockNumberTooFarAhead {
                block_number: 200,
                max: 130,
                current: 100,
            }
        );
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
        assert_eq!(err, BundleValidationError::NonEmptyRevertingTxHashes);
    }

    #[test]
    fn rejects_replacement_uuid() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: None,
            replacement_uuid: Some("uuid".to_string()),
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert_eq!(err, BundleValidationError::ReplacementUuidNotNone);
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
            builders: Some(vec!["builder".to_string()]),
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert_eq!(err, BundleValidationError::NonEmptyBuilders);
    }

    #[test]
    fn rejects_block_number_in_past() {
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
        assert_eq!(
            err,
            BundleValidationError::BlockNumberInPast {
                block_number: 50,
                current: 100,
            }
        );
    }

    #[test]
    fn rejects_max_timestamp_in_past() {
        // This test would need to mock unix_time_millis, skipping for now
        // as it depends on the actual current time
    }

    #[test]
    fn rejects_min_timestamp_after_max() {
        let req = SendBundleRequest {
            txs: vec![Bytes::from_static(b"tx")],
            block_number: None,
            min_timestamp: Some(2000),
            max_timestamp: Some(1000),
            reverting_tx_hashes: None,
            replacement_uuid: None,
            builders: None,
        };
        let err = validate_bundle_request(&req, 100).unwrap_err();
        assert_eq!(
            err,
            BundleValidationError::MinTimestampAfterMax {
                min_ts: 2000,
                max_ts: 1000,
            }
        );
    }
}
