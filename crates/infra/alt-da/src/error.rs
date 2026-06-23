use thiserror::Error;

use crate::commitment::GENERIC_COMMITMENT_LEN;

/// Generic commitment failed structural validation.
///
/// Independent of the server/store [`Error`] and the [`ClientError`] hierarchies so
/// both sides can validate a commitment and map the failure into their own type.
#[derive(Debug, Error)]
pub enum CommitmentError {
    /// Commitment was not exactly [`GENERIC_COMMITMENT_LEN`] bytes.
    #[error("invalid generic commitment length: {len} (expected {GENERIC_COMMITMENT_LEN})")]
    InvalidLength {
        /// Actual commitment length in bytes.
        len: usize,
    },
    /// Commitment did not start with the type + sentinel prefix.
    #[error("invalid generic commitment prefix")]
    InvalidPrefix,
}

/// Alt-DA server and store errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Invalid user input (bad hex, empty body).
    #[error("{0}")]
    BadRequest(String),
    /// Object not found in the backing store.
    #[error("commitment not found")]
    NotFound,
    /// `BASE_DA_URL` or store setup failed at startup.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Runtime store or network failure.
    #[error(transparent)]
    Store(StoreError),
    /// Internal server error.
    #[error(transparent)]
    Internal(#[from] InternalError),
}

/// `StoreOpener::open` failed (bad URL or unsupported scheme).
#[derive(Debug, Error)]
pub enum ConfigError {
    /// `BASE_DA_URL` used an unsupported scheme.
    #[error("unsupported da url scheme: {scheme}")]
    UnsupportedScheme {
        /// URL scheme from `BASE_DA_URL`.
        scheme: String,
    },
    /// `BASE_DA_URL` could not be parsed.
    #[error("invalid da url: {0}")]
    InvalidUrl(String),
    /// Local store root could not be created.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Runtime backing store operation failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Object not found in storage.
    #[error("object not found")]
    NotFound,
    /// Local filesystem I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// S3 request failed.
    #[error("s3 error: {0}")]
    S3(String),
    /// Object exceeds [`crate::MAX_OBJECT_BYTES`].
    #[error("object too large: {size} bytes (max {max})")]
    ObjectTooLarge {
        /// Stored or requested object size in bytes.
        size: u64,
        /// Configured maximum object size in bytes.
        max: usize,
    },
}

/// Unexpected internal failures.
#[derive(Debug, Error)]
pub enum InternalError {
    /// HTTP server failed to start or serve requests.
    #[error("http server error: {0}")]
    Http(String),
}

/// Alt-DA HTTP client request failed.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Request body was empty.
    #[error("empty alt-da put body")]
    EmptyBody,
    /// Request body exceeds [`crate::MAX_OBJECT_BYTES`].
    #[error("alt-da put body too large: {size} bytes (max {max})")]
    BodyTooLarge {
        /// Request body size in bytes.
        size: usize,
        /// Configured maximum object size in bytes.
        max: usize,
    },
    /// HTTP transport or response read failed.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// DA server returned a non-success status.
    #[error("alt-da put failed with status {status}: {detail}")]
    UnexpectedStatus {
        /// HTTP status code.
        status: u16,
        /// Bounded response body from the server, if any.
        detail: String,
    },
    /// PUT response was not a 34-byte generic commitment.
    #[error("alt-da put returned invalid commitment length: {len}")]
    InvalidCommitmentLen {
        /// Response body length in bytes.
        len: usize,
    },
    /// PUT response had a malformed generic commitment prefix.
    #[error("alt-da put returned malformed commitment prefix")]
    InvalidCommitment,
    /// GET found no object for the requested commitment.
    #[error("alt-da get: commitment not found")]
    NotFound,
    /// GET response body exceeds [`crate::MAX_OBJECT_BYTES`].
    #[error("alt-da get response too large: {size} bytes (max {max})")]
    ResponseTooLarge {
        /// Response body size in bytes.
        size: usize,
        /// Configured maximum object size in bytes.
        max: usize,
    },
}

impl From<CommitmentError> for ClientError {
    fn from(err: CommitmentError) -> Self {
        match err {
            CommitmentError::InvalidLength { len } => Self::InvalidCommitmentLen { len },
            CommitmentError::InvalidPrefix => Self::InvalidCommitment,
        }
    }
}

impl From<StoreError> for Error {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound => Self::NotFound,
            other => Self::Store(other),
        }
    }
}

impl From<aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>>
    for StoreError
{
    fn from(
        err: aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
    ) -> Self {
        match &err {
            aws_sdk_s3::error::SdkError::ServiceError(service_err)
                if service_err.err().is_no_such_key() =>
            {
                Self::NotFound
            }
            _ => Self::S3(err.to_string()),
        }
    }
}

impl From<aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::put_object::PutObjectError>>
    for StoreError
{
    fn from(
        err: aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::put_object::PutObjectError>,
    ) -> Self {
        Self::S3(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_not_found_maps_to_error_not_found() {
        let err: Error = StoreError::NotFound.into();
        assert!(matches!(err, Error::NotFound));
    }
}
