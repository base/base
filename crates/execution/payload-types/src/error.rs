//! Error types for payload operations.

use alloc::{boxed::Box, string::ToString};
use core::error;

use alloy_primitives::B256;
use base_common_types_payload::{ForkchoiceUpdateError, PayloadError, PayloadStatusEnum};
use base_evm_handler::BlockExecutionError;
use reth_storage_errors::provider::ProviderError;
use tokio::sync::{mpsc, oneshot};

/// Possible error variants during payload building.
#[derive(Debug, thiserror::Error)]
pub enum PayloadBuilderError {
    /// Thrown when the parent header cannot be found
    #[error("missing parent header: {0}")]
    MissingParentHeader(B256),
    /// Thrown when the parent block is missing.
    #[error("missing parent block {0}")]
    MissingParentBlock(B256),
    /// An oneshot channels has been closed.
    #[error("sender has been dropped")]
    ChannelClosed,
    /// If there's no payload to resolve.
    #[error("missing payload")]
    MissingPayload,
    /// Failed to access chain data.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Unrecoverable error during evm execution.
    #[error("evm execution error: {0}")]
    EvmExecutionError(Box<dyn core::error::Error + Send + Sync>),
    /// Any other payload building errors.
    #[error(transparent)]
    Other(Box<dyn core::error::Error + Send + Sync>),
}

impl PayloadBuilderError {
    /// Create a new EVM error from a boxed error.
    pub fn evm<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::EvmExecutionError(Box::new(error))
    }

    /// Create a new error from a boxed error.
    pub fn other<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::Other(Box::new(error))
    }
}

impl From<oneshot::error::RecvError> for PayloadBuilderError {
    fn from(_: oneshot::error::RecvError) -> Self {
        Self::ChannelClosed
    }
}

impl From<BlockExecutionError> for PayloadBuilderError {
    fn from(error: BlockExecutionError) -> Self {
        Self::evm(error)
    }
}

impl<T> From<mpsc::error::SendError<T>> for PayloadBuilderError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        Self::ChannelClosed
    }
}

/// Error validating payload received over `newPayload` API.
#[derive(thiserror::Error, Debug)]
pub enum NewPayloadError {
    /// Payload validation error.
    #[error(transparent)]
    Eth(#[from] PayloadError),
    /// Custom payload validation error.
    #[error(transparent)]
    Other(Box<dyn error::Error + Send + Sync>),
}

impl NewPayloadError {
    /// Creates instance of variant [`NewPayloadError::Other`].
    #[inline]
    pub fn other(err: impl error::Error + Send + Sync + 'static) -> Self {
        Self::Other(Box::new(err))
    }

    /// Returns `true` if the error is caused by a block hash mismatch.
    #[inline]
    pub const fn is_block_hash_mismatch(&self) -> bool {
        matches!(self, Self::Eth(PayloadError::BlockHash { .. }))
    }

    /// Returns `true` if the error is caused by invalid block hashes (Cancun).
    #[inline]
    pub const fn is_invalid_versioned_hashes(&self) -> bool {
        matches!(self, Self::Eth(PayloadError::InvalidVersionedHashes))
    }
}

impl From<NewPayloadError> for PayloadStatusEnum {
    fn from(error: NewPayloadError) -> Self {
        Self::Invalid { validation_error: error.to_string() }
    }
}

/// Thrown when validating the correctness of a payloadattributes object.
#[derive(thiserror::Error, Debug)]
pub enum InvalidPayloadAttributesError {
    /// Thrown if the timestamp of the payload attributes is invalid according to the engine specs.
    #[error("invalid timestamp")]
    InvalidTimestamp,
    /// Another type of error that is not covered by the above variants.
    #[error("Invalid params: {0}")]
    InvalidParams(#[from] Box<dyn core::error::Error + Send + Sync>),
}

impl From<InvalidPayloadAttributesError> for ForkchoiceUpdateError {
    fn from(_: InvalidPayloadAttributesError) -> Self {
        Self::UpdatedInvalidPayloadAttributes
    }
}
