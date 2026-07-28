//! Implementation of the `basectl conductor` command group.

use std::io::{self, Write};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use tracing::warn;
use url::Url;

use crate::{
    CommandOutcome, ConductorClusterSnapshot, ConductorCommandError, ConductorControl,
    ConductorFanoutReport, ConductorNodeConfig, ConductorNodeStatus, ConductorSource, Confirm,
    JsonOutput, KeyValueTable, MonitoringConfig,
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
            ConductorCommands::Pause(args) => run_node_action(config, source, args, true).await,
            ConductorCommands::Unpause(args) => run_node_action(config, source, args, false).await,
            ConductorCommands::PauseAll(args) => {
                run_cluster_action(config, source, args, true).await
            }
            ConductorCommands::UnpauseAll(args) => {
                run_cluster_action(config, source, args, false).await
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
    if args.json {
        JsonOutput::print(&conductor_status_json(&config.name, &snapshot))?;
    } else {
        print_status_pretty(&config.name, &snapshot)?;
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
    print_single_action(&config.name, "transferLeader", args.target, &message, args.json)?;
    Ok(CommandOutcome::Success)
}

async fn run_node_action(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorNodeActionArgs,
    pause: bool,
) -> Result<CommandOutcome> {
    let nodes = current_nodes_for_action(&source).await?;
    let node =
        ConductorNodeConfig::find(&nodes, &args.node).map_err(ConductorCommandError::from)?;
    let prompt = format!(
        "{} conductor control loop on {} ({})? [y/N] ",
        if pause { "Pause" } else { "Unpause" },
        node.name,
        node.conductor_rpc
    );
    if !Confirm::prompt_or_abort(&prompt, args.yes)? {
        return Ok(CommandOutcome::Success);
    }

    let message = if pause {
        ConductorControl::pause_node(node).await?
    } else {
        ConductorControl::resume_node(node).await?
    };
    print_single_action(
        &config.name,
        if pause { "pause" } else { "unpause" },
        Some(node.name.clone()),
        &message,
        args.json,
    )?;
    Ok(CommandOutcome::Success)
}

async fn run_cluster_action(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorClusterActionArgs,
    pause: bool,
) -> Result<CommandOutcome> {
    let (nodes, node_scope) = current_nodes_for_cluster_action(&source).await?;
    let names = nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>().join(", ");
    let verb = if pause { "pause" } else { "unpause" };
    let prompt = format!(
        "Type {} to {} conductor control loop on all {} {} ({}): ",
        config.name,
        verb,
        nodes.len(),
        node_scope,
        names
    );
    if !Confirm::typed_or_abort(&prompt, &config.name, args.yes)? {
        return Ok(CommandOutcome::Success);
    }

    let report = if pause {
        ConductorControl::pause_all(nodes).await
    } else {
        ConductorControl::resume_all(nodes).await
    };
    print_fanout_action(
        &config.name,
        if pause { "pauseAll" } else { "unpauseAll" },
        verb,
        if pause { "paused" } else { "resumed" },
        &report,
        args.json,
    )?;
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
) -> Result<(Vec<ConductorNodeConfig>, &'static str)> {
    match source {
        // Prefer live membership when it is reachable so stale static entries are
        // not mutated unnecessarily, but fall back to the configured list so a
        // temporary membership RPC outage does not block bulk actions entirely.
        ConductorSource::Static(nodes) => {
            match ConductorControl::current_membership(source).await {
                Ok(membership) => Ok((
                    ConductorControl::nodes_from_membership(source, &membership)?,
                    "current raft members",
                )),
                Err(error) => {
                    warn!(
                        error = %error,
                        "membership lookup failed for static conductor source; falling back to configured node list"
                    );
                    Ok((nodes.clone(), "configured conductors"))
                }
            }
        }
        ConductorSource::Discover { .. } => {
            let membership = ConductorControl::current_membership(source).await?;
            let nodes = ConductorControl::nodes_from_membership(source, &membership)?;
            Ok((nodes, "current raft members"))
        }
    }
}

const fn fanout_requires_failure_exit(report: &ConductorFanoutReport) -> bool {
    !report.is_success()
}

fn print_status_pretty(network: &str, snapshot: &ConductorClusterSnapshot) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", network)
        .row("source", if snapshot.discovered { "discovered" } else { "static" })
        .row("nodes", snapshot.nodes.len().to_string());
    if let Some(version) = snapshot.membership.as_ref().map(|membership| membership.version) {
        table.row("membership_version", version.to_string());
    }
    if let Some(error) = &snapshot.membership_error {
        table.row("membership_error", error);
    }
    let leader = snapshot
        .nodes
        .iter()
        .find(|node| {
            snapshot
                .statuses
                .iter()
                .find(|status| status.name == node.name)
                .is_some_and(|status| status.is_leader == Some(true))
        })
        .map(|node| node.name.as_str())
        .unwrap_or("unknown");
    let known_paused = snapshot
        .nodes
        .iter()
        .filter_map(|node| snapshot.statuses.iter().find(|status| status.name == node.name))
        .filter(|status| status.conductor_paused.is_some())
        .count();
    let paused = snapshot
        .nodes
        .iter()
        .filter_map(|node| snapshot.statuses.iter().find(|status| status.name == node.name))
        .filter(|status| status.conductor_paused == Some(true))
        .count();
    table.row("leader", leader);
    table.row("paused", format!("{paused}/{known_paused} known paused"));
    for node in &snapshot.nodes {
        let status = snapshot.statuses.iter().find(|status| status.name == node.name);
        table.row(format!("node.{}", node.name), conductor_node_compact_status(status));
    }
    table.print()?;
    Ok(())
}

fn print_single_action(
    network: &str,
    action: &str,
    target: Option<String>,
    message: &str,
    json_output: bool,
) -> Result<()> {
    if json_output {
        JsonOutput::print(&conductor_action_json(network, action, target, message))?;
    } else {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "OK {message}")?;
    }
    Ok(())
}

