//! Implementation of the `basectl sequencer` command group.

use std::{future::Future, str::FromStr, time::Duration};

use alloy_primitives::B256;
use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    ConductorClusterSnapshot, ConductorControl, ConductorNodeConfig, ConductorNodeStatus,
    ConductorSource, Confirm, JsonOutput, KeyValueTable, MonitoringConfig, NodeMetricsJson,
    OptionalValue, SequencerCommandError, StateConvergenceTimeoutError, fetch_sequencer_active,
    start_sequencer, stop_sequencer,
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
    /// Optional node name from the selected config or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub node: Option<String>,
    /// Emit a structured JSON status summary instead of pretty text.
    #[arg(long)]
    pub json: bool,
}

/// Flags for `basectl sequencer start`.
#[derive(Debug, Args)]
pub struct SequencerStartArgs {
    /// Sequencer node name from the selected config or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub node: String,
    /// Unsafe head hash to pass to `admin_startSequencer`.
    ///
    /// If omitted, basectl uses the node's currently observed unsafe L2 hash.
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
    /// Sequencer node name from the selected config or discovered raft server ID.
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

/// Leadership knowledge available during start preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadershipStatus {
    /// The target is the confirmed leader.
    ConfirmedLeader,
    /// Leadership could not be established locally.
    Unknown,
}

