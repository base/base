//! Unified receipt builder for Optimism transactions.
//!
//! This module provides a receipt builder that handles both deposit and non-deposit
//! transactions seamlessly, without requiring error handling at the call site.

use alloy_consensus::{Eip658Value, Receipt, transaction::Recovered};
use op_alloy_consensus::{OpDepositReceipt, OpTxEnvelope, OpTxType};
use reth_evm::Evm;
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_primitives::OpReceipt;
use revm::{Database, context::result::ExecutionResult};

/// Error type for receipt building operations.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptBuildError {
    /// Failed to load the deposit sender's account from the database.
    #[error("failed to load deposit account")]
    DepositAccountLoad,
}

/// A unified receipt builder that handles both deposit and non-deposit transactions
/// seamlessly without requiring error handling at the call site.
///
/// This builder automatically handles the deposit receipt case internally,
/// eliminating the need for callers to implement the try-catch pattern typically
/// required when using `build_receipt` followed by `build_deposit_receipt`.
///
/// # Example
///
/// ```ignore
/// let builder = UnifiedReceiptBuilder::new(chain_spec);
/// let receipt = builder.build(&mut evm, &transaction, result, cumulative_gas_used, timestamp)?;
/// ```
#[derive(Debug, Clone)]
pub struct UnifiedReceiptBuilder<C> {
    chain_spec: C,
}

impl<C> UnifiedReceiptBuilder<C> {
    /// Creates a new unified receipt builder with the given chain specification.
    pub const fn new(chain_spec: C) -> Self {
        Self { chain_spec }
    }

    /// Returns a reference to the chain specification.
    pub const fn chain_spec(&self) -> &C {
        &self.chain_spec
    }
}