fn print_fanout_action(
    network: &str,
    action: &str,
    infinitive: &str,
    past_tense: &str,
    report: &ConductorFanoutReport,
    json_output: bool,
) -> Result<()> {
    if json_output {
        JsonOutput::print(&conductor_fanout_json(network, action, report))?;
    } else {
        let mut stdout = io::stdout().lock();
        if report.total == 0 {
            writeln!(stdout, "WARN no conductor nodes to {infinitive}")?;
        } else if report.failures.is_empty() {
            writeln!(
                stdout,
                "OK conductor {} on {}/{} nodes",
                past_tense,
                report.successes.len(),
                report.total
            )?;
        } else {
            let failures = report
                .failures
                .iter()
                .map(|f| format!("{}: {}", f.name, f.error))
                .collect::<Vec<_>>()
                .join("; ");
            writeln!(
                stdout,
                "WARN conductor {} on {}/{} nodes; failures: {failures}",
                past_tense,
                report.successes.len(),
                report.total
            )?;
        }
    }
    Ok(())
}

fn conductor_action_json(
    network: &str,
    action: &str,
    target: Option<String>,
    message: &str,
) -> Value {
    let mut value = json!({
        "network": network,
        "action": action,
        "message": message,
    });
    if let Some(target) = target {
        value["target"] = json!(target);
    }
    value
}

fn conductor_fanout_json(network: &str, action: &str, report: &ConductorFanoutReport) -> Value {
    json!({
        "network": network,
        "action": action,
        "total": report.total,
        "successes": report.successes,
        "failures": report.failures.iter().map(|failure| json!({
            "name": failure.name,
            "error": failure.error,
        })).collect::<Vec<_>>(),
    })
}

fn conductor_status_json(network: &str, snapshot: &ConductorClusterSnapshot) -> Value {
    let nodes = snapshot
        .nodes
        .iter()
        .map(|node| {
            let status = snapshot.statuses.iter().find(|status| status.name == node.name);
            conductor_node_json(node, status, snapshot.discovered)
        })
        .collect::<Vec<_>>();
    let leader = snapshot
        .nodes
        .iter()
        .find(|node| {
            snapshot
                .statuses
                .iter()
                .find(|status| status.name == node.name)
                .is_some_and(|status| status.is_leader == Some(true))
        })
        .map(|node| node.name.clone());
    let mut value = json!({
        "network": network,
        "source": if snapshot.discovered { "discovered" } else { "static" },
        "leader": leader,
        "paused": {
            "known": snapshot.nodes.iter()
                .filter_map(|node| snapshot.statuses.iter().find(|status| status.name == node.name))
                .filter(|status| status.conductor_paused.is_some())
                .count(),
            "paused": snapshot.nodes.iter()
                .filter_map(|node| snapshot.statuses.iter().find(|status| status.name == node.name))
                .filter(|status| status.conductor_paused == Some(true))
                .count(),
        },
        "nodes": nodes,
    });
    if let Some(version) = snapshot.membership.as_ref().map(|membership| membership.version) {
        value["membershipVersion"] = json!(version);
    }
    if let Some(error) = &snapshot.membership_error {
        value["membershipError"] = json!(error);
    }
    value
}

