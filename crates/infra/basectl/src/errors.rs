//! Shared typed errors for basectl command validation and preflight checks.

use std::{fmt, time::Duration};

use alloy_primitives::B256;
use thiserror::Error;

/// Error returned when a CLI block reference cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum BlockRefParseError {
    /// The provided block reference was empty after trimming whitespace.
    #[error("invalid block reference: empty input")]
    Empty,
    /// A 32-byte hash-shaped block reference could not be parsed as a hash.
    #[error("invalid block reference: malformed hash")]
    MalformedHash {
        /// The original block reference supplied by the caller.
        raw: String,
    },
    /// The block reference was not a supported number, hash, or tag.
    #[error("invalid block reference: {message}")]
    InvalidTag {
        /// The original block reference supplied by the caller.
        raw: String,
        /// The parser error returned by the underlying tag parser.
        message: String,
    },
    /// The `pending` tag is rejected because typed block responses cannot deserialize it.
    #[error("the `pending` tag is not supported; use `latest`, `safe`, `finalized`, or `earliest`")]
    PendingUnsupported,
}

/// Error returned when a command requires a consensus RPC URL and none is configured.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "{command_name} needs a consensus-node RPC URL.\n\
     The '{config_name}' config does not set `consensus_node_rpc`.\n\
     Override with `--cl-rpc <url>` or set `consensus_node_rpc` in your YAML config."
)]
#[non_exhaustive]
pub struct MissingConsensusRpcError {
    /// The config name selected for the command.
    pub config_name: String,
    /// The command that needed a consensus RPC URL.
    pub command_name: String,
}

impl MissingConsensusRpcError {
    /// Builds an error for a command that could not resolve a consensus RPC URL.
    pub fn new(config_name: impl Into<String>, command_name: impl Into<String>) -> Self {
        Self { config_name: config_name.into(), command_name: command_name.into() }
    }
}

/// Error returned when shared conductor source or node lookup fails.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum NodeLookupError {
    /// The command could not resolve a conductor source from config or flags.
    #[error(
        "commands need conductor config or a bootstrap RPC URL for '{config_name}'. Set `conductors` or `discovery.bootstrap_rpc` in config, or pass `--conductor-rpc <url>`."
    )]
    MissingSource {
        /// The config name selected for the command.
        config_name: String,
    },
    /// The requested conductor node name was not found.
    #[error("node {requested_node} not found. Available nodes: {}", available_nodes.join(", "))]
    MissingNode {
        /// The node name requested by the caller.
        requested_node: String,
        /// The node names available to the command.
        available_nodes: Vec<String>,
    },
}

/// Error returned when a P2P command target is malformed or unsupported.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum P2pTargetError {
    /// The peer target was empty after trimming whitespace.
    #[error("peer target cannot be empty")]
    EmptyTarget,
    /// A multiaddr target did not include a `/p2p/<peer-id>` component.
    #[error("multiaddr target must include a `/p2p/<peer-id>` component")]
    MultiaddrMissingPeerId {
        /// The target supplied by the caller.
        target: String,
    },
    /// A peer target could not be parsed as an enode or ENR.
    #[error("parsing peer target `{target}` as enode or ENR: {message}")]
    InvalidBootnode {
        /// The target supplied by the caller.
        target: String,
        /// The parser error returned by the underlying bootnode parser.
        message: String,
    },
    /// An ENR target did not contain enough data to derive a libp2p multiaddr.
    #[error(
        "ENR target `{target}` does not include enough information to derive a libp2p multiaddr"
    )]
    EnrMissingMultiaddr {
        /// The target supplied by the caller.
        target: String,
    },
    /// `remove-peer` does not accept ENR targets.
    #[error(
        "remove-peer needs a bare libp2p peer ID for CL targets; ENR records are only accepted by add-peer"
    )]
    RemoveEnrTarget {
        /// The target supplied by the caller.
        target: String,
    },
    /// The peer target contained whitespace.
    #[error("peer target must not contain whitespace")]
    TargetContainsWhitespace {
        /// The target supplied by the caller.
        target: String,
    },
    /// A remove-peer EL target parsed to something other than an enode.
    #[error("remove-peer EL targets must be `enode://` records")]
    RemoveElTargetNotEnode {
        /// The target supplied by the caller.
        target: String,
    },
    /// A remove-peer CL target was URL-like or multiaddr-like instead of a bare peer ID.
    #[error("remove-peer needs a bare libp2p peer ID for CL targets, not a URL or multiaddr")]
    RemoveClTargetNotBarePeerId {
        /// The target supplied by the caller.
        target: String,
    },
    /// The CL peer ID was empty after trimming whitespace.
    #[error("CL peer ID cannot be empty")]
    EmptyClPeerId,
    /// A CL peer action was given an enode record.
    #[error("CL peer actions need a bare libp2p peer ID, not an enode record")]
    ClPeerIdIsEnode {
        /// The target supplied by the caller.
        target: String,
    },
    /// A CL peer action was given an ENR record.
    #[error(
        "CL peer actions need a bare libp2p peer ID; ENR records are only accepted by add-peer"
    )]
    ClPeerIdIsEnr {
        /// The target supplied by the caller.
        target: String,
    },
    /// The CL peer ID contained whitespace.
    #[error("CL peer ID must not contain whitespace")]
    ClPeerIdContainsWhitespace {
        /// The target supplied by the caller.
        target: String,
    },
    /// The CL peer ID was URL-like or multiaddr-like instead of a bare peer ID.
    #[error("CL peer actions need a bare libp2p peer ID, not a URL or multiaddr")]
    ClPeerIdNotBare {
        /// The target supplied by the caller.
        target: String,
    },
    /// The CL peer ID was too short to plausibly be a libp2p peer ID.
    #[error(
        "CL peer ID `{target}` looks too short to be a valid libp2p peer ID; expected a base58-encoded string (e.g. 16Uiu2HAm...)"
    )]
    ClPeerIdTooShort {
        /// The target supplied by the caller.
        target: String,
        /// The minimum accepted length for a libp2p peer ID.
        min_len: usize,
    },
}

