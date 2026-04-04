use std::time::Instant;

use alloy_consensus::transaction::Recovered;
use alloy_eips::Decodable2718;
use base_alloy_consensus::OpTransactionSigned;
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_transaction_pool::TransactionPool;
use tracing::debug;

use super::metrics::Metrics as BuilderApiMetrics;
use crate::{BasePooledTransaction, ValidatedTransaction};

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
}

impl<P> BuilderApiImpl<P> {
    /// Creates a new handler backed by the given transaction pool.
    pub const fn new(pool: P) -> Self {
        Self { pool }
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
            "rpc::insert_validated_transaction"
        );
        let sender = tx.sender;

        // Decode the EIP-2718 transaction bytes
        let consensus_tx = OpTransactionSigned::decode_2718(&mut tx.raw.as_ref()).map_err(|e| {
            BuilderApiMetrics::decode_errors().increment(1);
            ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("failed to decode transaction: {e}"),
                None::<()>,
            )
        })?;
        let encoded_len = tx.raw.len();

        let recovered = Recovered::new_unchecked(consensus_tx, sender);
        let pool_tx = BasePooledTransaction::new(recovered, encoded_len).with_bundle_metadata(
            tx.target_block_number,
            tx.min_timestamp,
            tx.max_timestamp,
        );

        // Insert into the pool
        let start = Instant::now();
        let result = self.pool.add_external_transaction(pool_tx).await;
        BuilderApiMetrics::insert_duration().record(start.elapsed().as_secs_f64());

        match result {
            Ok(_) => {
                debug!(sender = %sender, "inserted validated transaction");
                BuilderApiMetrics::txs_inserted().increment(1);
                Ok(())
            }
            Err(e) => {
                debug!(sender = %sender, error = %e, "pool rejected transaction");
                BuilderApiMetrics::txs_rejected().increment(1);
                Err(ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("pool rejected transaction: {e}"),
                    None::<()>,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::TxEip1559;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use base_alloy_consensus::{OpTransactionSigned, OpTypedTransaction, TxDeposit};
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
        let signed_tx: OpTransactionSigned = deposit_tx.into();
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
        let signed = OpTransactionSigned::new_unhashed(OpTypedTransaction::Eip1559(tx), sig);
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
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
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
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
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
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
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
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
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
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
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
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
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
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
        };

        let result = handler.insert_validated_transaction(tx).await;
        let err = result.unwrap_err();
        // Decode should succeed, but the txpool is a noop so it will reject the tx
        // This error code should be InternalError
        assert_eq!(err.code(), ErrorCode::InternalError.code());
    }
}