impl SequencerCommand {
    /// Runs the selected sequencer subcommand.
    pub async fn run(self, config: MonitoringConfig, conductor_rpc: Option<Url>) -> Result<()> {
        let source =
            config.resolve_conductor_source(conductor_rpc).map_err(SequencerCommandError::from)?;
        match self.command {
            SequencerCommands::Status(args) => run_status(config, source, args).await,
            SequencerCommands::Start(args) => run_start(config, source, args).await,
            SequencerCommands::Stop(args) => run_stop(config, source, args).await,
        }
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
    let status = SequencerStatusJson::from_snapshot(&config.name, &snapshot, args.node.as_deref())?;
    debug!(
        network = %config.name,
        leader = ?status.leader,
        node_count = status.nodes.len(),
        membership_version = ?status.membership_version,
        membership_error = ?status.membership_error,
        "sequencer status snapshot ready"
    );
    if args.json {
        JsonOutput::print(&status)?;
    } else {
        print_status_pretty(&status)?;
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
    let leadership_status = match ensure_start_allowed(&snapshot, node, status) {
        Ok(leadership_status) => leadership_status,
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
    if matches!(leadership_status, LeadershipStatus::Unknown) {
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
            unsafe_head_source = %unsafe_head_source.as_str(),
            "sequencer start unsafe head validation failed"
        );
        return Err(error.into());
    }
    if node.el_rpc.is_some() {
        let status =
            status.ok_or_else(|| SequencerCommandError::ExecutionLayerStatusUnavailable {
                node: node.name.clone(),
                field: "status",
            })?;
        let required_l2_block = status.unsafe_l2_block.ok_or_else(|| {
            SequencerCommandError::ExecutionLayerStatusUnavailable {
                node: node.name.clone(),
                field: "unsafe_l2_block",
            }
        })?;
        if let Err(error) = ensure_el_ready_for_sequencing(status, &node.name, required_l2_block) {
            warn!(
                error = %error,
                node = %node.name,
                cl_rpc = %node.cl_rpc,
                unsafe_head = %unsafe_head,
                required_l2_block,
                "sequencer start EL readiness preflight failed"
            );
            return Err(error.into());
        }
    } else {
        debug!(
            node = %node.name,
            cl_rpc = %node.cl_rpc,
            "skipping sequencer start EL readiness preflight because no EL RPC is configured"
        );
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
        unsafe_head_source = %unsafe_head_source.as_str(),
        "calling admin_startSequencer"
    );
    start_sequencer(&node.cl_rpc, unsafe_head).await?;
    wait_for_expected_state(node, SequencerAction::Start, Some(unsafe_head)).await?;
    info!(
        network = %config.name,
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        unsafe_head = %unsafe_head,
        unsafe_head_source = %unsafe_head_source.as_str(),
        "sequencer start completed"
    );

    let message = format!("sequencer started on {} at {}", node.name, unsafe_head);
    let outcome =
        SequencerActionJson::start(&config.name, node, unsafe_head, unsafe_head_source, message);
    JsonOutput::print_or_ok(&outcome, &outcome.message, args.json)?;
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
    wait_for_expected_state(node, SequencerAction::Stop, captured_head).await?;
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
    let outcome = SequencerActionJson::stop(&config.name, node, captured_head, message);
    JsonOutput::print_or_ok(&outcome, &outcome.message, args.json)?;
    Ok(())
}

fn resolve_start_hash(
    snapshot: &ConductorClusterSnapshot,
    node: &ConductorNodeConfig,
    unsafe_head: Option<&str>,
) -> Result<(B256, UnsafeHeadSource), SequencerCommandError> {
    match unsafe_head {
        Some(unsafe_head) => Ok((parse_unsafe_head(unsafe_head)?, UnsafeHeadSource::Explicit)),
        None => {
            let hash = snapshot_node_status(snapshot, &node.name)
                .and_then(|status| status.unsafe_l2_hash)
                .filter(|hash| *hash != B256::ZERO)
                .ok_or_else(|| SequencerCommandError::MissingUnsafeHead {
                    node: node.name.clone(),
                })?;
            Ok((hash, UnsafeHeadSource::Observed))
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
) -> Result<LeadershipStatus, SequencerCommandError> {
    if status.and_then(|status| status.sequencer_active) == Some(true) {
        return Err(SequencerCommandError::AlreadyActive { node: node.name.clone() });
    }
    ensure_leader_target(snapshot, node, status, SequencerAction::Start)
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
    action: SequencerAction,
) -> Result<LeadershipStatus, SequencerCommandError> {
    let leader = snapshot
        .statuses
        .iter()
        .find(|status| status.is_leader == Some(true))
        .map(|status| status.name.as_str());
    if let Some(leader) = leader {
        if leader == node.name {
            return Ok(LeadershipStatus::ConfirmedLeader);
        }
        return Err(SequencerCommandError::NotCurrentLeader {
            requested_node: node.name.clone(),
            current_leader: leader.to_string(),
            action: action.infinitive(),
        });
    }
    if status.and_then(|status| status.is_leader) == Some(false) {
        return Err(SequencerCommandError::NotLeader {
            requested_node: node.name.clone(),
            action: action.infinitive(),
        });
    }
    Ok(LeadershipStatus::Unknown)
}

fn ensure_start_request_matches_observed_head(
    status: Option<&ConductorNodeStatus>,
    unsafe_head: B256,
    unsafe_head_source: UnsafeHeadSource,
) -> Result<(), SequencerCommandError> {
    // Exhaustive match so a new source variant is forced to decide whether it
    // needs observed-head validation instead of silently skipping it.
    match unsafe_head_source {
        UnsafeHeadSource::Observed => return Ok(()),
        UnsafeHeadSource::Explicit => {}
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

fn ensure_el_ready_for_sequencing(
    status: &ConductorNodeStatus,
    node: &str,
    required_l2_block: u64,
) -> Result<(), SequencerCommandError> {
    let el_block =
        status.el_block.ok_or_else(|| SequencerCommandError::ExecutionLayerStatusUnavailable {
            node: node.to_string(),
            field: "el_block",
        })?;
    match status.el_syncing {
        Some(false) => {}
        Some(true) => {
            return Err(SequencerCommandError::ExecutionLayerSyncing {
                node: node.to_string(),
                el_block,
                required_l2_block,
            });
        }
        None => {
            return Err(SequencerCommandError::ExecutionLayerStatusUnavailable {
                node: node.to_string(),
                field: "el_syncing",
            });
        }
    }

    if el_block < required_l2_block {
        return Err(SequencerCommandError::ExecutionLayerBehind {
            node: node.to_string(),
            el_block,
            required_l2_block,
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
    action: SequencerAction,
    unsafe_head: Option<B256>,
) -> Result<(), SequencerCommandError> {
    wait_for_expected_state_with_fetch(
        node,
        action,
        unsafe_head,
        OBSERVATION_TIMEOUT,
        POLL_INTERVAL,
        || fetch_sequencer_active(&node.cl_rpc),
    )
    .await
}

async fn wait_for_expected_state_with_fetch<F, Fut>(
    node: &ConductorNodeConfig,
    action: SequencerAction,
    unsafe_head: Option<B256>,
    observation_timeout: Duration,
    poll_interval: Duration,
    mut fetch: F,
) -> Result<(), SequencerCommandError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let expected_active = action.expected_active();
    let deadline = Instant::now() + observation_timeout;
    let mut matching_observations = 0usize;
    let mut last_observed = None;
    let mut last_error = None;

    debug!(
        node = %node.name,
        cl_rpc = %node.cl_rpc,
        action = %action.infinitive(),
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
                    action = %action.infinitive(),
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
                        action = %action.infinitive(),
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
                    action = %action.infinitive(),
                    "failed to poll sequencer state"
                );
                last_error = Some(error.to_string());
            }
            Err(_) => {
                debug!(
                    node = %node.name,
                    cl_rpc = %node.cl_rpc,
                    action = %action.infinitive(),
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
        action = %action.infinitive(),
        expected_active,
        unsafe_head = ?unsafe_head,
        last_observed,
        last_error = ?last_error,
        "sequencer state did not converge after successful RPC"
    );
    Err(action.timeout_error(node, unsafe_head, observation_timeout, last_observed, last_error))
}

fn print_status_pretty(status: &SequencerStatusJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &status.network)
        .row("source", status.source)
        .row("nodes", status.nodes.len().to_string());
    if let Some(selected_node) = &status.selected_node {
        table.row("selected_node", selected_node);
    }
    if let Some(version) = status.membership_version {
        table.row("membership_version", version.to_string());
    }
    if let Some(error) = &status.membership_error {
        table.row("membership_error", error);
    }
    table.row("leader", status.leader.as_deref().unwrap_or("unknown"));
    for node in &status.nodes {
        table.row(format!("node.{}", node.name), node.compact_status());
    }
    table.print()?;
    Ok(())
}

/// Sequencer mutation represented in command output.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum SequencerAction {
    /// Start sequencing.
    #[serde(rename = "start")]
    Start,
    /// Stop sequencing.
    #[serde(rename = "stop")]
    Stop,
}

impl SequencerAction {
    /// The `admin_sequencerActive` state this action converges toward.
    pub const fn expected_active(self) -> bool {
        match self {
            Self::Start => true,
            Self::Stop => false,
        }
    }

    /// Machine-readable action name for JSON output, errors, and tracing fields.
    pub const fn infinitive(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }

    /// Builds the convergence-timeout error for this action.
    pub fn timeout_error(
        self,
        node: &ConductorNodeConfig,
        unsafe_head: Option<B256>,
        observation_timeout: Duration,
        last_observed: Option<bool>,
        last_error: Option<String>,
    ) -> SequencerCommandError {
        SequencerCommandError::StateConvergenceTimeout(Box::new(StateConvergenceTimeoutError {
            action: self.infinitive(),
            node: node.name.clone(),
            cl_rpc: node.cl_rpc.to_string(),
            unsafe_head,
            expected_active: self.expected_active(),
            timeout: observation_timeout,
            last_observed,
            last_error,
        }))
    }
}

/// Source of the unsafe head used to start sequencing.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsafeHeadSource {
    /// Supplied by the operator.
    Explicit,
    /// Read from the target node.
    Observed,
}

impl UnsafeHeadSource {
    /// Machine-readable source name for tracing fields.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Observed => "observed",
        }
    }
}

/// JSON result for a sequencer action.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SequencerActionJson {
    /// Network name.
    pub network: String,
    /// Action performed.
    pub action: SequencerAction,
    /// Target node name.
    pub node: String,
    /// Target consensus-layer RPC URL.
    pub cl_rpc: String,
    /// Unsafe head associated with the action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsafe_head: Option<String>,
    /// Source of the unsafe head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsafe_head_source: Option<UnsafeHeadSource>,
    /// Human-readable result.
    pub message: String,
}

impl SequencerActionJson {
    /// Builds the outcome for a completed sequencer start.
    pub fn start(
        network: &str,
        node: &ConductorNodeConfig,
        unsafe_head: B256,
        unsafe_head_source: UnsafeHeadSource,
        message: String,
    ) -> Self {
        Self {
            network: network.to_string(),
            action: SequencerAction::Start,
            node: node.name.clone(),
            cl_rpc: node.cl_rpc.to_string(),
            unsafe_head: Some(unsafe_head.to_string()),
            unsafe_head_source: Some(unsafe_head_source),
            message,
        }
    }

    /// Builds the outcome for a completed sequencer stop.
    pub fn stop(
        network: &str,
        node: &ConductorNodeConfig,
        unsafe_head: Option<B256>,
        message: String,
    ) -> Self {
        Self {
            network: network.to_string(),
            action: SequencerAction::Stop,
            node: node.name.clone(),
            cl_rpc: node.cl_rpc.to_string(),
            unsafe_head: unsafe_head.map(|unsafe_head| unsafe_head.to_string()),
            unsafe_head_source: None,
            message,
        }
    }
}

/// JSON sequencer cluster status.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SequencerStatusJson {
    /// Network name.
    pub network: String,
    /// Membership source.
    pub source: &'static str,
    /// Selected node filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_node: Option<String>,
    /// Raft membership version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub membership_version: Option<u64>,
    /// Membership lookup error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub membership_error: Option<String>,
    /// Current leader node.
    pub leader: Option<String>,
    /// Per-node statuses.
    pub nodes: Vec<SequencerNodeJson>,
}

impl SequencerStatusJson {
    /// Builds the status summary from a cluster snapshot, optionally filtered to one node.
    pub fn from_snapshot(
        network: &str,
        snapshot: &ConductorClusterSnapshot,
        selected_node: Option<&str>,
    ) -> Result<Self, SequencerCommandError> {
        if let Some(selected_node) = selected_node {
            ConductorNodeConfig::find(&snapshot.nodes, selected_node)
                .map_err(SequencerCommandError::from)?;
        }

        let nodes = snapshot
            .nodes
            .iter()
            .filter(|node| selected_node.is_none_or(|selected_node| node.name == selected_node))
            .map(|node| {
                let status = snapshot.statuses.iter().find(|status| status.name == node.name);
                SequencerNodeJson::from_node_status(node, status, snapshot.discovered)
            })
            .collect::<Vec<_>>();

        Ok(Self {
            network: network.to_string(),
            source: snapshot.source_label(),
            selected_node: selected_node.map(str::to_owned),
            membership_version: snapshot.membership.as_ref().map(|membership| membership.version),
            membership_error: snapshot.membership_error.clone(),
            leader: snapshot
                .statuses
                .iter()
                .find(|status| status.is_leader == Some(true))
                .map(|status| status.name.clone()),
            nodes,
        })
    }
}

/// Sequencer node role derived from leadership status.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SequencerRole {
    /// Current raft leader.
    Leader,
    /// Confirmed follower.
    Follower,
    /// Leadership status unavailable.
    Unknown,
}

