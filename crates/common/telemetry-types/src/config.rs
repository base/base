//! The allowlisted, normalized view of node configuration reported as `config.*`.

use serde::{Deserialize, Serialize};

/// How much history the node retains.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneMode {
    /// Full history retained.
    #[default]
    Archive,
    /// History pruned to the default recent window.
    Full,
    /// History pruned according to a node-specific prune configuration.
    Custom,
}

impl PruneMode {
    /// Returns the stable wire label for this prune mode.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Full => "full",
            Self::Custom => "custom",
        }
    }
}

/// The `config.*` block, present on every report event.
///
/// This is an allowlist by construction. The raw command line is never reported, because it
/// carries L1 RPC URLs with credentials, JWT paths, and signer endpoints. Adding a field here
/// means deciding that this specific normalized value is safe to send, and `experimental_flags`
/// carries flag *names* only, never their values.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeConfigReport {
    /// How much history the node retains, absent on a node that has no prune setting at all.
    ///
    /// A consensus-only node omits this rather than reporting a placeholder, so the field
    /// distinguishes "not applicable" from a real retention choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune_mode: Option<PruneMode>,
    /// Whether the node participates in p2p gossip.
    pub p2p_enabled: bool,
    /// Whether discv5 discovery is enabled.
    pub discovery_enabled: bool,
    /// Whether the node is configured to sequence blocks.
    pub sequencer_enabled: bool,
    /// Whether the node runs the interop supervisor integration.
    pub supervisor_enabled: bool,
    /// Whether the node consumes flashblocks.
    pub flashblocks_enabled: bool,
    /// Whether the operator enabled the Prometheus metrics endpoint.
    pub metrics_enabled: bool,
    /// Names of enabled experimental flags, sorted. Names only, never values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub experimental_flags: Vec<String>,
    /// How often the node sends a report, in seconds.
    pub report_interval_secs: u64,
    /// How often the node samples head lag between reports, in seconds.
    pub sample_interval_secs: u64,
}
