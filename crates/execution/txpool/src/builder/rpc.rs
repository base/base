use std::{marker::PhantomData, sync::Arc, time::Instant};

use alloy_consensus::transaction::Recovered;
use alloy_eips::Decodable2718;
use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use base_common_consensus::BaseTransactionSigned;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_transaction_pool::{TransactionOrigin, TransactionPool};
use serde_json::{Map, json};
use tracing::debug;

use super::metrics::Metrics as BuilderApiMetrics;
use crate::{
    BasePooledTransaction, NoExtensions, PoolRejectionLabel, ValidatedTransaction,
    ValidatedTransactionExtensions,
};

/// Writes inbound `insertValidatedTransaction` metering into the builder cache.
pub trait InsertMetering: core::fmt::Debug + Send + Sync + 'static {
    /// Stores metering for `tx_hash`.
    fn insert_metering(&self, tx_hash: TxHash, metering: MeterBundleResponse);
}

/// RPC interface for submitting pre-validated transactions to a block builder.
///
/// `E` is the wire extension payload; see [`ValidatedTransactionExtensions`].
#[rpc(server, namespace = "base")]
pub trait BuilderApi<E> {
    /// Inserts a single pre-validated transaction into the builder's pool.
    ///
    /// The transaction is EIP-2718 encoded with its sender address pre-recovered,
    /// allowing the builder to skip signature verification.
    ///
    /// Returns an error if decoding or pool insertion fails.
    /// Use JSON-RPC batch requests for efficient bulk submission.
    #[method(name = "insertValidatedTransaction")]
    async fn insert_validated_transaction(&self, tx: ValidatedTransaction<E>) -> RpcResult<()>;
}

/// Server implementation of [`BuilderApi`] backed by a transaction pool.
///
/// This handler decodes incoming pre-validated transactions and inserts them
/// directly into the pool without re-validating signatures, since the sender
/// addresses are trusted from the forwarding mempool node.
#[derive(Debug)]
pub struct BuilderApiImpl<P, E = NoExtensions> {
    pool: P,
    accept_extensions: bool,
    max_extension_items: usize,
    metering_cache: Option<Arc<dyn InsertMetering>>,
    _extensions: PhantomData<E>,
}

impl<P> BuilderApiImpl<P, NoExtensions> {
    /// Creates a new handler backed by the given transaction pool.
    ///
    /// This constructor is defined only for [`NoExtensions`] so that
    /// `BuilderApiImpl::new(pool)` resolves without a type annotation. A single
    /// constructor on the generic impl would leave `E` unconstrained at every
    /// call site (`E0282`), because type-parameter defaults do not participate
    /// in inference for associated-function calls.
    pub const fn new(pool: P) -> Self {
        Self {
            pool,
            accept_extensions: false,
            max_extension_items: 0,
            metering_cache: None,
            _extensions: PhantomData,
        }
    }
}

impl<P, E> BuilderApiImpl<P, E> {
    /// Creates a new handler carrying the wire extension payload `E`.
    ///
    /// Non-empty extension payloads are rejected unless `accept_extensions` is
    /// explicitly enabled. Extension payloads validate `max_extension_items`
    /// according to their own item semantics.
    pub const fn with_extensions(
        pool: P,
        accept_extensions: bool,
        max_extension_items: usize,
    ) -> Self {
        Self {
            pool,
            accept_extensions,
            max_extension_items,
            metering_cache: None,
            _extensions: PhantomData,
        }
    }

    /// Writes inbound metering into the builder cache on insert.
    #[must_use]
    pub fn with_metering_cache(mut self, cache: Arc<dyn InsertMetering>) -> Self {
        self.metering_cache = Some(cache);
        self
    }
}

#[async_trait::async_trait]
impl<P, E> BuilderApiServer<E> for BuilderApiImpl<P, E>
where
    P: TransactionPool<Transaction = BasePooledTransaction> + Send + Sync + 'static,
    E: ValidatedTransactionExtensions<BasePooledTransaction>,
{
    async fn insert_validated_transaction(&self, tx: ValidatedTransaction<E>) -> RpcResult<()> {
        debug!(
            sender = %tx.sender,
            "rpc::insert_validated_transaction"
        );
        let sender = tx.sender;
        let has_extensions = !tx.extensions.is_empty();

        if has_extensions && !self.accept_extensions {
            BuilderApiMetrics::extension_errors().increment(1);
            return Err(ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                "transaction extensions are disabled",
                None::<()>,
            ));
        }

        tx.extensions.validate(self.max_extension_items).map_err(|e| {
            BuilderApiMetrics::extension_errors().increment(1);
            ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), e.to_string(), None::<()>)
        })?;

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

        let recovered = Recovered::new_unchecked(consensus_tx, sender);
        let pool_tx = BasePooledTransaction::new(recovered, encoded_len).with_bundle_metadata(
            tx.min_block_number,
            tx.max_block_number,
            tx.min_timestamp,
            tx.max_timestamp,
        );
        // `MeterBundleResponse::default` is a mempool placeholder after sim
        // timeout/error. Keep it on the pooled tx; do not treat it as real cache data.
        let pool_tx = match tx.metering {
            Some(metering) if metering != MeterBundleResponse::default() => {
                if let Some(cache) = &self.metering_cache {
                    cache.insert_metering(tx_hash, metering.clone());
                }
                pool_tx.with_metering(metering)
            }
            Some(metering) => pool_tx.with_metering(metering),
            None => pool_tx,
        };

        // Attach any extension data carried on the wire. This is a no-op for
        // `NoExtensions`, the default payload.
        let pool_tx = tx.extensions.apply(pool_tx).map_err(|e| {
            BuilderApiMetrics::extension_errors().increment(1);
            ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), e.to_string(), None::<()>)
        })?;

        // Extension-bearing transactions remain private so normal P2P gossip
        // cannot propagate the raw transaction without its extension metadata.
        let start = Instant::now();
        let result = if has_extensions {
            self.pool.add_transaction(TransactionOrigin::Private, pool_tx).await
        } else {
            self.pool.add_external_transaction(pool_tx).await
        };
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

