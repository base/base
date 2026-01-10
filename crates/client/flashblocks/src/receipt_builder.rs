//! Unified receipt builder for Optimism transactions.
//!
//! This module provides a receipt builder that handles both deposit and non-deposit
//! transactions seamlessly, without requiring error handling at the call site.

use alloy_consensus::{Eip658Value, Receipt, transaction::Recovered};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use op_alloy_consensus::{OpDepositReceipt, OpTxEnvelope};
use reth::revm::Database;
use reth_evm::{Evm, eth::receipt_builder::ReceiptBuilderCtx};
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_evm::OpRethReceiptBuilder;
use reth_optimism_primitives::OpReceipt;

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
/// This builder wraps [`OpRethReceiptBuilder`] and automatically handles the deposit
/// receipt case internally, eliminating the need for callers to implement the
/// try-catch pattern typically required when using `build_receipt` followed by
/// `build_deposit_receipt`.
///
/// # Example
///
/// ```ignore
/// let builder = UnifiedReceiptBuilder::new(chain_spec);
/// let receipt = builder.build(&mut evm, &transaction, result, &state, cumulative_gas_used, timestamp)?;
/// ```
#[derive(Debug, Clone)]
pub struct UnifiedReceiptBuilder<C> {
    inner: OpRethReceiptBuilder,
    chain_spec: C,
}

impl<C> UnifiedReceiptBuilder<C> {
    /// Creates a new unified receipt builder with the given chain specification.
    pub fn new(chain_spec: C) -> Self {
        Self { inner: OpRethReceiptBuilder, chain_spec }
    }

    /// Returns a reference to the chain specification.
    pub fn chain_spec(&self) -> &C {
        &self.chain_spec
    }
}

impl<C: OpHardforks> UnifiedReceiptBuilder<C> {
    /// Builds a receipt for any transaction type, handling deposit receipts internally.
    ///
    /// This method attempts to build a regular receipt first. If the transaction is a
    /// deposit transaction, it automatically fetches the required deposit-specific data
    /// (nonce and receipt version) and builds the deposit receipt.
    ///
    /// # Arguments
    ///
    /// * `evm` - Mutable reference to the EVM, used for database access
    /// * `transaction` - The recovered transaction to build a receipt for
    /// * `result` - The execution result
    /// * `state` - The resulting EVM state
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
        result: reth::revm::context::result::ExecutionResult,
        state: &reth::revm::state::EvmState,
        cumulative_gas_used: u64,
        timestamp: u64,
    ) -> Result<OpReceipt, ReceiptBuildError>
    where
        E: Evm,
        E::DB: Database,
    {
        let ctx = ReceiptBuilderCtx { tx: transaction, evm, result, state, cumulative_gas_used };

        match self.inner.build_receipt(ctx) {
            Ok(receipt) => Ok(receipt),
            Err(ctx) => self.build_deposit_receipt_internal(ctx, transaction, timestamp),
        }
    }

    /// Builds a deposit receipt from the returned context.
    ///
    /// This is called internally when `build_receipt` returns an error (indicating
    /// a deposit transaction).
    fn build_deposit_receipt_internal<E>(
        &self,
        ctx: ReceiptBuilderCtx<'_, Recovered<OpTxEnvelope>, E>,
        transaction: &Recovered<OpTxEnvelope>,
        timestamp: u64,
    ) -> Result<OpReceipt, ReceiptBuildError>
    where
        E: Evm,
        E::DB: Database,
    {
        let is_canyon_active = self.chain_spec.is_canyon_active_at_timestamp(timestamp);
        let is_regolith_active = self.chain_spec.is_regolith_active_at_timestamp(timestamp);

        // Build the inner receipt from the execution result
        let receipt = Receipt {
            status: Eip658Value::Eip658(ctx.result.is_success()),
            cumulative_gas_used: ctx.cumulative_gas_used,
            logs: ctx.result.into_logs(),
        };

        // Fetch deposit nonce if Regolith is active
        let deposit_nonce = if is_regolith_active {
            Some(
                ctx.evm
                    .db_mut()
                    .basic(transaction.signer())
                    .map_err(|_| ReceiptBuildError::DepositAccountLoad)?
                    .map(|acc| acc.nonce)
                    .unwrap_or_default(),
            )
        } else {
            None
        };

        // Build and return the deposit receipt
        Ok(self.inner.build_deposit_receipt(OpDepositReceipt {
            inner: receipt,
            deposit_nonce,
            deposit_receipt_version: is_canyon_active.then_some(1),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reth_optimism_chainspec::OpChainSpecBuilder;

    use super::*;

    #[test]
    fn test_unified_receipt_builder_creation() {
        let chain_spec = Arc::new(OpChainSpecBuilder::base_mainnet().build());
        let builder = UnifiedReceiptBuilder::new(chain_spec);
        assert!(std::mem::size_of_val(&builder) > 0);
    }
}
