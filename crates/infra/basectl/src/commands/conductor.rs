//! Implementation of the `basectl conductor` command group.

use std::io::{self, Write};

use anyhow::Result;
use base_consensus_rpc::ServerSuffrage;
use clap::{Args, Subcommand};
use serde::Serialize;
use tracing::warn;
use url::Url;

use crate::{
    CommandOutcome, ConductorClusterSnapshot, ConductorCommandError, ConductorControl,
    ConductorFanoutAction, ConductorFanoutReport, ConductorNodeConfig, ConductorNodeFailure,
    ConductorNodeStatus, ConductorSource, Confirm, JsonOutput, KeyValueTable, MonitoringConfig,
    NodeMetricsJson, OptionalValue,
};

/// Inspect and control an HA conductor cluster.
#[derive(Debug, Args)]
pub struct ConductorCommand {
    /// Conductor operation to run.
    #[command(subcommand)]
    pub command: ConductorCommands,
}

/// HA conductor inspection and control commands.
#[derive(Debug, Subcommand)]
pub enum ConductorCommands {
    /// Show current cluster status.
    Status(ConductorStatusArgs),
    /// Transfer raft leadership away from the current leader or to a target node.
    TransferLeader(ConductorLeaderArgs),
    /// Pause op-conductor's control loop on one node.
    Pause(ConductorNodeActionArgs),
    /// Resume op-conductor's control loop on one node.
    Unpause(ConductorNodeActionArgs),
    /// Pause op-conductor's control loop on every current raft member, falling
    /// back to the configured conductor list if static membership lookup is unavailable.
    PauseAll(ConductorClusterActionArgs),
    /// Resume op-conductor's control loop on every current raft member, falling
    /// back to the configured conductor list if static membership lookup is unavailable.
    UnpauseAll(ConductorClusterActionArgs),
}

/// Flags for `basectl conductor status`.
#[derive(Debug, Args)]
pub struct ConductorStatusArgs {
    /// Emit a structured JSON status summary instead of pretty text.
    #[arg(long)]
    pub json: bool,
}

/// Flags for `basectl conductor transfer-leader`.
#[derive(Debug, Args)]
pub struct ConductorLeaderArgs {
    /// Optional target node name. If omitted, the leader transfers to any available peer.
    #[arg(value_name = "TARGET")]
    pub target: Option<String>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

/// Shared flags for single-node destructive conductor commands.
#[derive(Debug, Args)]
pub struct ConductorNodeActionArgs {
    /// Conductor node name from the selected config or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub node: String,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

/// Shared flags for cluster-wide destructive conductor commands.
#[derive(Debug, Args)]
pub struct ConductorClusterActionArgs {
    /// Skip the typed network-name confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text. Requires `--yes`.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

/// Machine-readable conductor operation represented in JSON output.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum ConductorActionName {
    /// Transfer raft leadership.
    #[serde(rename = "transferLeader")]
    TransferLeader,
    /// Pause one conductor.
    #[serde(rename = "pause")]
    Pause,
    /// Resume one conductor.
    #[serde(rename = "unpause")]
    Unpause,
    /// Pause every conductor.
    #[serde(rename = "pauseAll")]
    PauseAll,
    /// Resume every conductor.
    #[serde(rename = "unpauseAll")]
    UnpauseAll,
}

/// Membership source used for a cluster-wide action.
#[derive(Debug, Clone, Copy)]
pub enum ClusterNodeScope {
    /// Nodes returned by live raft membership.
    CurrentRaftMembers,
    /// Nodes from static configuration.
    ConfiguredNodes,
}

impl ClusterNodeScope {
    /// Human-readable scope description for confirmation prompts.
    pub const fn description(self) -> &'static str {
        match self {
            Self::CurrentRaftMembers => "current raft members",
            Self::ConfiguredNodes => "configured conductors",
        }
    }
}

/// Pause/unpause direction for conductor control-loop actions.
///
/// Centralizes the machine-readable action names and human-facing verb forms so
/// JSON output and prompts cannot drift apart across call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConductorAction {
    /// Pause the conductor control loop.
    Pause,
    /// Resume the conductor control loop.
    Unpause,
}

impl ConductorAction {
    /// Machine-readable action for single-node JSON output.
    pub const fn name(self) -> ConductorActionName {
        match self {
            Self::Pause => ConductorActionName::Pause,
            Self::Unpause => ConductorActionName::Unpause,
        }
    }

    /// Machine-readable action for cluster-wide JSON output.
    pub const fn cluster_name(self) -> ConductorActionName {
        match self {
            Self::Pause => ConductorActionName::PauseAll,
            Self::Unpause => ConductorActionName::UnpauseAll,
        }
    }

