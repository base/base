//! Implementation of the `basectl conductor` command group.

use std::io::{self, Write};

use anyhow::{Result, anyhow, bail};
use basectl_cli::{
    ConductorClusterSnapshot, ConductorControl, ConductorFanoutReport, ConductorNodeConfig,
    ConductorNodeFailure, ConductorNodeStatus, ConductorSource, JsonOutput, KeyValueTable,
    MonitoringConfig,
};
use serde::Serialize;
use tracing::warn;
use url::Url;

use crate::{
    cli::{
        ConductorClusterActionArgs, ConductorCommands, ConductorLeaderArgs,
        ConductorNodeActionArgs, ConductorStatusArgs,
    },
    confirm::{confirm_or_abort, confirm_typed_or_abort},
};

#[derive(Debug, Clone, Copy)]
enum NodeActionKind {
    Pause,
    Unpause,
}

impl NodeActionKind {
    const fn action(self) -> ConductorAction {
        match self {
            Self::Pause => ConductorAction::Pause,
            Self::Unpause => ConductorAction::Unpause,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ClusterActionKind {
    PauseAll,
    UnpauseAll,
}

#[derive(Debug, Clone, Copy)]
enum ClusterNodeScope {
    CurrentRaftMembers,
    ConfiguredNodes,
}

impl ClusterNodeScope {
    const fn description(self) -> &'static str {
        match self {
            Self::CurrentRaftMembers => "current raft members",
            Self::ConfiguredNodes => "configured conductors",
        }
    }
}

impl ClusterActionKind {
    const fn action(self) -> ConductorAction {
        match self {
            Self::PauseAll => ConductorAction::PauseAll,
            Self::UnpauseAll => ConductorAction::UnpauseAll,
        }
    }
}

/// Runs the `basectl conductor` command group.
pub(crate) async fn run(
    config: MonitoringConfig,
    conductor_rpc: Option<Url>,
    command: ConductorCommands,
) -> Result<()> {
    let source = resolve_source(&config, conductor_rpc)?;
    match command {
        ConductorCommands::Status(args) => run_status(config, source, args).await,
        ConductorCommands::TransferLeader(args) => run_transfer_leader(config, source, args).await,
        ConductorCommands::Pause(args) => {
            run_node_action(config, source, args, NodeActionKind::Pause).await
        }
        ConductorCommands::Unpause(args) => {
            run_node_action(config, source, args, NodeActionKind::Unpause).await
        }
        ConductorCommands::PauseAll(args) => {
            run_cluster_action(config, source, args, ClusterActionKind::PauseAll).await
        }
        ConductorCommands::UnpauseAll(args) => {
            run_cluster_action(config, source, args, ClusterActionKind::UnpauseAll).await
        }
    }
}

async fn run_status(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorStatusArgs,
) -> Result<()> {
    let snapshot = ConductorControl::snapshot(source).await?;
    let status = ConductorStatusJson::from_snapshot(&config.name, &snapshot);
    if args.json {
        JsonOutput::print(&status)?;
    } else {
        print_status_pretty(&status)?;
    }
    Ok(())
}

async fn run_transfer_leader(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorLeaderArgs,
) -> Result<()> {
    let nodes = current_nodes_for_action(&source).await?;
    if let Some(target) = args.target.as_deref() {
        // Validate before prompting so a typo does not ask for confirmation and only
        // fail after the operator already answered yes.
        find_node(&nodes, target)?;
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
    if !confirm_or_abort(&prompt, args.yes)? {
        return Ok(());
    }

    let message = ConductorControl::transfer_leader(&nodes, args.target.as_deref()).await?;
    print_single_action(
        &ConductorActionJson::single(
            &config.name,
            ConductorAction::TransferLeader,
            args.target,
            message,
        ),
        args.json,
    )
}

async fn run_node_action(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorNodeActionArgs,
    action: NodeActionKind,
) -> Result<()> {
    let nodes = current_nodes_for_action(&source).await?;
    let node = find_node(&nodes, &args.node)?;
    let json_action = action.action();
    let prompt = match action {
        NodeActionKind::Pause => {
            format!(
                "Pause conductor control loop on {} ({})? [y/N] ",
                node.name, node.conductor_rpc
            )
        }
        NodeActionKind::Unpause => {
            format!(
                "Unpause conductor control loop on {} ({})? [y/N] ",
                node.name, node.conductor_rpc
            )
        }
    };
    if !confirm_or_abort(&prompt, args.yes)? {
        return Ok(());
    }

    let message = match action {
        NodeActionKind::Pause => ConductorControl::pause_node(node).await?,
        NodeActionKind::Unpause => ConductorControl::resume_node(node).await?,
    };
    print_single_action(
        &ConductorActionJson::single(&config.name, json_action, Some(node.name.clone()), message),
        args.json,
    )
}

async fn run_cluster_action(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorClusterActionArgs,
    action: ClusterActionKind,
) -> Result<()> {
    let (nodes, node_scope) = current_nodes_for_cluster_action(&source).await?;
    let names = nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>().join(", ");
    let json_action = action.action();
    let prompt = match action {
        ClusterActionKind::PauseAll => format!(
            "Type {} to pause conductor control loop on all {} {} ({}): ",
            config.name,
            nodes.len(),
            node_scope.description(),
            names
        ),
        ClusterActionKind::UnpauseAll => format!(
            "Type {} to unpause conductor control loop on all {} {} ({}): ",
            config.name,
            nodes.len(),
            node_scope.description(),
            names
        ),
    };
    if !confirm_typed_or_abort(&prompt, &config.name, args.yes)? {
        return Ok(());
    }

    let report = match action {
        ClusterActionKind::PauseAll => ConductorControl::pause_all(nodes).await,
        ClusterActionKind::UnpauseAll => ConductorControl::resume_all(nodes).await,
    };
    print_fanout_action(
        &ConductorFanoutJson::from_report(&config.name, json_action, &report),
        args.json,
    )?;
    if report.is_success() { Ok(()) } else { bail!(report.summary(json_action.past_tense())) }
}

fn resolve_source(
    config: &MonitoringConfig,
    conductor_rpc: Option<Url>,
) -> Result<ConductorSource> {
    config.conductor_source(conductor_rpc).ok_or_else(|| {
        anyhow!(
            "conductor commands need conductor config or a bootstrap RPC URL for '{}'. Set `conductors` or `discovery.bootstrap_rpc` in config, or pass `--conductor-rpc <url>`.",
            config.name
        )
    })
}

async fn current_nodes_for_action(source: &ConductorSource) -> Result<Vec<ConductorNodeConfig>> {
    match source {
        ConductorSource::Static(nodes) => Ok(nodes.clone()),
        ConductorSource::Discover { .. } => current_nodes_for_all(source).await,
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
                Err(err) => {
                    warn!(
                        error = %err,
                        "membership lookup failed for static conductor source; falling back to configured node list"
                    );
                    Ok((nodes.clone(), ClusterNodeScope::ConfiguredNodes))
                }
            }
        }
        ConductorSource::Discover { .. } => {
            Ok((current_nodes_for_all(source).await?, ClusterNodeScope::CurrentRaftMembers))
        }
    }
}

async fn current_nodes_for_all(source: &ConductorSource) -> Result<Vec<ConductorNodeConfig>> {
    let membership = ConductorControl::current_membership(source).await?;
    ConductorControl::nodes_from_membership(source, &membership)
}

fn find_node<'a>(nodes: &'a [ConductorNodeConfig], name: &str) -> Result<&'a ConductorNodeConfig> {
    nodes.iter().find(|node| node.name == name).ok_or_else(|| {
        anyhow!(
            "conductor node {name} not found. Available nodes: {}",
            nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>().join(", ")
        )
    })
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