fn conductor_node_json(
    node: &ConductorNodeConfig,
    status: Option<&ConductorNodeStatus>,
    discovered: bool,
) -> Value {
    json!({
        "name": node.name,
        "serverId": node.server_id,
        "raftAddr": node.raft_addr,
        "conductorRpc": node.conductor_rpc.to_string(),
        "isLeader": status.and_then(|status| status.is_leader),
        "conductorActive": status.and_then(|status| status.conductor_active),
        "conductorPaused": status.and_then(|status| status.conductor_paused),
        "conductorStopped": status.and_then(|status| status.conductor_stopped),
        "sequencerHealthy": status.and_then(|status| status.sequencer_healthy),
        "sequencerActive": status.and_then(|status| status.sequencer_active),
        "unsafeL2Block": status.and_then(|status| status.unsafe_l2_block),
        "unsafeL2Hash": status.and_then(|status| status.unsafe_l2_hash).map(|hash| hash.to_string()),
        "safeL2Block": status.and_then(|status| status.safe_l2_block),
        "safeL2Hash": status.and_then(|status| status.safe_l2_hash).map(|hash| hash.to_string()),
        "finalizedL2Block": status.and_then(|status| status.finalized_l2_block),
        "currentL1Block": status.and_then(|status| status.current_l1_block),
        "headL1Block": status.and_then(|status| status.head_l1_block),
        "clPeerCount": status.and_then(|status| status.cl_peer_count),
        "elBlock": status.and_then(|status| status.el_block),
        "elSyncing": status.and_then(|status| status.el_syncing),
        "elPeerCount": status.and_then(|status| status.el_peer_count),
        "suffrage": status.and_then(|status| {
            status.suffrage.map(|suffrage| format!("{suffrage:?}").to_ascii_lowercase())
        }),
        "discovered": discovered,
    })
}

fn conductor_node_compact_status(status: Option<&ConductorNodeStatus>) -> String {
    let boolean = |value| match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    };
    let u64 =
        |value: Option<u64>| value.map_or_else(|| "unknown".to_string(), |value| value.to_string());
    let u32 =
        |value: Option<u32>| value.map_or_else(|| "unknown".to_string(), |value| value.to_string());
    format!(
        "leader={} conductor_active={} conductor_paused={} conductor_stopped={} sequencer_active={} sequencer_healthy={} unsafe={} safe={} cl_peers={} el_peers={}",
        boolean(status.and_then(|status| status.is_leader)),
        boolean(status.and_then(|status| status.conductor_active)),
        boolean(status.and_then(|status| status.conductor_paused)),
        boolean(status.and_then(|status| status.conductor_stopped)),
        boolean(status.and_then(|status| status.sequencer_active)),
        boolean(status.and_then(|status| status.sequencer_healthy)),
        u64(status.and_then(|status| status.unsafe_l2_block)),
        u64(status.and_then(|status| status.safe_l2_block)),
        u32(status.and_then(|status| status.cl_peer_count)),
        u32(status.and_then(|status| status.el_peer_count)),
    )
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use serde_json::json;
    use url::Url;

    use super::{
        conductor_action_json, conductor_fanout_json, conductor_node_compact_status,
        conductor_status_json, fanout_requires_failure_exit,
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
        let value = conductor_action_json(
            "devnet",
            "transferLeader",
            Some("op-conductor-1".to_string()),
            "leadership transferred to op-conductor-1",
        );

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

        let value = conductor_fanout_json("devnet", "pauseAll", &report);

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

        let value = conductor_status_json("devnet", &snapshot);

        assert_eq!(value["leader"], "op-conductor-0");
        assert_eq!(value["paused"], json!({"known": 2, "paused": 1}));
        assert_eq!(value["nodes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn compact_status_distinguishes_conductor_and_sequencer_activity() {
        let mut node_status = status("op-conductor-0", true, true);
        node_status.conductor_active = Some(false);
        node_status.conductor_paused = Some(true);
        node_status.sequencer_active = Some(true);

        let compact = conductor_node_compact_status(Some(&node_status));

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

        let value = conductor_status_json("devnet", &snapshot);

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

        let value = conductor_status_json("devnet", &snapshot);

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
