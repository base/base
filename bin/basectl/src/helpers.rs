//! Shared helpers for non-TUI conductor and sequencer commands.

use anyhow::{Result, anyhow};
use basectl_cli::{ConductorNodeConfig, ConductorSource, MonitoringConfig};
use url::Url;

pub(crate) fn resolve_source(
    config: &MonitoringConfig,
    conductor_rpc: Option<Url>,
    command: &str,
) -> Result<ConductorSource> {
    config.conductor_source(conductor_rpc).ok_or_else(|| {
        anyhow!(
            "{command} commands need conductor config or a bootstrap RPC URL for '{}'. Set `conductors` or `discovery.bootstrap_rpc` in config, or pass `--conductor-rpc <url>`.",
            config.name
        )
    })
}

pub(crate) fn find_node<'a>(
    nodes: &'a [ConductorNodeConfig],
    name: &str,
    command: &str,
) -> Result<&'a ConductorNodeConfig> {
    nodes.iter().find(|node| node.name == name).ok_or_else(|| {
        anyhow!(
            "{command} node {name} not found. Available nodes: {}",
            nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>().join(", ")
        )
    })
}

pub(crate) const fn fmt_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

pub(crate) fn fmt_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

pub(crate) fn fmt_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}
