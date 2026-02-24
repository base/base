//! RPC trait and implementation for inserting pre-validated transactions.

use alloy_primitives::B256;
use base_alloy_consensus::OpPooledTransaction;
use base_primitives::{InsertResult, ValidatedTransaction};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use reth_optimism_txpool::OpPooledTx;
use reth_transaction_pool::{
    BlobStore, Pool, PoolTransaction, TransactionOrdering, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator, validate::ValidTransaction,
};
use tracing::{debug, warn};

/// RPC trait for inserting pre-validated transactions into the pool.
#[rpc(server, namespace = "base")]
pub trait ValidatedTxApi {
    /// Inserts pre-validated transactions into the transaction pool.
    ///
    /// Transactions are expected to have been validated by the caller.
    /// The `from` field is trusted as the recovered sender address.
    /// Validation is bypassed by using `PoolInner::add_transactions()` directly.
    ///
    /// Returns a result for each transaction indicating success or failure.
    #[method(name = "insertValidatedTransactions")]
    async fn insert_validated_transactions(
        &self,
        txs: Vec<ValidatedTransaction>,
    ) -> RpcResult<Vec<InsertResult>>;
}

/// Implementation of the validated transactions RPC.
///
/// This implementation uses the concrete `Pool<V, T, S>` type to access `inner()`,
/// allowing us to call `add_transactions()` with pre-constructed validation outcomes,
/// thereby bypassing re-validation.
#[derive(Debug)]
pub struct ValidatedTxApiImpl<V, T: TransactionOrdering, S> {
    pool: Pool<V, T, S>,
}

