//! Implementation of the `basectl sequencer` command group.

use std::{
    future::Future,
    io::{self, Write},
    str::FromStr,
    time::Duration,
};

use alloy_primitives::B256;
use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    CommandOutcome, ConductorClusterSnapshot, ConductorControl, ConductorNodeConfig,
    ConductorNodeStatus, ConductorSource, Confirm, JsonOutput, KeyValueTable, MonitoringConfig,
    SequencerCommandError, StateConvergenceTimeoutError, fetch_sequencer_active, start_sequencer,
    stop_sequencer,
};

/// Inspect and control sequencer activity on HA conductor nodes.
#[derive(Debug, Args)]
pub struct SequencerCommand {
    /// Sequencer operation to run.
    #[command(subcommand)]
    pub command: SequencerCommands,
}

/// Sequencer inspection and control commands.
#[derive(Debug, Subcommand)]
pub enum SequencerCommands {
    /// Show sequencer state for every node or one selected node.
    Status(SequencerStatusArgs),
    /// Start sequencing on one node.
    Start(SequencerStartArgs),
    /// Stop sequencing on one node.
    Stop(SequencerNodeActionArgs),
}

/// Flags for `basectl sequencer status`.
#[derive(Debug, Args)]
pub struct SequencerStatusArgs {
    /// Optional node name or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub node: Option<String>,
    /// Emit a structured JSON status summary instead of pretty text.
    #[arg(long)]
    pub json: bool,
}