impl SequencerRole {
    /// Derives the role from an optionally known leadership flag.
    pub const fn from_is_leader(is_leader: Option<bool>) -> Self {
        match is_leader {
            Some(true) => Self::Leader,
            Some(false) => Self::Follower,
            None => Self::Unknown,
        }
    }

    /// Machine-readable role name for pretty output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Leader => "leader",
            Self::Follower => "follower",
            Self::Unknown => "unknown",
        }
    }
}

/// JSON status for one sequencer node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SequencerNodeJson {
    /// Node name.
    pub name: String,
    /// Consensus-layer RPC URL.
    pub cl_rpc: String,
    /// Derived sequencer role.
    pub role: SequencerRole,
    /// Metrics shared with conductor node output.
    #[serde(flatten)]
    pub metrics: NodeMetricsJson,
    /// Whether runtime discovery produced this node.
    pub discovered: bool,
}

impl SequencerNodeJson {
    /// Builds the node entry from its configuration and polled status.
    pub fn from_node_status(
        node: &ConductorNodeConfig,
        status: Option<&ConductorNodeStatus>,
        discovered: bool,
    ) -> Self {
        let metrics = NodeMetricsJson::from_status(status);
        Self {
            name: node.name.clone(),
            cl_rpc: node.cl_rpc.to_string(),
            role: SequencerRole::from_is_leader(metrics.is_leader),
            metrics,
            discovered,
        }
    }