impl<V, T: TransactionOrdering, S> ValidatedTxApiImpl<V, T, S> {
    /// Creates a new validated transactions API instance.
    pub const fn new(pool: Pool<V, T, S>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl<V, T, S> ValidatedTxApiServer for ValidatedTxApiImpl<V, T, S>
where
    V: TransactionValidator + 'static,
    T: TransactionOrdering<Transaction = V::Transaction> + 'static,
    T::Transaction: OpPooledTx + PoolTransaction<Pooled = OpPooledTransaction>,
    S: BlobStore + 'static,
{
    async fn insert_validated_transactions(
        &self,
        txs: Vec<ValidatedTransaction>,
    ) -> RpcResult<Vec<InsertResult>> {
        let tx_count = txs.len();
        debug!(target: "rpc::insert_validated_transactions", count = tx_count, "inserting validated transactions");

        // Convert ValidatedTransactions to TransactionValidationOutcome::Valid
        let mut outcomes = Vec::with_capacity(tx_count);
        let mut tx_hashes = Vec::with_capacity(tx_count);
        let mut early_failures = Vec::new();

        for (idx, validated_tx) in txs.into_iter().enumerate() {
            // Extract validation metadata before converting
            let balance = validated_tx.balance;
            let state_nonce = validated_tx.state_nonce;
            let bytecode_hash = validated_tx.bytecode_hash;

            let tx_hash = match validated_tx.compute_tx_hash() {
                Ok(hash) => hash,
                Err(e) => {
                    warn!(target: "rpc::insert_validated_transactions", error = %e, "failed to compute tx hash");
                    early_failures.push((idx, InsertResult::failure(B256::ZERO, e.to_string())));
                    continue;
                }
            };

            tx_hashes.push((idx, tx_hash));

            let recovered = match validated_tx.into_recovered() {
                Ok(r) => r,
                Err(e) => {
                    warn!(target: "rpc::insert_validated_transactions", %tx_hash, error = %e, "failed to convert transaction");
                    early_failures.push((idx, InsertResult::failure(tx_hash, e.to_string())));
                    continue;
                }
            };

            // Convert from Recovered<OpPooledTransaction> (alloy) to Pool::Transaction
            let pool_tx = T::Transaction::from_pooled(recovered);

            // Construct the validation outcome with pre-validated metadata
            let outcome = TransactionValidationOutcome::Valid {
                balance,
                state_nonce,
                bytecode_hash,
                transaction: ValidTransaction::Valid(pool_tx),
                propagate: true,
                authorities: None,
            };

            outcomes.push(outcome);
        }

        // Insert all transactions at once using PoolInner, bypassing validation
        let pool_results =
            self.pool.inner().add_transactions(TransactionOrigin::External, outcomes);

        // Build results in original order
        let mut results =
            vec![InsertResult::failure(B256::ZERO, "not processed".to_string()); tx_count];

        // Fill in early failures
        for (idx, result) in early_failures {
            results[idx] = result;
        }

        // Fill in pool results (matched by order of successful conversions)
        for ((idx, tx_hash), pool_result) in tx_hashes.into_iter().zip(pool_results) {
            match pool_result {
                Ok(_outcome) => {
                    debug!(target: "rpc::insert_validated_transactions", %tx_hash, "transaction inserted");
                    results[idx] = InsertResult::success(tx_hash);
                }
                Err(e) => {
                    warn!(target: "rpc::insert_validated_transactions", %tx_hash, error = %e, "failed to insert transaction");
                    results[idx] = InsertResult::failure(tx_hash, e.to_string());
                }
            }
        }

        let success_count = results.iter().filter(|r| r.success).count();
        let failure_count = results.len() - success_count;

        debug!(
            target: "rpc::insert_validated_transactions",
            total = tx_count,
            success = success_count,
            failures = failure_count,
            "completed inserting validated transactions"
        );

        if failure_count > 0 && success_count == 0 {
            return Err(ErrorObjectOwned::owned(
                -32000,
                format!("all {failure_count} transactions failed to insert"),
                Some(results),
            ));
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use base_primitives::{TransactionSignature, ValidatedTransaction};

    /// Helper to create a valid EIP-1559 `ValidatedTransaction` for testing.
    fn create_test_validated_tx(nonce: u64, from: Address) -> ValidatedTransaction {
        ValidatedTransaction {
            from,
            balance: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
            state_nonce: nonce,
            bytecode_hash: None,
            tx_type: 2,           // EIP-1559
            chain_id: Some(8453), // Base mainnet
            nonce,
            to: Some(Address::repeat_byte(0x42)),
            value: U256::from(1000),
            gas_limit: 21000,
            gas_price: None,
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(100_000_000),
            input: alloy_primitives::Bytes::default(),
            access_list: None,
            signature: TransactionSignature { v: 0, r: U256::from(1), s: U256::from(2) },
        }
    }

    #[test]
    fn test_validated_tx_compute_hash_success() {
        let tx = create_test_validated_tx(0, Address::repeat_byte(0x01));
        let hash = tx.compute_tx_hash();
        assert!(hash.is_ok(), "hash computation should succeed for valid tx");
        assert!(!hash.unwrap().is_zero(), "hash should not be zero");
    }

    #[test]
    fn test_validated_tx_into_recovered_success() {
        let tx = create_test_validated_tx(0, Address::repeat_byte(0x01));
        let recovered = tx.into_recovered();
        assert!(recovered.is_ok(), "conversion should succeed for valid tx");
    }

    #[test]
    fn test_validated_tx_missing_max_fee_fails() {
        let mut tx = create_test_validated_tx(0, Address::repeat_byte(0x01));
        tx.max_fee_per_gas = None; // Remove required field for EIP-1559
        let hash = tx.compute_tx_hash();
        assert!(hash.is_err(), "should fail without max_fee_per_gas");
    }

    #[test]
    fn test_validated_tx_legacy_type() {
        let tx = ValidatedTransaction {
            from: Address::repeat_byte(0x01),
            balance: U256::from(1_000_000_000_000_000_000u128),
            state_nonce: 0,
            bytecode_hash: None,
            tx_type: 0, // Legacy
            chain_id: Some(8453),
            nonce: 0,
            to: Some(Address::repeat_byte(0x42)),
            value: U256::from(1000),
            gas_limit: 21000,
            gas_price: Some(1_000_000_000),
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            input: alloy_primitives::Bytes::default(),
            access_list: None,
            signature: TransactionSignature { v: 27, r: U256::from(1), s: U256::from(2) },
        };

        let hash = tx.compute_tx_hash();
        assert!(hash.is_ok(), "legacy tx hash computation should succeed");

        let recovered = tx.into_recovered();
        assert!(recovered.is_ok(), "legacy tx conversion should succeed");
    }

    #[test]
    fn test_validated_tx_unsupported_type() {
        let mut tx = create_test_validated_tx(0, Address::repeat_byte(0x01));
        tx.tx_type = 99; // Unsupported type
        let hash = tx.compute_tx_hash();
        assert!(hash.is_err(), "unsupported tx type should fail");
    }

    #[test]
    fn test_validated_tx_eip2930_type() {
        let tx = ValidatedTransaction {
            from: Address::repeat_byte(0x01),
            balance: U256::from(1_000_000_000_000_000_000u128),
            state_nonce: 0,
            bytecode_hash: None,
            tx_type: 1, // EIP-2930
            chain_id: Some(8453),
            nonce: 0,
            to: Some(Address::repeat_byte(0x42)),
            value: U256::from(1000),
            gas_limit: 21000,
            gas_price: Some(1_000_000_000),
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            input: alloy_primitives::Bytes::default(),
            access_list: Some(Default::default()),
            signature: TransactionSignature { v: 0, r: U256::from(1), s: U256::from(2) },
        };

        let hash = tx.compute_tx_hash();
        assert!(hash.is_ok(), "EIP-2930 tx hash computation should succeed");

        let recovered = tx.into_recovered();
        assert!(recovered.is_ok(), "EIP-2930 tx conversion should succeed");
    }

    #[test]
    fn test_validated_tx_preserves_validation_metadata() {
        let balance = U256::from(999_000_000_000u128);
        let state_nonce = 42u64;
        let bytecode_hash = Some(B256::repeat_byte(0xAB));

        let tx = ValidatedTransaction {
            from: Address::repeat_byte(0x01),
            balance,
            state_nonce,
            bytecode_hash,
            tx_type: 2,
            chain_id: Some(8453),
            nonce: 5,
            to: Some(Address::repeat_byte(0x42)),
            value: U256::from(1000),
            gas_limit: 21000,
            gas_price: None,
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(100_000_000),
            input: alloy_primitives::Bytes::default(),
            access_list: None,
            signature: TransactionSignature { v: 0, r: U256::from(1), s: U256::from(2) },
        };

        // Verify the metadata is preserved
        assert_eq!(tx.balance, balance);
        assert_eq!(tx.state_nonce, state_nonce);
        assert_eq!(tx.bytecode_hash, bytecode_hash);
    }
}
