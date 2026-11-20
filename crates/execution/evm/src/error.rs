//! Error types for the Base EVM module.

use core::any::Any;

use evm2::registry::HandlerError;
use reth_evm::{InvalidTxError, execute::BlockExecutionError};

/// L1 Block Info specific errors
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum L1BlockInfoError {
    /// Could not find L1 block info transaction in the L2 block
    #[error("could not find l1 block info tx in the L2 block")]
    MissingTransaction,
    /// Invalid L1 block info transaction calldata
    #[error("invalid l1 block info transaction calldata in the L2 block")]
    InvalidCalldata,
    /// Unexpected L1 block info transaction calldata length
    #[error("unexpected l1 block info tx calldata length found")]
    UnexpectedCalldataLength,
    /// Base fee conversion error
    #[error("could not convert l1 base fee")]
    BaseFeeConversion,
    /// Fee overhead conversion error
    #[error("could not convert l1 fee overhead")]
    FeeOverheadConversion,
    /// Fee scalar conversion error
    #[error("could not convert l1 fee scalar")]
    FeeScalarConversion,
    /// Base Fee Scalar conversion error
    #[error("could not convert base fee scalar")]
    BaseFeeScalarConversion,
    /// Blob base fee conversion error
    #[error("could not convert l1 blob base fee")]
    BlobBaseFeeConversion,
    /// Blob base fee scalar conversion error
    #[error("could not convert l1 blob base fee scalar")]
    BlobBaseFeeScalarConversion,
    /// Operator fee scalar conversion error
    #[error("could not convert operator fee scalar")]
    OperatorFeeScalarConversion,
    /// Operator fee constant conversion error
    #[error("could not convert operator fee constant")]
    OperatorFeeConstantConversion,
    /// DA foootprint gas scalar constant conversion error
    #[error("could not convert DA footprint gas scalar constant")]
    DaFootprintGasScalarConversion,
}

/// Base Block Executor Errors
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BaseBlockExecutionError {
    /// Error when trying to parse L1 block info
    #[error(transparent)]
    L1BlockInfo(#[from] L1BlockInfoError),
}

impl From<BaseBlockExecutionError> for BlockExecutionError {
    fn from(err: BaseBlockExecutionError) -> Self {
        Self::other(err)
    }
}

/// A transaction that is invalid for the block it was executed in.
///
/// Classifies the EVM2 handler error so callers such as the payload builder can tell a
/// too-low nonce or an out-of-range gas limit apart from other invalid transactions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(transparent)]
pub struct BaseInvalidTxError(HandlerError);

impl InvalidTxError for BaseInvalidTxError {
    fn is_nonce_too_low(&self) -> bool {
        matches!(self.0, HandlerError::InvalidNonce { expected, got } if got < expected)
    }

    fn is_gas_limit_too_high(&self) -> bool {
        matches!(
            self.0,
            HandlerError::GasLimitMoreThanBlock { .. }
                | HandlerError::TxGasLimitGreaterThanCap { .. }
        )
    }

    fn is_gas_limit_too_low(&self) -> bool {
        matches!(self.0, HandlerError::IntrinsicGasTooLow { .. })
    }

    fn as_any(&self) -> &(dyn Any + 'static) {
        &self.0
    }
}

impl TryFrom<HandlerError> for BaseInvalidTxError {
    type Error = HandlerError;

    /// Returns database, fatal, and registry-wiring errors back: they are not properties of the
    /// transaction.
    fn try_from(error: HandlerError) -> Result<Self, Self::Error> {
        match error {
            HandlerError::Database(_)
            | HandlerError::Fatal(_)
            | HandlerError::WrongTransactionType { .. } => Err(error),
            error => Ok(Self(error)),
        }
    }
}
