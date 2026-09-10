use std::ops::RangeInclusive;

use alloy_primitives::{B256, BlockNumber};
use base_common_types_chain::{GotExpected, GotExpectedBoxed};
use base_execution_evm_blocks::ConsensusError;
use base_execution_network_wire::RequestError;
use base_execution_state_types::{DatabaseError, ProviderError};
use derive_more::{Display, Error};

/// The download result type
pub type DownloadResult<T> = Result<T, DownloadError>;

/// The downloader error type
#[derive(Debug, Clone, Display, Error)]
pub enum DownloadError {
    /* ==================== HEADER ERRORS ==================== */
    /// Header validation failed.
    #[display("failed to validate header {hash}, block number {number}: {error}")]
    HeaderValidation {
        /// Hash of header failing validation
        hash: B256,
        /// Number of header failing validation
        number: u64,
        /// The details of validation failure
        #[error(source)]
        error: Box<ConsensusError>,
    },
    /// Received an invalid tip.
    #[display("received invalid tip: {_0}")]
    InvalidTip(GotExpectedBoxed<B256>),
    /// Received a tip with an invalid tip number.
    #[display("received invalid tip number: {_0}")]
    InvalidTipNumber(GotExpected<u64>),
    /// Received a response to a request with unexpected start block
    #[display("headers response starts at unexpected block: {_0}")]
    HeadersResponseStartBlockMismatch(GotExpected<u64>),
    /// Received more headers than requested.
    #[display("received more headers than requested: {_0}")]
    HeadersResponseTooLong(GotExpected<u64>),

    /* ==================== BODIES ERRORS ==================== */
    /// Block validation failed
    #[display("failed to validate body for header {hash}, block number {number}: {error}")]
    BodyValidation {
        /// Hash of the block failing validation
        hash: B256,
        /// Number of the block failing validation
        number: u64,
        /// The details of validation failure
        error: Box<ConsensusError>,
    },
    /// Received more bodies than requested.
    #[display("received more bodies than requested: {_0}")]
    TooManyBodies(GotExpected<usize>),
    /// Headers missing from the database.
    #[display("header missing from the database: {block_number}")]
    MissingHeader {
        /// Missing header block number.
        block_number: BlockNumber,
    },
    /// Body range invalid
    #[display("requested body range is invalid: {range:?}")]
    InvalidBodyRange {
        /// Invalid block number range.
        range: RangeInclusive<BlockNumber>,
    },
    /* ==================== COMMON ERRORS ==================== */
    /// Timed out while waiting for request id response.
    #[display("timed out while waiting for response")]
    Timeout,
    /// Received empty response while expecting non empty
    #[display("received empty response")]
    EmptyResponse,
    /// Error while executing the request.
    RequestError(RequestError),
    /// Provider error.
    Provider(ProviderError),
}

impl From<DatabaseError> for DownloadError {
    fn from(error: DatabaseError) -> Self {
        Self::Provider(ProviderError::Database(error))
    }
}

impl From<RequestError> for DownloadError {
    fn from(error: RequestError) -> Self {
        Self::RequestError(error)
    }
}

impl From<ProviderError> for DownloadError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}