/// Flags for `basectl sequencer start`.
#[derive(Debug, Args)]
pub struct SequencerStartArgs {
    /// Sequencer node name or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub node: String,
    /// Unsafe head hash passed to `admin_startSequencer`.
    #[arg(value_name = "UNSAFE_HEAD")]
    pub unsafe_head: Option<String>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

/// Flags for `basectl sequencer stop`.
#[derive(Debug, Args)]
pub struct SequencerNodeActionArgs {
    /// Sequencer node name or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub node: String,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

// Allow two full `admin_sequencerActive` polls plus the stabilization sleep,
// with a little slack for scheduling jitter and connection setup.
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(12);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const REQUIRED_OBSERVATIONS: usize = 2;

impl SequencerCommand {
    /// Runs the selected sequencer subcommand.
    pub async fn run(
        self,
        config: MonitoringConfig,
        conductor_rpc: Option<Url>,
    ) -> Result<CommandOutcome> {
        let source =
            config.resolve_conductor_source(conductor_rpc).map_err(SequencerCommandError::from)?;
        match self.command {
            SequencerCommands::Status(args) => run_status(config, source, args).await,
            SequencerCommands::Start(args) => run_start(config, source, args).await,
            SequencerCommands::Stop(args) => run_stop(config, source, args).await,
        }?;
        Ok(CommandOutcome::Success)
    }
}

async fn run_status(
    config: MonitoringConfig,
    source: ConductorSource,
    args: SequencerStatusArgs,
) -> Result<()> {
    info!(
        network = %config.name,
        selected_node = %args.node.as_deref().unwrap_or("all"),
        "fetching sequencer status"
    );
    let snapshot = ConductorControl::snapshot(source).await?;
    let status = sequencer_status_json(&config.name, &snapshot, args.node.as_deref())?;
    debug!(
        network = %config.name,
        leader = ?snapshot
            .statuses
            .iter()
            .find(|status| status.is_leader == Some(true))
            .map(|status| &status.name),
        node_count = snapshot
            .nodes
            .iter()
            .filter(|node| args.node.as_ref().is_none_or(|selected| node.name == *selected))
            .count(),
        membership_version = ?snapshot.membership.as_ref().map(|membership| membership.version),
        membership_error = ?snapshot.membership_error,
        "sequencer status snapshot ready"
    );
    if args.json {
        JsonOutput::print(&status)?;
    } else {
        print_status_pretty(&config.name, &snapshot, args.node.as_deref())?;
    }
    Ok(())
}

async fn run_start(
    config: MonitoringConfig,
    source: ConductorSource,
    args: SequencerStartArgs,
) -> Result<()> {
    info!(
        network = %config.name,
        requested_node = %args.node,
        requested_unsafe_head = ?args.unsafe_head,
        "running sequencer start command"
    );
    let snapshot = ConductorControl::snapshot(source).await?;
    let node = ConductorNodeConfig::find(&snapshot.nodes, &args.node)
        .map_err(SequencerCommandError::from)?;
    let status = snapshot_node_status(&snapshot, &node.name);
    debug!(
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        conductor_leader = ?status.and_then(|status| status.is_leader),
        sequencer_active = ?status.and_then(|status| status.sequencer_active),
        "resolved sequencer start target"
    );
    let leadership_confirmed = match ensure_start_allowed(&snapshot, node, status) {
        Ok(leadership_confirmed) => leadership_confirmed,
        Err(error) => {
            warn!(
                error = %error,
                node = %node.name,
                cl_rpc = %node.cl_rpc,
                "sequencer start preflight failed"
            );
            return Err(error.into());
        }
    };
    if !leadership_confirmed {
        warn!(
            node = %node.name,
            cl_rpc = %node.cl_rpc,
            "conductor leadership is unknown; deferring start leader validation to server-side RPC"
        );
    }
    let (unsafe_head, unsafe_head_source) =
        match resolve_start_hash(&snapshot, node, args.unsafe_head.as_deref()) {
            Ok(value) => value,
            Err(error) => {
                warn!(
                    error = %error,
                    node = %node.name,
                    cl_rpc = %node.cl_rpc,
                    "failed to resolve sequencer start unsafe head"
                );
                return Err(error.into());
            }
        };
    if let Err(error) =
        ensure_start_request_matches_observed_head(status, unsafe_head, unsafe_head_source)
    {
        warn!(
            error = %error,
            node = %node.name,
            cl_rpc = %node.cl_rpc,
            unsafe_head = %unsafe_head,
            unsafe_head_source,
            "sequencer start unsafe head validation failed"
        );
        return Err(error.into());
    }
    let prompt =
        format!("Start sequencer on {} ({}) at {}? [y/N] ", node.name, node.cl_rpc, unsafe_head);
    if !Confirm::prompt_or_abort(&prompt, args.yes)? {
        debug!(node = %node.name, cl_rpc = %node.cl_rpc, "sequencer start confirmation declined");
        return Ok(());
    }

    info!(
        network = %config.name,
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        unsafe_head = %unsafe_head,
        unsafe_head_source,
        "calling admin_startSequencer"
    );
    start_sequencer(&node.cl_rpc, unsafe_head).await?;
    wait_for_expected_state(node, true, Some(unsafe_head)).await?;
    info!(
        network = %config.name,
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        unsafe_head = %unsafe_head,
        unsafe_head_source,
        "sequencer start completed"
    );

    let message = format!("sequencer started on {} at {}", node.name, unsafe_head);
    print_action(
        &sequencer_start_json(&config.name, node, unsafe_head, unsafe_head_source, &message),
        &message,
        args.json,
    )?;
    Ok(())
}

async fn run_stop(
    config: MonitoringConfig,
    source: ConductorSource,
    args: SequencerNodeActionArgs,
) -> Result<()> {
    info!(
        network = %config.name,
        requested_node = %args.node,
        "running sequencer stop command"
    );
    let snapshot = ConductorControl::snapshot(source).await?;
    let node = ConductorNodeConfig::find(&snapshot.nodes, &args.node)
        .map_err(SequencerCommandError::from)?;
    let status = snapshot_node_status(&snapshot, &node.name);
    debug!(
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        conductor_leader = ?status.and_then(|status| status.is_leader),
        sequencer_active = ?status.and_then(|status| status.sequencer_active),
        "resolved sequencer stop target"
    );
    if let Err(error) = ensure_stop_allowed(node, status) {
        warn!(
            error = %error,
            node = %node.name,
            cl_rpc = %node.cl_rpc,
            "sequencer stop preflight failed"
        );
        return Err(error.into());
    }
    let prompt = format!("Stop sequencer on {} ({})? [y/N] ", node.name, node.cl_rpc);
    if !Confirm::prompt_or_abort(&prompt, args.yes)? {
        debug!(node = %node.name, cl_rpc = %node.cl_rpc, "sequencer stop confirmation declined");
        return Ok(());
    }

    info!(
        network = %config.name,
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        "calling admin_stopSequencer"
    );
    let unsafe_head = stop_sequencer(&node.cl_rpc).await?;
    // A zero head means the sequencer stopped but the captured head is unavailable,
    // so do not surface it as a valid restart point.
    let captured_head = (unsafe_head != B256::ZERO).then_some(unsafe_head);
    wait_for_expected_state(node, false, captured_head).await?;
    info!(
        network = %config.name,
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        unsafe_head = ?captured_head,
        "sequencer stop completed"
    );

    let message = captured_head.map_or_else(
        || format!("sequencer stopped on {} (unsafe head unavailable)", node.name),
        |unsafe_head| format!("sequencer stopped on {} at {unsafe_head}", node.name),
    );
    print_action(
        &sequencer_stop_json(&config.name, node, captured_head, &message),
        &message,
        args.json,
    )?;
    Ok(())
}

fn resolve_start_hash(
    snapshot: &ConductorClusterSnapshot,
    node: &ConductorNodeConfig,
    unsafe_head: Option<&str>,
) -> Result<(B256, &'static str), SequencerCommandError> {
    match unsafe_head {
        Some(unsafe_head) => Ok((parse_unsafe_head(unsafe_head)?, "explicit")),
        None => {
            let hash = snapshot_node_status(snapshot, &node.name)
                .and_then(|status| status.unsafe_l2_hash)
                .filter(|hash| *hash != B256::ZERO)
                .ok_or_else(|| SequencerCommandError::MissingUnsafeHead {
                    node: node.name.clone(),
                })?;
            Ok((hash, "observed"))
        }
    }
}

fn snapshot_node_status<'a>(
    snapshot: &'a ConductorClusterSnapshot,
    name: &str,
) -> Option<&'a ConductorNodeStatus> {
    snapshot.statuses.iter().find(|status| status.name == name)
}