    /// Lowercase verb for confirmation prompts and fanout summaries.
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Unpause => "unpause",
        }
    }

    /// Capitalized verb for confirmation prompts.
    pub const fn prompt_verb(self) -> &'static str {
        match self {
            Self::Pause => "Pause",
            Self::Unpause => "Unpause",
        }
    }

    /// Cluster-wide fanout operation performed by this action.
    pub const fn fanout(self) -> ConductorFanoutAction {
        match self {
            Self::Pause => ConductorFanoutAction::Pause,
            Self::Unpause => ConductorFanoutAction::Resume,
        }
    }
}

impl ConductorCommand {
    /// Runs the selected conductor subcommand.
    pub async fn run(
        self,
        config: MonitoringConfig,
        conductor_rpc: Option<Url>,
    ) -> Result<CommandOutcome> {
        let source =
            config.resolve_conductor_source(conductor_rpc).map_err(ConductorCommandError::from)?;
        match self.command {
            ConductorCommands::Status(args) => run_status(config, source, args).await,
            ConductorCommands::TransferLeader(args) => {
                run_transfer_leader(config, source, args).await
            }
            ConductorCommands::Pause(args) => {
                run_node_action(config, source, args, ConductorAction::Pause).await
            }
            ConductorCommands::Unpause(args) => {
                run_node_action(config, source, args, ConductorAction::Unpause).await
            }
            ConductorCommands::PauseAll(args) => {
                run_cluster_action(config, source, args, ConductorAction::Pause).await
            }
            ConductorCommands::UnpauseAll(args) => {
                run_cluster_action(config, source, args, ConductorAction::Unpause).await
            }
        }
    }
}

async fn run_status(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorStatusArgs,
) -> Result<CommandOutcome> {
    let snapshot = ConductorControl::snapshot(source).await?;
    let status = ConductorStatusJson::from_snapshot(&config.name, &snapshot);
    if args.json {
        JsonOutput::print(&status)?;
    } else {
        print_status_pretty(&status)?;
    }
    Ok(CommandOutcome::Success)
}

async fn run_transfer_leader(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorLeaderArgs,
) -> Result<CommandOutcome> {
    let snapshot = ConductorControl::snapshot(source).await?;
    let nodes = &snapshot.nodes;
    if let Some(target) = args.target.as_deref() {
        // Validate before prompting so a typo does not ask for confirmation and only
        // fail after the operator already answered yes.
        ConductorNodeConfig::find(nodes, target).map_err(ConductorCommandError::from)?;
    }
    if let Err(error) = ensure_transfer_leader_el_readiness(&snapshot, args.target.as_deref()) {
        warn!(
            error = %error,
            target = %args.target.as_deref().unwrap_or("replacement"),
            "conductor leadership transfer EL readiness preflight failed"
        );
        return Err(error.into());
    }

    let prompt = args.target.as_deref().map_or_else(
        || {
            format!(
                "Transfer conductor leadership away from the current leader for {}? [y/N] ",
                config.name
            )
        },
        |target| format!("Transfer conductor leadership to {target} for {}? [y/N] ", config.name),
    );
    if !Confirm::prompt_or_abort(&prompt, args.yes)? {
        return Ok(CommandOutcome::Success);
    }

    let message = ConductorControl::transfer_leader(nodes, args.target.as_deref()).await?;
    let outcome = ConductorActionJson::single(
        &config.name,
        ConductorActionName::TransferLeader,
        args.target,
        message,
    );
    JsonOutput::print_or_ok(&outcome, &outcome.message, args.json)?;
    Ok(CommandOutcome::Success)
}

fn ensure_transfer_leader_el_readiness(
    snapshot: &ConductorClusterSnapshot,
    target: Option<&str>,
) -> Result<(), ConductorCommandError> {
    let Some(leader_status) =
        snapshot.statuses.iter().find(|status| status.is_leader == Some(true))
    else {
        return Ok(());
    };
    if target == Some(leader_status.name.as_str()) {
        return Ok(());
    }
    let required_l2_block = leader_status.unsafe_l2_block.ok_or_else(|| {
        ConductorCommandError::ExecutionLayerStatusUnavailable {
            node: leader_status.name.clone(),
            field: "unsafe_l2_block",
        }
    })?;

    if let Some(target) = target {
        let target_node = ConductorNodeConfig::find(&snapshot.nodes, target)
            .map_err(ConductorCommandError::from)?;
        if target_node.el_rpc.is_none() {
            return Ok(());
        }
        let target_status = snapshot_node_status(snapshot, target).ok_or_else(|| {
            ConductorCommandError::ExecutionLayerStatusUnavailable {
                node: target.to_string(),
                field: "status",
            }
        })?;
        return ensure_node_el_ready_for_leadership_transfer(
            target_status,
            target,
            required_l2_block,
        );
    }

    for node in snapshot
        .nodes
        .iter()
        .filter(|node| node.name != leader_status.name && node.el_rpc.is_some())
    {
        let status = snapshot_node_status(snapshot, &node.name);
        let status =
            status.ok_or_else(|| ConductorCommandError::ExecutionLayerStatusUnavailable {
                node: node.name.clone(),
                field: "status",
            })?;
        ensure_node_el_ready_for_leadership_transfer(status, &node.name, required_l2_block)?;
    }
    Ok(())
}

