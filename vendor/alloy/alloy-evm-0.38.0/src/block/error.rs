use crate::{EvmError, InvalidTxError};
use alloc::{
    boxed::Box,
    string::{String, ToString},
};
use alloy_primitives::B256;
use core::error::Error;

/// Block validation error.
#[derive(Debug, thiserror::Error)]
pub enum BlockValidationError {
    /// EVM error with transaction hash and message
    #[error("EVM reported invalid transaction ({hash}): {error}")]
    InvalidTx {
        /// The hash of the transaction
        hash: B256,
        /// The EVM error.
        error: Box<dyn InvalidTxError>,
    },
    /// EVM error that is not a transaction validation error. Most likely a BAL error propagated
    /// from the database.
    #[error("EVM execution failed: {error}")]
    EVM {
        /// The hash of the transaction
        hash: B256,
        /// The EVM error.
        error: Box<dyn Error + Send + Sync + 'static>,
    },
    /// Error when incrementing balance in post execution
    #[error("incrementing balance in post execution failed")]
    IncrementBalanceFailed,
    /// Error when transaction gas limit exceeds available block gas
    #[error(
        "transaction gas limit {transaction_gas_limit} is more than blocks available gas {block_available_gas}"
    )]
    TransactionGasLimitMoreThanAvailableBlockGas {
        /// The transaction's gas limit
        transaction_gas_limit: u64,
        /// The available block gas
        block_available_gas: u64,
    },
    /// Error for EIP-4788 when parent beacon block root is missing
    #[error("EIP-4788 parent beacon block root missing for active Cancun block")]
    MissingParentBeaconBlockRoot,
    /// Error for Cancun genesis block when parent beacon block root is not zero
    #[error(
        "the parent beacon block root is not zero for Cancun genesis block: {parent_beacon_block_root}"
    )]
    CancunGenesisParentBeaconBlockRootNotZero {
        /// The beacon block root
        parent_beacon_block_root: B256,
    },
    /// EVM error during [EIP-4788] beacon root contract call.
    ///
    /// [EIP-4788]: https://eips.ethereum.org/EIPS/eip-4788
    #[error("failed to apply beacon root contract call at {parent_beacon_block_root}: {message}")]
    BeaconRootContractCall {
        /// The beacon block root
        parent_beacon_block_root: Box<B256>,
        /// The error message.
        message: String,
    },
    /// EVM error during [EIP-2935] blockhash contract call.
    ///
    /// [EIP-2935]: https://eips.ethereum.org/EIPS/eip-2935
    #[error("failed to apply blockhash contract call: {message}")]
    BlockHashContractCall {
        /// The error message.
        message: String,
    },
    /// EVM error during withdrawal requests contract call [EIP-7002]
    ///
    /// [EIP-7002]: https://eips.ethereum.org/EIPS/eip-7002
    #[error("failed to apply withdrawal requests contract call: {message}")]
    WithdrawalRequestsContractCall {
        /// The error message.
        message: String,
    },
    /// EVM error during consolidation requests contract call [EIP-7251]
    ///
    /// [EIP-7251]: https://eips.ethereum.org/EIPS/eip-7251
    #[error("failed to apply consolidation requests contract call: {message}")]
    ConsolidationRequestsContractCall {
        /// The error message.
        message: String,
    },
    /// EVM error during builder deposit requests contract call [EIP-8282]
    ///
    /// [EIP-8282]: https://eips.ethereum.org/EIPS/eip-8282
    #[error("failed to apply builder deposit requests contract call: {message}")]
    BuilderDepositRequestsContractCall {
        /// The error message.
        message: String,
    },
    /// EVM error during builder exit requests contract call [EIP-8282]
    ///
    /// [EIP-8282]: https://eips.ethereum.org/EIPS/eip-8282
    #[error("failed to apply builder exit requests contract call: {message}")]
    BuilderExitRequestsContractCall {
        /// The error message.
        message: String,
    },
    /// Error when decoding deposit requests from receipts [EIP-6110]
    ///
    /// [EIP-6110]: https://eips.ethereum.org/EIPS/eip-6110
    #[error("failed to decode deposit requests from receipts: {_0}")]
    DepositRequestDecode(String),
    /// Error when block's total gas used exceeds the block gas limit
    ///
    /// [EIP-8037]: https://eips.ethereum.org/EIPS/eip-8037
    #[error("block gas used exceeds block gas limit")]
    BlockGasExceeded,
    /// Arbitrary Block validation errors.
    #[error(transparent)]
    Other(Box<dyn core::error::Error + Send + Sync + 'static>),
}

