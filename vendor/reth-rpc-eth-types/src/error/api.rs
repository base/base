//! Concrete Ethereum error classification used by Base RPC.

use crate::{EthApiError, error::RpcInvalidTransactionError, simulate::EthSimulateError};

impl EthApiError {
    /// Returns [`EthSimulateError`] if this error maps to a simulate-specific error code.
    pub fn as_simulate_error(&self) -> Option<EthSimulateError> {
        match self {
            EthApiError::InvalidTransaction(tx_err) => match tx_err {
                RpcInvalidTransactionError::NonceTooLow { tx, state } => {
                    Some(EthSimulateError::NonceTooLow { tx: *tx, state: *state })
                }
                RpcInvalidTransactionError::NonceTooHigh => Some(EthSimulateError::NonceTooHigh),
                RpcInvalidTransactionError::NonceMaxValue => Some(EthSimulateError::NonceMaxValue),
                RpcInvalidTransactionError::FeeCapTooLow => {
                    Some(EthSimulateError::BaseFeePerGasTooLow)
                }
                RpcInvalidTransactionError::GasTooLow => Some(EthSimulateError::IntrinsicGasTooLow),
                RpcInvalidTransactionError::InsufficientFunds { cost, balance } => {
                    Some(EthSimulateError::InsufficientFunds { cost: *cost, balance: *balance })
                }
                RpcInvalidTransactionError::SenderNoEOA => Some(EthSimulateError::SenderNotEOA),
                RpcInvalidTransactionError::MaxInitCodeSizeExceeded => {
                    Some(EthSimulateError::MaxInitCodeSizeExceeded)
                }
                _ => None,
            },
            _ => None,
        }
    }
}
