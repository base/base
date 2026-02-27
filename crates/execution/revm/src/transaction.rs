//! Contains the `[OpTransaction]` type and its implementation.

mod abstraction;
pub use abstraction::{OpBuildError, OpTransaction, OpTransactionBuilder, OpTxTr};

mod deposit;
pub use deposit::{DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts};

mod error;
pub use error::OpTransactionError;

/// Estimates the compressed size of a transaction.
pub fn estimate_tx_compressed_size(input: &[u8]) -> u64 {
    base_alloy_flz::tx_estimated_size_fjord(input)
}