    /// Renders a single-line status summary for pretty output.
    pub fn compact_status(&self) -> String {
        format!(
            "role={} active={} healthy={} paused={} unsafe={} safe={} finalized={} current_l1={} head_l1={} cl_peers={} el_peers={}",
            self.role.as_str(),
            OptionalValue::boolean(self.metrics.sequencer_active),
            OptionalValue::boolean(self.metrics.sequencer_healthy),
            OptionalValue::boolean(self.metrics.conductor_paused),
            OptionalValue::u64(self.metrics.unsafe_l2_block),
            OptionalValue::u64(self.metrics.safe_l2_block),
            OptionalValue::u64(self.metrics.finalized_l2_block),
            OptionalValue::u64(self.metrics.current_l1_block),
            OptionalValue::u64(self.metrics.head_l1_block),
            OptionalValue::u32(self.metrics.cl_peer_count),
            OptionalValue::u32(self.metrics.el_peer_count),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use serde_json::json;
    use tokio::time::{Duration, Instant};
    use url::Url;

    use super::{
        LeadershipStatus, OBSERVATION_TIMEOUT, POLL_INTERVAL, REQUIRED_OBSERVATIONS,
        SequencerAction, SequencerActionJson, SequencerNodeJson, SequencerStatusJson,
        UnsafeHeadSource, ensure_el_ready_for_sequencing, ensure_leader_target,
        ensure_start_allowed, ensure_start_request_matches_observed_head, ensure_stop_allowed,
        parse_unsafe_head, resolve_start_hash, wait_for_expected_state_with_fetch,
    };
    use crate::{
        ConductorClusterSnapshot, ConductorNodeConfig, ConductorNodeStatus, NodeLookupError,
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
            UnsafeHeadSource::Explicit,
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
            UnsafeHeadSource::Explicit,
        )
        .expect_err("zero observed hash should error");

        assert!(matches!(err, SequencerCommandError::UninitializedUnsafeHead));
    }

    #[test]
    fn start_el_readiness_rejects_syncing_el() {
        let mut observed = status("op-conductor-0", true, false);
        observed.el_syncing = Some(true);
        observed.el_block = Some(9);

        let err = ensure_el_ready_for_sequencing(&observed, "op-conductor-0", 10)
            .expect_err("syncing EL should block start");

        assert!(matches!(
            err,
            SequencerCommandError::ExecutionLayerSyncing {
                node,
                el_block,
                required_l2_block,
            } if node == "op-conductor-0"
                && el_block == 9
                && required_l2_block == 10
        ));
    }

    #[test]
    fn start_el_readiness_rejects_missing_sync_status() {
        let mut observed = status("op-conductor-0", true, false);
        observed.el_syncing = None;

        let err = ensure_el_ready_for_sequencing(&observed, "op-conductor-0", 10)
            .expect_err("missing EL sync status should block start");

        assert!(matches!(
            err,
            SequencerCommandError::ExecutionLayerStatusUnavailable { node, field }
                if node == "op-conductor-0" && field == "el_syncing"
        ));
    }

    #[test]
    fn start_el_readiness_rejects_missing_el_block() {
        let mut observed = status("op-conductor-0", true, false);
        observed.el_block = None;

        let err = ensure_el_ready_for_sequencing(&observed, "op-conductor-0", 10)
            .expect_err("missing EL block should block start");

        assert!(matches!(
            err,
            SequencerCommandError::ExecutionLayerStatusUnavailable { node, field }
                if node == "op-conductor-0" && field == "el_block"
        ));
    }

    #[test]
    fn start_el_readiness_rejects_el_behind_unsafe_head() {
        let mut observed = status("op-conductor-0", true, false);
        observed.el_block = Some(9);

        let err = ensure_el_ready_for_sequencing(&observed, "op-conductor-0", 10)
            .expect_err("EL behind unsafe head should block start");

        assert!(matches!(
            err,
            SequencerCommandError::ExecutionLayerBehind {
                node,
                el_block,
                required_l2_block,
            } if node == "op-conductor-0" && el_block == 9 && required_l2_block == 10
        ));
    }

    #[test]
    fn start_el_readiness_allows_caught_up_el() {
        let observed = status("op-conductor-0", true, false);

        ensure_el_ready_for_sequencing(&observed, "op-conductor-0", 10)
            .expect("caught-up EL should allow start");
    }

    #[test]
    fn start_el_readiness_allows_genesis() {
        let mut observed = status("op-conductor-0", true, false);
        observed.unsafe_l2_block = Some(0);
        observed.el_block = Some(0);

        ensure_el_ready_for_sequencing(&observed, "op-conductor-0", 0)
            .expect("genesis block should be a valid start point");
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
            SequencerAction::Start,
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

        let leadership_status =
            ensure_start_allowed(&snapshot, &snapshot.nodes[0], Some(&unknown_status))
                .expect("unknown leadership should defer to server-side RPC");

        assert_eq!(leadership_status, LeadershipStatus::Unknown);
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
            SequencerAction::Start,
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
        let value = serde_json::to_value(SequencerActionJson::start(
            "devnet",
            &node("op-conductor-0"),
            B256::with_last_byte(9),
            UnsafeHeadSource::Observed,
            "sequencer started on op-conductor-0 at 0x09".to_string(),
        ))
        .unwrap();

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
        let value = serde_json::to_value(SequencerActionJson::stop(
            "devnet",
            &node("op-conductor-0"),
            None,
            "sequencer stopped on op-conductor-0 (unsafe head unavailable)".to_string(),
        ))
        .unwrap();

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
    fn node_json_flattens_shared_metrics_at_top_level() {
        let value = serde_json::to_value(SequencerNodeJson::from_node_status(
            &node("op-conductor-0"),
            Some(&status("op-conductor-0", true, true)),
            false,
        ))
        .unwrap();

        assert_eq!(
            value,
            json!({
                "name": "op-conductor-0",
                "clRpc": "http://127.0.0.1:7545/",
                "role": "leader",
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
                "discovered": false,
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

        let value = serde_json::to_value(
            SequencerStatusJson::from_snapshot("devnet", &snapshot, Some("op-conductor-1"))
                .unwrap(),
        )
        .unwrap();

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

        let value = serde_json::to_value(
            SequencerStatusJson::from_snapshot("devnet", &snapshot, None).unwrap(),
        )
        .unwrap();

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
            SequencerCommandError::NodeLookup(NodeLookupError::MissingNode {
                requested_node,
                available_nodes,
            }) if requested_node == "op-conductor-1"
                && available_nodes == vec!["op-conductor-0".to_string()]
        ));
    }
}
