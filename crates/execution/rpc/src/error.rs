//! RPC errors specific to OP.

use std::convert::Infallible;

use alloy_json_rpc::ErrorPayload;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{BlockError, error::EthRpcErrorCode};
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_evm::{BaseHaltReason, BaseTransactionError};
use base_execution_evm::BaseBlockExecutionError;
use jsonrpsee_types::error::INTERNAL_ERROR_CODE;
use reth_evm::execute::ProviderError;
use reth_rpc_eth_api::{AsEthApiError, EthTxEnvError, TransactionConversionError};
use reth_rpc_eth_types::{
    EthApiError,
    error::api::{FromEvmHalt, FromRevert},
};
use reth_rpc_server_types::result::{internal_rpc_err, rpc_err};
use revm::context_interface::result::{EVMError, InvalidTransaction};

/// Base-specific errors, that extend [`EthApiError`].
#[derive(Debug, thiserror::Error)]
pub enum BaseEthApiError {
    /// L1 ethereum error.
    #[error(transparent)]
    Eth(#[from] EthApiError),
    /// EVM error originating from invalid Base data.
    #[error(transparent)]
    Evm(#[from] BaseBlockExecutionError),
    /// Thrown when calculating L1 gas fee.
    #[error("failed to calculate l1 gas fee")]
    L1BlockFeeError,
    /// Thrown when calculating L1 gas used
    #[error("failed to calculate l1 gas used")]
    L1BlockGasError,
    /// Wrapper for [`revm_primitives::InvalidTransaction`](InvalidTransaction).
    #[error(transparent)]
    InvalidTransaction(#[from] BaseInvalidTransactionError),
    /// Sequencer client error.
    #[error(transparent)]
    Sequencer(#[from] SequencerClientError),
}

impl AsEthApiError for BaseEthApiError {
    fn as_err(&self) -> Option<&EthApiError> {
        match self {
            Self::Eth(err) => Some(err),
            _ => None,
        }
    }
}

impl From<BaseEthApiError> for jsonrpsee_types::error::ErrorObject<'static> {
    fn from(err: BaseEthApiError) -> Self {
        match err {
            BaseEthApiError::Eth(err) => err.into(),
            BaseEthApiError::InvalidTransaction(err) => err.into(),
            BaseEthApiError::Evm(_)
            | BaseEthApiError::L1BlockFeeError
            | BaseEthApiError::L1BlockGasError => internal_rpc_err(err.to_string()),
            BaseEthApiError::Sequencer(err) => err.into(),
        }
    }
}

/// Base-specific invalid transaction errors
#[derive(thiserror::Error, Debug)]
pub enum BaseInvalidTransactionError {
    /// A deposit transaction was submitted as a system transaction post-regolith.
    #[error("no system transactions allowed after regolith")]
    DepositSystemTxPostRegolith,
    /// A deposit transaction halted post-regolith
    #[error("deposit transaction halted after regolith")]
    HaltedDepositPostRegolith,
    /// The encoded transaction was missing during evm execution.
    #[error("missing enveloped transaction bytes")]
    MissingEnvelopedTx,
}

impl From<BaseInvalidTransactionError> for jsonrpsee_types::error::ErrorObject<'static> {
    fn from(err: BaseInvalidTransactionError) -> Self {
        match err {
            BaseInvalidTransactionError::DepositSystemTxPostRegolith
            | BaseInvalidTransactionError::HaltedDepositPostRegolith
            | BaseInvalidTransactionError::MissingEnvelopedTx => {
                rpc_err(EthRpcErrorCode::TransactionRejected.code(), err.to_string(), None)
            }
        }
    }
}

impl TryFrom<BaseTransactionError> for BaseInvalidTransactionError {
    type Error = InvalidTransaction;

    fn try_from(err: BaseTransactionError) -> Result<Self, Self::Error> {
        match err {
            BaseTransactionError::DepositSystemTxPostRegolith => {
                Ok(Self::DepositSystemTxPostRegolith)
            }
            BaseTransactionError::HaltedDepositPostRegolith => Ok(Self::HaltedDepositPostRegolith),
            BaseTransactionError::MissingEnvelopedTx => Ok(Self::MissingEnvelopedTx),
            BaseTransactionError::Base(err) => Err(err),
        }
    }
}

/// Error type when interacting with the Sequencer
#[derive(Debug, thiserror::Error)]
pub enum SequencerClientError {
    /// Wrapper around an [`RpcError<TransportErrorKind>`].
    #[error(transparent)]
    HttpError(#[from] RpcError<TransportErrorKind>),
}

impl From<SequencerClientError> for jsonrpsee_types::error::ErrorObject<'static> {
    fn from(err: SequencerClientError) -> Self {
        match err {
            SequencerClientError::HttpError(RpcError::ErrorResp(ErrorPayload {
                code,
                message,
                data,
            })) => jsonrpsee_types::error::ErrorObject::owned(code as i32, message, data),
            err => jsonrpsee_types::error::ErrorObject::owned(
                INTERNAL_ERROR_CODE,
                err.to_string(),
                None::<String>,
            ),
        }
    }
}

impl<T> From<EVMError<T, BaseTransactionError>> for BaseEthApiError
where
    T: Into<EthApiError>,
{
    fn from(error: EVMError<T, BaseTransactionError>) -> Self {
        match error {
            EVMError::Transaction(err) => match err.try_into() {
                Ok(err) => Self::InvalidTransaction(err),
                Err(err) => Self::Eth(EthApiError::InvalidTransaction(err.into())),
            },
            EVMError::Database(err) => Self::Eth(err.into()),
            EVMError::Header(err) => Self::Eth(err.into()),
            EVMError::Custom(err) => Self::Eth(EthApiError::EvmCustom(err)),
        }
    }
}

impl FromEvmHalt<BaseHaltReason> for BaseEthApiError {
    fn from_evm_halt(halt: BaseHaltReason, gas_limit: u64) -> Self {
        match halt {
            BaseHaltReason::FailedDeposit => {
                BaseInvalidTransactionError::HaltedDepositPostRegolith.into()
            }
            BaseHaltReason::Base(halt) => EthApiError::from_evm_halt(halt, gas_limit).into(),
        }
    }
}

impl FromRevert for BaseEthApiError {
    fn from_revert(output: Bytes) -> Self {
        Self::Eth(EthApiError::from_revert(output))
    }
}

impl From<TransactionConversionError> for BaseEthApiError {
    fn from(value: TransactionConversionError) -> Self {
        Self::Eth(EthApiError::from(value))
    }
}

impl From<EthTxEnvError> for BaseEthApiError {
    fn from(value: EthTxEnvError) -> Self {
        Self::Eth(EthApiError::from(value))
    }
}

impl From<ProviderError> for BaseEthApiError {
    fn from(value: ProviderError) -> Self {
        Self::Eth(EthApiError::from(value))
    }
}

impl From<BlockError> for BaseEthApiError {
    fn from(value: BlockError) -> Self {
        Self::Eth(EthApiError::from(value))
    }
}

impl From<Infallible> for BaseEthApiError {
    fn from(value: Infallible) -> Self {
        match value {}
    }
}
