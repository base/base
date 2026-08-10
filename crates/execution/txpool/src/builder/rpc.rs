use std::time::Instant;

use alloy_consensus::transaction::Recovered;
use alloy_eips::Decodable2718;
use alloy_primitives::TxHash;
use base_common_consensus::BaseTransactionSigned;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_transaction_pool::TransactionPool;
use serde_json::{Map, json};
use tracing::debug;

use super::metrics::Metrics as BuilderApiMetrics;
use crate::{
    BasePooledTransaction, PoolRejectionLabel, SharedMeteringResponseSink, ValidatedTransaction,
};

/// RPC interface for submitting pre-validated transactions to a block builder.
#[rpc(server, namespace = "base")]
pub trait BuilderApi {
    /// Inserts a single pre-validated transaction into the builder's pool.
    ///
    /// The transaction is EIP-2718 encoded with its sender address pre-recovered,
    /// allowing the builder to skip signature verification.
    ///
    /// Returns an error if decoding or pool insertion fails.
    /// Use JSON-RPC batch requests for efficient bulk submission.
    #[method(name = "insertValidatedTransaction")]
    async fn insert_validated_transaction(&self, tx: ValidatedTransaction) -> RpcResult<()>;
}

/// Server implementation of [`BuilderApi`] backed by a transaction pool.
///
/// This handler decodes incoming pre-validated transactions and inserts them
/// directly into the pool without re-validating signatures, since the sender
/// addresses are trusted from the forwarding mempool node.
#[derive(Debug)]
pub struct BuilderApiImpl<P> {
    pool: P,
    metering: Option<SharedMeteringResponseSink>,
}

impl<P> BuilderApiImpl<P> {
    /// Creates a new handler backed by the given transaction pool.
    pub const fn new(pool: P) -> Self {
        Self { pool, metering: None }
    }

    /// Creates a handler that also inserts metering responses into the builder store.
    pub fn with_metering(pool: P, metering: SharedMeteringResponseSink) -> Self {
        Self { pool, metering: Some(metering) }
    }
}

#[async_trait::async_trait]
impl<P> BuilderApiServer for BuilderApiImpl<P>
where
    P: TransactionPool<Transaction = BasePooledTransaction> + Send + Sync + 'static,
{
    async fn insert_validated_transaction(&self, tx: ValidatedTransaction) -> RpcResult<()> {
        debug!(
            sender = %tx.sender,
            has_metering = tx.meter_bundle_response.is_some(),
            "rpc::insert_validated_transaction"
        );
        let sender = tx.sender;
        let meter_bundle_response = tx.meter_bundle_response;

        // Decode the EIP-2718 transaction bytes
        let consensus_tx =
            BaseTransactionSigned::decode_2718(&mut tx.raw.as_ref()).map_err(|e| {
                BuilderApiMetrics::decode_errors().increment(1);
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    format!("failed to decode transaction: {e}"),
                    None::<()>,
                )
            })?;
        let tx_hash = *consensus_tx.hash();
        let encoded_len = tx.raw.len();

        if let (Some(metering), Some(response)) = (self.metering.as_ref(), meter_bundle_response) {
            metering.insert(tx_hash, response);
        }

        let recovered = Recovered::new_unchecked(consensus_tx, sender);
        let pool_tx = BasePooledTransaction::new(recovered, encoded_len).with_bundle_metadata(
            tx.min_block_number,
            tx.max_block_number,
            tx.min_timestamp,
            tx.max_timestamp,
        );

        // Insert into the pool
        let start = Instant::now();
        let result = self.pool.add_external_transaction(pool_tx).await;
        BuilderApiMetrics::insert_duration().record(start.elapsed().as_secs_f64());

        match result {
            Ok(_) => {
                self.emit_validated_insert_event(
                    TransactionEventType::TxpoolValidatedInsertAccepted,
                    tx_hash,
                    Map::from_iter([("encoded_len".to_string(), json!(encoded_len))]),
                );
                debug!(sender = %sender, "inserted validated transaction");
                BuilderApiMetrics::txs_inserted().increment(1);
                Ok(())
            }
            Err(e) => {
                self.emit_validated_insert_event(
                    TransactionEventType::TxpoolValidatedInsertRejected,
                    tx_hash,
                    Map::from_iter([
                        ("rejection_stage".to_string(), json!("pool_insert")),
                        ("encoded_len".to_string(), json!(encoded_len)),
                        ("error".to_string(), json!(e.to_string())),
                    ]),
                );
                debug!(sender = %sender, error = %e, "pool rejected transaction");
                BuilderApiMetrics::txs_rejected(PoolRejectionLabel::from_error(&e)).increment(1);
                Err(ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("pool rejected transaction: {e}"),
                    None::<()>,
                ))
            }
        }
    }
}