fn ensure_node_el_ready_for_leadership_transfer(
    status: &ConductorNodeStatus,
    node: &str,
    required_l2_block: u64,
) -> Result<(), ConductorCommandError> {
    let el_block =
        status.el_block.ok_or_else(|| ConductorCommandError::ExecutionLayerStatusUnavailable {
            node: node.to_string(),
            field: "el_block",
        })?;
    match status.el_syncing {
        Some(false) => {}
        Some(true) => {
            return Err(ConductorCommandError::ExecutionLayerSyncing {
                node: node.to_string(),
                el_block,
                required_l2_block,
            });
        }
        None => {
            return Err(ConductorCommandError::ExecutionLayerStatusUnavailable {
                node: node.to_string(),
                field: "el_syncing",
            });
        }
    }

    if el_block < required_l2_block {
        return Err(ConductorCommandError::ExecutionLayerBehind {
            node: node.to_string(),
            el_block,
            required_l2_block,
        });
    }
    Ok(())
}

fn snapshot_node_status<'a>(
    snapshot: &'a ConductorClusterSnapshot,
    name: &str,
) -> Option<&'a ConductorNodeStatus> {
    snapshot.statuses.iter().find(|status| status.name == name)
}

async fn run_node_action(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorNodeActionArgs,
    action: ConductorAction,
) -> Result<CommandOutcome> {
    let nodes = current_nodes_for_action(&source).await?;
    let node =
        ConductorNodeConfig::find(&nodes, &args.node).map_err(ConductorCommandError::from)?;
    let prompt = format!(
        "{} conductor control loop on {} ({})? [y/N] ",
        action.prompt_verb(),
        node.name,
        node.conductor_rpc
    );
    if !Confirm::prompt_or_abort(&prompt, args.yes)? {
        return Ok(CommandOutcome::Success);
    }

    let message = match action {
        ConductorAction::Pause => ConductorControl::pause_node(node).await?,
        ConductorAction::Unpause => ConductorControl::resume_node(node).await?,
    };
    let outcome =
        ConductorActionJson::single(&config.name, action.name(), Some(node.name.clone()), message);
    JsonOutput::print_or_ok(&outcome, &outcome.message, args.json)?;
    Ok(CommandOutcome::Success)
}

async fn run_cluster_action(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorClusterActionArgs,
    action: ConductorAction,
) -> Result<CommandOutcome> {
    let (nodes, node_scope) = current_nodes_for_cluster_action(&source).await?;
    let names = nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>().join(", ");
    let prompt = format!(
        "Type {} to {} conductor control loop on all {} {} ({}): ",
        config.name,
        action.verb(),
        nodes.len(),
        node_scope.description(),
        names
    );
    if !Confirm::typed_or_abort(&prompt, &config.name, args.yes)? {
        return Ok(CommandOutcome::Success);
    }

    let report = match action {
        ConductorAction::Pause => ConductorControl::pause_all(nodes).await,
        ConductorAction::Unpause => ConductorControl::resume_all(nodes).await,
    };
    print_fanout_action(&report, &config.name, action, args.json)?;
    Ok(CommandOutcome::from_failures(!report.is_success()))
}

async fn current_nodes_for_action(source: &ConductorSource) -> Result<Vec<ConductorNodeConfig>> {
    match source {
        ConductorSource::Static(nodes) => Ok(nodes.clone()),
        ConductorSource::Discover { .. } => {
            let membership = ConductorControl::current_membership(source).await?;
            ConductorControl::nodes_from_membership(source, &membership)
        }
    }
}

