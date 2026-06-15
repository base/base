//! Implementation of the `basectl p2p` command group.

use std::io::{self, Write};

use anyhow::{Context, Result, anyhow, bail};
use base_consensus_peers::BootNode;
use basectl_cli::{
    JsonOutput, KeyValueTable, MonitoringConfig, NodeEndpoint, PeerListReport, PeerStatsReport,
    PeerSummary, add_peer, ban_peer, connect_peer, disconnect_peer, fetch_connected_peers,
    fetch_info, fetch_raw_info, fetch_raw_peers, list_banned_peers, remove_peer, unban_peer,
};
use serde::Serialize;
use url::Url;

use crate::{
    cli::{
        DestructiveClBulkArgs, DestructiveClPeerArgs, DestructivePeerArgs, P2pArgs, P2pCommands,
    },
    confirm::confirm,
};

/// Runs the `basectl p2p` command group.
pub(crate) async fn run(config: MonitoringConfig, command: P2pCommands) -> Result<()> {
    match command {
        P2pCommands::Peers(args) => run_peers(config, args).await,
        P2pCommands::Info(args) => run_info(config, args).await,
        P2pCommands::AddPeer(args) => run_add_peer(config, args).await,
        P2pCommands::RemovePeer(args) => run_remove_peer(config, args).await,
        P2pCommands::Ban(args) => run_ban_peer(config, args).await,
        P2pCommands::Unban(args) => run_unban_peer(config, args).await,
        P2pCommands::UnbanAll(args) => run_unban_all(config, args).await,
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

async fn run_add_peer(config: MonitoringConfig, args: DestructivePeerArgs) -> Result<()> {
    let DestructivePeerArgs { target, el_rpc: el_rpc_override, cl_rpc: cl_rpc_override, yes, json } =
        args;
    let target = parse_add_target(&target)?;

    match target {
        AddTarget::Enode(enode) => {
            warn_ignored_rpc_override(
                cl_rpc_override.as_ref(),
                "--cl-rpc",
                "enode targets",
                PeerLayer::El,
            );
            let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
            let prompt = format!("Add EL peer {enode} through {el_rpc}? [y/N] ");
            if !confirm(&prompt, yes)? {
                println!("aborted");
                return Ok(());
            }
            let accepted = add_peer(&el_rpc, &enode).await?;
            print_peer_action(
                &PeerActionJson::el(&config.name, PeerAction::Add, enode, accepted),
                json,
            )?;
        }
        AddTarget::Multiaddr(multiaddr) => {
            warn_ignored_rpc_override(
                el_rpc_override.as_ref(),
                "--el-rpc",
                "CL targets",
                PeerLayer::Cl,
            );
            let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref(), "p2p add-peer")?;
            let prompt = format!("Connect CL peer {multiaddr} through {cl_rpc}? [y/N] ");
            if !confirm(&prompt, yes)? {
                println!("aborted");
                return Ok(());
            }
            connect_peer(&cl_rpc, &multiaddr).await?;
            print_peer_action(&PeerActionJson::cl(&config.name, PeerAction::Add, multiaddr), json)?;
        }
    }

    Ok(())
}

async fn run_remove_peer(config: MonitoringConfig, args: DestructivePeerArgs) -> Result<()> {
    let DestructivePeerArgs { target, el_rpc: el_rpc_override, cl_rpc: cl_rpc_override, yes, json } =
        args;
    let target = parse_remove_target(&target)?;

    match target {
        RemoveTarget::Enode(enode) => {
            warn_ignored_rpc_override(
                cl_rpc_override.as_ref(),
                "--cl-rpc",
                "enode targets",
                PeerLayer::El,
            );
            let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
            let prompt = format!("Remove EL peer {enode} through {el_rpc}? [y/N] ");
            if !confirm(&prompt, yes)? {
                println!("aborted");
                return Ok(());
            }
            let accepted = remove_peer(&el_rpc, &enode).await?;
            print_peer_action(
                &PeerActionJson::el(&config.name, PeerAction::Remove, enode, accepted),
                json,
            )?;
        }
        RemoveTarget::PeerId(peer_id) => {
            warn_ignored_rpc_override(
                el_rpc_override.as_ref(),
                "--el-rpc",
                "CL targets",
                PeerLayer::Cl,
            );
            let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref(), "p2p remove-peer")?;
            let prompt = format!("Disconnect CL peer {peer_id} from {cl_rpc}? [y/N] ");
            if !confirm(&prompt, yes)? {
                println!("aborted");
                return Ok(());
            }
            disconnect_peer(&cl_rpc, &peer_id).await?;
            print_peer_action(
                &PeerActionJson::cl(&config.name, PeerAction::Remove, peer_id),
                json,
            )?;
        }
    }

    Ok(())
}

