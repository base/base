//! RPC errors specific to Base.

use std::convert::Infallible;

use alloy_json_rpc::ErrorPayload;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{BlockError, error::EthRpcErrorCode};
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_consensus::TxBaseCommittedPhasesError;
use base_common_evm::{BaseHaltReason, BaseTransactionError};
use base_common_rpc_types::BaseTxEnvError;
use base_execution_evm::BaseBlockExecutionError;
use jsonrpsee_types::error::INTERNAL_ERROR_CODE;
use reth_evm::execute::ProviderError;
use reth_rpc_eth_api::{AsEthApiError, EthTxEnvError, TransactionConversionError};
use reth_rpc_eth_types::{
    EthApiError, RpcInvalidTransactionError,
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
    /// A stored Base transaction receipt carries a committed phase count that violates the receipt
    /// invariant.
    #[error("invalid Base transaction receipt: {0}")]
    TxBaseCommittedPhases(#[from] TxBaseCommittedPhasesError),
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
            // geth and Nethermind answer an unknown block with -32000 "header not found", not
            // EIP-1474's -32001. Missing receipts (gas oracle, otterscan) keep reth's answer.
            BaseEthApiError::Eth(EthApiError::HeaderNotFound(_)) => {
                rpc_err(EthRpcErrorCode::InvalidInput.code(), "header not found", None)
            }
            BaseEthApiError::Eth(err @ EthApiError::HeaderRangeNotFound(..)) => {
                rpc_err(EthRpcErrorCode::InvalidInput.code(), err.to_string(), None)
            }
            // As in geth and Nethermind, not EIP-1474's -32003.
            BaseEthApiError::Eth(
                err @ EthApiError::InvalidTransaction(RpcInvalidTransactionError::TipAboveFeeCap),
            ) => rpc_err(EthRpcErrorCode::InvalidInput.code(), err.to_string(), None),
            BaseEthApiError::Eth(err) => err.into(),
            BaseEthApiError::InvalidTransaction(err) => err.into(),
            BaseEthApiError::Evm(_)
            | BaseEthApiError::L1BlockFeeError
            | BaseEthApiError::L1BlockGasError
            | BaseEthApiError::TxBaseCommittedPhases(_) => internal_rpc_err(err.to_string()),
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
    /// A Base transaction was submitted via
    /// `eth_sendRawTransaction` before the Everest fork was active.
    ///
    /// The transaction type byte (`0x79`) is recognised by the consensus layer for
    /// decoding/serialization purposes, but RPC admission is rejected until the
    /// Everest fork is active. The txpool validator enforces the same fork gate for
    /// transactions arriving over devp2p.
    #[error("{}", base_common_consensus::BASE_TX_REJECTION_MSG)]
    TxBaseNotAccepted,
    /// A Base transaction was rejected during its
    /// enshrined execution pipeline (authorization, nonce, intrinsic gas, fee, or
    /// account-change apply). The string carries the underlying rejection reason.
    #[error("Base transaction rejected: {0}")]
    TxBaseRejected(String),
}

impl From<BaseInvalidTransactionError> for jsonrpsee_types::error::ErrorObject<'static> {
    fn from(err: BaseInvalidTransactionError) -> Self {
        match err {
            BaseInvalidTransactionError::DepositSystemTxPostRegolith
            | BaseInvalidTransactionError::HaltedDepositPostRegolith
            | BaseInvalidTransactionError::MissingEnvelopedTx
            | BaseInvalidTransactionError::TxBaseNotAccepted
            | BaseInvalidTransactionError::TxBaseRejected(_) => {
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
            BaseTransactionError::TxBase(reason) => Ok(Self::TxBaseRejected(reason)),
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
            EVMError::CustomAny(err) => Self::Eth(EthApiError::EvmCustom(err.to_string())),
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

impl From<BaseTxEnvError> for BaseEthApiError {
    fn from(value: BaseTxEnvError) -> Self {
        match value {
            BaseTxEnvError::Eth(error) => error.into(),
            BaseTxEnvError::TxBaseUnsupported | BaseTxEnvError::DepositUnsupported => {
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
    use alloy_primitives::B256;
    use alloy_rpc_types_eth::BlockId;
    use jsonrpsee_types::ErrorObject;

    use super::*;

    #[test]
    fn unknown_block_is_header_not_found() {
        let error: ErrorObject<'static> =
            BaseEthApiError::Eth(EthApiError::HeaderNotFound(BlockId::number(0xffff_ffff))).into();
        assert_eq!((error.code(), error.message()), (-32000, "header not found"));
        // Missing receipts of an existing block are not an unknown block.
        let receipts: ErrorObject<'static> = BaseEthApiError::Eth(EthApiError::ReceiptsNotFound(
            BlockId::hash(B256::repeat_byte(0x11)),
        ))
        .into();
        assert_eq!(receipts.code(), -32001);
        let range: ErrorObject<'static> =
            BaseEthApiError::Eth(EthApiError::HeaderRangeNotFound(1.into(), 2.into())).into();
        assert_eq!(range.code(), -32000);
        let tip: ErrorObject<'static> = BaseEthApiError::Eth(EthApiError::InvalidTransaction(
            RpcInvalidTransactionError::TipAboveFeeCap,
        ))
        .into();
        assert_eq!(tip.code(), -32000);
        // Halts keep EIP-1474's -32003, as Nethermind answers them.
        let halt: ErrorObject<'static> = BaseEthApiError::Eth(EthApiError::from_evm_halt(
            revm::context_interface::result::HaltReason::OpcodeNotFound,
            0,
        ))
        .into();
        assert_eq!(halt.code(), -32003);
    }
}
