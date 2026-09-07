//! Abstraction over EVM errors.

use core::{any::Any, error::Error};
use revm::{
    context::DBErrorMarker,
    context_interface::result::{EVMError, InvalidTransaction},
};

/// Abstraction over transaction validation error.
pub trait InvalidTxError: Error + Send + Sync + Any + 'static {
    /// Returns whether the error cause by transaction having a nonce lower than expected.
    fn is_nonce_too_low(&self) -> bool {
        self.as_invalid_tx_err()
            .is_some_and(|err| matches!(err, InvalidTransaction::NonceTooLow { .. }))
    }

    /// Returns whether the error is due to the transaction gas limit being higher than allowed.
    fn is_gas_limit_too_high(&self) -> bool {
        self.as_invalid_tx_err().is_some_and(|err| {
            matches!(
                err,
                InvalidTransaction::TxGasLimitGreaterThanCap { .. }
                    | InvalidTransaction::CallerGasLimitMoreThanBlock
            )
        })
    }

    /// Returns whether the error is due to the transaction gas limit being lower than required.
    fn is_gas_limit_too_low(&self) -> bool {
        self.as_invalid_tx_err().is_some_and(|err| {
            matches!(
                err,
                InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
                    | InvalidTransaction::GasFloorMoreThanGasLimit { .. }
            )
        })
    }

    /// Returns the underlying [`InvalidTransaction`] if any.
    ///
    /// This is primarily used for error conversions, e.g. for rpc responses.
    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction>;
}

impl InvalidTxError for InvalidTransaction {
    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        Some(self)
    }
}

/// Abstraction over errors that can occur during EVM execution.
///
/// It's assumed that errors can occur either because of an invalid transaction, meaning that other
/// transaction might still result in successful execution, or because of a general EVM
/// misconfiguration.
///
/// If caller occurs a error different from [`EvmError::InvalidTransaction`], it should most likely
/// be treated as fatal error flagging some EVM misconfiguration.
pub trait EvmError: Sized + Error + Send + Sync + 'static {
    /// Errors which might occur as a result of an invalid transaction. i.e unrelated to general EVM
    /// configuration.
    type InvalidTransaction: InvalidTxError;

    /// Returns the [`EvmError::InvalidTransaction`] if the error is an invalid transaction error.
    fn as_invalid_tx_err(&self) -> Option<&Self::InvalidTransaction>;

    /// Attempts to convert the error into [`EvmError::InvalidTransaction`].
    fn try_into_invalid_tx_err(self) -> Result<Self::InvalidTransaction, Self>;

    /// Returns `true` if the error is fatal and should be treated as an internal error during
    /// execution.
    fn is_fatal(&self) -> bool;

    /// Returns `true` if the error is an invalid transaction error.
    fn is_invalid_tx_err(&self) -> bool {
        self.as_invalid_tx_err().is_some()
    }
}

impl<DBError, TxError> EvmError for EVMError<DBError, TxError>
where
    DBError: DBErrorMarker,
    TxError: InvalidTxError,
{
    type InvalidTransaction = TxError;

    fn as_invalid_tx_err(&self) -> Option<&Self::InvalidTransaction> {
        match self {
            Self::Transaction(err) => Some(err),
            _ => None,
        }
    }

    fn try_into_invalid_tx_err(self) -> Result<Self::InvalidTransaction, Self> {
        match self {
            Self::Transaction(err) => Ok(err),
            err => Err(err),
        }
    }

    fn is_fatal(&self) -> bool {
        match self {
            Self::Transaction(_) => false,
            Self::Custom(_) | Self::CustomAny(_) | Self::Header(_) => true,
            Self::Database(err) => err.is_fatal(),
        }
    }
}