fn ensure_start_allowed(
    snapshot: &ConductorClusterSnapshot,
    node: &ConductorNodeConfig,
    status: Option<&ConductorNodeStatus>,
) -> Result<bool, SequencerCommandError> {
    if status.and_then(|status| status.sequencer_active) == Some(true) {
        return Err(SequencerCommandError::AlreadyActive { node: node.name.clone() });
    }
    ensure_leader_target(snapshot, node, status, "start")
}

fn ensure_stop_allowed(
    node: &ConductorNodeConfig,
    status: Option<&ConductorNodeStatus>,
) -> Result<(), SequencerCommandError> {
    if status.and_then(|status| status.sequencer_active) == Some(false) {
        return Err(SequencerCommandError::AlreadyStopped { node: node.name.clone() });
    }
    Ok(())
}

fn ensure_leader_target(
    snapshot: &ConductorClusterSnapshot,
    node: &ConductorNodeConfig,
    status: Option<&ConductorNodeStatus>,
    action: &str,
) -> Result<bool, SequencerCommandError> {
    let leader = snapshot
        .statuses
        .iter()
        .find(|status| status.is_leader == Some(true))
        .map(|status| status.name.as_str());
    if let Some(leader) = leader {
        if leader == node.name {
            return Ok(true);
        }
        return Err(SequencerCommandError::NotCurrentLeader {
            requested_node: node.name.clone(),
            current_leader: leader.to_string(),
            action: action.to_string(),
        });
    }
    if status.and_then(|status| status.is_leader) == Some(false) {
        return Err(SequencerCommandError::NotLeader {
            requested_node: node.name.clone(),
            action: action.to_string(),
        });
    }
    Ok(false)
}

fn ensure_start_request_matches_observed_head(
    status: Option<&ConductorNodeStatus>,
    unsafe_head: B256,
    unsafe_head_source: &str,
) -> Result<(), SequencerCommandError> {
    if unsafe_head_source != "explicit" {
        return Ok(());
    }

    let Some(observed_head) = status.and_then(|status| status.unsafe_l2_hash) else {
        return Ok(());
    };
    if observed_head == B256::ZERO {
        return Err(SequencerCommandError::UninitializedUnsafeHead);
    }
    if observed_head != unsafe_head {
        return Err(SequencerCommandError::UnsafeHeadMismatch {
            observed_hash: observed_head,
            requested_hash: unsafe_head,
        });
    }
    Ok(())
}