/// Error returned by the `p2p` command group.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum P2pCommandError {
    /// Some peers failed during `unban-all`.
    #[error("failed to unban {failed} CL peer(s)")]
    UnbanAllPartialFailure {
        /// The number of failed peer unban attempts.
        failed: usize,
    },
    /// The pretty-printer received a peer action shape it does not support.
    #[error("unsupported p2p pretty output action {action}")]
    UnsupportedPrettyAction {
        /// The unsupported action name.
        action: String,
    },
}

/// Error returned by the `conductor` command group.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConductorCommandError {
    /// The command could not resolve a conductor source from config or flags.
    #[error(
        "conductor commands need conductor config or a bootstrap RPC URL for '{config_name}'. Set `conductors` or `discovery.bootstrap_rpc` in config, or pass `--conductor-rpc <url>`."
    )]
    MissingSource {
        /// The config name selected for the command.
        config_name: String,
    },
    /// The requested conductor node name was not found.
    #[error("conductor node {requested_node} not found. Available nodes: {}", available_nodes.join(", "))]
    MissingNode {
        /// The node name requested by the caller.
        requested_node: String,
        /// The node names available to the command.
        available_nodes: Vec<String>,
    },
}

impl From<NodeLookupError> for ConductorCommandError {
    fn from(error: NodeLookupError) -> Self {
        match error {
            NodeLookupError::MissingSource { config_name } => Self::MissingSource { config_name },
            NodeLookupError::MissingNode { requested_node, available_nodes } => {
                Self::MissingNode { requested_node, available_nodes }
            }
        }
    }
}

/// Error returned by sequencer command validation and preflight checks.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SequencerCommandError {
    /// The command could not resolve a conductor source from config or flags.
    MissingSource {
        /// The config name selected for the command.
        config_name: String,
    },
    /// The requested sequencer node name was not found.
    MissingNode {
        /// The node name requested by the caller.
        requested_node: String,
        /// The node names available to the command.
        available_nodes: Vec<String>,
    },
    /// The command could not infer an unsafe head hash from the target node.
    MissingUnsafeHead {
        /// The target node name.
        node: String,
    },
    /// The target sequencer is already active.
    AlreadyActive {
        /// The target node name.
        node: String,
    },
    /// The target sequencer is already stopped.
    AlreadyStopped {
        /// The target node name.
        node: String,
    },
    /// The command targeted a node that is known not to be the conductor leader.
    NotCurrentLeader {
        /// The node name requested by the caller.
        requested_node: String,
        /// The node currently observed as conductor leader.
        current_leader: String,
        /// The sequencer action being validated.
        action: String,
    },
    /// The command targeted a follower while no current leader name was available.
    NotLeader {
        /// The node name requested by the caller.
        requested_node: String,
        /// The sequencer action being validated.
        action: String,
    },
    /// The observed unsafe head is zero, so no safe prestate exists for start.
    UninitializedUnsafeHead,
    /// The requested unsafe head did not match the node's observed unsafe head.
    UnsafeHeadMismatch {
        /// The unsafe head observed from the node.
        observed_hash: B256,
        /// The unsafe head requested by the caller.
        requested_hash: B256,
    },
    /// The unsafe head input was empty after trimming whitespace.
    EmptyUnsafeHead,
    /// The unsafe head input could not be parsed as a 32-byte hash.
    InvalidUnsafeHead {
        /// The original unsafe head supplied by the caller.
        raw: String,
        /// The parser error returned by the underlying hash parser.
        message: String,
    },
    /// The unsafe head input was the zero hash.
    ZeroUnsafeHead {
        /// The parsed zero hash requested by the caller.
        requested_hash: B256,
    },
    /// The sequencer active state did not converge after the command RPC succeeded.
    StateConvergenceTimeout(Box<StateConvergenceTimeoutError>),
}

