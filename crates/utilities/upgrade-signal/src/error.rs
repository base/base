//! Upgrade signal error types.

/// Error returned by upgrade signal readers and observers.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeSignalError {
    /// An L1 provider request failed.
    #[error("{context}: {error}")]
    Provider {
        /// Request context.
        context: &'static str,
        /// Provider error string.
        error: String,
    },
    /// A contract return value failed to decode.
    #[error("{context}: {error}")]
    Decode {
        /// Decode context.
        context: &'static str,
        /// Decode error string.
        error: String,
    },
    /// The activation timestamp does not fit in a `u64`.
    #[error("activation timestamp {0} does not fit in u64")]
    TimestampOverflow(String),
}

impl UpgradeSignalError {
    /// Creates a provider error.
    pub fn provider(context: &'static str, error: impl ToString) -> Self {
        Self::Provider { context, error: error.to_string() }
    }

    /// Creates a decode error.
    pub fn decode(context: &'static str, error: impl ToString) -> Self {
        Self::Decode { context, error: error.to_string() }
    }

    /// Creates a timestamp overflow error.
    pub fn timestamp_overflow(value: impl ToString) -> Self {
        Self::TimestampOverflow(value.to_string())
    }
}