fn parse_unsafe_head(raw: &str) -> Result<B256, SequencerCommandError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SequencerCommandError::EmptyUnsafeHead);
    }

    let normalized = if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        trimmed.to_string()
    } else if trimmed.len() == 64 && trimmed.chars().all(|char| char.is_ascii_hexdigit()) {
        format!("0x{trimmed}")
    } else {
        trimmed.to_string()
    };
    let hash =
        B256::from_str(&normalized).map_err(|error| SequencerCommandError::InvalidUnsafeHead {
            raw: trimmed.to_string(),
            message: error.to_string(),
        })?;
    if hash == B256::ZERO {
        return Err(SequencerCommandError::ZeroUnsafeHead { requested_hash: hash });
    }
    Ok(hash)
}

async fn wait_for_expected_state(
    node: &ConductorNodeConfig,
    expected_active: bool,
    unsafe_head: Option<B256>,
) -> Result<(), SequencerCommandError> {
    wait_for_expected_state_with_fetch(
        node,
        expected_active,
        unsafe_head,
        OBSERVATION_TIMEOUT,
        POLL_INTERVAL,
        || fetch_sequencer_active(&node.cl_rpc),
    )
    .await
}

async fn wait_for_expected_state_with_fetch<F, Fut>(
    node: &ConductorNodeConfig,
    expected_active: bool,
    unsafe_head: Option<B256>,
    observation_timeout: Duration,
    poll_interval: Duration,
    mut fetch: F,
) -> Result<(), SequencerCommandError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let action = if expected_active { "start" } else { "stop" };
    let deadline = Instant::now() + observation_timeout;
    let mut matching_observations = 0usize;
    let mut last_observed = None;
    let mut last_error = None;

    debug!(
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        action,
        expected_active,
        observation_timeout_ms = observation_timeout.as_millis(),
        poll_interval_ms = poll_interval.as_millis(),
        "waiting for sequencer state convergence"
    );

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, fetch()).await {
            Ok(Ok(is_active)) => {
                last_observed = Some(is_active);
                let _ = last_error.take();
                let next_matching_observations =
                    if is_active == expected_active { matching_observations + 1 } else { 0 };
                debug!(
                    node = %node.name,
                    cl_rpc = %node.cl_rpc,
                    action,
                    observed_active = is_active,
                    matching_observations = next_matching_observations,
                    required_observations = REQUIRED_OBSERVATIONS,
                    "observed sequencer state"
                );
                matching_observations = next_matching_observations;
                if matching_observations >= REQUIRED_OBSERVATIONS {
                    info!(
                        node = %node.name,
                        cl_rpc = %node.cl_rpc,
                        action,
                        expected_active,
                        matching_observations,
                        "sequencer state converged"
                    );
                    return Ok(());
                }
            }
            Ok(Err(error)) => {
                matching_observations = 0;
                debug!(
                    error = %error,
                    node = %node.name,
                    cl_rpc = %node.cl_rpc,
                    action,
                    "failed to poll sequencer state"
                );
                last_error = Some(error.to_string());
            }
            Err(_) => {
                debug!(
                    node = %node.name,
                    cl_rpc = %node.cl_rpc,
                    action,
                    "timed out waiting for sequencer state poll"
                );
                last_error = Some("timed out waiting for admin_sequencerActive".to_string());
                break;
            }
        }

        let sleep_for = poll_interval.min(deadline.saturating_duration_since(Instant::now()));
        if sleep_for.is_zero() {
            break;
        }
        sleep(sleep_for).await;
    }

    warn!(
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        action,
        expected_active,
        unsafe_head = ?unsafe_head,
        last_observed,
        last_error = ?last_error,
        "sequencer state did not converge after successful RPC"
    );
    Err(SequencerCommandError::StateConvergenceTimeout(Box::new(StateConvergenceTimeoutError {
        action,
        node: node.name.clone(),
        cl_rpc: node.cl_rpc.to_string(),
        unsafe_head,
        expected_active,
        timeout: observation_timeout,
        last_observed,
        last_error,
    })))
}