async fn run_ban_peer(config: MonitoringConfig, args: DestructiveClPeerArgs) -> Result<()> {
    let DestructiveClPeerArgs { peer_id, cl_rpc: cl_rpc_override, yes, json } = args;
    let peer_id = parse_cl_peer_id(&peer_id)?;
    let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref(), "p2p ban")?;
    let prompt = format!("Ban CL peer {peer_id} through {cl_rpc}? [y/N] ");
    if !confirm(&prompt, yes)? {
        println!("aborted");
        return Ok(());
    }

    ban_peer(&cl_rpc, &peer_id).await?;
    let disconnect_error =
        disconnect_peer(&cl_rpc, &peer_id).await.err().map(|err| err.to_string());
    print_peer_action(
        &PeerActionJson::cl_with_disconnect_error(
            &config.name,
            PeerAction::Ban,
            peer_id,
            disconnect_error,
        ),
        json,
    )?;
    Ok(())
}

async fn run_unban_peer(config: MonitoringConfig, args: DestructiveClPeerArgs) -> Result<()> {
    let DestructiveClPeerArgs { peer_id, cl_rpc: cl_rpc_override, yes, json } = args;
    let peer_id = parse_cl_peer_id(&peer_id)?;
    let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref(), "p2p unban")?;
    let prompt = format!("Unban CL peer {peer_id} through {cl_rpc}? [y/N] ");
    if !confirm(&prompt, yes)? {
        println!("aborted");
        return Ok(());
    }

    unban_peer(&cl_rpc, &peer_id).await?;
    print_peer_action(&PeerActionJson::cl(&config.name, PeerAction::Unban, peer_id), json)?;
    Ok(())
}