impl<C: OpHardforks> UnifiedReceiptBuilder<C> {
    /// Builds a receipt for any transaction type, handling deposit receipts internally.
    ///
    /// This method builds either a regular receipt or a deposit receipt based on
    /// the transaction type. For deposit transactions, it automatically fetches
    /// the required deposit-specific data (nonce and receipt version).
    ///
    /// # Arguments
    ///
    /// * `evm` - Mutable reference to the EVM, used for database access
    /// * `transaction` - The recovered transaction to build a receipt for
    /// * `result` - The execution result
    /// * `cumulative_gas_used` - Cumulative gas used up to and including this transaction
    /// * `timestamp` - The block timestamp, used to determine active hardforks
    ///
    /// # Returns
    ///
    /// Returns the built receipt on success, or a [`ReceiptBuildError`] if the deposit
    /// account could not be loaded from the database.
    pub fn build<E>(
        &self,
        evm: &mut E,
        transaction: &Recovered<OpTxEnvelope>,
        result: ExecutionResult<E::HaltReason>,
        cumulative_gas_used: u64,
        timestamp: u64,
    ) -> Result<OpReceipt, ReceiptBuildError>
    where
        E: Evm,
        E::DB: Database,
    {
        let tx_type = transaction.tx_type();

        // Build the inner receipt from the execution result
        let receipt = Receipt {
            status: Eip658Value::Eip658(result.is_success()),
            cumulative_gas_used,
            logs: result.into_logs(),
        };

        if tx_type == OpTxType::Deposit {
            // Handle deposit transaction
            let is_canyon_active = self.chain_spec.is_canyon_active_at_timestamp(timestamp);
            let is_regolith_active = self.chain_spec.is_regolith_active_at_timestamp(timestamp);

            // Fetch deposit nonce if Regolith is active
            let deposit_nonce = if is_regolith_active {
                Some(
                    evm.db_mut()
                        .basic(transaction.signer())
                        .map_err(|_| ReceiptBuildError::DepositAccountLoad)?
                        .map(|acc| acc.nonce)
                        .unwrap_or_default(),
                )
            } else {
                None
            };

            Ok(OpReceipt::Deposit(OpDepositReceipt {
                inner: receipt,
                deposit_nonce,
                deposit_receipt_version: is_canyon_active.then_some(1),
            }))
        } else {
            // Handle non-deposit transaction
            Ok(match tx_type {
                OpTxType::Legacy => OpReceipt::Legacy(receipt),
                OpTxType::Eip2930 => OpReceipt::Eip2930(receipt),
                OpTxType::Eip1559 => OpReceipt::Eip1559(receipt),
                OpTxType::Eip7702 => OpReceipt::Eip7702(receipt),
                OpTxType::Deposit => unreachable!(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header;
    use alloy_primitives::{Address, Log, LogData, TxKind, address};
    use op_alloy_consensus::TxDeposit;
    use reth_evm::{ConfigureEvm, op_revm::OpHaltReason};
    use reth_optimism_chainspec::OpChainSpecBuilder;
    use reth_optimism_evm::OpEvmConfig;
    use revm::database::InMemoryDB;

    use super::*;

    fn create_legacy_tx() -> Recovered<OpTxEnvelope> {
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1000000000,
            gas_limit: 21000,
            to: TxKind::Call(Address::ZERO),
            value: alloy_primitives::U256::ZERO,
            input: alloy_primitives::Bytes::new(),
        };
        let envelope = OpTxEnvelope::Legacy(alloy_consensus::Signed::new_unchecked(
            tx,
            alloy_primitives::Signature::test_signature(),
            alloy_primitives::B256::ZERO,
        ));
        Recovered::new_unchecked(envelope, Address::ZERO)
    }

    fn create_deposit_tx() -> Recovered<OpTxEnvelope> {
        let deposit = TxDeposit {
            source_hash: alloy_primitives::B256::ZERO,
            from: address!("0x1234567890123456789012345678901234567890"),
            to: TxKind::Call(Address::ZERO),
            mint: 0,
            value: alloy_primitives::U256::ZERO,
            gas_limit: 21000,
            is_system_transaction: false,
            input: alloy_primitives::Bytes::new(),
        };
        let sealed = alloy_consensus::Sealed::new_unchecked(deposit, alloy_primitives::B256::ZERO);
        let envelope = OpTxEnvelope::Deposit(sealed);
        Recovered::new_unchecked(envelope, address!("0x1234567890123456789012345678901234567890"))
    }

    fn create_success_result() -> ExecutionResult<OpHaltReason> {
        ExecutionResult::Success {
            reason: revm::context::result::SuccessReason::Stop,
            gas_used: 21000,
            gas_refunded: 0,
            logs: vec![Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![], alloy_primitives::Bytes::new()),
            }],
            output: revm::context::result::Output::Call(alloy_primitives::Bytes::new()),
        }
    }

    #[test]
    fn test_unified_receipt_builder_creation() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let builder = UnifiedReceiptBuilder::new(chain_spec.clone());
        assert!(Arc::ptr_eq(builder.chain_spec(), &chain_spec));
    }

    #[test]
    fn test_receipt_from_success_result() {
        let result: ExecutionResult<OpHaltReason> = create_success_result();
        let receipt = Receipt {
            status: Eip658Value::Eip658(result.is_success()),
            cumulative_gas_used: 21000,
            logs: result.into_logs(),
        };
        assert!(receipt.status.coerce_status());
        assert_eq!(receipt.cumulative_gas_used, 21000);
        assert_eq!(receipt.logs.len(), 1);
    }

    #[test]
    fn test_receipt_from_revert_result() {
        let result: ExecutionResult<OpHaltReason> =
            ExecutionResult::Revert { gas_used: 10000, output: alloy_primitives::Bytes::new() };
        let receipt = Receipt {
            status: Eip658Value::Eip658(result.is_success()),
            cumulative_gas_used: 10000,
            logs: result.into_logs(),
        };
        assert!(!receipt.status.coerce_status());
        assert_eq!(receipt.cumulative_gas_used, 10000);
        assert!(receipt.logs.is_empty());
    }

    #[test]
    fn test_op_receipt_legacy_variant() {
        let receipt =
            Receipt { status: Eip658Value::Eip658(true), cumulative_gas_used: 21000, logs: vec![] };
        let op_receipt = OpReceipt::Legacy(receipt);
        assert!(matches!(op_receipt, OpReceipt::Legacy(_)));
    }

    #[test]
    fn test_op_receipt_deposit_variant() {
        let receipt =
            Receipt { status: Eip658Value::Eip658(true), cumulative_gas_used: 21000, logs: vec![] };
        let op_receipt = OpReceipt::Deposit(OpDepositReceipt {
            inner: receipt,
            deposit_nonce: Some(1),
            deposit_receipt_version: Some(1),
        });
        assert!(matches!(op_receipt, OpReceipt::Deposit(_)));
        if let OpReceipt::Deposit(deposit) = op_receipt {
            assert_eq!(deposit.deposit_nonce, Some(1));
            assert_eq!(deposit.deposit_receipt_version, Some(1));
        }
    }

    /// Helper to create an EVM instance for testing
    fn create_test_evm(
        chain_spec: Arc<reth_optimism_chainspec::OpChainSpec>,
        db: &mut InMemoryDB,
    ) -> impl Evm<HaltReason = OpHaltReason, DB = &mut InMemoryDB> + '_ {
        let evm_config = OpEvmConfig::optimism(chain_spec);
        let header = Header::default();
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        evm_config.evm_with_env(db, evm_env)
    }

