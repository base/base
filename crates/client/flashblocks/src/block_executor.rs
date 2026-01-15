//! Block execution trait for EVM abstraction.
//!
//! This module provides the [`BlockExecutor`] trait which abstracts transaction
//! execution, enabling future EVM mocking for testing purposes.

use alloy_consensus::transaction::Recovered;
use op_alloy_consensus::OpTxEnvelope;

use crate::{ExecutedPendingTransaction, StateProcessorError};

/// Trait for executing transactions within a block.
///
/// This abstraction allows for different execution strategies and enables
/// mocking the EVM for unit testing without requiring a full EVM setup.
pub trait BlockExecutor {
    /// Executes a single transaction and returns the execution result.
    ///
    /// # Arguments
    /// * `index` - The index of the transaction within the block.
    /// * `transaction` - The recovered transaction with its sender.
    ///
    /// # Returns
    /// The executed transaction result including receipt and state changes.
    ///
    /// # Errors
    /// Returns an error if transaction execution fails.
    fn execute_transaction(
        &mut self,
        index: usize,
        transaction: Recovered<OpTxEnvelope>,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError>;
}