async fn current_nodes_for_cluster_action(
    source: &ConductorSource,
) -> Result<(Vec<ConductorNodeConfig>, ClusterNodeScope)> {
    match source {
        // Prefer live membership when it is reachable so stale static entries are
        // not mutated unnecessarily, but fall back to the configured list so a
        // temporary membership RPC outage does not block bulk actions entirely.
        ConductorSource::Static(nodes) => {
            match ConductorControl::current_membership(source).await {
                Ok(membership) => Ok((
                    ConductorControl::nodes_from_membership(source, &membership)?,
                    ClusterNodeScope::CurrentRaftMembers,
                )),
                Err(error) => {
                    warn!(
                        error = %error,
                        "membership lookup failed for static conductor source; falling back to configured node list"
                    );
                    Ok((nodes.clone(), ClusterNodeScope::ConfiguredNodes))
                }
            }
        }
        ConductorSource::Discover { .. } => {
            let membership = ConductorControl::current_membership(source).await?;
            let nodes = ConductorControl::nodes_from_membership(source, &membership)?;
            Ok((nodes, ClusterNodeScope::CurrentRaftMembers))
        }
    }
}

fn print_status_pretty(status: &ConductorStatusJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &status.network)
        .row("source", status.source)
        .row("nodes", status.nodes.len().to_string());
    if let Some(version) = status.membership_version {
        table.row("membership_version", version.to_string());
    }
    if let Some(error) = &status.membership_error {
        table.row("membership_error", error);
    }
    table.row("leader", status.leader.as_deref().unwrap_or("unknown"));
    table.row("paused", format!("{}/{} known paused", status.paused.paused, status.paused.known));
    for node in &status.nodes {
        table.row(format!("node.{}", node.name), node.compact_status());
    }
    table.print()?;
    Ok(())
}

fn print_fanout_action(
    report: &ConductorFanoutReport,
    network: &str,
    action: ConductorAction,
    json: bool,
) -> Result<()> {
    if json {
        JsonOutput::print(&ConductorFanoutJson::from_report(
            network,
            action.cluster_name(),
            report,
        ))?;
    } else {
        let mut stdout = io::stdout().lock();
        let prefix = if report.is_success() { "OK" } else { "WARN" };
        writeln!(stdout, "{prefix} {}", report.summary(action.fanout()))?;
    }
    Ok(())
}

/// JSON result for a single conductor action.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConductorActionJson {
    /// Network name.
    pub network: String,
    /// Action performed.
    pub action: ConductorActionName,
    /// Optional target node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Human-readable result.
    pub message: String,
}

impl ConductorActionJson {
    /// Builds the outcome for a single conductor action.
    pub fn single(
        network: &str,
        action: ConductorActionName,
        target: Option<String>,
        message: String,
    ) -> Self {
        Self { network: network.to_string(), action, target, message }
    }
}

/// JSON result for a cluster-wide conductor action.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConductorFanoutJson {
    /// Network name.
    pub network: String,
    /// Action performed.
    pub action: ConductorActionName,
    /// Number of targeted nodes.
    pub total: usize,
    /// Nodes where the action succeeded.
    pub successes: Vec<String>,
    /// Per-node failures.
    pub failures: Vec<ConductorFailureJson>,
}

impl ConductorFanoutJson {
    /// Builds the outcome for a cluster-wide conductor action.
    pub fn from_report(
        network: &str,
        action: ConductorActionName,
        report: &ConductorFanoutReport,
    ) -> Self {
        Self {
            network: network.to_string(),
            action,
            total: report.total,
            successes: report.successes.clone(),
            failures: report.failures.iter().map(ConductorFailureJson::from_failure).collect(),
        }
    }
}

/// JSON representation of a conductor node action failure.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConductorFailureJson {
    /// Node name.
    pub name: String,
    /// Failure message.
    pub error: String,
}

impl ConductorFailureJson {
    /// Builds the failure entry from a fanout report failure.
    pub fn from_failure(failure: &ConductorNodeFailure) -> Self {
        Self { name: failure.name.clone(), error: failure.error.clone() }
    }
}

/// JSON conductor cluster status.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConductorStatusJson {
    /// Network name.
    pub network: String,
    /// Membership source.
    pub source: &'static str,
    /// Raft membership version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub membership_version: Option<u64>,
    /// Membership lookup error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub membership_error: Option<String>,
    /// Current leader node.
    pub leader: Option<String>,
    /// Pause-state summary.
    pub paused: PausedSummaryJson,
    /// Per-node statuses.
    pub nodes: Vec<ConductorNodeJson>,
}