    #[test]
    fn test_build_legacy_receipt() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let mut db = InMemoryDB::default();
        let mut evm = create_test_evm(chain_spec.clone(), &mut db);

        let builder = UnifiedReceiptBuilder::new(chain_spec);
        let tx = create_legacy_tx();
        let result = create_success_result();

        let receipt = builder.build(&mut evm, &tx, result, 21000, 0).expect("build should succeed");

        assert!(matches!(receipt, OpReceipt::Legacy(_)));
        if let OpReceipt::Legacy(inner) = receipt {
            assert!(inner.status.coerce_status());
            assert_eq!(inner.cumulative_gas_used, 21000);
        }
    }

    #[test]
    fn test_build_deposit_receipt() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let mut db = InMemoryDB::default();
        let mut evm = create_test_evm(chain_spec.clone(), &mut db);

        let builder = UnifiedReceiptBuilder::new(chain_spec);
        let tx = create_deposit_tx();
        let result = create_success_result();

        let receipt = builder.build(&mut evm, &tx, result, 21000, 0).expect("build should succeed");

        assert!(matches!(receipt, OpReceipt::Deposit(_)));
        if let OpReceipt::Deposit(deposit) = receipt {
            assert!(deposit.inner.status.coerce_status());
            assert_eq!(deposit.inner.cumulative_gas_used, 21000);
        }
    }

    #[test]
    fn test_build_deposit_receipt_with_canyon_active() {
        // Canyon activates deposit_receipt_version
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let mut db = InMemoryDB::default();
        let mut evm = create_test_evm(chain_spec.clone(), &mut db);

        let builder = UnifiedReceiptBuilder::new(chain_spec);
        let tx = create_deposit_tx();
        let result = create_success_result();

        // Use a timestamp after Canyon activation (Base mainnet Canyon: 1704992401)
        let canyon_timestamp = 1704992401 + 1000;
        let receipt = builder
            .build(&mut evm, &tx, result, 21000, canyon_timestamp)
            .expect("build should succeed");

        if let OpReceipt::Deposit(deposit) = receipt {
            assert_eq!(deposit.deposit_receipt_version, Some(1));
        } else {
            panic!("Expected deposit receipt");
        }
    }

    #[test]
    fn test_build_failed_transaction_receipt() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let mut db = InMemoryDB::default();
        let mut evm = create_test_evm(chain_spec.clone(), &mut db);

        let builder = UnifiedReceiptBuilder::new(chain_spec);
        let tx = create_legacy_tx();
        let result: ExecutionResult<OpHaltReason> =
            ExecutionResult::Revert { gas_used: 10000, output: alloy_primitives::Bytes::new() };

        let receipt = builder.build(&mut evm, &tx, result, 10000, 0).expect("build should succeed");

        if let OpReceipt::Legacy(inner) = receipt {
            assert!(!inner.status.coerce_status()); // Failed transaction
            assert_eq!(inner.cumulative_gas_used, 10000);
        } else {
            panic!("Expected legacy receipt");
        }
    }
}
