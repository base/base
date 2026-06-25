//! Implementation of the `basectl conductor` command group.

use std::io::{self, Write};

use anyhow::Result;
use basectl_cli::{
    ConductorClusterSnapshot, ConductorCommandError, ConductorControl, ConductorFanoutReport,
    ConductorNodeConfig, ConductorNodeFailure, ConductorNodeStatus, ConductorSource, JsonOutput,
    KeyValueTable, MonitoringConfig,
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
    helpers::{
        CommandOutcome, find_conductor_node, fmt_bool, fmt_u32, fmt_u64, resolve_conductor_source,
    },
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

    const fn verb(self) -> &'static str {
        match self {
            Self::Pause => "Pause",
            Self::Unpause => "Unpause",
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

    const fn verb(self) -> &'static str {
        match self {
            Self::PauseAll => "pause",
            Self::UnpauseAll => "unpause",
        }
    }
}

/// Runs the `basectl conductor` command group.
pub(crate) async fn run(
    config: MonitoringConfig,
    conductor_rpc: Option<Url>,
    command: ConductorCommands,
) -> Result<CommandOutcome> {
    let source =
        resolve_conductor_source(&config, conductor_rpc).map_err(ConductorCommandError::from)?;
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
        find_conductor_node(nodes, target).map_err(ConductorCommandError::from)?;
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
    if !confirm_or_abort(&prompt, args.yes)? {
        return Ok(CommandOutcome::Success);
    }

    let message = ConductorControl::transfer_leader(nodes, args.target.as_deref()).await?;
    print_single_action(
        &ConductorActionJson::single(
            &config.name,
            ConductorAction::TransferLeader,
            args.target,
            message,
        ),
        args.json,
    )?;
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
    let required_l2_block = leader_status.unsafe_l2_block.ok_or_else(|| {
        ConductorCommandError::ExecutionLayerStatusUnavailable {
            node: leader_status.name.clone(),
            field: "unsafe_l2_block",
        }
    })?;

    if let Some(target) = target {
        if target == leader_status.name {
            return Ok(());
        }
        let target_node =
            find_conductor_node(&snapshot.nodes, target).map_err(ConductorCommandError::from)?;
        if target_node.el_rpc.is_none() {
            return Ok(());
        }
        let target_status = snapshot_node_status(snapshot, target);
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
        ensure_node_el_ready_for_leadership_transfer(status, &node.name, required_l2_block)?;
    }
    Ok(())
}

fn ensure_node_el_ready_for_leadership_transfer(
    status: Option<&ConductorNodeStatus>,
    node: &str,
    required_l2_block: u64,
) -> Result<(), ConductorCommandError> {
    let status = status.ok_or_else(|| ConductorCommandError::ExecutionLayerStatusUnavailable {
        node: node.to_string(),
        field: "el_syncing",
    })?;
    match status.el_syncing {
        Some(false) => {}
        Some(true) => {
            return Err(ConductorCommandError::ExecutionLayerSyncing {
                node: node.to_string(),
                el_block: status.el_block,
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

    let el_block =
        status.el_block.ok_or_else(|| ConductorCommandError::ExecutionLayerStatusUnavailable {
            node: node.to_string(),
            field: "el_block",
        })?;
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
    action: NodeActionKind,
) -> Result<CommandOutcome> {
    let nodes = current_nodes_for_action(&source).await?;
    let node = find_conductor_node(&nodes, &args.node).map_err(ConductorCommandError::from)?;
    let json_action = action.action();
    let prompt = format!(
        "{} conductor control loop on {} ({})? [y/N] ",
        action.verb(),
        node.name,
        node.conductor_rpc
    );
    if !confirm_or_abort(&prompt, args.yes)? {
        return Ok(CommandOutcome::Success);
    }

    let message = match action {
        NodeActionKind::Pause => ConductorControl::pause_node(node).await?,
        NodeActionKind::Unpause => ConductorControl::resume_node(node).await?,
    };
    print_single_action(
        &ConductorActionJson::single(&config.name, json_action, Some(node.name.clone()), message),
        args.json,
    )?;
    Ok(CommandOutcome::Success)
}

async fn run_cluster_action(
    config: MonitoringConfig,
    source: ConductorSource,
    args: ConductorClusterActionArgs,
    action: ClusterActionKind,
) -> Result<CommandOutcome> {
    let (nodes, node_scope) = current_nodes_for_cluster_action(&source).await?;
    let names = nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>().join(", ");
    let json_action = action.action();
    let prompt = format!(
        "Type {} to {} conductor control loop on all {} {} ({}): ",
        config.name,
        action.verb(),
        nodes.len(),
        node_scope.description(),
        names
    );
    if !confirm_typed_or_abort(&prompt, &config.name, args.yes)? {
        return Ok(CommandOutcome::Success);
    }

    let report = match action {
        ClusterActionKind::PauseAll => ConductorControl::pause_all(nodes).await,
        ClusterActionKind::UnpauseAll => ConductorControl::resume_all(nodes).await,
    };
    print_fanout_action(
        &ConductorFanoutJson::from_report(&config.name, json_action, &report),
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
    fn from_node_status(
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

    fn compact_status(&self) -> String {
        format!(
            "leader={} conductor_active={} conductor_paused={} conductor_stopped={} sequencer_active={} sequencer_healthy={} unsafe={} safe={} cl_peers={} el_peers={}",
            fmt_bool(self.is_leader),
            fmt_bool(self.conductor_active),
            fmt_bool(self.conductor_paused),
            fmt_bool(self.conductor_stopped),
            fmt_bool(self.sequencer_active),
            fmt_bool(self.sequencer_healthy),
            fmt_u64(self.unsafe_l2_block),
            fmt_u64(self.safe_l2_block),
            fmt_u32(self.cl_peer_count),
            fmt_u32(self.el_peer_count),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use basectl_cli::{
        ConductorClusterSnapshot, ConductorCommandError, ConductorNodeConfig, ConductorNodeFailure,
    };
    use serde_json::json;
    use url::Url;

    use super::{
        ConductorAction, ConductorActionJson, ConductorFanoutJson, ConductorNodeJson,
        ConductorStatusJson, ensure_transfer_leader_el_readiness, fanout_requires_failure_exit,
    };
    use crate::helpers::{CommandOutcome, find_conductor_node};

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

    fn snapshot(
        nodes: Vec<ConductorNodeConfig>,
        statuses: Vec<basectl_cli::ConductorNodeStatus>,
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
    fn fanout_failure_exit_matches_report_status() {
        let success = basectl_cli::ConductorFanoutReport {
            total: 2,
            successes: vec!["op-conductor-0".to_string(), "op-conductor-1".to_string()],
            failures: Vec::new(),
        };
        let partial_failure = basectl_cli::ConductorFanoutReport {
            total: 2,
            successes: vec!["op-conductor-0".to_string()],
            failures: vec![ConductorNodeFailure {
                name: "op-conductor-1".to_string(),
                error: "request timed out".to_string(),
            }],
        };
        let empty = basectl_cli::ConductorFanoutReport {
            total: 0,
            successes: Vec::new(),
            failures: Vec::new(),
        };

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
                && el_block == Some(9)
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

        let err = find_conductor_node(&nodes, "op-conductor-1")
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
