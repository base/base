//! Upgrade signal error types.

/// Error returned by upgrade signal readers.
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
    /// A positive activation timestamp was not paired with a minimum node protocol version.
    #[error(
        "upgrade signal for {0} has an activation timestamp but no minimum node protocol version"
    )]
    MissingProtocolVersion(String),
    /// The contract requires a newer node protocol version than this binary supports.
    #[error(
        "upgrade signal for {hardfork_id} requires node protocol version {minimum_protocol_version}, but this binary supports {node_protocol_version}"
    )]
    UnsupportedProtocolVersion {
        /// Hardfork ID whose signal required a newer protocol version.
        hardfork_id: String,
        /// Minimum node protocol version read from L1.
        minimum_protocol_version: String,
        /// Node protocol version supported by this binary.
        node_protocol_version: String,
    },
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

    /// Creates a missing protocol version error.
    pub fn missing_protocol_version(hardfork_id: String) -> Self {
        Self::MissingProtocolVersion(hardfork_id)
    }

    /// Creates an unsupported protocol version error.
    pub fn unsupported_protocol_version(
        hardfork_id: String,
        minimum_protocol_version: impl ToString,
        node_protocol_version: impl ToString,
    ) -> Self {
        Self::UnsupportedProtocolVersion {
            hardfork_id,
            minimum_protocol_version: minimum_protocol_version.to_string(),
            node_protocol_version: node_protocol_version.to_string(),
        }
    }
}
