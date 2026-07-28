//! Shared helpers for basectl's non-TUI subcommands.

use url::Url;

use crate::{ConductorNodeConfig, ConductorSource, MonitoringConfig, NodeLookupError};

/// Whether a non-TUI subcommand observed failures, used by `main` to set the
/// process exit code.
///
/// Replaces a bare `bool` whose meaning was easy to invert: `Success` exits 0
/// while `HasFailures` exits non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The command completed without failures; the process should exit 0.
    Success,
    /// The command observed failures; the process should exit non-zero.
    HasFailures,
}

impl CommandOutcome {
    /// Builds an outcome from whether failures were observed.
    pub const fn from_failures(has_failures: bool) -> Self {
        if has_failures { Self::HasFailures } else { Self::Success }
    }

    /// Returns true when the process should exit with a non-zero status.
    pub const fn has_failures(self) -> bool {
        matches!(self, Self::HasFailures)
    }
}

/// Formats an optional boolean, using `unknown` when unavailable.
pub const fn fmt_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

/// Formats an optional `u64`, using `unknown` when unavailable.
pub fn fmt_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

/// Formats an optional `u32`, using `unknown` when unavailable.
pub fn fmt_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

/// Resolves the conductor source selected by configuration and an optional override.
pub fn resolve_conductor_source(
    config: &MonitoringConfig,
    conductor_rpc: Option<Url>,
) -> Result<ConductorSource, NodeLookupError> {
    config
        .conductor_source(conductor_rpc)
        .ok_or_else(|| NodeLookupError::MissingSource { config_name: config.name.clone() })
}

/// Finds a conductor node by name or returns a typed lookup error.
pub fn find_conductor_node<'a>(
    nodes: &'a [ConductorNodeConfig],
    name: &str,
) -> Result<&'a ConductorNodeConfig, NodeLookupError> {
    nodes.iter().find(|node| node.name == name).ok_or_else(|| NodeLookupError::MissingNode {
        requested_node: name.to_string(),
        available_nodes: nodes.iter().map(|node| node.name.clone()).collect(),
    })
}