async fn run_unban_all(config: MonitoringConfig, args: DestructiveClBulkArgs) -> Result<()> {
    let DestructiveClBulkArgs { cl_rpc: cl_rpc_override, yes, json } = args;
    let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref(), "p2p unban-all")?;
    let mut peer_ids = list_banned_peers(&cl_rpc).await?;
    peer_ids.sort();

    if peer_ids.is_empty() {
        if json {
            print_peer_action(
                &PeerActionJson::cl_bulk(&config.name, PeerAction::UnbanAll, vec![]),
                json,
            )?;
        } else {
            println!("no peers are currently banned");
        }
        return Ok(());
    }

    let prompt = format!("Unban all {} banned CL peers through {cl_rpc}? [y/N] ", peer_ids.len());
    if !confirm(&prompt, yes)? {
        println!("aborted");
        return Ok(());
    }

    let mut results = Vec::with_capacity(peer_ids.len());
    for peer_id in peer_ids {
        match unban_peer(&cl_rpc, &peer_id).await {
            Ok(()) => results.push(PeerBulkActionResultJson::ok(peer_id)),
            Err(err) => results.push(PeerBulkActionResultJson::err(peer_id, err.to_string())),
        }
    }
    let action = PeerActionJson::cl_bulk(&config.name, PeerAction::UnbanAll, results);
    let failed = action.failed_count();
    print_peer_action(&action, json)?;
    if failed > 0 {
        bail!("failed to unban {failed} CL peer(s)");
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

/// Minimum length used to catch obvious non-libp2p peer IDs before hitting the CL RPC.
const MIN_LIBP2P_PEER_ID_LEN: usize = 40;

fn warn_ignored_rpc_override(
    override_url: Option<&Url>,
    flag: &str,
    target_kind: &str,
    layer: PeerLayer,
) {
    if override_url.is_some() {
        eprintln!("warning: {flag} is ignored for {target_kind} (routed to {})", layer.as_str());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AddTarget {
    Enode(String),
    Multiaddr(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoveTarget {
    Enode(String),
    PeerId(String),
}

fn parse_add_target(raw: &str) -> Result<AddTarget> {
    let target = raw.trim();
    if target.is_empty() {
        bail!("peer target cannot be empty");
    }
    if target.starts_with('/') {
        if !target.contains("/p2p/") {
            bail!("multiaddr target must include a `/p2p/<peer-id>` component");
        }
        return Ok(AddTarget::Multiaddr(target.to_string()));
    }

    let bootnode = BootNode::parse_bootnode(target)
        .with_context(|| format!("parsing peer target `{target}` as enode or ENR"))?;
    match &bootnode {
        BootNode::Enode(_) => Ok(AddTarget::Enode(target.to_string())),
        BootNode::Enr(_) => {
            let multiaddr = bootnode.to_multiaddr().ok_or_else(|| {
                anyhow!(
                    "ENR target `{target}` does not include enough information to derive a libp2p multiaddr"
                )
            })?;
            Ok(AddTarget::Multiaddr(multiaddr.to_string()))
        }
    }
}

fn parse_remove_target(raw: &str) -> Result<RemoveTarget> {
    let target = raw.trim();
    if target.is_empty() {
        bail!("peer target cannot be empty");
    }
    if target.starts_with("enr:") {
        bail!(
            "remove-peer needs a bare libp2p peer ID for CL targets; ENR records are only accepted by add-peer"
        );
    }
    if target.split_whitespace().count() != 1 {
        bail!("peer target must not contain whitespace");
    }

    if target.starts_with("enode://") {
        let bootnode = BootNode::parse_bootnode(target)
            .with_context(|| format!("parsing remove-peer target `{target}` as an enode"))?;
        if !matches!(bootnode, BootNode::Enode(_)) {
            bail!("remove-peer EL targets must be `enode://` records");
        }
        return Ok(RemoveTarget::Enode(target.to_string()));
    }
    if target.contains(':') || target.contains('/') {
        bail!("remove-peer needs a bare libp2p peer ID for CL targets, not a URL or multiaddr");
    }

    Ok(RemoveTarget::PeerId(parse_cl_peer_id(target)?))
}

fn parse_cl_peer_id(raw: &str) -> Result<String> {
    let target = raw.trim();
    if target.is_empty() {
        bail!("CL peer ID cannot be empty");
    }
    if target.starts_with("enode://") {
        bail!("CL peer actions need a bare libp2p peer ID, not an enode record");
    }
    if target.starts_with("enr:") {
        bail!(
            "CL peer actions need a bare libp2p peer ID; ENR records are only accepted by add-peer"
        );
    }
    if target.split_whitespace().count() != 1 {
        bail!("CL peer ID must not contain whitespace");
    }
    if target.contains(':') || target.contains('/') {
        bail!("CL peer actions need a bare libp2p peer ID, not a URL or multiaddr");
    }
    if target.len() < MIN_LIBP2P_PEER_ID_LEN {
        bail!(
            "CL peer ID `{target}` looks too short to be a valid libp2p peer ID; expected a base58-encoded string (e.g. 16Uiu2HAm...)"
        );
    }

    Ok(target.to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged, rename_all = "camelCase")]
enum PeerActionJson {
    El {
        network: String,
        action: PeerAction,
        layer: PeerLayer,
        target: String,
        accepted: bool,
    },
    Cl {
        network: String,
        action: PeerAction,
        layer: PeerLayer,
        target: String,
        #[serde(rename = "disconnectError")]
        #[serde(skip_serializing_if = "Option::is_none")]
        disconnect_error: Option<String>,
    },
    ClBulk {
        network: String,
        action: PeerAction,
        layer: PeerLayer,
        attempted: usize,
        succeeded: usize,
        failed: usize,
        results: Vec<PeerBulkActionResultJson>,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
enum PeerAction {
    #[serde(rename = "addPeer")]
    Add,
    #[serde(rename = "removePeer")]
    Remove,
    #[serde(rename = "banPeer")]
    Ban,
    #[serde(rename = "unbanPeer")]
    Unban,
    #[serde(rename = "unbanAll")]
    UnbanAll,
}

#[derive(Debug, Clone, Copy, Serialize)]
enum PeerLayer {
    #[serde(rename = "el")]
    El,
    #[serde(rename = "cl")]
    Cl,
}

impl PeerLayer {
    const fn as_str(self) -> &'static str {
        match self {
            Self::El => "EL",
            Self::Cl => "CL",
        }
    }
}

impl PeerActionJson {
    fn el(network: &str, action: PeerAction, target: String, accepted: bool) -> Self {
        Self::El { network: network.to_string(), action, layer: PeerLayer::El, target, accepted }
    }

    fn cl(network: &str, action: PeerAction, target: String) -> Self {
        Self::cl_with_disconnect_error(network, action, target, None)
    }

    fn cl_with_disconnect_error(
        network: &str,
        action: PeerAction,
        target: String,
        disconnect_error: Option<String>,
    ) -> Self {
        Self::Cl {
            network: network.to_string(),
            action,
            layer: PeerLayer::Cl,
            target,
            disconnect_error,
        }
    }

    fn cl_bulk(network: &str, action: PeerAction, results: Vec<PeerBulkActionResultJson>) -> Self {
        let attempted = results.len();
        let succeeded = results.iter().filter(|result| result.ok).count();
        let failed = attempted.saturating_sub(succeeded);
        Self::ClBulk {
            network: network.to_string(),
            action,
            layer: PeerLayer::Cl,
            attempted,
            succeeded,
            failed,
            results,
        }
    }

    const fn failed_count(&self) -> usize {
        match self {
            Self::ClBulk { failed, .. } => *failed,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PeerBulkActionResultJson {
    target: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl PeerBulkActionResultJson {
    const fn ok(target: String) -> Self {
        Self { target, ok: true, error: None }
    }

    const fn err(target: String, error: String) -> Self {
        Self { target, ok: false, error: Some(error) }
    }
}

fn print_peer_action(action: &PeerActionJson, json: bool) -> Result<()> {
    if json {
        JsonOutput::print(action)?;
    } else {
        print_peer_action_pretty(action)?;
    }
    Ok(())
}

fn print_peer_action_pretty(action: &PeerActionJson) -> Result<()> {
    let mut stdout = io::stdout().lock();
    match action {
        PeerActionJson::El { action: PeerAction::Add, target, accepted, .. } => {
            if *accepted {
                writeln!(stdout, "OK EL accepted peer {target}")?;
            } else {
                writeln!(stdout, "OK EL did not accept peer {target}")?;
            }
        }
        PeerActionJson::El { action: PeerAction::Remove, target, accepted, .. } => {
            if *accepted {
                writeln!(stdout, "OK EL removed peer {target}")?;
            } else {
                writeln!(stdout, "OK EL did not remove peer {target}")?;
            }
        }
        PeerActionJson::Cl { action: PeerAction::Add, target, .. } => {
            writeln!(stdout, "OK CL connected {target}")?;
        }
        PeerActionJson::Cl { action: PeerAction::Remove, target, .. } => {
            writeln!(stdout, "OK CL disconnected {target}")?;
        }
        PeerActionJson::Cl { action: PeerAction::Ban, target, disconnect_error, .. } => {
            if let Some(error) = disconnect_error {
                writeln!(stdout, "OK CL banned {target} (disconnect warning: {error})")?;
            } else {
                writeln!(stdout, "OK CL banned {target}")?;
            }
        }
        PeerActionJson::Cl { action: PeerAction::Unban, target, .. } => {
            writeln!(stdout, "OK CL unbanned {target}")?;
        }
        PeerActionJson::ClBulk { succeeded, failed, results, .. } => {
            writeln!(stdout, "OK CL unbanned {succeeded} banned peer(s)")?;
            if *failed > 0 {
                writeln!(stdout, "failed to unban {failed} banned peer(s)")?;
                for result in results.iter().filter(|result| !result.ok) {
                    let error = result.error.as_deref().unwrap_or("unknown error");
                    writeln!(stdout, "  {}: {error}", result.target)?;
                }
            }
        }
        PeerActionJson::El { action, .. } | PeerActionJson::Cl { action, .. } => {
            bail!("unsupported peer action for pretty output: {action:?}");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LayerInfoJson {
    advertised_ip: Option<String>,
    tcp_port: Option<u16>,
    discovery: Option<basectl_cli::DiscoveryInfo>,
    peer_count: Option<u32>,
}

impl LayerInfoJson {
    fn from_endpoint(endpoint: Option<NodeEndpoint>, peer_count: Option<u32>) -> Self {
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
            cl: LayerInfoJson::from_endpoint(report.cl, peer_stats.cl.map(|stats| stats.connected)),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PeersJson {
    network: String,
    el: Option<Vec<PeerSummary>>,
    cl: Option<Vec<PeerSummary>>,
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
        .row(
            "el_peer_count",
            peer_stats
                .el_count
                .map(|count| count.to_string())
                .unwrap_or_else(unavailable_el_peer_count),
        )
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
        .row(
            "cl_peer_count",
            peer_stats
                .cl
                .map(|stats| stats.connected.to_string())
                .unwrap_or_else(unavailable_cl_peer_stats),
        );
    table.print()?;
    Ok(())
}

fn print_peers_pretty(network: &str, report: &PeerListReport) -> Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "network  {network}")?;
    writeln!(stdout)?;
    write_peer_section(&mut stdout, "execution", report.el.as_deref())?;
    writeln!(stdout)?;
    write_peer_section(&mut stdout, "consensus", report.cl.as_deref())?;
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

fn unavailable_el_peer_count() -> String {
    "unavailable (`net_peerCount` not exposed by this RPC)".to_string()
}

fn unavailable_cl_endpoint() -> String {
    "unavailable (could not parse advertised endpoint from `opp2p_self`)".to_string()
}

fn unavailable_cl_peer_stats() -> String {
    "unavailable (`opp2p_peerStats` not exposed by this RPC)".to_string()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use serde_json::json;
    use url::Url;

    use super::{
        AddTarget, PeerAction, PeerActionJson, PeerBulkActionResultJson, RemoveTarget,
        parse_add_target, parse_cl_peer_id, parse_remove_target, resolve_cl_rpc,
    };

    const VALID_ENODE: &str = "enode://d7dfaea49c7ef37701e668652bcf1bc63d3abb2ae97593374a949e175e4ff128730a2f35199f3462a56298b981dfc395a5abebd2d6f0284ffe5bdc3d8e258b86@127.0.0.1:30304?discport=30301";
    const VALID_ENR: &str = "enr:-J64QBbwPjPLZ6IOOToOLsSjtFUjjzN66qmBZdUexpO32Klrc458Q24kbty2PdRaLacHM5z-cZQr8mjeQu3pik6jPSOGAYYFIqBfgmlkgnY0gmlwhDaRWFWHb3BzdGFja4SzlAUAiXNlY3AyNTZrMaECmeSnJh7zjKrDSPoNMGXoopeDF4hhpj5I0OsQUUt4u8uDdGNwgiQGg3VkcIIkBg";

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

    #[test]
    fn parse_add_target_routes_enode_to_el() {
        assert_eq!(
            parse_add_target(VALID_ENODE).unwrap(),
            AddTarget::Enode(VALID_ENODE.to_string())
        );
    }

    #[test]
    fn parse_add_target_routes_enr_to_cl_multiaddr() {
        let AddTarget::Multiaddr(multiaddr) = parse_add_target(VALID_ENR).unwrap() else {
            panic!("expected ENR to route to CL multiaddr");
        };

        assert!(multiaddr.starts_with("/ip4/"));
        assert!(multiaddr.contains("/p2p/"));
    }

    #[test]
    fn parse_add_target_rejects_garbage() {
        assert!(parse_add_target("not-a-peer").is_err());
    }

    #[test]
    fn parse_add_target_routes_multiaddr_to_cl() {
        let multiaddr = "/ip4/127.0.0.1/tcp/9000/p2p/16Uiu2HAmExample";

        assert_eq!(
            parse_add_target(multiaddr).unwrap(),
            AddTarget::Multiaddr(multiaddr.to_string())
        );
    }

    #[test]
    fn parse_add_target_rejects_multiaddr_without_peer_id() {
        let err = parse_add_target("/ip4/127.0.0.1/tcp/9000")
            .expect_err("multiaddr without peer ID should be rejected");

        assert!(
            err.to_string().contains("multiaddr target must include a `/p2p/<peer-id>` component")
        );
    }

    #[test]
    fn parse_remove_target_routes_enode_to_el() {
        assert_eq!(
            parse_remove_target(VALID_ENODE).unwrap(),
            RemoveTarget::Enode(VALID_ENODE.to_string())
        );
    }

    #[test]
    fn parse_remove_target_routes_peer_id_to_cl() {
        let peer_id = "16Uiu2HAkxp9nAsXsCthNWPkkpm4yG1eW7L4ENpVyzDZM8HE1yr12";

        assert_eq!(
            parse_remove_target(peer_id).unwrap(),
            RemoveTarget::PeerId(peer_id.to_string())
        );
    }

    #[test]
    fn parse_remove_target_rejects_enr() {
        assert!(parse_remove_target(VALID_ENR).is_err());
    }

    #[test]
    fn parse_remove_target_rejects_multiaddr() {
        assert!(parse_remove_target("/ip4/127.0.0.1/tcp/9000/p2p/16Uiu2HAmExample").is_err());
    }

    #[test]
    fn parse_remove_target_rejects_url_like_target() {
        assert!(parse_remove_target("https://example.com").is_err());
    }

    #[test]
    fn parse_remove_target_rejects_obviously_short_peer_id() {
        let err = parse_remove_target("hello").expect_err("short peer ID should be rejected");

        assert!(err.to_string().contains("looks too short to be a valid libp2p peer ID"));
    }

    #[test]
    fn parse_cl_peer_id_accepts_peer_id() {
        let peer_id = "16Uiu2HAkxp9nAsXsCthNWPkkpm4yG1eW7L4ENpVyzDZM8HE1yr12";

        assert_eq!(parse_cl_peer_id(peer_id).unwrap(), peer_id);
    }

    #[test]
    fn parse_cl_peer_id_rejects_non_peer_ids() {
        for target in [
            "",
            "hello",
            VALID_ENODE,
            VALID_ENR,
            "/ip4/127.0.0.1/tcp/9000/p2p/16Uiu2HAmExample",
            "https://example.com",
            "16Uiu2HAkxp9nAsXsCthNWPkkpm4yG1eW7L4ENpVyzDZM8HE1yr12 extra",
        ] {
            assert!(parse_cl_peer_id(target).is_err(), "target should be rejected: {target}");
        }
    }

    #[test]
    fn peer_action_json_serializes_typed_action_and_layer() {
        let el = serde_json::to_value(PeerActionJson::el(
            "devnet",
            PeerAction::Add,
            "enode://example".to_string(),
            false,
        ))
        .unwrap();

        assert_eq!(
            el,
            json!({
                "network": "devnet",
                "action": "addPeer",
                "layer": "el",
                "target": "enode://example",
                "accepted": false,
            })
        );

        let cl = serde_json::to_value(PeerActionJson::cl(
            "devnet",
            PeerAction::Remove,
            "16Uiu2HAmExamplePeerId".to_string(),
        ))
        .unwrap();

        assert_eq!(
            cl,
            json!({
                "network": "devnet",
                "action": "removePeer",
                "layer": "cl",
                "target": "16Uiu2HAmExamplePeerId",
            })
        );

        let ban = serde_json::to_value(PeerActionJson::cl_with_disconnect_error(
            "devnet",
            PeerAction::Ban,
            "16Uiu2HAmExamplePeerId".to_string(),
            Some("already disconnected".to_string()),
        ))
        .unwrap();

        assert_eq!(
            ban,
            json!({
                "network": "devnet",
                "action": "banPeer",
                "layer": "cl",
                "target": "16Uiu2HAmExamplePeerId",
                "disconnectError": "already disconnected",
            })
        );

        let unban = serde_json::to_value(PeerActionJson::cl(
            "devnet",
            PeerAction::Unban,
            "16Uiu2HAmExamplePeerId".to_string(),
        ))
        .unwrap();

        assert_eq!(
            unban,
            json!({
                "network": "devnet",
                "action": "unbanPeer",
                "layer": "cl",
                "target": "16Uiu2HAmExamplePeerId",
            })
        );

        let unban_all = serde_json::to_value(PeerActionJson::cl_bulk(
            "devnet",
            PeerAction::UnbanAll,
            vec![
                PeerBulkActionResultJson::ok("16Uiu2HAmExamplePeerId".to_string()),
                PeerBulkActionResultJson::err(
                    "12D3KooExamplePeerId".to_string(),
                    "unavailable".to_string(),
                ),
            ],
        ))
        .unwrap();

        assert_eq!(
            unban_all,
            json!({
                "network": "devnet",
                "action": "unbanAll",
                "layer": "cl",
                "attempted": 2,
                "succeeded": 1,
                "failed": 1,
                "results": [
                    { "target": "16Uiu2HAmExamplePeerId", "ok": true },
                    { "target": "12D3KooExamplePeerId", "ok": false, "error": "unavailable" }
                ],
            })
        );
    }
}
