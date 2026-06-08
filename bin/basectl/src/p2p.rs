//! Implementation of the `basectl p2p` command group.

use std::io::{self, Write};

use anyhow::{Result, anyhow};
use basectl_cli::{
    JsonOutput, KeyValueTable, MonitoringConfig, NodeEndpoint, PeerListReport, PeerStatsReport,
    PeerSummary, fetch_connected_peers, fetch_info, fetch_raw_info, fetch_raw_peers,
};
use serde::Serialize;
use url::Url;

use crate::cli::{P2pArgs, P2pCommands};

/// Runs the `basectl p2p` command group.
pub(crate) async fn run(config: MonitoringConfig, command: P2pCommands) -> Result<()> {
    match command {
        P2pCommands::Peers(args) => run_peers(config, args).await,
        P2pCommands::Info(args) => run_info(config, args).await,
    }
}

async fn run_peers(config: MonitoringConfig, args: P2pArgs) -> Result<()> {
    let P2pArgs { el_rpc: el_rpc_override, cl_rpc: cl_rpc_override, json, raw } = args;
    let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
    let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref(), "p2p peers")?;

    match (json, raw) {
        (true, true) => JsonOutput::print(&fetch_raw_peers(&el_rpc, &cl_rpc).await?)?,
        (true, false) => {
            let report = fetch_connected_peers(&el_rpc, &cl_rpc).await?;
            JsonOutput::print(&PeersJson::from_report(&config.name, &report))?;
        }
        (false, _) => {
            let report = fetch_connected_peers(&el_rpc, &cl_rpc).await?;
            print_peers_pretty(&config.name, &report)?;
        }
    }

    Ok(())
}

async fn run_info(config: MonitoringConfig, args: P2pArgs) -> Result<()> {
    let P2pArgs { el_rpc: el_rpc_override, cl_rpc: cl_rpc_override, json, raw } = args;
    let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
    let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref(), "p2p info")?;

    match (json, raw) {
        (true, true) => JsonOutput::print(&fetch_raw_info(&el_rpc, &cl_rpc).await?)?,
        (true, false) => {
            let (node_info, peer_stats) = fetch_info(&el_rpc, &cl_rpc).await?;
            JsonOutput::print(&InfoJson::from_report(&config.name, &node_info, &peer_stats))?;
        }
        (false, _) => {
            let (node_info, peer_stats) = fetch_info(&el_rpc, &cl_rpc).await?;
            print_info_pretty(&config.name, &node_info, &peer_stats)?;
        }
    }

    Ok(())
}