impl<P> BuilderApiImpl<P> {
    fn emit_validated_insert_event(
        &self,
        event_type: TransactionEventType,
        tx_hash: TxHash,
        mut data: Map<String, serde_json::Value>,
    ) {
        data.entry("rpc_method".to_string())
            .or_insert_with(|| json!("base_insertValidatedTransaction"));

        let _ = transaction_event!(
            producer: TransactionEventProducer::BaseBuilder,
            event_type: event_type,
            tx_hash: tx_hash,
            id: {
                "tx_hash" => format!("{tx_hash:#x}"),
            },
            data: data,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::TxEip1559;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction, TxDeposit};
    use reth_transaction_pool::noop::NoopTransactionPool;

    use super::*;
    use crate::{BasePooledTransaction, ValidatedTransaction};

    // ==========================================================================
    // Helper functions for creating test transactions
    // ==========================================================================

    /// Creates a valid EIP-2718 encoded deposit transaction.
    fn create_deposit_tx() -> (Address, Bytes) {
        let sender = Address::repeat_byte(0x42);
        let deposit_tx = TxDeposit {
            source_hash: Default::default(),
            from: sender,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21000,
            is_system_transaction: false,
            input: Default::default(),
        };
        let signed_tx: BaseTransactionSigned = deposit_tx.into();
        let encoded = signed_tx.encoded_2718();
        (sender, Bytes::from(encoded))
    }

    /// Creates a valid EIP-1559 transaction with a fake signature.
    fn create_eip1559_tx() -> (Address, Bytes) {
        let sender = Address::repeat_byte(0xAB);
        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::from(1000),
            access_list: Default::default(),
            input: Default::default(),
        };
        let sig = Signature::new(U256::from(1), U256::from(2), false);
        let signed = BaseTransactionSigned::new_unhashed(BaseTypedTransaction::Eip1559(tx), sig);
        let encoded = signed.encoded_2718();
        (sender, Bytes::from(encoded))
    }

    fn handler() -> BuilderApiImpl<NoopTransactionPool<BasePooledTransaction>> {
        BuilderApiImpl::new(NoopTransactionPool::<BasePooledTransaction>::new())
    }

    // ==========================================================================
    // Decode error tests
    // ==========================================================================

