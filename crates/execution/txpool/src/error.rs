use std::any::Any;

use reth_transaction_pool::error::PoolTransactionError;

use crate::supervisor::InteropTxValidatorError;

/// Error type for invalid cross-chain transactions.
#[derive(thiserror::Error, Debug)]
pub enum InvalidCrossTx {
    /// Errors produced by supervisor validation
    #[error(transparent)]
    ValidationError(#[from] InteropTxValidatorError),
}

impl PoolTransactionError for InvalidCrossTx {
    fn is_bad_transaction(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
