//! Implementation of the `basectl p2p` command group.

use std::io::{self, Write};

use anyhow::{Context, Result, anyhow};
use base_consensus_peers::BootNode;
use serde::Serialize;
use url::Url;

use crate::{
    CommandOutcome, Confirm, JsonOutput, KeyValueTable, MonitoringConfig, P2pCommandError,
    P2pInfoJson, P2pInfoTable, P2pTargetError, PeerListReport, PeerSummary, ReachabilityOutcome,
    ReachabilityResponse, TelemetryClient, add_peer, ban_el_peer, ban_peer, connect_peer,
    disconnect_peer, el_peer_is_trusted, fetch_connected_peers, fetch_info, fetch_l2_chain_id,
    fetch_raw_info, fetch_raw_peers, list_banned_peers, remove_peer, unban_el_peer, unban_peer,
};

/// Inspect p2p peers and advertised endpoints.
#[derive(Debug, clap::Args)]
pub struct P2pCommand {
    /// P2P operation to run.
    #[command(subcommand)]
    pub command: P2pCommands,
}

/// P2P inspection and peer-management commands.
#[derive(Debug, clap::Subcommand)]
pub enum P2pCommands {
    /// List connected peers per layer.
    Peers(P2pArgs),
    /// Show advertised endpoints and peer-count summary per layer.
    Info(P2pArgs),
    /// Ask the Base telemetry service to probe an execution or consensus peer endpoint.
    Reachability {
        /// Peer endpoint to probe: an execution-layer `enode://` URL, a
        /// consensus-layer `enr:` record, or a public `IPv4`
        /// `/ip4/.../tcp/.../p2p/<peer-id>` multiaddr.
        #[arg(value_name = "TARGET")]
        target: String,
        /// Emit the telemetry response as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add a single execution or consensus peer.
    AddPeer(DestructivePeerArgs),
    /// Remove a single execution or consensus peer.
    RemovePeer(DestructivePeerArgs),
    /// Ban a single execution or consensus peer.
    Ban(DestructivePeerArgs),
    /// Unban a single execution or consensus peer.
    Unban(DestructivePeerArgs),
    /// Unban all currently banned consensus peers.
    UnbanAll(DestructiveClBulkArgs),
}

/// Shared flags for the read-only `basectl p2p` subcommands.
#[derive(Debug, clap::Args)]
pub struct P2pArgs {
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's local `rpc` field.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub el_rpc: Option<Url>,
    /// Override the consensus-node RPC URL.
    ///
    /// Defaults to the chain config's `consensus_node_rpc` field.
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub cl_rpc: Option<Url>,
    /// Emit JSON instead of the pretty table output.
    #[arg(long)]
    pub json: bool,
    /// With `--json`, emit raw RPC wire shapes instead of the humanized summary.
    #[arg(long, requires = "json")]
    pub raw: bool,
}

/// Shared flags for destructive `basectl p2p` subcommands.
#[derive(Debug, clap::Args)]
pub struct DestructivePeerArgs {
    /// Peer target. `enode://...` routes to EL; CL add accepts ENR or multiaddr, while other actions use a peer ID.
    #[arg(value_name = "TARGET")]
    pub target: String,
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's local `rpc` field.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub el_rpc: Option<Url>,
    /// Override the consensus-node RPC URL.
    ///
    /// Defaults to the chain config's `consensus_node_rpc` field.
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub cl_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

/// Shared flags for destructive consensus-only `basectl p2p` bulk subcommands.
#[derive(Debug, clap::Args)]
pub struct DestructiveClBulkArgs {
    /// Override the consensus-node RPC URL.
    ///
    /// Defaults to the chain config's `consensus_node_rpc` field.
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub cl_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

impl P2pCommand {
    /// Runs the selected `basectl p2p` operation.
    pub async fn run(self, config: MonitoringConfig) -> Result<CommandOutcome> {
        let success = CommandOutcome::Success;
        match self.command {
            P2pCommands::Reachability { target, json } => {
                run_reachability(&config, &target, json).await
            }
            P2pCommands::UnbanAll(args) => run_unban_all(config, args).await,
            P2pCommands::Peers(args) => run_peers(config, args).await.map(|()| success),
            P2pCommands::Info(args) => run_info(config, args).await.map(|()| success),
            P2pCommands::AddPeer(args) => run_add_peer(config, args).await.map(|()| success),
            P2pCommands::RemovePeer(args) => run_remove_peer(config, args).await.map(|()| success),
            P2pCommands::Ban(args) => {
                run_peer_ban_action(config, args, BanAction::Ban).await.map(|()| success)
            }
            P2pCommands::Unban(args) => {
                run_peer_ban_action(config, args, BanAction::Unban).await.map(|()| success)
            }
        }
    }
}

/// Runs `basectl p2p reachability`, exiting non-zero when the probe completed
/// but the node was not reachable. `enode://` targets route to the
/// execution-layer endpoint; `enr:` records and `/ip4/.../tcp/.../p2p/<peer-id>`
/// multiaddrs route to the consensus layer.
async fn run_reachability(
    config: &MonitoringConfig,
    target: &str,
    json: bool,
) -> Result<CommandOutcome> {
    let target = target.trim();
    if target.is_empty() {
        return Err(P2pTargetError::EmptyTarget.into());
    }
    let is_el = target.starts_with("enode://");
    let is_cl_multiaddr = target.starts_with("/ip4/");
    if is_cl_multiaddr {
        if !target.contains("/p2p/") {
            return Err(
                P2pTargetError::MultiaddrMissingPeerId { target: target.to_string() }.into()
            );
        }
    } else if is_el || target.starts_with("enr:") {
        BootNode::parse_bootnode(target).map_err(|error| P2pTargetError::InvalidBootnode {
            target: target.to_string(),
            message: error.to_string(),
        })?;
    } else {
        return Err(
            P2pTargetError::ReachabilityTargetUnsupported { target: target.to_string() }.into()
        );
    }
    let chain_id = fetch_l2_chain_id(&config.rpc).await.with_context(|| {
        format!("could not detect network from selected config RPC {}", config.rpc)
    })?;
    let telemetry_url = TelemetryClient::backend_base_url(chain_id).ok_or_else(|| {
        anyhow!(
            "hosted reachability checks are unavailable for chain ID {chain_id}; supported chain IDs are 8453 (Base mainnet) and 84532 (Base Sepolia)"
        )
    })?;
    let client = TelemetryClient::new(telemetry_url)?;
    let response = if is_el {
        client.check_el_reachability(target).await?
    } else {
        client.check_cl_reachability(target).await?
    };
    print_reachability(&response, json)?;
    Ok(CommandOutcome::from_failures(response.outcome != ReachabilityOutcome::Reachable))
}

/// Prints one telemetry reachability response as JSON or a key-value table.
fn print_reachability(response: &ReachabilityResponse, json: bool) -> Result<()> {
    if json {
        JsonOutput::print(response)?;
    } else {
        let mut table = KeyValueTable::new();
        table
            .row("outcome", response.outcome.as_str())
            .row("stage", response.stage.as_str())
            .row("observed address", response.observed_address.to_string())
            .row("elapsed", format!("{} ms", response.elapsed_ms));
        if let Some(client_version) = response.client_version.as_deref() {
            table.row("client version", client_version);
        }
        table.print()?;
    }
    Ok(())
}

async fn run_peers(config: MonitoringConfig, args: P2pArgs) -> Result<()> {
    let P2pArgs { el_rpc: el_rpc_override, cl_rpc: cl_rpc_override, json, raw } = args;
    let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
    let cl_rpc = config.resolve_cl_rpc(cl_rpc_override.as_ref(), "p2p peers")?;

    match (json, raw) {
        (true, true) => {
            let report = fetch_raw_peers(&el_rpc, &cl_rpc).await?;
            JsonOutput::print(&report)?;
        }
        (true, false) => {
            let report = fetch_connected_peers(&el_rpc, &cl_rpc).await?;
            JsonOutput::print(&PeersJson::from_report(&config.name, report))?;
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
    let target = AddTarget::parse(&target)?;

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
            if !Confirm::prompt_or_abort(&prompt, yes)? {
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
            let cl_rpc = config.resolve_cl_rpc(cl_rpc_override.as_ref(), "p2p add-peer")?;
            let prompt = format!("Connect CL peer {multiaddr} through {cl_rpc}? [y/N] ");
            if !Confirm::prompt_or_abort(&prompt, yes)? {
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
    let target = PeerTarget::parse(&target)?;

    match target {
        PeerTarget::Enode(enode) => {
            warn_ignored_rpc_override(
                cl_rpc_override.as_ref(),
                "--cl-rpc",
                "enode targets",
                PeerLayer::El,
            );
            let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
            let prompt = format!("Remove EL peer {enode} through {el_rpc}? [y/N] ");
            if !Confirm::prompt_or_abort(&prompt, yes)? {
                return Ok(());
            }
            let accepted = remove_peer(&el_rpc, &enode).await?;
            print_peer_action(
                &PeerActionJson::el(&config.name, PeerAction::Remove, enode, accepted),
                json,
            )?;
        }
        PeerTarget::PeerId(peer_id) => {
            warn_ignored_rpc_override(
                el_rpc_override.as_ref(),
                "--el-rpc",
                "CL targets",
                PeerLayer::Cl,
            );
            let cl_rpc = config.resolve_cl_rpc(cl_rpc_override.as_ref(), "p2p remove-peer")?;
            let prompt = format!("Disconnect CL peer {peer_id} from {cl_rpc}? [y/N] ");
            if !Confirm::prompt_or_abort(&prompt, yes)? {
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

async fn run_peer_ban_action(
    config: MonitoringConfig,
    args: DestructivePeerArgs,
    action: BanAction,
) -> Result<()> {
    let DestructivePeerArgs { target, el_rpc: el_rpc_override, cl_rpc: cl_rpc_override, yes, json } =
        args;
    let (verb, command_name) = match action {
        BanAction::Ban => ("Ban", "p2p ban"),
        BanAction::Unban => ("Unban", "p2p unban"),
    };
    match PeerTarget::parse(&target)? {
        PeerTarget::Enode(enode) => {
            warn_ignored_rpc_override(
                cl_rpc_override.as_ref(),
                "--cl-rpc",
                "enode targets",
                PeerLayer::El,
            );
            let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
            if matches!(action, BanAction::Ban) && el_peer_is_trusted(&el_rpc, &enode).await? {
                return Err(P2pCommandError::TrustedElPeerBan { target: enode }.into());
            }
            let prompt = format!("{verb} EL peer {enode} through {el_rpc}? [y/N] ");
            if !Confirm::prompt_or_abort(&prompt, yes)? {
                return Ok(());
            }
            let accepted = match action {
                BanAction::Ban => ban_el_peer(&el_rpc, &enode).await?,
                BanAction::Unban => unban_el_peer(&el_rpc, &enode).await?,
            };
            print_peer_action(
                &PeerActionJson::el(&config.name, action.peer_action(), enode, accepted),
                json,
            )?;
        }
        PeerTarget::PeerId(peer_id) => {
            warn_ignored_rpc_override(
                el_rpc_override.as_ref(),
                "--el-rpc",
                "CL targets",
                PeerLayer::Cl,
            );
            let cl_rpc = config.resolve_cl_rpc(cl_rpc_override.as_ref(), command_name)?;
            let prompt = format!("{verb} CL peer {peer_id} through {cl_rpc}? [y/N] ");
            if !Confirm::prompt_or_abort(&prompt, yes)? {
                return Ok(());
            }
            let disconnect_error = match action {
                BanAction::Ban => {
                    ban_peer(&cl_rpc, &peer_id).await?;
                    disconnect_peer(&cl_rpc, &peer_id).await.err().map(|err| err.to_string())
                }
                BanAction::Unban => {
                    unban_peer(&cl_rpc, &peer_id).await?;
                    None
                }
            };
            print_peer_action(
                &PeerActionJson::cl_with_disconnect_error(
                    &config.name,
                    action.peer_action(),
                    peer_id,
                    disconnect_error,
                ),
                json,
            )?;
        }
    }
    Ok(())
}

async fn run_unban_all(
    config: MonitoringConfig,
    args: DestructiveClBulkArgs,
) -> Result<CommandOutcome> {
    let DestructiveClBulkArgs { cl_rpc: cl_rpc_override, yes, json } = args;
    let cl_rpc = config.resolve_cl_rpc(cl_rpc_override.as_ref(), "p2p unban-all")?;
    let mut peer_ids = list_banned_peers(&cl_rpc).await?;
    peer_ids.sort();

    if peer_ids.is_empty() {
        if json {
            print_peer_action(
                &PeerActionJson::cl_bulk(&config.name, PeerBulkAction::UnbanAll, vec![]),
                json,
            )?;
        } else {
            println!("no peers are currently banned");
        }
        return Ok(CommandOutcome::Success);
    }

    let prompt = format!("Unban all {} banned CL peers through {cl_rpc}? [y/N] ", peer_ids.len());
    if !Confirm::prompt_or_abort(&prompt, yes)? {
        return Ok(CommandOutcome::Success);
    }

    let mut results = Vec::with_capacity(peer_ids.len());
    for peer_id in peer_ids {
        match unban_peer(&cl_rpc, &peer_id).await {
            Ok(()) => results.push(PeerBulkActionResultJson::ok(peer_id)),
            Err(err) => results.push(PeerBulkActionResultJson::err(peer_id, err.to_string())),
        }
    }
    let action = PeerActionJson::cl_bulk(&config.name, PeerBulkAction::UnbanAll, results);
    let failed = action.failed_count();
    print_peer_action(&action, json)?;
    Ok(CommandOutcome::from_failures(failed > 0))
}

async fn run_info(config: MonitoringConfig, args: P2pArgs) -> Result<()> {
    let P2pArgs { el_rpc: el_rpc_override, cl_rpc: cl_rpc_override, json, raw } = args;
    let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
    let cl_rpc = config.resolve_cl_rpc(cl_rpc_override.as_ref(), "p2p info")?;

    match (json, raw) {
        (true, true) => {
            let report = fetch_raw_info(&el_rpc, &cl_rpc).await?;
            JsonOutput::print(&report)?;
        }
        (true, false) => {
            let (node_info, peer_stats) = fetch_info(&el_rpc, &cl_rpc).await?;
            JsonOutput::print(&P2pInfoJson::from_report(&config.name, &node_info, &peer_stats))?;
        }
        (false, _) => {
            let (node_info, peer_stats) = fetch_info(&el_rpc, &cl_rpc).await?;
            P2pInfoTable::from_report(&config.name, &node_info, &peer_stats).print()?;
        }
    }

    Ok(())
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

/// Parsed peer target accepted by `basectl p2p add-peer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddTarget {
    /// Execution-layer enode target.
    Enode(String),
    /// Consensus-layer multiaddr target.
    Multiaddr(String),
}

/// Parsed peer target accepted by remove, ban, and unban operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTarget {
    /// Execution-layer enode target.
    Enode(String),
    /// Consensus-layer libp2p peer ID.
    PeerId(String),
}

impl AddTarget {
    /// Parses an add-peer target and determines its execution or consensus layer.
    pub fn parse(raw: &str) -> Result<Self, P2pTargetError> {
        let target = raw.trim();
        if target.is_empty() {
            return Err(P2pTargetError::EmptyTarget);
        }
        if target.starts_with('/') {
            if !target.contains("/p2p/") {
                return Err(P2pTargetError::MultiaddrMissingPeerId { target: target.to_string() });
            }
            return Ok(Self::Multiaddr(target.to_string()));
        }

        let bootnode =
            BootNode::parse_bootnode(target).map_err(|error| P2pTargetError::InvalidBootnode {
                target: target.to_string(),
                message: error.to_string(),
            })?;
        match &bootnode {
            BootNode::Enode(_) => Ok(Self::Enode(target.to_string())),
            BootNode::Enr(_) => {
                let multiaddr = bootnode.to_multiaddr().ok_or_else(|| {
                    P2pTargetError::EnrMissingMultiaddr { target: target.to_string() }
                })?;
                Ok(Self::Multiaddr(multiaddr.to_string()))
            }
        }
    }
}

impl PeerTarget {
    /// Parses a remove, ban, or unban target and determines its peer layer.
    pub fn parse(raw: &str) -> Result<Self, P2pTargetError> {
        let target = raw.trim();
        if target.is_empty() {
            return Err(P2pTargetError::EmptyTarget);
        }
        if target.starts_with("enr:") {
            return Err(P2pTargetError::PeerActionEnrTarget { target: target.to_string() });
        }
        if target.split_whitespace().count() != 1 {
            return Err(P2pTargetError::TargetContainsWhitespace { target: target.to_string() });
        }

        if target.starts_with("enode://") {
            BootNode::parse_bootnode(target).map_err(|error| P2pTargetError::InvalidBootnode {
                target: target.to_string(),
                message: error.to_string(),
            })?;
            return Ok(Self::Enode(target.to_string()));
        }
        if target.contains(':') || target.contains('/') {
            return Err(P2pTargetError::PeerActionClTargetNotBarePeerId {
                target: target.to_string(),
            });
        }
        if target.len() < MIN_LIBP2P_PEER_ID_LEN {
            return Err(P2pTargetError::ClPeerIdTooShort {
                target: target.to_string(),
                min_len: MIN_LIBP2P_PEER_ID_LEN,
            });
        }

        Ok(Self::PeerId(target.to_string()))
    }
}

/// Structured JSON outcome for a peer-management action.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged, rename_all = "camelCase")]
pub enum PeerActionJson {
    /// Execution-layer peer action outcome.
    El {
        /// Selected network name.
        network: String,
        /// Peer action performed.
        action: PeerAction,
        /// Peer layer targeted.
        layer: PeerLayer,
        /// Enode target.
        target: String,
        /// Whether the execution client accepted the action.
        accepted: bool,
    },
    /// Consensus-layer peer action outcome.
    Cl {
        /// Selected network name.
        network: String,
        /// Peer action performed.
        action: PeerAction,
        /// Peer layer targeted.
        layer: PeerLayer,
        /// Peer target.
        target: String,
        /// Best-effort disconnect error after a successful ban.
        #[serde(rename = "disconnectError")]
        #[serde(skip_serializing_if = "Option::is_none")]
        disconnect_error: Option<String>,
    },
    /// Consensus-layer bulk peer action outcome.
    ClBulk {
        /// Selected network name.
        network: String,
        /// Peer action performed.
        action: PeerBulkAction,
        /// Peer layer targeted.
        layer: PeerLayer,
        /// Number of peers attempted.
        attempted: usize,
        /// Number of successful actions.
        succeeded: usize,
        /// Number of failed actions.
        failed: usize,
        /// Per-peer action results.
        results: Vec<PeerBulkActionResultJson>,
    },
}

/// Single-peer management action represented in command output.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum PeerAction {
    /// Add or connect a peer.
    #[serde(rename = "addPeer")]
    Add,
    /// Remove or disconnect a peer.
    #[serde(rename = "removePeer")]
    Remove,
    /// Ban a peer.
    #[serde(rename = "banPeer")]
    Ban,
    /// Unban a peer.
    #[serde(rename = "unbanPeer")]
    Unban,
}

/// Bulk peer-management action represented in command output.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum PeerBulkAction {
    /// Unban every banned consensus peer.
    #[serde(rename = "unbanAll")]
    UnbanAll,
}

/// Ban operation handled by the shared peer ban workflow.
#[derive(Debug, Clone, Copy)]
pub enum BanAction {
    /// Ban a peer.
    Ban,
    /// Unban a peer.
    Unban,
}

impl BanAction {
    const fn peer_action(self) -> PeerAction {
        match self {
            Self::Ban => PeerAction::Ban,
            Self::Unban => PeerAction::Unban,
        }
    }
}

/// Peer layer targeted by a p2p operation.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum PeerLayer {
    /// Execution layer.
    #[serde(rename = "el")]
    El,
    /// Consensus layer.
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

    fn cl_bulk(
        network: &str,
        action: PeerBulkAction,
        results: Vec<PeerBulkActionResultJson>,
    ) -> Self {
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

/// Structured result for one peer in a bulk peer action.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBulkActionResultJson {
    /// Peer target.
    pub target: String,
    /// Whether the action succeeded.
    pub ok: bool,
    /// Failure message when the action did not succeed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
        PeerActionJson::El { action: PeerAction::Ban, target, accepted, .. } => {
            if *accepted {
                writeln!(stdout, "OK EL accepted ban for peer {target}")?;
            } else {
                writeln!(stdout, "OK EL did not accept ban for peer {target}")?;
            }
        }
        PeerActionJson::El { action: PeerAction::Unban, target, accepted, .. } => {
            if *accepted {
                writeln!(stdout, "OK EL accepted unban for peer {target}")?;
            } else {
                writeln!(stdout, "OK EL did not accept unban for peer {target}")?;
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
    }
    Ok(())
}

/// Humanized JSON shape for connected peers by layer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeersJson {
    /// Selected network name.
    pub network: String,
    /// Connected execution-layer peers, when the RPC exposes them.
    pub el: Option<Vec<PeerSummary>>,
    /// Connected consensus-layer peers, when the RPC exposes them.
    pub cl: Option<Vec<PeerSummary>>,
}

impl PeersJson {
    fn from_report(network: &str, report: PeerListReport) -> Self {
        Self { network: network.to_string(), el: report.el, cl: report.cl }
    }
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

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use alloy_primitives::Address;
    use serde_json::json;
    use url::Url;

    use super::{
        AddTarget, PeerAction, PeerActionJson, PeerBulkAction, PeerBulkActionResultJson,
        PeerTarget, run_reachability,
    };
    use crate::{MonitoringConfig, P2pTargetError};

    const VALID_ENODE: &str = "enode://d7dfaea49c7ef37701e668652bcf1bc63d3abb2ae97593374a949e175e4ff128730a2f35199f3462a56298b981dfc395a5abebd2d6f0284ffe5bdc3d8e258b86@127.0.0.1:30304?discport=30301";
    const VALID_ENR: &str = "enr:-J64QBbwPjPLZ6IOOToOLsSjtFUjjzN66qmBZdUexpO32Klrc458Q24kbty2PdRaLacHM5z-cZQr8mjeQu3pik6jPSOGAYYFIqBfgmlkgnY0gmlwhDaRWFWHb3BzdGFja4SzlAUAiXNlY3AyNTZrMaECmeSnJh7zjKrDSPoNMGXoopeDF4hhpj5I0OsQUUt4u8uDdGNwgiQGg3VkcIIkBg";

    fn test_config(consensus_node_rpc: Option<Url>) -> MonitoringConfig {
        MonitoringConfig {
            name: "devnet".to_string(),
            rpc: Url::parse("http://127.0.0.1:8545").unwrap(),
            el_ws_rpc: None,
            public_rpc: None,
            flashblocks_ws: Url::parse("ws://127.0.0.1:7111").unwrap(),
            l1_rpc: Url::parse("http://127.0.0.1:9545").unwrap(),
            consensus_node_rpc,
            chain_id: None,
            prover_rpc: None,
            upgrades: None,
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

    #[tokio::test]
    async fn reachability_reports_network_detection_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut config = test_config(None);
        config.rpc = Url::parse(&format!("http://{address}")).unwrap();

        let error = run_reachability(&config, VALID_ENODE, false).await.unwrap_err();

        assert!(
            format!("{error:#}").starts_with("could not detect network from selected config RPC")
        );
    }

    /// Runs `run_reachability` against an unused config and returns the
    /// `P2pTargetError` produced before any network access.
    async fn reachability_target_error(target: &str) -> P2pTargetError {
        let error = run_reachability(&test_config(None), target, false).await.unwrap_err();
        error.downcast::<P2pTargetError>().expect("expected a target validation error")
    }

    #[tokio::test]
    async fn reachability_rejects_unsupported_scheme() {
        assert!(matches!(
            reachability_target_error("16Uiu2HAkxp9nAsXsCthNWPkkpm4yG1eW7L4ENpVyzDZM8HE1yr12")
                .await,
            P2pTargetError::ReachabilityTargetUnsupported { target }
                if target == "16Uiu2HAkxp9nAsXsCthNWPkkpm4yG1eW7L4ENpVyzDZM8HE1yr12"
        ));
    }

    #[tokio::test]
    async fn reachability_rejects_empty_target() {
        assert!(matches!(reachability_target_error("  ").await, P2pTargetError::EmptyTarget));
    }

    #[tokio::test]
    async fn reachability_rejects_malformed_enode() {
        assert!(matches!(
            reachability_target_error("enode://nope").await,
            P2pTargetError::InvalidBootnode { target, .. } if target == "enode://nope"
        ));
    }

    #[tokio::test]
    async fn reachability_rejects_malformed_enr() {
        assert!(matches!(
            reachability_target_error("enr:!!!").await,
            P2pTargetError::InvalidBootnode { target, .. } if target == "enr:!!!"
        ));
    }

    #[tokio::test]
    async fn reachability_rejects_multiaddr_without_peer_id() {
        assert!(matches!(
            reachability_target_error("/ip4/8.8.8.8/tcp/9222").await,
            P2pTargetError::MultiaddrMissingPeerId { target }
                if target == "/ip4/8.8.8.8/tcp/9222"
        ));
    }

    #[tokio::test]
    async fn reachability_accepts_ip4_multiaddr() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut config = test_config(None);
        config.rpc = Url::parse(&format!("http://{address}")).unwrap();

        let error = run_reachability(&config, "/ip4/8.8.8.8/tcp/9222/p2p/16Uiu2HAmExample", false)
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").starts_with("could not detect network from selected config RPC")
        );
    }

    #[test]
    fn parse_add_target_routes_enode_to_el() {
        assert_eq!(
            AddTarget::parse(VALID_ENODE).unwrap(),
            AddTarget::Enode(VALID_ENODE.to_string())
        );
    }

    #[test]
    fn parse_add_target_routes_enr_to_cl_multiaddr() {
        let AddTarget::Multiaddr(multiaddr) = AddTarget::parse(VALID_ENR).unwrap() else {
            panic!("expected ENR to route to CL multiaddr");
        };

        assert!(multiaddr.starts_with("/ip4/"));
        assert!(multiaddr.contains("/p2p/"));
    }

    #[test]
    fn parse_add_target_rejects_garbage() {
        assert!(matches!(
            AddTarget::parse("not-a-peer").unwrap_err(),
            P2pTargetError::InvalidBootnode { target, .. } if target == "not-a-peer"
        ));
    }

    #[test]
    fn parse_add_target_routes_multiaddr_to_cl() {
        let multiaddr = "/ip4/127.0.0.1/tcp/9000/p2p/16Uiu2HAmExample";

        assert_eq!(
            AddTarget::parse(multiaddr).unwrap(),
            AddTarget::Multiaddr(multiaddr.to_string())
        );
    }

    #[test]
    fn parse_add_target_rejects_multiaddr_without_peer_id() {
        let err = AddTarget::parse("/ip4/127.0.0.1/tcp/9000")
            .expect_err("multiaddr without peer ID should be rejected");

        assert!(matches!(
            err,
            P2pTargetError::MultiaddrMissingPeerId { target }
                if target == "/ip4/127.0.0.1/tcp/9000"
        ));
    }

    #[test]
    fn parse_peer_target_routes_enode_to_el() {
        assert_eq!(
            PeerTarget::parse(VALID_ENODE).unwrap(),
            PeerTarget::Enode(VALID_ENODE.to_string())
        );
    }

    #[test]
    fn parse_peer_target_routes_peer_id_to_cl() {
        let peer_id = "16Uiu2HAkxp9nAsXsCthNWPkkpm4yG1eW7L4ENpVyzDZM8HE1yr12";

        assert_eq!(PeerTarget::parse(peer_id).unwrap(), PeerTarget::PeerId(peer_id.to_string()));
    }

    #[test]
    fn parse_peer_target_rejects_enr() {
        assert!(matches!(
            PeerTarget::parse(VALID_ENR).unwrap_err(),
            P2pTargetError::PeerActionEnrTarget { target } if target == VALID_ENR
        ));
    }

    #[test]
    fn parse_peer_target_rejects_multiaddr() {
        assert!(matches!(
            PeerTarget::parse("/ip4/127.0.0.1/tcp/9000/p2p/16Uiu2HAmExample").unwrap_err(),
            P2pTargetError::PeerActionClTargetNotBarePeerId { .. }
        ));
    }

    #[test]
    fn parse_peer_target_rejects_url_like_target() {
        assert!(matches!(
            PeerTarget::parse("https://example.com").unwrap_err(),
            P2pTargetError::PeerActionClTargetNotBarePeerId { target }
                if target == "https://example.com"
        ));
    }

    #[test]
    fn parse_peer_target_rejects_obviously_short_peer_id() {
        let err = PeerTarget::parse("hello").expect_err("short peer ID should be rejected");

        assert!(matches!(
            err,
            P2pTargetError::ClPeerIdTooShort { target, min_len }
                if target == "hello" && min_len == super::MIN_LIBP2P_PEER_ID_LEN
        ));
    }

    #[test]
    fn parse_peer_target_rejects_whitespace() {
        assert!(matches!(
            PeerTarget::parse("16Uiu2HAkxp9nAsXsCthNWPkkpm4yG1eW7L4ENpVyzDZM8HE1yr12 extra")
                .unwrap_err(),
            P2pTargetError::TargetContainsWhitespace { .. }
        ));
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
            PeerBulkAction::UnbanAll,
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