impl<P, E> BuilderApiImpl<P, E> {
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
    use std::sync::{Arc, Mutex};

    use alloy_consensus::TxEip1559;
    use alloy_eips::eip2718::{Decodable2718, Encodable2718};
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use base_bundles::MeterBundleResponse;
    use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction, TxDeposit};
    use reth_transaction_pool::noop::NoopTransactionPool;

    use super::*;
    use crate::{BasePooledTransaction, NoExtensions, ValidatedTransaction};

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

    fn validated_transaction<E>(
        sender: Address,
        raw: Bytes,
        extensions: E,
    ) -> ValidatedTransaction<E> {
        ValidatedTransaction {
            sender,
            raw,
            min_block_number: None,
            max_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
            metering: None,
            extensions,
        }
    }

    // ==========================================================================
    // Wire extension tests
    // ==========================================================================

    /// Extension payload that records whether `apply` ran, and can be told to
    /// fail so the handler's error mapping is observable.
    #[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
    struct TestExtensions {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reject: Option<bool>,
    }

    impl ValidatedTransactionExtensions<BasePooledTransaction> for TestExtensions {
        fn is_empty(&self) -> bool {
            self.reject.is_none()
        }

        fn extract(
            _tx: &reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>,
        ) -> Self {
            Self::default()
        }

        fn apply(
            self,
            tx: BasePooledTransaction,
        ) -> Result<BasePooledTransaction, crate::ExtensionError> {
            if self.reject == Some(true) {
                return Err(crate::ExtensionError("rejected by test extension".to_string()));
            }
            Ok(tx)
        }
    }

    fn extension_handler(
        accept_extensions: bool,
    ) -> BuilderApiImpl<NoopTransactionPool<BasePooledTransaction>, TestExtensions> {
        BuilderApiImpl::<_, TestExtensions>::with_extensions(
            NoopTransactionPool::<BasePooledTransaction>::new(),
            accept_extensions,
            usize::MAX,
        )
    }

    #[tokio::test]
    async fn extension_apply_failure_returns_invalid_params() {
        let handler = extension_handler(true);
        let (sender, raw) = create_eip1559_tx();

        let tx = validated_transaction(sender, raw, TestExtensions { reject: Some(true) });

        let err = handler.insert_validated_transaction(tx).await.unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::InvalidParams.code(),
            "a failing extension must surface as InvalidParams, not reach the pool"
        );
        assert!(
            err.message().contains("rejected by test extension"),
            "extension error should be propagated: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn extension_apply_success_reaches_the_pool() {
        let handler = extension_handler(true);
        let (sender, raw) = create_eip1559_tx();

        let tx = validated_transaction(sender, raw, TestExtensions { reject: None });

        let err = handler.insert_validated_transaction(tx).await.unwrap_err();
        // The extension passed, so the request got as far as the noop pool,
        // which rejects everything with an internal error.
        assert_eq!(
            err.code(),
            ErrorCode::InternalError.code(),
            "a passing extension must fall through to pool insertion"
        );
    }

    #[tokio::test]
    async fn disabled_extensions_reject_non_empty_payloads() {
        let handler = extension_handler(false);
        let (sender, raw) = create_eip1559_tx();
        let tx = validated_transaction(sender, raw, TestExtensions { reject: Some(false) });

        let err = handler.insert_validated_transaction(tx).await.unwrap_err();

        assert_eq!(err.code(), ErrorCode::InvalidParams.code());
        assert_eq!(err.message(), "transaction extensions are disabled");
    }

    #[tokio::test]
    async fn disabled_extensions_allow_empty_payloads() {
        let handler = extension_handler(false);
        let (sender, raw) = create_eip1559_tx();
        let tx = validated_transaction(sender, raw, TestExtensions::default());

        let err = handler.insert_validated_transaction(tx).await.unwrap_err();

        assert_eq!(err.code(), ErrorCode::InternalError.code());
    }

    #[tokio::test]
    async fn metering_does_not_require_extension_opt_in() {
        let handler = handler();
        let (sender, raw) = create_eip1559_tx();
        let mut tx = validated_transaction(sender, raw, NoExtensions {});
        tx.metering = Some(MeterBundleResponse::default());

        let err = handler.insert_validated_transaction(tx).await.unwrap_err();

        assert_eq!(
            err.code(),
            ErrorCode::InternalError.code(),
            "metering must not be gated on experimental validity extensions"
        );
    }

    #[derive(Debug, Default)]
    struct RecordingMetering {
        inserted: Mutex<Vec<(TxHash, MeterBundleResponse)>>,
    }

    impl InsertMetering for RecordingMetering {
        fn insert_metering(&self, tx_hash: TxHash, metering: MeterBundleResponse) {
            self.inserted.lock().expect("recording lock").push((tx_hash, metering));
        }
    }

    #[tokio::test]
    async fn insert_writes_metering_to_the_builder_cache() {
        let cache = Arc::new(RecordingMetering::default());
        let handler = handler().with_metering_cache(Arc::clone(&cache) as Arc<dyn InsertMetering>);
        let (sender, raw) = create_eip1559_tx();
        let metering = MeterBundleResponse {
            total_gas_used: 21_000,
            total_execution_time_us: 500,
            ..MeterBundleResponse::default()
        };
        let expected_hash = *BaseTransactionSigned::decode_2718(&mut raw.as_ref()).unwrap().hash();
        let mut tx = validated_transaction(sender, raw, NoExtensions {});
        tx.metering = Some(metering.clone());

        let _ = handler.insert_validated_transaction(tx).await;

        let inserted = cache.inserted.lock().expect("recording lock");
        assert_eq!(inserted.len(), 1, "insert should write metering even if the pool later rejects");
        assert_eq!(inserted[0].0, expected_hash);
        assert_eq!(inserted[0].1, metering);
    }

    #[tokio::test]
    async fn insert_skips_default_metering_in_the_builder_cache() {
        let cache = Arc::new(RecordingMetering::default());
        let handler = handler().with_metering_cache(Arc::clone(&cache) as Arc<dyn InsertMetering>);
        let (sender, raw) = create_eip1559_tx();
        let mut tx = validated_transaction(sender, raw, NoExtensions {});
        tx.metering = Some(MeterBundleResponse::default());

        let _ = handler.insert_validated_transaction(tx).await;

        let inserted = cache.inserted.lock().expect("recording lock");
        assert!(
            inserted.is_empty(),
            "placeholder MeterBundleResponse::default must not enter the builder cache"
        );
    }

    #[test]
    fn both_monomorphizations_build_rpc_modules() {
        // The stock call-site shape must keep working untouched...
        let _stock = handler().into_rpc();
        // ...and a custom extension payload must also produce a module.
        let _custom = extension_handler(true).into_rpc();
    }

    // ==========================================================================
    // Decode error tests
    // ==========================================================================

    #[tokio::test]
    async fn decode_invalid_bytes_returns_invalid_params() {
        let handler = handler();

        let tx = validated_transaction(
            Address::repeat_byte(0x01),
            Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            NoExtensions {},
        );

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

        let tx = validated_transaction(Address::ZERO, Bytes::new(), NoExtensions {});

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

        let tx = validated_transaction(sender, truncated, NoExtensions {});

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
        let tx = validated_transaction(
            Address::repeat_byte(0x01),
            Bytes::from_static(&[0xFF, 0x01, 0x02, 0x03]),
            NoExtensions {},
        );

        let result = handler.insert_validated_transaction(tx).await;
        assert!(result.is_err(), "expected decode error for invalid type byte");

        let err = result.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParams.code());
    }

    #[tokio::test]
    async fn decode_single_byte_returns_invalid_params() {
        let handler = handler();

        // Just the type byte, without a payload.
        let tx = validated_transaction(Address::ZERO, Bytes::from_static(&[0x02]), NoExtensions {});

        let result = handler.insert_validated_transaction(tx).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParams.code());
    }

    #[tokio::test]
    async fn decode_legacy_tx_type_byte_returns_invalid_params() {
        let handler = handler();

        // Legacy tx (type 0x00) followed by invalid RLP
        let tx = validated_transaction(
            Address::ZERO,
            Bytes::from_static(&[0x00, 0x01, 0x02]),
            NoExtensions {},
        );

        let result = handler.insert_validated_transaction(tx).await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParams.code());
    }

    #[tokio::test]
    async fn decode_valid_eip1559_tx() {
        let handler = handler();

        let (sender, raw) = create_eip1559_tx();
        let tx = validated_transaction(sender, raw, NoExtensions {});

        let result = handler.insert_validated_transaction(tx).await;
        let err = result.unwrap_err();
        // Decode should succeed, but the txpool is a noop so it will reject the tx
        // This error code should be InternalError
        assert_eq!(err.code(), ErrorCode::InternalError.code());
    }
}