fn print_single_action(action: &ConductorActionJson, json: bool) -> Result<()> {
    if json {
        JsonOutput::print(action)?;
    } else {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "OK {}", action.message)?;
    }
    Ok(())
}

fn print_fanout_action(action: &ConductorFanoutJson, json: bool) -> Result<()> {
    if json {
        JsonOutput::print(action)?;
    } else {
        let mut stdout = io::stdout().lock();
        if action.total == 0 {
            writeln!(stdout, "WARN no conductor nodes to {}", action.action.infinitive())?;
        } else if action.failures.is_empty() {
            writeln!(
                stdout,
                "OK conductor {} on {}/{} nodes",
                action.action.past_tense(),
                action.successes.len(),
                action.total
            )?;
        } else {
            let failures = action
                .failures
                .iter()
                .map(|f| format!("{}: {}", f.name, f.error))
                .collect::<Vec<_>>()
                .join("; ");
            writeln!(
                stdout,
                "WARN conductor {} on {}/{} nodes; failures: {failures}",
                action.action.past_tense(),
                action.successes.len(),
                action.total
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize)]
enum ConductorAction {
    #[serde(rename = "transferLeader")]
    TransferLeader,
    #[serde(rename = "pause")]
    Pause,
    #[serde(rename = "unpause")]
    Unpause,
    #[serde(rename = "pauseAll")]
    PauseAll,
    #[serde(rename = "unpauseAll")]
    UnpauseAll,
}

impl ConductorAction {
    const fn past_tense(self) -> &'static str {
        match self {
            Self::TransferLeader => "transferred",
            Self::Pause | Self::PauseAll => "paused",
            Self::Unpause | Self::UnpauseAll => "resumed",
        }
    }

    const fn infinitive(self) -> &'static str {
        match self {
            Self::TransferLeader => "transfer",
            Self::Pause | Self::PauseAll => "pause",
            Self::Unpause | Self::UnpauseAll => "resume",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConductorActionJson {
    network: String,
    action: ConductorAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    message: String,
}

impl ConductorActionJson {
    fn single(
        network: &str,
        action: ConductorAction,
        target: Option<String>,
        message: String,
    ) -> Self {
        Self { network: network.to_string(), action, target, message }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConductorFanoutJson {
    network: String,
    action: ConductorAction,
    total: usize,
    successes: Vec<String>,
    failures: Vec<ConductorFailureJson>,
}

impl ConductorFanoutJson {
    fn from_report(network: &str, action: ConductorAction, report: &ConductorFanoutReport) -> Self {
        Self {
            network: network.to_string(),
            action,
            total: report.total,
            successes: report.successes.clone(),
            failures: report.failures.iter().map(ConductorFailureJson::from_failure).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConductorFailureJson {
    name: String,
    error: String,
}

impl ConductorFailureJson {
    fn from_failure(failure: &ConductorNodeFailure) -> Self {
        Self { name: failure.name.clone(), error: failure.error.clone() }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConductorStatusJson {
    network: String,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    membership_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    membership_error: Option<String>,
    leader: Option<String>,
    paused: PausedSummaryJson,
    nodes: Vec<ConductorNodeJson>,
}

impl ConductorStatusJson {
    fn from_snapshot(network: &str, snapshot: &ConductorClusterSnapshot) -> Self {
        let source = if snapshot.discovered { "discovered" } else { "static" };
        let nodes = snapshot
            .nodes
            .iter()
            .map(|node| {
                let status = snapshot.statuses.iter().find(|status| status.name == node.name);
                ConductorNodeJson::from_node_status(node, status)
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

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct PausedSummaryJson {
    known: usize,
    paused: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConductorNodeJson {
    name: String,
    server_id: String,
    raft_addr: String,
    conductor_rpc: String,
    is_leader: Option<bool>,
    conductor_active: Option<bool>,
    conductor_paused: Option<bool>,
    conductor_stopped: Option<bool>,
    sequencer_healthy: Option<bool>,
    sequencer_active: Option<bool>,
    unsafe_l2_block: Option<u64>,
    unsafe_l2_hash: Option<String>,
    safe_l2_block: Option<u64>,
    safe_l2_hash: Option<String>,
    finalized_l2_block: Option<u64>,
    current_l1_block: Option<u64>,
    head_l1_block: Option<u64>,
    cl_peer_count: Option<u32>,
    el_block: Option<u64>,
    el_syncing: Option<bool>,
    el_peer_count: Option<u32>,
    suffrage: Option<String>,
    discovered: bool,
}

impl ConductorNodeJson {
    fn from_node_status(node: &ConductorNodeConfig, status: Option<&ConductorNodeStatus>) -> Self {
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
            discovered: status.is_some_and(|status| status.discovered),
        }
    }

    fn compact_status(&self) -> String {
        format!(
            "leader={} active={} paused={} stopped={} healthy={} unsafe={} safe={} cl_peers={} el_peers={}",
            fmt_bool(self.is_leader),
            fmt_bool(self.conductor_active),
            fmt_bool(self.conductor_paused),
            fmt_bool(self.conductor_stopped),
            fmt_bool(self.sequencer_healthy),
            fmt_u64(self.unsafe_l2_block),
            fmt_u64(self.safe_l2_block),
            fmt_u32(self.cl_peer_count),
            fmt_u32(self.el_peer_count),
        )
    }
}

const fn fmt_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

fn fmt_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

fn fmt_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use basectl_cli::{ConductorClusterSnapshot, ConductorNodeConfig, ConductorNodeFailure};
    use serde_json::json;
    use url::Url;

    use super::{
        ConductorAction, ConductorActionJson, ConductorFanoutJson, ConductorStatusJson, find_node,
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

    fn status(name: &str, leader: bool, paused: bool) -> basectl_cli::ConductorNodeStatus {
        basectl_cli::ConductorNodeStatus {
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
            ConductorAction::TransferLeader,
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
        let report = basectl_cli::ConductorFanoutReport {
            total: 2,
            successes: vec!["op-conductor-0".to_string()],
            failures: vec![ConductorNodeFailure {
                name: "op-conductor-1".to_string(),
                error: "request timed out".to_string(),
            }],
        };

        let value = serde_json::to_value(ConductorFanoutJson::from_report(
            "devnet",
            ConductorAction::PauseAll,
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

        let err = find_node(&nodes, "op-conductor-1").expect_err("missing node should error");

        assert!(err.to_string().contains("op-conductor-1"));
        assert!(err.to_string().contains("op-conductor-0"));
    }
}
