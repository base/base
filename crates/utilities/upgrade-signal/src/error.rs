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
    /// A positive activation timestamp was not paired with a minimum node protocol version.
    #[error(
        "upgrade signal for {0} has an activation timestamp but no minimum node protocol version"
    )]
    MissingProtocolVersion(String),
    /// The contract requires a newer node protocol version than this binary supports.
    #[error(
        "upgrade signal for {upgrade_id} requires node protocol version {minimum_protocol_version}, but this binary supports {node_protocol_version}"
    )]
    UnsupportedProtocolVersion {
        /// Upgrade ID whose signal required a newer protocol version.
        upgrade_id: String,
        /// Minimum node protocol version read from L1.
        minimum_protocol_version: String,
        /// Node protocol version supported by this binary.
        node_protocol_version: String,
    },
    /// A runtime Beryl schedule was requested without a known activation admin address.
    #[error("missing activation admin address for Beryl-enabled chain ID: {chain_id}")]
    MissingActivationAdminAddress {
        /// L2 chain ID whose runtime schedule was validated.
        chain_id: u64,
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

    /// Creates a missing protocol version error.
    pub const fn missing_protocol_version(upgrade_id: String) -> Self {
        Self::MissingProtocolVersion(upgrade_id)
    }

    /// Creates an unsupported protocol version error.
    pub fn unsupported_protocol_version(
        upgrade_id: String,
        minimum_protocol_version: impl ToString,
        node_protocol_version: impl ToString,
    ) -> Self {
        Self::UnsupportedProtocolVersion {
            upgrade_id,
            minimum_protocol_version: minimum_protocol_version.to_string(),
            node_protocol_version: node_protocol_version.to_string(),
        }
    }

    /// Creates a missing activation admin address error.
    pub const fn missing_activation_admin_address(chain_id: u64) -> Self {
        Self::MissingActivationAdminAddress { chain_id }
    }
}