fn print_status_pretty(
    network: &str,
    snapshot: &ConductorClusterSnapshot,
    selected_node: Option<&str>,
) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", network)
        .row("source", if snapshot.discovered { "discovered" } else { "static" })
        .row(
            "nodes",
            snapshot
                .nodes
                .iter()
                .filter(|node| selected_node.is_none_or(|selected| node.name == selected))
                .count()
                .to_string(),
        );
    if let Some(selected_node) = selected_node {
        table.row("selected_node", selected_node);
    }
    if let Some(version) = snapshot.membership.as_ref().map(|membership| membership.version) {
        table.row("membership_version", version.to_string());
    }
    if let Some(error) = &snapshot.membership_error {
        table.row("membership_error", error);
    }
    let leader = snapshot
        .statuses
        .iter()
        .find(|status| status.is_leader == Some(true))
        .map(|status| status.name.as_str())
        .unwrap_or("unknown");
    table.row("leader", leader);
    for node in snapshot
        .nodes
        .iter()
        .filter(|node| selected_node.is_none_or(|selected| node.name == selected))
    {
        let status = snapshot.statuses.iter().find(|status| status.name == node.name);
        table.row(format!("node.{}", node.name), sequencer_node_compact_status(status));
    }
    table.print()?;
    Ok(())
}

fn print_action(action: &Value, message: &str, json_output: bool) -> Result<()> {
    if json_output {
        JsonOutput::print(action)?;
    } else {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "OK {message}")?;
    }
    Ok(())
}

fn sequencer_start_json(
    network: &str,
    node: &ConductorNodeConfig,
    unsafe_head: B256,
    unsafe_head_source: &str,
    message: &str,
) -> Value {
    json!({
        "network": network,
        "action": "start",
        "node": node.name,
        "clRpc": node.cl_rpc.to_string(),
        "unsafeHead": unsafe_head.to_string(),
        "unsafeHeadSource": unsafe_head_source,
        "message": message,
    })
}

fn sequencer_stop_json(
    network: &str,
    node: &ConductorNodeConfig,
    unsafe_head: Option<B256>,
    message: &str,
) -> Value {
    let mut value = json!({
        "network": network,
        "action": "stop",
        "node": node.name,
        "clRpc": node.cl_rpc.to_string(),
        "message": message,
    });
    if let Some(unsafe_head) = unsafe_head {
        value["unsafeHead"] = json!(unsafe_head.to_string());
    }
    value
}

fn sequencer_status_json(
    network: &str,
    snapshot: &ConductorClusterSnapshot,
    selected_node: Option<&str>,
) -> Result<Value, SequencerCommandError> {
    if let Some(selected_node) = selected_node {
        ConductorNodeConfig::find(&snapshot.nodes, selected_node)
            .map_err(SequencerCommandError::from)?;
    }

    let nodes = snapshot
        .nodes
        .iter()
        .filter(|node| selected_node.is_none_or(|selected| node.name == selected))
        .map(|node| {
            let status = snapshot.statuses.iter().find(|status| status.name == node.name);
            sequencer_node_json(node, status, snapshot.discovered)
        })
        .collect::<Vec<_>>();
    let mut value = json!({
        "network": network,
        "source": if snapshot.discovered { "discovered" } else { "static" },
        "leader": snapshot
            .statuses
            .iter()
            .find(|status| status.is_leader == Some(true))
            .map(|status| status.name.clone()),
        "nodes": nodes,
    });
    if let Some(selected_node) = selected_node {
        value["selectedNode"] = json!(selected_node);
    }
    if let Some(version) = snapshot.membership.as_ref().map(|membership| membership.version) {
        value["membershipVersion"] = json!(version);
    }
    if let Some(error) = &snapshot.membership_error {
        value["membershipError"] = json!(error);
    }
    Ok(value)
}