impl BlockValidationError {
    /// Create a new [`BlockValidationError::Other`] variant.
    pub fn other<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::Other(Box::new(error))
    }

    /// Create a new [`BlockValidationError::Other`] variant from a given message.
    pub fn msg(msg: impl core::fmt::Display) -> Self {
        Self::Other(msg.to_string().into())
    }
}

/// `BlockExecutor` Errors
#[derive(Debug, thiserror::Error)]
pub enum BlockExecutionError {
    /// Validation error, transparently wrapping [`BlockValidationError`]
    #[error(transparent)]
    Validation(#[from] BlockValidationError),
    /// Internal, i.e. non consensus or validation related Block Executor Errors
    #[error(transparent)]
    Internal(#[from] InternalBlockExecutionError),
}

impl BlockExecutionError {
    /// Create a new [`BlockExecutionError::Internal`] variant, containing a
    /// [`InternalBlockExecutionError::Other`] error.
    pub fn other<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::Internal(InternalBlockExecutionError::other(error))
    }

    /// Create a new [`BlockExecutionError::Internal`] variant, containing a
    /// [`InternalBlockExecutionError::Other`] error with the given message.
    pub fn msg(msg: impl core::fmt::Display) -> Self {
        Self::Internal(InternalBlockExecutionError::msg(msg))
    }

    /// Returns the inner `BlockValidationError` if the error is a validation error.
    pub const fn as_validation(&self) -> Option<&BlockValidationError> {
        match self {
            Self::Validation(err) => Some(err),
            _ => None,
        }
    }

    /// Returns the inner [`InternalBlockExecutionError`] if this is an internal error.
    pub const fn as_internal(&self) -> Option<&InternalBlockExecutionError> {
        match self {
            Self::Internal(err) => Some(err),
            _ => None,
        }
    }

    /// Handles an EVM error occurred when executing a transaction.
    ///
    /// If an error matches [`EvmError::InvalidTransaction`], it will be wrapped into
    /// [`BlockValidationError::InvalidTx`], otherwise if [`EvmError::is_fatal`] return `true`, the
    /// error would be wrapped into [`InternalBlockExecutionError::EVM`], otherwise it would be
    /// wrapped into [`BlockValidationError::EVM`].
    pub fn evm<E: EvmError>(error: E, hash: B256) -> Self {
        match error.try_into_invalid_tx_err() {
            Ok(err) => {
                Self::Validation(BlockValidationError::InvalidTx { hash, error: Box::new(err) })
            }
            Err(err) => {
                // Route errors based on whether they are fatal or not.
                if err.is_fatal() {
                    Self::Internal(InternalBlockExecutionError::EVM { hash, error: Box::new(err) })
                } else {
                    Self::Validation(BlockValidationError::EVM { hash, error: Box::new(err) })
                }
            }
        }
    }
}

/// Internal (i.e., not validation or consensus related) `BlockExecutor` Errors
#[derive(Debug, thiserror::Error)]
pub enum InternalBlockExecutionError {
    /// EVM error occurred when executing a transaction. This is different from
    /// [`BlockValidationError::InvalidTx`] because it will only contain EVM errors which are not
    /// transaction validation errors and are assumed to be fatal.
    ///
    /// Common errors that end up here:
    /// - `EVMError::Database` — database access failures
    /// - `EVMError::Header` — header validation failures
    /// - `EVMError::Custom` — custom errors, including fatal precompile errors
    ///   (`PrecompileErrors::Fatal` surfaces as `EVMError::Custom(String)`)
    ///
    /// Downcasting via [`InternalBlockExecutionError::downcast_evm`] requires knowing the concrete
    /// `EVMError<DBError, TxError>` type parameters.
    #[error("internal EVM error occurred when executing transaction {hash}: {error}")]
    EVM {
        /// The hash of the transaction
        hash: B256,
        /// The EVM error.
        error: Box<dyn core::error::Error + Send + Sync + 'static>,
    },
    /// Arbitrary Block Executor Errors
    #[error(transparent)]
    Other(Box<dyn core::error::Error + Send + Sync + 'static>),
}

impl InternalBlockExecutionError {
    /// Create a new [`InternalBlockExecutionError::Other`] variant.
    pub fn other<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::Other(Box::new(error))
    }

