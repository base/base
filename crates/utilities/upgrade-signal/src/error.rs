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
    /// A successful `getSchedule` call returned no schedule entries.
    ///
    /// A healthy append-only `ProtocolVersions` contract always reports at least the oldest
    /// registered upgrade, so an empty read signals a misconfigured, uninitialized, or mock
    /// contract rather than an authoritative instruction to clear every runtime override. It is
    /// kept distinct from a read failure so the empty-success case never advances local state.
    #[error("getSchedule returned an empty schedule from a contract that should be append-only")]
    EmptySchedule,
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
    /// A schedule change would alter the fork rules of L2 blocks the node has already processed.
    ///
    /// Applying it would silently reinterpret canonical history, so it is refused: the runtime
    /// registry is consulted live for every fork check, including historical timestamps, so moving
    /// or clearing an activation that has already elapsed changes the fork membership of blocks the
    /// node has already built or validated.
    #[error(
        "upgrade signal for {upgrade_id} would retroactively change fork rules: activation moves from {current_activation} to {incoming_activation}, and the earliest affected timestamp {earliest_affected_timestamp} is not after the current time {now_secs}"
    )]
    RetroactiveScheduleChange {
        /// Upgrade ID whose activation change reaches already-processed blocks.
        upgrade_id: String,
        /// Activation the node currently applies.
        current_activation: String,
        /// Activation the L1 schedule now announces.
        incoming_activation: String,
        /// Earliest L2 timestamp whose fork membership the change would flip.
        earliest_affected_timestamp: u64,
        /// Time the change was evaluated against.
        now_secs: u64,
    },
    /// The node halted (fail closed) because a scheduled upgrade it is too old to support is
    /// activating imminently; continuing would fork the node off the network.
    #[error(
        "node halted (fail closed): upgrade {upgrade_id} activates at {activation_timestamp} and requires node protocol version {minimum_protocol_version} (this binary supports {node_protocol_version}); upgrade this node to a supported version"
    )]
    NodeUpgradeRequired {
        /// Upgrade ID whose activation forced the halt.
        upgrade_id: String,
        /// L2 activation timestamp of the unsupportable upgrade.
        activation_timestamp: u64,
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

    /// Creates a retroactive schedule change error.
    pub fn retroactive_schedule_change(
        upgrade_id: String,
        current_activation: impl ToString,
        incoming_activation: impl ToString,
        earliest_affected_timestamp: u64,
        now_secs: u64,
    ) -> Self {
        Self::RetroactiveScheduleChange {
            upgrade_id,
            current_activation: current_activation.to_string(),
            incoming_activation: incoming_activation.to_string(),
            earliest_affected_timestamp,
            now_secs,
        }
    }
}