fn sequencer_node_json(
    node: &ConductorNodeConfig,
    status: Option<&ConductorNodeStatus>,
    discovered: bool,
) -> Value {
    let is_leader = status.and_then(|status| status.is_leader);
    json!({
        "name": node.name,
        "clRpc": node.cl_rpc.to_string(),
        "role": sequencer_role(is_leader),
        "isLeader": is_leader,
        "sequencerActive": status.and_then(|status| status.sequencer_active),
        "sequencerHealthy": status.and_then(|status| status.sequencer_healthy),
        "conductorPaused": status.and_then(|status| status.conductor_paused),
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
        "discovered": discovered,
    })
}

const fn sequencer_role(is_leader: Option<bool>) -> &'static str {
    match is_leader {
        Some(true) => "leader",
        Some(false) => "follower",
        None => "unknown",
    }
}

fn sequencer_node_compact_status(status: Option<&ConductorNodeStatus>) -> String {
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
        "role={} active={} healthy={} paused={} unsafe={} safe={} finalized={} current_l1={} head_l1={} cl_peers={} el_peers={}",
        sequencer_role(status.and_then(|status| status.is_leader)),
        boolean(status.and_then(|status| status.sequencer_active)),
        boolean(status.and_then(|status| status.sequencer_healthy)),
        boolean(status.and_then(|status| status.conductor_paused)),
        u64(status.and_then(|status| status.unsafe_l2_block)),
        u64(status.and_then(|status| status.safe_l2_block)),
        u64(status.and_then(|status| status.finalized_l2_block)),
        u64(status.and_then(|status| status.current_l1_block)),
        u64(status.and_then(|status| status.head_l1_block)),
        u32(status.and_then(|status| status.cl_peer_count)),
        u32(status.and_then(|status| status.el_peer_count)),
    )
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use serde_json::json;
    use tokio::time::{Duration, Instant};
    use url::Url;

    use super::{
        OBSERVATION_TIMEOUT, POLL_INTERVAL, REQUIRED_OBSERVATIONS, ensure_leader_target,
        ensure_start_allowed, ensure_start_request_matches_observed_head, ensure_stop_allowed,
        parse_unsafe_head, resolve_start_hash, sequencer_start_json, sequencer_status_json,
        sequencer_stop_json, wait_for_expected_state_with_fetch,
    };
    use crate::{
        ConductorClusterSnapshot, ConductorNodeConfig, ConductorNodeStatus,
        SEQUENCER_ACTIVE_RPC_TIMEOUT, SequencerCommandError,
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

    fn status(name: &str, leader: bool, active: bool) -> ConductorNodeStatus {
        ConductorNodeStatus {
            name: name.to_string(),
            is_leader: Some(leader),
            conductor_active: Some(leader),
            conductor_paused: Some(false),
            conductor_stopped: Some(false),
            sequencer_healthy: Some(true),
            sequencer_active: Some(active),
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
    fn parse_unsafe_head_accepts_prefixed_and_bare_hex() {
        let raw = "1111111111111111111111111111111111111111111111111111111111111111";

        assert_eq!(
            parse_unsafe_head(raw).unwrap(),
            parse_unsafe_head(&format!("0x{raw}")).unwrap()
        );
    }

    #[test]
    fn parse_unsafe_head_rejects_zero_hash() {
        let err =
            parse_unsafe_head("0x0000000000000000000000000000000000000000000000000000000000000000")
                .expect_err("zero hash should error");

        assert!(matches!(
            err,
            SequencerCommandError::ZeroUnsafeHead {
                requested_hash,
            } if requested_hash == B256::ZERO
        ));
    }

    #[test]
    fn explicit_start_hash_must_match_observed_head() {
        let err = ensure_start_request_matches_observed_head(
            Some(&status("op-conductor-0", true, false)),
            B256::with_last_byte(9),
            "explicit",
        )
        .expect_err("mismatched explicit hash should error");

        assert!(matches!(
            err,
            SequencerCommandError::UnsafeHeadMismatch {
                observed_hash,
                requested_hash,
            } if observed_hash == B256::with_last_byte(1)
                && requested_hash == B256::with_last_byte(9)
        ));
    }

    #[test]
    fn explicit_start_hash_rejects_uninitialized_observed_head() {
        let mut observed = status("op-conductor-0", true, false);
        observed.unsafe_l2_hash = Some(B256::ZERO);

        let err = ensure_start_request_matches_observed_head(
            Some(&observed),
            B256::with_last_byte(9),
            "explicit",
        )
        .expect_err("zero observed hash should error");

        assert!(matches!(err, SequencerCommandError::UninitializedUnsafeHead));
    }

    #[test]
    fn resolve_start_hash_errors_when_observed_head_is_missing() {
        let mut observed = status("op-conductor-0", true, false);
        observed.unsafe_l2_hash = None;
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0")],
            statuses: vec![observed],
            membership: None,
            membership_error: None,
            discovered: false,
        };

        let err = resolve_start_hash(&snapshot, &snapshot.nodes[0], None)
            .expect_err("missing observed hash should error");

        assert!(matches!(
            err,
            SequencerCommandError::MissingUnsafeHead { node } if node == "op-conductor-0"
        ));
    }

    #[test]
    fn start_rejects_when_sequencer_is_already_active() {
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0")],
            statuses: vec![status("op-conductor-0", true, true)],
            membership: None,
            membership_error: None,
            discovered: false,
        };

        let err = ensure_start_allowed(&snapshot, &snapshot.nodes[0], Some(&snapshot.statuses[0]))
            .expect_err("active node should reject start");

        assert!(matches!(
            err,
            SequencerCommandError::AlreadyActive { node } if node == "op-conductor-0"
        ));
    }

    #[test]
    fn start_requires_targeting_the_current_leader() {
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0"), node("op-conductor-1")],
            statuses: vec![
                status("op-conductor-0", true, true),
                status("op-conductor-1", false, false),
            ],
            membership: None,
            membership_error: None,
            discovered: false,
        };

        let err = ensure_leader_target(
            &snapshot,
            &snapshot.nodes[1],
            Some(&snapshot.statuses[1]),
            "start",
        )
        .expect_err("follower target should error");

        assert!(matches!(
            err,
            SequencerCommandError::NotCurrentLeader {
                requested_node,
                current_leader,
                action,
            } if requested_node == "op-conductor-1"
                && current_leader == "op-conductor-0"
                && action == "start"
        ));
    }

    #[test]
    fn start_rejects_when_another_leader_is_known_but_target_status_is_missing() {
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0"), node("op-conductor-1")],
            statuses: vec![status("op-conductor-0", true, true)],
            membership: None,
            membership_error: None,
            discovered: false,
        };

        let err = ensure_start_allowed(&snapshot, &snapshot.nodes[1], None)
            .expect_err("target should still reject when another leader is known");

        assert!(matches!(
            err,
            SequencerCommandError::NotCurrentLeader {
                requested_node,
                current_leader,
                action,
            } if requested_node == "op-conductor-1"
                && current_leader == "op-conductor-0"
                && action == "start"
        ));
    }

    #[test]
    fn start_allows_unknown_leadership_with_status_signal() {
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0")],
            statuses: vec![status("op-conductor-0", false, false)],
            membership: None,
            membership_error: None,
            discovered: false,
        };
        let mut unknown_status = snapshot.statuses[0].clone();
        unknown_status.is_leader = None;

        let leadership_confirmed =
            ensure_start_allowed(&snapshot, &snapshot.nodes[0], Some(&unknown_status))
                .expect("unknown leadership should defer to server-side RPC");

        assert!(!leadership_confirmed);
    }

    #[test]
    fn stop_allows_active_follower_targets() {
        let follower = status("op-conductor-1", false, true);

        ensure_stop_allowed(&node("op-conductor-1"), Some(&follower))
            .expect("active follower should still be stoppable");
    }

    #[test]
    fn stop_rejects_when_sequencer_is_already_inactive() {
        let err = ensure_stop_allowed(
            &node("op-conductor-0"),
            Some(&status("op-conductor-0", true, false)),
        )
        .expect_err("inactive node should reject stop");

        assert!(matches!(
            err,
            SequencerCommandError::AlreadyStopped { node } if node == "op-conductor-0"
        ));
    }

    #[tokio::test]
    async fn wait_for_expected_state_honors_deadline_when_fetch_hangs() {
        let node = node("op-conductor-0");
        let start = Instant::now();

        let err = wait_for_expected_state_with_fetch(
            &node,
            true,
            Some(B256::with_last_byte(1)),
            Duration::from_millis(40),
            Duration::from_millis(5),
            || async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<bool, anyhow::Error>(false)
            },
        )
        .await
        .expect_err("hung fetch should time out");

        assert!(start.elapsed() < Duration::from_millis(120));
        assert!(matches!(
            err,
            SequencerCommandError::StateConvergenceTimeout(error)
                if error.action == "start"
                && error.node == "op-conductor-0"
                && error.unsafe_head == Some(B256::with_last_byte(1))
                && error.expected_active
                && error.timeout == Duration::from_millis(40)
                && error.last_observed.is_none()
                && error.last_error.as_deref()
                    == Some("timed out waiting for admin_sequencerActive")
        ));
    }

    #[test]
    fn observation_timeout_allows_two_full_status_polls() {
        let minimum_timeout = SEQUENCER_ACTIVE_RPC_TIMEOUT
            .checked_mul(REQUIRED_OBSERVATIONS as u32)
            .and_then(|timeout| {
                POLL_INTERVAL
                    .checked_mul(REQUIRED_OBSERVATIONS.saturating_sub(1) as u32)
                    .and_then(|poll_sleep| timeout.checked_add(poll_sleep))
            })
            .expect("valid timeout calculation");

        assert!(OBSERVATION_TIMEOUT >= minimum_timeout);
    }

    #[test]
    fn sequencer_action_json_includes_hash_source() {
        let value = sequencer_start_json(
            "devnet",
            &node("op-conductor-0"),
            B256::with_last_byte(9),
            "observed",
            "sequencer started on op-conductor-0 at 0x09",
        );

        assert_eq!(
            value,
            json!({
                "network": "devnet",
                "action": "start",
                "node": "op-conductor-0",
                "clRpc": "http://127.0.0.1:7545/",
                "unsafeHead": "0x0000000000000000000000000000000000000000000000000000000000000009",
                "unsafeHeadSource": "observed",
                "message": "sequencer started on op-conductor-0 at 0x09",
            })
        );
    }

    #[test]
    fn sequencer_stop_json_omits_missing_unsafe_head() {
        let value = sequencer_stop_json(
            "devnet",
            &node("op-conductor-0"),
            None,
            "sequencer stopped on op-conductor-0 (unsafe head unavailable)",
        );

        assert_eq!(
            value,
            json!({
                "network": "devnet",
                "action": "stop",
                "node": "op-conductor-0",
                "clRpc": "http://127.0.0.1:7545/",
                "message": "sequencer stopped on op-conductor-0 (unsafe head unavailable)",
            })
        );
    }

    #[test]
    fn status_json_filters_selected_node() {
        let snapshot = ConductorClusterSnapshot {
            nodes: vec![node("op-conductor-0"), node("op-conductor-1")],
            statuses: vec![
                status("op-conductor-0", true, true),
                status("op-conductor-1", false, false),
            ],
            membership: None,
            membership_error: Some("membership request timed out".to_string()),
            discovered: false,
        };

        let value = sequencer_status_json("devnet", &snapshot, Some("op-conductor-1")).unwrap();

        assert_eq!(value["selectedNode"], "op-conductor-1");
        assert_eq!(value["membershipError"], "membership request timed out");
        assert_eq!(value["leader"], "op-conductor-0");
        assert_eq!(value["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(value["nodes"][0]["name"], "op-conductor-1");
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

        let value = sequencer_status_json("devnet", &snapshot, None).unwrap();

        assert_eq!(value["nodes"][0]["discovered"], true);
    }

    #[test]
    fn find_node_reports_missing_name() {
        let nodes = vec![node("op-conductor-0")];

        let err = ConductorNodeConfig::find(&nodes, "op-conductor-1")
            .map_err(SequencerCommandError::from)
            .expect_err("missing node should error");

        assert!(matches!(
            err,
            SequencerCommandError::MissingNode {
                requested_node,
                available_nodes,
            } if requested_node == "op-conductor-1"
                && available_nodes == vec!["op-conductor-0".to_string()]
        ));
    }
}
