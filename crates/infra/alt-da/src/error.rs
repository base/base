use thiserror::Error;

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