/// Error returned when the sequencer active state does not converge after a command RPC succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateConvergenceTimeoutError {
    /// The sequencer action being observed.
    pub action: &'static str,
    /// The target node name.
    pub node: String,
    /// The target node consensus-layer RPC URL.
    pub cl_rpc: String,
    /// The unsafe head returned or requested for the command, if known.
    pub unsafe_head: Option<B256>,
    /// The expected `sequencer_active` state.
    pub expected_active: bool,
    /// The observation timeout used for state convergence.
    pub timeout: Duration,
    /// The last observed `sequencer_active` state, if any poll succeeded.
    pub last_observed: Option<bool>,
    /// The last polling error, if any poll failed.
    pub last_error: Option<String>,
}

impl fmt::Display for SequencerCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSource { config_name } => write!(
                f,
                "sequencer commands need conductor config or a bootstrap RPC URL for \
                 '{config_name}'. Set `conductors` or `discovery.bootstrap_rpc` in config, or \
                 pass `--conductor-rpc <url>`."
            ),
            Self::MissingNode { requested_node, available_nodes } => write!(
                f,
                "sequencer node {requested_node} not found. Available nodes: {}",
                available_nodes.join(", ")
            ),
            Self::MissingUnsafeHead { node } => write!(
                f,
                "could not determine unsafe head for {node}; pass an explicit 32-byte hash or \
                 restore CL reachability"
            ),
            Self::AlreadyActive { node } => {
                write!(f, "sequencer already active on {node}; stop it before starting again")
            }
            Self::AlreadyStopped { node } => write!(f, "sequencer already stopped on {node}"),
            Self::NotCurrentLeader { requested_node, current_leader, action } => write!(
                f,
                "Node is not the conductor leader. Current leader: {current_leader}. `basectl \
                 sequencer {action}` must target the leader instead of {requested_node}."
            ),
            Self::NotLeader { requested_node, action } => write!(
                f,
                "Node is not the conductor leader. `basectl sequencer {action}` must target the \
                 current leader instead of {requested_node}."
            ),
            Self::UninitializedUnsafeHead => write!(
                f,
                "no prestate: engine unsafe head is uninitialized, cannot safely start sequencer"
            ),
            Self::UnsafeHeadMismatch { observed_hash, requested_hash } => write!(
                f,
                "block hash mismatch: engine unsafe head is {observed_hash}, caller requested \
                 {requested_hash}"
            ),
            Self::EmptyUnsafeHead => write!(f, "unsafe head hash cannot be empty"),
            Self::InvalidUnsafeHead { raw, message } => {
                write!(f, "parsing unsafe head hash `{raw}`: {message}")
            }
            Self::ZeroUnsafeHead { .. } => write!(f, "unsafe head hash must not be zero"),
            Self::StateConvergenceTimeout(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SequencerCommandError {}

impl fmt::Display for StateConvergenceTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unsafe_head_suffix =
            self.unsafe_head.map_or_else(String::new, |unsafe_head| format!(" at {unsafe_head}"));
        let last_status_suffix = match (self.last_observed, self.last_error.as_deref()) {
            (Some(observed), Some(error)) => {
                format!(" (last observed `sequencer_active={observed}`; last poll error: {error})")
            }
            (Some(observed), None) => {
                format!(" (last observed `sequencer_active={observed}`)")
            }
            (None, Some(error)) => format!(" (last poll error: {error})"),
            (None, None) => String::new(),
        };

        write!(
            f,
            "{} RPC succeeded on {} ({}){}, but `sequencer_active={}` was not observed within \
             {:?}{}",
            self.action,
            self.node,
            self.cl_rpc,
            unsafe_head_suffix,
            self.expected_active,
            self.timeout,
            last_status_suffix
        )
    }
}

impl std::error::Error for StateConvergenceTimeoutError {}

impl From<NodeLookupError> for SequencerCommandError {
    fn from(error: NodeLookupError) -> Self {
        match error {
            NodeLookupError::MissingSource { config_name } => Self::MissingSource { config_name },
            NodeLookupError::MissingNode { requested_node, available_nodes } => {
                Self::MissingNode { requested_node, available_nodes }
            }
        }
    }
}

/// Error returned by doctor argument validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DoctorArgsError {
    /// The head-lag warning threshold is greater than or equal to the failure threshold.
    #[error("`--head-lag-warn-blocks` must be less than `--head-lag-fail-blocks`")]
    HeadLagWarnMustBeLessThanFail {
        /// The configured warning threshold.
        warn_blocks: u64,
        /// The configured failure threshold.
        fail_blocks: u64,
    },
    /// The safe-head recency warning threshold is greater than or equal to the failure threshold.
    #[error("`--safe-recency-warn-blocks` must be less than `--safe-recency-fail-blocks`")]
    SafeRecencyWarnMustBeLessThanFail {
        /// The configured warning threshold.
        warn_blocks: u64,
        /// The configured failure threshold.
        fail_blocks: u64,
    },
}