fn resolve_cl_rpc(
    config: &MonitoringConfig,
    override_url: Option<&Url>,
    command_name: &str,
) -> Result<Url> {
    if let Some(u) = override_url {
        return Ok(u.clone());
    }
    config.consensus_node_rpc.clone().ok_or_else(|| {
        anyhow!(
            "{command_name} needs a consensus-node RPC URL.\n\
             The '{}' config does not set `consensus_node_rpc`.\n\
             Override with `--cl-rpc <url>` or set `consensus_node_rpc` in your YAML config.",
            config.name
        )
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LayerInfoJson {
    advertised_ip: Option<String>,
    tcp_port: Option<u16>,
    discovery: Option<basectl_cli::DiscoveryInfo>,
    peer_count: u32,
}

impl LayerInfoJson {
    fn from_endpoint(endpoint: Option<NodeEndpoint>, peer_count: u32) -> Self {
        let (advertised_ip, tcp_port, discovery) = endpoint.map_or((None, None, None), |ep| {
            (Some(ep.advertised_ip.to_string()), Some(ep.tcp_port), Some(ep.discovery))
        });
        Self { advertised_ip, tcp_port, discovery, peer_count }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoJson {
    network: String,
    el: LayerInfoJson,
    cl: LayerInfoJson,
}

impl InfoJson {
    fn from_report(
        network: &str,
        report: &basectl_cli::NodeInfoReport,
        peer_stats: &PeerStatsReport,
    ) -> Self {
        Self {
            network: network.to_string(),
            el: LayerInfoJson::from_endpoint(report.el, peer_stats.el_count),
            cl: LayerInfoJson::from_endpoint(report.cl, peer_stats.cl.connected),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PeersJson {
    network: String,
    el: Option<Vec<PeerSummary>>,
    cl: Vec<PeerSummary>,
}

impl PeersJson {
    fn from_report(network: &str, report: &PeerListReport) -> Self {
        Self { network: network.to_string(), el: report.el.clone(), cl: report.cl.clone() }
    }
}

fn print_info_pretty(
    network: &str,
    report: &basectl_cli::NodeInfoReport,
    peer_stats: &PeerStatsReport,
) -> Result<()> {
    let mut table = KeyValueTable::new();
    let el_discovery = report.el.map(|endpoint| {
        format_discovery_flags(endpoint.discovery.v4_enabled, endpoint.discovery.v5_enabled)
            .to_string()
    });
    let cl_discovery = report.cl.map(|endpoint| {
        format_discovery_flags(endpoint.discovery.v4_enabled, endpoint.discovery.v5_enabled)
            .to_string()
    });
    table
        .row("network", network)
        .row(
            "el_advertised_ip",
            report
                .el
                .map(|endpoint| endpoint.advertised_ip.to_string())
                .unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
        )
        .row(
            "el_p2p_port",
            report
                .el
                .map(|endpoint| endpoint.tcp_port.to_string())
                .unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
        )
        .row(
            "el_discovery_port",
            report
                .el
                .map(|endpoint| endpoint.discovery.udp_port.to_string())
                .unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
        )
        .row(
            "el_discovery",
            el_discovery.unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
        )
        .row("el_peer_count", peer_stats.el_count.to_string())
        .row(
            "cl_advertised_ip",
            report
                .cl
                .map(|endpoint| endpoint.advertised_ip.to_string())
                .unwrap_or_else(unavailable_cl_endpoint),
        )
        .row(
            "cl_p2p_port",
            report
                .cl
                .map(|endpoint| endpoint.tcp_port.to_string())
                .unwrap_or_else(unavailable_cl_endpoint),
        )
        .row(
            "cl_discovery_port",
            report
                .cl
                .map(|endpoint| endpoint.discovery.udp_port.to_string())
                .unwrap_or_else(unavailable_cl_endpoint),
        )
        .row("cl_discovery", cl_discovery.unwrap_or_else(unavailable_cl_endpoint))
        .row("cl_peer_count", peer_stats.cl.connected.to_string());
    table.print()?;
    Ok(())
}

fn print_peers_pretty(network: &str, report: &PeerListReport) -> Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "network  {network}")?;
    writeln!(stdout)?;
    write_peer_section(&mut stdout, "execution", report.el.as_deref())?;
    writeln!(stdout)?;
    write_peer_section(&mut stdout, "consensus", Some(&report.cl))?;
    Ok(())
}

fn write_peer_section<W: Write>(
    writer: &mut W,
    label: &str,
    peers: Option<&[PeerSummary]>,
) -> io::Result<()> {
    let Some(peers) = peers else {
        writeln!(writer, "{label} peers unavailable (RPC does not expose admin peer listing)")?;
        return Ok(());
    };
    writeln!(writer, "{label} peers ({})", peers.len())?;
    if peers.is_empty() {
        writeln!(writer, "  none")?;
        return Ok(());
    }

    let id_width = peers.iter().map(|peer| peer.id.len()).max().unwrap_or(2).max(2);
    let addr_width = peers.iter().map(|peer| peer.address.len()).max().unwrap_or(4).max(4);
    writeln!(writer, "  {0:<id_width$}  {1:<addr_width$}  direction", "id", "addr")?;
    for peer in peers {
        writeln!(
            writer,
            "  {0:<id_width$}  {1:<addr_width$}  {2}",
            peer.id, peer.address, peer.direction,
        )?;
    }
    Ok(())
}

const fn format_discovery_flags(v4_enabled: bool, v5_enabled: bool) -> &'static str {
    match (v4_enabled, v5_enabled) {
        (true, true) => "v4+v5",
        (true, false) => "v4",
        (false, true) => "v5",
        (false, false) => "disabled",
    }
}

fn unavailable_admin_method(method: &str) -> String {
    format!("unavailable (`{method}` not exposed by this RPC)")
}

fn unavailable_cl_endpoint() -> String {
    "unavailable (could not parse advertised endpoint from `opp2p_self`)".to_string()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use url::Url;

    use super::resolve_cl_rpc;

    fn test_config(consensus_node_rpc: Option<Url>) -> basectl_cli::MonitoringConfig {
        basectl_cli::MonitoringConfig {
            name: "devnet".to_string(),
            rpc: Url::parse("http://127.0.0.1:8545").unwrap(),
            flashblocks_ws: Url::parse("ws://127.0.0.1:7111").unwrap(),
            l1_rpc: Url::parse("http://127.0.0.1:9545").unwrap(),
            consensus_node_rpc,
            hardforks: None,
            system_config: Address::ZERO,
            batcher_address: None,
            l1_blob_target: 14,
            conductors: None,
            discovery: None,
            validators: None,
            proofs: None,
            pods: None,
        }
    }

    #[test]
    fn resolve_cl_rpc_prefers_flag() {
        let config = test_config(None);
        let override_url = Url::parse("http://127.0.0.1:9545").unwrap();

        let resolved = resolve_cl_rpc(&config, Some(&override_url), "p2p info").unwrap();

        assert_eq!(resolved, override_url);
    }

    #[test]
    fn resolve_cl_rpc_falls_back_to_config() {
        let cl_url = Url::parse("http://127.0.0.1:7545").unwrap();
        let config = test_config(Some(cl_url.clone()));

        let resolved = resolve_cl_rpc(&config, None, "p2p info").unwrap();

        assert_eq!(resolved, cl_url);
    }

    #[test]
    fn resolve_cl_rpc_errors_without_config() {
        let config = test_config(None);

        assert!(resolve_cl_rpc(&config, None, "p2p info").is_err());
    }
}
