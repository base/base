//! Implementation of the `basectl conductor` command group.

use std::io::{self, Write};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;
use tracing::warn;
use url::Url;

use crate::{
    CommandOutcome, ConductorClusterSnapshot, ConductorCommandError, ConductorControl,
    ConductorFanoutReport, ConductorNodeConfig, ConductorNodeFailure, ConductorNodeStatus,
    ConductorSource, Confirm, JsonOutput, KeyValueTable, MonitoringConfig, OptionalValue,
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
    /// Pause the control loop on every current raft member, falling back to
    /// the configured conductor list if static membership lookup is unavailable.
    PauseAll(ConductorClusterActionArgs),
    /// Resume the control loop on every current raft member, falling back to
    /// the configured conductor list if static membership lookup is unavailable.
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
    /// Optional target node name.
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
    /// Conductor node name or discovered raft server ID.
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
    /// Emit a structured JSON action outcome instead of pretty text.
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

    /// Past tense used in fanout summaries.
    pub const fn past_tense(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Unpause => "resumed",
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
    let nodes = current_nodes_for_action(&source).await?;
    if let Some(target) = args.target.as_deref() {
        // Validate before prompting so a typo does not ask for confirmation and only
        // fail after the operator already answered yes.
        ConductorNodeConfig::find(&nodes, target).map_err(ConductorCommandError::from)?;
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

    let message = ConductorControl::transfer_leader(&nodes, args.target.as_deref()).await?;
    let outcome = ConductorActionJson::single(
        &config.name,
        ConductorActionName::TransferLeader,
        args.target,
        message,
    );
    JsonOutput::print_or_ok(&outcome, &outcome.message, args.json)?;
    Ok(CommandOutcome::Success)
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
    Ok(CommandOutcome::from_failures(fanout_requires_failure_exit(&report)))
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

const fn fanout_requires_failure_exit(report: &ConductorFanoutReport) -> bool {
    !report.is_success()
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
        writeln!(stdout, "{prefix} {}", report.summary(action.past_tense()))?;
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
        let leader =
            nodes.iter().find(|node| node.is_leader == Some(true)).map(|node| node.name.clone());
        let paused = PausedSummaryJson {
            known: nodes.iter().filter(|node| node.conductor_paused.is_some()).count(),
            paused: nodes.iter().filter(|node| node.conductor_paused == Some(true)).count(),
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
    /// Whether this node is leader.
    pub is_leader: Option<bool>,
    /// Whether the conductor is active.
    pub conductor_active: Option<bool>,
    /// Whether the conductor is paused.
    pub conductor_paused: Option<bool>,
    /// Whether the conductor is stopped.
    pub conductor_stopped: Option<bool>,
    /// Whether the sequencer is healthy.
    pub sequencer_healthy: Option<bool>,
    /// Whether the sequencer is active.
    pub sequencer_active: Option<bool>,
    /// Unsafe L2 block number.
    pub unsafe_l2_block: Option<u64>,
    /// Unsafe L2 block hash.
    pub unsafe_l2_hash: Option<String>,
    /// Safe L2 block number.
    pub safe_l2_block: Option<u64>,
    /// Safe L2 block hash.
    pub safe_l2_hash: Option<String>,
    /// Finalized L2 block number.
    pub finalized_l2_block: Option<u64>,
    /// Current L1 block number.
    pub current_l1_block: Option<u64>,
    /// Head L1 block number.
    pub head_l1_block: Option<u64>,
    /// Consensus-layer peer count.
    pub cl_peer_count: Option<u32>,
    /// Execution-layer block number.
    pub el_block: Option<u64>,
    /// Whether the execution layer is syncing.
    pub el_syncing: Option<bool>,
    /// Execution-layer peer count.
    pub el_peer_count: Option<u32>,
    /// Raft suffrage.
    pub suffrage: Option<String>,
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
            is_leader: status.and_then(|status| status.is_leader),
            conductor_active: status.and_then(|status| status.conductor_active),
            conductor_paused: status.and_then(|status| status.conductor_paused),
            conductor_stopped: status.and_then(|status| status.conductor_stopped),
            sequencer_healthy: status.and_then(|status| status.sequencer_healthy),
            sequencer_active: status.and_then(|status| status.sequencer_active),
            unsafe_l2_block: status.and_then(|status| status.unsafe_l2_block),
            unsafe_l2_hash: status
                .and_then(|status| status.unsafe_l2_hash)
                .map(|hash| hash.to_string()),
            safe_l2_block: status.and_then(|status| status.safe_l2_block),
            safe_l2_hash: status
                .and_then(|status| status.safe_l2_hash)
                .map(|hash| hash.to_string()),
            finalized_l2_block: status.and_then(|status| status.finalized_l2_block),
            current_l1_block: status.and_then(|status| status.current_l1_block),
            head_l1_block: status.and_then(|status| status.head_l1_block),
            cl_peer_count: status.and_then(|status| status.cl_peer_count),
            el_block: status.and_then(|status| status.el_block),
            el_syncing: status.and_then(|status| status.el_syncing),
            el_peer_count: status.and_then(|status| status.el_peer_count),
            suffrage: status.and_then(|status| {
                status.suffrage.map(|suffrage| format!("{suffrage:?}").to_ascii_lowercase())
            }),
            discovered,
        }
    }

    /// Renders a single-line status summary for pretty output.
    pub fn compact_status(&self) -> String {
        format!(
            "leader={} conductor_active={} conductor_paused={} conductor_stopped={} sequencer_active={} sequencer_healthy={} unsafe={} safe={} cl_peers={} el_peers={}",
            OptionalValue::boolean(self.is_leader),
            OptionalValue::boolean(self.conductor_active),
            OptionalValue::boolean(self.conductor_paused),
            OptionalValue::boolean(self.conductor_stopped),
            OptionalValue::boolean(self.sequencer_active),
            OptionalValue::boolean(self.sequencer_healthy),
            OptionalValue::u64(self.unsafe_l2_block),
            OptionalValue::u64(self.safe_l2_block),
            OptionalValue::u32(self.cl_peer_count),
            OptionalValue::u32(self.el_peer_count),
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
        ConductorNodeJson, ConductorStatusJson, fanout_requires_failure_exit,
    };
    use crate::{
        CommandOutcome, ConductorClusterSnapshot, ConductorCommandError, ConductorNodeConfig,
        ConductorNodeFailure,
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

        assert_eq!(
            CommandOutcome::from_failures(fanout_requires_failure_exit(&success)),
            CommandOutcome::Success
        );
        assert_eq!(
            CommandOutcome::from_failures(fanout_requires_failure_exit(&partial_failure)),
            CommandOutcome::HasFailures
        );
        assert_eq!(
            CommandOutcome::from_failures(fanout_requires_failure_exit(&empty)),
            CommandOutcome::HasFailures
        );
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
            ConductorCommandError::MissingNode {
                requested_node,
                available_nodes,
            } if requested_node == "op-conductor-1"
                && available_nodes == vec!["op-conductor-0".to_string()]
        ));
    }
}