impl ConductorStatusJson {
    /// Builds the status summary from a cluster snapshot.
    pub fn from_snapshot(network: &str, snapshot: &ConductorClusterSnapshot) -> Self {
        let source = snapshot.source_label();
        let nodes = snapshot
            .nodes
            .iter()
            .map(|node| {
                let status = snapshot.statuses.iter().find(|status| status.name == node.name);
                ConductorNodeJson::from_node_status(node, status, snapshot.discovered)
            })
            .collect::<Vec<_>>();
        let leader = nodes
            .iter()
            .find(|node| node.metrics.is_leader == Some(true))
            .map(|node| node.name.clone());
        let paused = PausedSummaryJson {
            known: nodes.iter().filter(|node| node.metrics.conductor_paused.is_some()).count(),
            paused: nodes.iter().filter(|node| node.metrics.conductor_paused == Some(true)).count(),
        };

        Self {
            network: network.to_string(),
            source,
            membership_version: snapshot.membership.as_ref().map(|membership| membership.version),
            membership_error: snapshot.membership_error.clone(),
            leader,
            paused,
            nodes,
        }
    }
}

/// Summary of known paused conductor nodes.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PausedSummaryJson {
    /// Nodes with a known pause state.
    pub known: usize,
    /// Known paused nodes.
    pub paused: usize,
}

/// JSON status for one conductor node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConductorNodeJson {
    /// Node name.
    pub name: String,
    /// Raft server ID.
    pub server_id: String,
    /// Raft address.
    pub raft_addr: String,
    /// Conductor RPC URL.
    pub conductor_rpc: String,
    /// Metrics shared with sequencer node output.
    #[serde(flatten)]
    pub metrics: NodeMetricsJson,
    /// Whether the conductor is active.
    pub conductor_active: Option<bool>,
    /// Whether the conductor is stopped.
    pub conductor_stopped: Option<bool>,
    /// Raft suffrage.
    pub suffrage: Option<&'static str>,
    /// Whether runtime discovery produced this node.
    pub discovered: bool,
}

impl ConductorNodeJson {
    /// Builds the node entry from its configuration and polled status.
    pub fn from_node_status(
        node: &ConductorNodeConfig,
        status: Option<&ConductorNodeStatus>,
        discovered: bool,
    ) -> Self {
        Self {
            name: node.name.clone(),
            server_id: node.server_id.clone(),
            raft_addr: node.raft_addr.clone(),
            conductor_rpc: node.conductor_rpc.to_string(),
            metrics: NodeMetricsJson::from_status(status),
            conductor_active: status.and_then(|status| status.conductor_active),
            conductor_stopped: status.and_then(|status| status.conductor_stopped),
            suffrage: status.and_then(|status| status.suffrage).map(|suffrage| match suffrage {
                ServerSuffrage::Voter => "voter",
                ServerSuffrage::Nonvoter => "nonvoter",
            }),
            discovered,
        }
    }

