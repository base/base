//! RPC errors specific to Base.

use std::convert::Infallible;

use alloy_json_rpc::ErrorPayload;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{BlockError, error::EthRpcErrorCode};
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_consensus::Eip8130CommittedPhasesError;
use base_common_rpc_types::BaseTxEnvError;
use base_execution_eip8130::Eip8130Error;
use base_execution_evm::BaseBlockExecutionError;
use evm2::{interpreter::InstrStop, registry::HandlerError};
use jsonrpsee_types::error::INTERNAL_ERROR_CODE;
use reth_errors::ProviderError;
use reth_evm::execute::{BlockExecutionError, BlockValidationError};
use reth_rpc_eth_api::{AsEthApiError, EthTxEnvError, TransactionConversionError};
use reth_rpc_eth_types::{
    EthApiError,
    error::api::{FromEvmHalt, FromRevert},
};
use reth_rpc_server_types::result::{internal_rpc_err, rpc_err};

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
    /// A Base-specific invalid transaction.
    #[error(transparent)]
    InvalidTransaction(#[from] BaseInvalidTransactionError),
    /// Sequencer client error.
    #[error(transparent)]
    Sequencer(#[from] SequencerClientError),
    /// A stored EIP-8130 receipt carries a committed phase count that violates the receipt
    /// invariant.
    #[error("invalid EIP-8130 receipt: {0}")]
    Eip8130CommittedPhases(#[from] Eip8130CommittedPhasesError),
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
            | BaseEthApiError::L1BlockGasError
            | BaseEthApiError::Eip8130CommittedPhases(_) => internal_rpc_err(err.to_string()),
            BaseEthApiError::Sequencer(err) => err.into(),
        }
    }
}

/// Base-specific invalid transaction errors
#[derive(thiserror::Error, Debug)]
pub enum BaseInvalidTransactionError {
    /// An EIP-8130 (account-abstraction) transaction was submitted via
    /// `eth_sendRawTransaction` before the Everest fork was active.
    ///
    /// The transaction type byte (`0x79`) is recognised by the consensus layer for
    /// decoding/serialization purposes, but RPC admission is rejected until the
    /// Everest fork is active. The txpool validator enforces the same fork gate for
    /// transactions arriving over devp2p.
    #[error("{}", base_common_consensus::EIP8130_REJECTION_MSG)]
    Eip8130NotAccepted,
    /// An EIP-8130 (account-abstraction) transaction was rejected during its
    /// enshrined execution pipeline (authorization, nonce, intrinsic gas, fee, or
    /// account-change apply). The string carries the underlying rejection reason.
    #[error("EIP-8130 transaction rejected: {0}")]
    Eip8130Rejected(String),
}

impl From<BaseInvalidTransactionError> for jsonrpsee_types::error::ErrorObject<'static> {
    fn from(err: BaseInvalidTransactionError) -> Self {
        match err {
            BaseInvalidTransactionError::Eip8130NotAccepted
            | BaseInvalidTransactionError::Eip8130Rejected(_) => {
                rpc_err(EthRpcErrorCode::TransactionRejected.code(), err.to_string(), None)
            }
        }
    }
}

impl BaseInvalidTransactionError {
    /// Returns the Base-specific rejection an EVM handler error carries, if any.
    pub fn from_handler_error(error: &HandlerError) -> Option<Self> {
        match error.external_ref::<Eip8130Error>()? {
            Eip8130Error::Invalid(reason) => Some(Self::Eip8130Rejected(reason.clone())),
            Eip8130Error::PayerCannotPay { .. } => None,
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

/// Execution errors keep their Base-specific transaction rejections; the rest map as on
/// Ethereum.
impl From<BlockExecutionError> for BaseEthApiError {
    fn from(error: BlockExecutionError) -> Self {
        let handler_error = match &error {
            BlockExecutionError::Validation(BlockValidationError::Other(error)) => {
                error.downcast_ref::<HandlerError>()
            }
            BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. }) => {
                error.as_any().downcast_ref::<HandlerError>()
            }
            _ => None,
        };
        let rejection = handler_error.and_then(BaseInvalidTransactionError::from_handler_error);
        rejection.map_or_else(|| Self::Eth(error.into()), Self::InvalidTransaction)
    }
}

impl FromEvmHalt<InstrStop> for BaseEthApiError {
    fn from_evm_halt(halt: InstrStop, gas_limit: u64) -> Self {
        EthApiError::from_evm_halt(halt, gas_limit).into()
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

impl From<BaseTxEnvError> for BaseEthApiError {
    fn from(value: BaseTxEnvError) -> Self {
        match value {
            BaseTxEnvError::Eth(error) => error.into(),
            BaseTxEnvError::Eip8130Unsupported => {
                Self::Eth(EthApiError::InvalidParams(value.to_string()))
            }
        }
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

#[cfg(test)]
mod tests {
    use reth_evm::execute::map_handler_error;

    use super::*;

    #[test]
    fn eip8130_rejections_keep_their_reason_as_a_rejected_transaction() {
        let error = map_handler_error(HandlerError::external(Eip8130Error::invalid(
            "payer authorization expired",
        )));

        let error = BaseEthApiError::from(error);

        assert!(matches!(
            &error,
            BaseEthApiError::InvalidTransaction(BaseInvalidTransactionError::Eip8130Rejected(reason))
                if reason == "payer authorization expired"
        ));
        let error = jsonrpsee_types::error::ErrorObject::from(error);
        assert_eq!(error.code(), EthRpcErrorCode::TransactionRejected.code());
        assert_eq!(error.message(), "EIP-8130 transaction rejected: payer authorization expired");
    }

    #[test]
    fn other_handler_errors_map_as_on_ethereum() {
        let error = map_handler_error(HandlerError::InvalidNonce { expected: 2, got: 1 });

        assert!(matches!(BaseEthApiError::from(error), BaseEthApiError::Eth(_)));
    }
}
