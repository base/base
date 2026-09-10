use alloc::boxed::Box;

use base_execution_evm_runtime::{
    BlockExecutionError, BlockValidationError, InternalBlockExecutionError,
};
use base_execution_state_types::ProviderError;

use crate::ConsensusError;

/// All error variants possible when inserting or validating a block.
#[derive(Debug, thiserror::Error)]
pub enum InsertBlockErrorKind {
    /// Block violated consensus rules.
    #[error(transparent)]
    Consensus(#[from] ConsensusError),
    /// Block execution failed.
    #[error(transparent)]
    Execution(#[from] BlockExecutionError),
    /// Provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Other errors.
    #[error(transparent)]
    Other(#[from] Box<dyn core::error::Error + Send + Sync + 'static>),
}

impl InsertBlockErrorKind {
    /// Returns whether the error was caused by an invalid block.
    pub const fn is_validation_error(&self) -> bool {
        matches!(self, Self::Consensus(_) | Self::Execution(BlockExecutionError::Validation(_)))
    }

    /// Returns an [`InsertBlockValidationError`] if the error is caused by an invalid block.
    ///
    /// Returns an [`InsertBlockFatalError`] if the error is caused by an error that is not
    /// validation related or is otherwise fatal.
    ///
    /// This is intended to be used to determine if we should respond `INVALID` as a response when
    /// processing a new block.
    pub fn ensure_validation_error(
        self,
    ) -> Result<InsertBlockValidationError, InsertBlockFatalError> {
        match self {
            Self::Consensus(err) => Ok(InsertBlockValidationError::Consensus(err)),
            Self::Execution(err) => match err {
                BlockExecutionError::Validation(err) => {
                    Ok(InsertBlockValidationError::Validation(err))
                }
                BlockExecutionError::Internal(error) => {
                    Err(InsertBlockFatalError::BlockExecutionError(error))
                }
            },
            Self::Provider(err) => Err(InsertBlockFatalError::Provider(err)),
            Self::Other(err) => Err(InternalBlockExecutionError::Other(err).into()),
        }
    }
}

/// Error variants that are not caused by invalid blocks.
#[derive(Debug, thiserror::Error)]
pub enum InsertBlockFatalError {
    /// A provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// An internal or fatal block execution error.
    #[error(transparent)]
    BlockExecutionError(#[from] InternalBlockExecutionError),
}

/// Error variants that are caused by invalid blocks.
#[derive(Debug, thiserror::Error)]
pub enum InsertBlockValidationError {
    /// Block violated consensus rules.
    #[error(transparent)]
    Consensus(#[from] ConsensusError),
    /// Validation error, transparently wrapping [`BlockValidationError`].
    #[error(transparent)]
    Validation(#[from] BlockValidationError),
}