    /// Create a new [`InternalBlockExecutionError::Other`] from a given message.
    pub fn msg(msg: impl core::fmt::Display) -> Self {
        Self::Other(msg.to_string().into())
    }

    /// Returns the arbitrary error if it is [`InternalBlockExecutionError::Other`]
    pub fn as_other(&self) -> Option<&(dyn core::error::Error + Send + Sync + 'static)> {
        match self {
            Self::Other(err) => Some(&**err),
            _ => None,
        }
    }

    /// Attempts to downcast the [`InternalBlockExecutionError::Other`] variant to a concrete type
    pub fn downcast<T: core::error::Error + 'static>(self) -> Result<Box<T>, Self> {
        match self {
            Self::Other(err) => err.downcast().map_err(Self::Other),
            err => Err(err),
        }
    }

    /// Returns a reference to the [`InternalBlockExecutionError::Other`] value if this type is a
    /// [`InternalBlockExecutionError::Other`] of that type. Returns None otherwise.
    pub fn downcast_other<T: core::error::Error + 'static>(&self) -> Option<&T> {
        let other = self.as_other()?;
        other.downcast_ref()
    }

    /// Returns true if the this type is a [`InternalBlockExecutionError::Other`] of that error
    /// type. Returns false otherwise.
    pub fn is_other<T: core::error::Error + 'static>(&self) -> bool {
        self.as_other().map(|err| err.is::<T>()).unwrap_or(false)
    }

    /// Returns the EVM error and transaction hash if this is
    /// [`InternalBlockExecutionError::EVM`].
    pub fn as_evm(&self) -> Option<(&B256, &(dyn core::error::Error + Send + Sync + 'static))> {
        match self {
            Self::EVM { hash, error } => Some((hash, &**error)),
            _ => None,
        }
    }

    /// Returns a reference to the inner EVM error if it is of type `T`.
    pub fn downcast_evm<T: core::error::Error + 'static>(&self) -> Option<&T> {
        let (_, err) = self.as_evm()?;
        err.downcast_ref()
    }

    /// Returns true if this is an [`InternalBlockExecutionError::EVM`] error of type `T`.
    pub fn is_evm<T: core::error::Error + 'static>(&self) -> bool {
        self.as_evm().map(|(_, err)| err.is::<T>()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{
        context::DBErrorMarker,
        context_interface::result::{EVMError, InvalidTransaction},
    };

    #[derive(thiserror::Error, Debug)]
    #[error("err")]
    struct E;

    impl DBErrorMarker for E {}

    #[test]
    fn other_downcast() {
        let err = InternalBlockExecutionError::other(E);
        assert!(err.is_other::<E>());

        assert!(err.downcast_other::<E>().is_some());
        assert!(err.downcast::<E>().is_ok());
    }

    #[test]
    fn evm_downcast() {
        let hash = B256::with_last_byte(1);
        let evm_err: EVMError<E, InvalidTransaction> =
            EVMError::Custom("fatal precompile error".to_string());
        let err = BlockExecutionError::evm(evm_err, hash);

        // Lands in Internal(EVM { .. })
        let internal = err.as_internal().expect("should be internal");

        // Type checks
        assert!(internal.is_evm::<EVMError<E, InvalidTransaction>>());
        assert!(!internal.is_evm::<E>());

        // Downcast and inspect
        let downcasted =
            internal.downcast_evm::<EVMError<E, InvalidTransaction>>().expect("should downcast");
        assert!(matches!(downcasted, EVMError::Custom(msg) if msg == "fatal precompile error"));

        // Hash preserved
        let (h, _) = internal.as_evm().expect("should be evm");
        assert_eq!(*h, hash);
    }
}