    /// Renders a single-line status summary for pretty output.
    pub fn compact_status(&self) -> String {
        format!(
            "leader={} conductor_active={} conductor_paused={} conductor_stopped={} sequencer_active={} sequencer_healthy={} unsafe={} safe={} cl_peers={} el_peers={}",
            OptionalValue::boolean(self.metrics.is_leader),
            OptionalValue::boolean(self.conductor_active),
            OptionalValue::boolean(self.metrics.conductor_paused),
            OptionalValue::boolean(self.conductor_stopped),
            OptionalValue::boolean(self.metrics.sequencer_active),
            OptionalValue::boolean(self.metrics.sequencer_healthy),
            OptionalValue::u64(self.metrics.unsafe_l2_block),
            OptionalValue::u64(self.metrics.safe_l2_block),
            OptionalValue::u32(self.metrics.cl_peer_count),
            OptionalValue::u32(self.metrics.el_peer_count),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use serde_json::json;
    use url::Url;

    use super::{
        ConductorAction, ConductorActionJson, ConductorActionName, ConductorFanoutJson,
        ConductorNodeJson, ConductorStatusJson, ensure_transfer_leader_el_readiness,
    };
    use crate::{
        CommandOutcome, ConductorClusterSnapshot, ConductorCommandError, ConductorNodeConfig,
        ConductorNodeFailure, NodeLookupError,
    };

    fn node(name: &str) -> ConductorNodeConfig {
        ConductorNodeConfig {
            name: name.to_string(),
            conductor_rpc: Url::parse("http://127.0.0.1:6545").unwrap(),
            cl_rpc: Url::parse("http://127.0.0.1:7545").unwrap(),
            server_id: name.to_string(),
            raft_addr: format!("{name}:5050"),
            el_rpc: None,
            docker_conductor: None,
            docker_el: None,
            docker_cl: None,
            flashblocks_ws: None,
        }
    }

    fn node_with_el_rpc(name: &str) -> ConductorNodeConfig {
        let mut node = node(name);
        node.el_rpc = Some(Url::parse("http://127.0.0.1:8545").unwrap());
        node
    }

    fn status(name: &str, leader: bool, paused: bool) -> crate::ConductorNodeStatus {
        crate::ConductorNodeStatus {
            name: name.to_string(),
            is_leader: Some(leader),
            conductor_active: Some(leader),
            conductor_paused: Some(paused),
            conductor_stopped: Some(false),
            sequencer_healthy: Some(true),
            sequencer_active: Some(leader),
            unsafe_l2_block: Some(10),
            unsafe_l2_hash: Some(B256::with_last_byte(1)),
            safe_l2_block: Some(8),
            safe_l2_hash: Some(B256::with_last_byte(2)),
            finalized_l2_block: Some(6),
            current_l1_block: Some(100),
            head_l1_block: Some(101),
            cl_peer_count: Some(3),
            el_block: Some(10),
            el_syncing: Some(false),
            el_peer_count: Some(4),
            suffrage: None,
            discovered: false,
        }
    }

    fn snapshot(
        nodes: Vec<ConductorNodeConfig>,
        statuses: Vec<crate::ConductorNodeStatus>,
    ) -> ConductorClusterSnapshot {
        ConductorClusterSnapshot {
            nodes,
            statuses,
            membership: None,
            membership_error: None,
            discovered: false,
        }
    }

    #[test]
    fn conductor_action_json_serializes_camel_case_action() {
        let value = serde_json::to_value(ConductorActionJson::single(
            "devnet",
            ConductorActionName::TransferLeader,
            Some("op-conductor-1".to_string()),
            "leadership transferred to op-conductor-1".to_string(),
        ))
        .unwrap();

        assert_eq!(
            value,
            json!({
                "network": "devnet",
                "action": "transferLeader",
                "target": "op-conductor-1",
                "message": "leadership transferred to op-conductor-1",
            })
        );
    }

    #[test]
    fn conductor_fanout_json_serializes_failures() {
        let report = crate::ConductorFanoutReport {
            total: 2,
            successes: vec!["op-conductor-0".to_string()],
            failures: vec![ConductorNodeFailure {
                name: "op-conductor-1".to_string(),
                error: "request timed out".to_string(),
            }],
        };

        let value = serde_json::to_value(ConductorFanoutJson::from_report(
            "devnet",
            ConductorAction::Pause.cluster_name(),
            &report,
        ))
        .unwrap();

        assert_eq!(
            value,
            json!({
                "network": "devnet",
                "action": "pauseAll",
                "total": 2,
                "successes": ["op-conductor-0"],
                "failures": [{"name": "op-conductor-1", "error": "request timed out"}],
            })
        );
    }

    #[test]
    fn fanout_failure_exit_matches_report_status() {
        let success = crate::ConductorFanoutReport {
            total: 2,
            successes: vec!["op-conductor-0".to_string(), "op-conductor-1".to_string()],
            failures: Vec::new(),
        };
        let partial_failure = crate::ConductorFanoutReport {
            total: 2,
            successes: vec!["op-conductor-0".to_string()],
            failures: vec![ConductorNodeFailure {
                name: "op-conductor-1".to_string(),
                error: "request timed out".to_string(),
            }],
        };
        let empty =
            crate::ConductorFanoutReport { total: 0, successes: Vec::new(), failures: Vec::new() };

        assert_eq!(CommandOutcome::from_failures(!success.is_success()), CommandOutcome::Success);
        assert_eq!(
            CommandOutcome::from_failures(!partial_failure.is_success()),
            CommandOutcome::HasFailures
        );
        assert_eq!(CommandOutcome::from_failures(!empty.is_success()), CommandOutcome::HasFailures);
    }

    #[test]
    fn transfer_leader_rejects_syncing_target_el() {
        let mut target_status = status("op-conductor-1", false, false);
        target_status.el_syncing = Some(true);
        target_status.el_block = Some(9);
        let snapshot = snapshot(
            vec![node_with_el_rpc("op-conductor-0"), node_with_el_rpc("op-conductor-1")],
            vec![status("op-conductor-0", true, false), target_status],
        );

        let err = ensure_transfer_leader_el_readiness(&snapshot, Some("op-conductor-1"))
            .expect_err("syncing target EL should block leadership transfer");

        assert!(matches!(
            err,
            ConductorCommandError::ExecutionLayerSyncing {
                node,
                el_block,
                required_l2_block,
            } if node == "op-conductor-1"
                && el_block == 9
                && required_l2_block == 10
        ));
    }

    #[test]
    fn transfer_leader_rejects_target_el_behind_leader_unsafe_head() {
        let mut target_status = status("op-conductor-1", false, false);
        target_status.el_block = Some(9);
        let snapshot = snapshot(
            vec![node_with_el_rpc("op-conductor-0"), node_with_el_rpc("op-conductor-1")],
            vec![status("op-conductor-0", true, false), target_status],
        );

        let err = ensure_transfer_leader_el_readiness(&snapshot, Some("op-conductor-1"))
            .expect_err("target EL behind leader unsafe head should block leadership transfer");

        assert!(matches!(
            err,
            ConductorCommandError::ExecutionLayerBehind {
                node,
                el_block,
                required_l2_block,
            } if node == "op-conductor-1" && el_block == 9 && required_l2_block == 10
        ));
    }

    #[test]
    fn transfer_leader_rejects_missing_target_el_status() {
        let mut target_status = status("op-conductor-1", false, false);
        target_status.el_syncing = None;
        let snapshot = snapshot(
            vec![node_with_el_rpc("op-conductor-0"), node_with_el_rpc("op-conductor-1")],
            vec![status("op-conductor-0", true, false), target_status],
        );

        let err = ensure_transfer_leader_el_readiness(&snapshot, Some("op-conductor-1"))
            .expect_err("missing target EL status should block leadership transfer");

        assert!(matches!(
            err,
            ConductorCommandError::ExecutionLayerStatusUnavailable { node, field }
                if node == "op-conductor-1" && field == "el_syncing"
        ));
    }

    #[test]
    fn transfer_leader_rejects_missing_target_status_record() {
        let snapshot = snapshot(
            vec![node_with_el_rpc("op-conductor-0"), node_with_el_rpc("op-conductor-1")],
            vec![status("op-conductor-0", true, false)],
        );

        let err = ensure_transfer_leader_el_readiness(&snapshot, Some("op-conductor-1"))
            .expect_err("missing target status record should block leadership transfer");

        assert!(matches!(
            err,
            ConductorCommandError::ExecutionLayerStatusUnavailable { node, field }
                if node == "op-conductor-1" && field == "status"
        ));
    }

    #[test]
    fn transfer_leader_allows_caught_up_target_el() {
        let snapshot = snapshot(
            vec![node_with_el_rpc("op-conductor-0"), node_with_el_rpc("op-conductor-1")],
            vec![status("op-conductor-0", true, false), status("op-conductor-1", false, false)],
        );

        ensure_transfer_leader_el_readiness(&snapshot, Some("op-conductor-1"))
            .expect("caught-up target EL should allow leadership transfer");
    }

    #[test]
    fn transfer_leader_allows_genesis_target_el() {
        let mut leader_status = status("op-conductor-0", true, false);
        leader_status.unsafe_l2_block = Some(0);
        let mut target_status = status("op-conductor-1", false, false);
        target_status.el_block = Some(0);
        let snapshot = snapshot(
            vec![node_with_el_rpc("op-conductor-0"), node_with_el_rpc("op-conductor-1")],
            vec![leader_status, target_status],
        );

        ensure_transfer_leader_el_readiness(&snapshot, Some("op-conductor-1"))
            .expect("genesis target EL should allow leadership transfer");
    }

    #[test]
    fn transfer_leader_allows_current_leader_target_without_el_check() {
        let mut leader_status = status("op-conductor-0", true, false);
        leader_status.el_syncing = Some(true);
        leader_status.el_block = Some(0);
        leader_status.unsafe_l2_block = None;
        let snapshot = snapshot(
            vec![node_with_el_rpc("op-conductor-0"), node_with_el_rpc("op-conductor-1")],
            vec![leader_status, status("op-conductor-1", false, false)],
        );

        ensure_transfer_leader_el_readiness(&snapshot, Some("op-conductor-0"))
            .expect("target already leader should not need replacement EL readiness");
    }

    #[test]
    fn transfer_leader_without_target_rejects_any_configured_candidate_behind() {
        let mut ready_target = status("op-conductor-1", false, false);
        ready_target.el_block = Some(10);
        let mut behind_target = status("op-conductor-2", false, false);
        behind_target.el_block = Some(9);
        let snapshot = snapshot(
            vec![
                node_with_el_rpc("op-conductor-0"),
                node_with_el_rpc("op-conductor-1"),
                node_with_el_rpc("op-conductor-2"),
            ],
            vec![status("op-conductor-0", true, false), ready_target, behind_target],
        );

        let err = ensure_transfer_leader_el_readiness(&snapshot, None).expect_err(
            "untargeted transfer should reject any candidate that op-conductor may pick",
        );

        assert!(matches!(
            err,
            ConductorCommandError::ExecutionLayerBehind {
                node,
                el_block,
                required_l2_block,
            } if node == "op-conductor-2" && el_block == 9 && required_l2_block == 10
        ));
    }

    #[test]
    fn transfer_leader_without_target_skips_candidates_without_el_rpc() {
        let mut behind_target = status("op-conductor-1", false, false);
        behind_target.el_block = Some(9);
        let snapshot = snapshot(
            vec![node_with_el_rpc("op-conductor-0"), node("op-conductor-1")],
            vec![status("op-conductor-0", true, false), behind_target],
        );

        ensure_transfer_leader_el_readiness(&snapshot, None)
            .expect("candidates without EL RPC are legacy configs and should not block preflight");
    }

    #[test]
    fn status_json_derives_leader_and_paused_summary() {
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0"), node("op-conductor-1")],
            statuses: vec![
                status("op-conductor-0", true, false),
                status("op-conductor-1", false, true),
            ],
            membership: None,
            membership_error: None,
            discovered: false,
        };

        let value =
            serde_json::to_value(ConductorStatusJson::from_snapshot("devnet", &snapshot)).unwrap();

        assert_eq!(value["leader"], "op-conductor-0");
        assert_eq!(value["paused"], json!({"known": 2, "paused": 1}));
        assert_eq!(value["nodes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn compact_status_distinguishes_conductor_and_sequencer_activity() {
        let node = node("op-conductor-0");
        let mut node_status = status("op-conductor-0", true, true);
        node_status.conductor_active = Some(false);
        node_status.conductor_paused = Some(true);
        node_status.sequencer_active = Some(true);

        let compact =
            ConductorNodeJson::from_node_status(&node, Some(&node_status), false).compact_status();

        assert!(compact.contains("conductor_active=false"));
        assert!(compact.contains("conductor_paused=true"));
        assert!(compact.contains("sequencer_active=true"));
        assert!(compact.contains("sequencer_healthy=true"));
    }

    #[test]
    fn node_json_flattens_shared_metrics_at_top_level() {
        let value = serde_json::to_value(ConductorNodeJson::from_node_status(
            &node("op-conductor-0"),
            Some(&status("op-conductor-0", true, false)),
            false,
        ))
        .unwrap();

        assert_eq!(
            value,
            json!({
                "name": "op-conductor-0",
                "serverId": "op-conductor-0",
                "raftAddr": "op-conductor-0:5050",
                "conductorRpc": "http://127.0.0.1:6545/",
                "isLeader": true,
                "sequencerActive": true,
                "sequencerHealthy": true,
                "conductorPaused": false,
                "unsafeL2Block": 10,
                "unsafeL2Hash": B256::with_last_byte(1).to_string(),
                "safeL2Block": 8,
                "safeL2Hash": B256::with_last_byte(2).to_string(),
                "finalizedL2Block": 6,
                "currentL1Block": 100,
                "headL1Block": 101,
                "clPeerCount": 3,
                "elBlock": 10,
                "elSyncing": false,
                "elPeerCount": 4,
                "conductorActive": true,
                "conductorStopped": false,
                "suffrage": null,
                "discovered": false,
            })
        );
    }

    #[test]
    fn status_json_preserves_discovered_provenance_for_offline_nodes() {
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0")],
            statuses: Vec::new(),
            membership: None,
            membership_error: None,
            discovered: true,
        };

        let value =
            serde_json::to_value(ConductorStatusJson::from_snapshot("devnet", &snapshot)).unwrap();

        assert_eq!(value["nodes"][0]["discovered"], true);
    }

    #[test]
    fn status_json_includes_membership_error_when_lookup_fails() {
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0")],
            statuses: vec![status("op-conductor-0", true, false)],
            membership: None,
            membership_error: Some("membership request timed out".to_string()),
            discovered: false,
        };

        let value =
            serde_json::to_value(ConductorStatusJson::from_snapshot("devnet", &snapshot)).unwrap();

        assert_eq!(value["membershipError"], "membership request timed out");
    }

    #[test]
    fn find_node_reports_missing_name() {
        let nodes = vec![node("op-conductor-0")];

        let err = ConductorNodeConfig::find(&nodes, "op-conductor-1")
            .map_err(ConductorCommandError::from)
            .expect_err("missing node should error");

        assert!(matches!(
            err,
            ConductorCommandError::NodeLookup(NodeLookupError::MissingNode {
                requested_node,
                available_nodes,
            }) if requested_node == "op-conductor-1"
                && available_nodes == vec!["op-conductor-0".to_string()]
        ));
    }
}