    #[tokio::test]
    async fn decode_invalid_bytes_returns_invalid_params() {
        let handler = handler();

        let tx = ValidatedTransaction {
            sender: Address::repeat_byte(0x01),
            raw: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: None,
        };

        let result = handler.insert_validated_transaction(tx).await;
        assert!(result.is_err(), "expected decode error for invalid bytes");

        let err = result.unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::InvalidParams.code(),
            "expected InvalidParams error code (-32602)"
        );
        assert!(
            err.message().contains("failed to decode"),
            "error message should mention decode failure: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn decode_empty_bytes_returns_invalid_params() {
        let handler = handler();

        let tx = ValidatedTransaction {
            sender: Address::ZERO,
            raw: Bytes::new(),
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: None,
        };

        let result = handler.insert_validated_transaction(tx).await;
        assert!(result.is_err(), "expected decode error for empty bytes");

        let err = result.unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::InvalidParams.code(),
            "expected InvalidParams for empty input"
        );
    }

    #[tokio::test]
    async fn decode_truncated_tx_returns_invalid_params() {
        let handler = handler();

        // Create valid tx then truncate it
        let (sender, full_raw) = create_deposit_tx();
        let truncated = Bytes::from(full_raw[..full_raw.len() / 2].to_vec());

        let tx = ValidatedTransaction {
            sender,
            raw: truncated,
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: None,
        };

        let result = handler.insert_validated_transaction(tx).await;
        assert!(result.is_err(), "expected decode error for truncated tx");

        let err = result.unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::InvalidParams.code(),
            "truncated tx should fail decoding"
        );
    }

    #[tokio::test]
    async fn decode_invalid_type_byte_returns_invalid_params() {
        let handler = handler();

        // Type byte 0xFF is not a valid EIP-2718 tx type
        let tx = ValidatedTransaction {
            sender: Address::repeat_byte(0x01),
            raw: Bytes::from_static(&[0xFF, 0x01, 0x02, 0x03]),
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: None,
        };

        let result = handler.insert_validated_transaction(tx).await;
        assert!(result.is_err(), "expected decode error for invalid type byte");

        let err = result.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParams.code());
    }

    #[tokio::test]
    async fn decode_single_byte_returns_invalid_params() {
        let handler = handler();

        let tx = ValidatedTransaction {
            sender: Address::ZERO,
            raw: Bytes::from_static(&[0x02]), // Just type byte, no payload
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: None,
        };

        let result = handler.insert_validated_transaction(tx).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParams.code());
    }

    #[tokio::test]
    async fn decode_legacy_tx_type_byte_returns_invalid_params() {
        let handler = handler();

        // Legacy tx (type 0x00) followed by invalid RLP
        let tx = ValidatedTransaction {
            sender: Address::ZERO,
            raw: Bytes::from_static(&[0x00, 0x01, 0x02]),
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: None,
        };

        let result = handler.insert_validated_transaction(tx).await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParams.code());
    }

    #[tokio::test]
    async fn decode_valid_eip1559_tx() {
        let handler = handler();

        let (sender, raw) = create_eip1559_tx();
        let tx = ValidatedTransaction {
            sender,
            raw,
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: None,
        };

        let result = handler.insert_validated_transaction(tx).await;
        let err = result.unwrap_err();
        // Decode should succeed, but the txpool is a noop so it will reject the tx
        // This error code should be InternalError
        assert_eq!(err.code(), ErrorCode::InternalError.code());
    }

    #[derive(Debug, Default)]
    struct RecordingMeteringSink {
        inserted: std::sync::Mutex<Vec<(TxHash, base_bundles::MeterBundleResponse)>>,
    }

    impl crate::MeteringResponseSink for RecordingMeteringSink {
        fn insert(&self, tx_hash: TxHash, metering: base_bundles::MeterBundleResponse) {
            self.inserted.lock().unwrap().push((tx_hash, metering));
        }
    }

    #[tokio::test]
    async fn insert_with_metering_response_stores_metering() {
        let sink = Arc::new(RecordingMeteringSink::default());
        let handler = BuilderApiImpl::with_metering(
            NoopTransactionPool::<BasePooledTransaction>::new(),
            Arc::clone(&sink),
        );

        let (sender, raw) = create_eip1559_tx();
        let consensus =
            BaseTransactionSigned::decode_2718(&mut raw.as_ref()).expect("valid eip1559");
        let tx_hash = *consensus.hash();
        let response = base_bundles::MeterBundleResponse {
            total_execution_time_us: 1234,
            ..Default::default()
        };

        let tx = ValidatedTransaction {
            sender,
            raw,
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: Some(response.clone()),
        };

        let _ = handler.insert_validated_transaction(tx).await;

        let inserted = sink.inserted.lock().unwrap();
        assert_eq!(inserted.len(), 1, "exactly one metering insert");
        assert_eq!(inserted[0].0, tx_hash, "metering keyed by tx hash");
        assert_eq!(inserted[0].1.total_execution_time_us, 1234, "metering response preserved");
    }

    #[tokio::test]
    async fn insert_without_metering_response_skips_sink() {
        let sink = std::sync::Arc::new(RecordingMeteringSink::default());
        let handler = BuilderApiImpl::with_metering(
            NoopTransactionPool::<BasePooledTransaction>::new(),
            Arc::clone(&sink),
        );

        let (sender, raw) = create_eip1559_tx();
        let tx = ValidatedTransaction {
            sender,
            raw,
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            meter_bundle_response: None,
        };

        let _ = handler.insert_validated_transaction(tx).await;
        assert!(
            sink.inserted.lock().unwrap().is_empty(),
            "no metering insert when response absent"
        );
    }
}
