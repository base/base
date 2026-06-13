//! Implementation of the `basectl sequencer` command group.

use std::{
    future::Future,
    io::{self, Write},
    str::FromStr,
    time::Duration,
};

use alloy_primitives::B256;
use anyhow::{Context, Result, anyhow, bail};
use basectl_cli::{
    ConductorClusterSnapshot, ConductorControl, ConductorNodeConfig, ConductorNodeStatus,
    ConductorSource, JsonOutput, KeyValueTable, MonitoringConfig, fetch_sequencer_active,
    start_sequencer, stop_sequencer,
};
use serde::Serialize;
use tokio::time::{Instant, sleep, timeout};
use tracing::warn;
use url::Url;

use crate::{
    cli::{SequencerCommands, SequencerNodeActionArgs, SequencerStartArgs, SequencerStatusArgs},
    confirm::confirm_or_abort,
};

const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const REQUIRED_OBSERVATIONS: usize = 2;

/// Runs the `basectl sequencer` command group.
pub(crate) async fn run(
    config: MonitoringConfig,
    conductor_rpc: Option<Url>,
    command: SequencerCommands,
) -> Result<()> {
    let source = resolve_source(&config, conductor_rpc)?;
    match command {
        SequencerCommands::Status(args) => run_status(config, source, args).await,
        SequencerCommands::Start(args) => run_start(config, source, args).await,
        SequencerCommands::Stop(args) => run_stop(config, source, args).await,
    }
}

async fn run_status(
    config: MonitoringConfig,
    source: ConductorSource,
    args: SequencerStatusArgs,
) -> Result<()> {
    let snapshot = ConductorControl::snapshot(source).await?;
    let status = SequencerStatusJson::from_snapshot(&config.name, &snapshot, args.node.as_deref())?;
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
    let snapshot = ConductorControl::snapshot(source.clone()).await?;
    let node = find_node(&snapshot.nodes, &args.node)?;
    let status = snapshot_node_status(&snapshot, &node.name);
    ensure_start_allowed(&snapshot, node, status)?;
    if snapshot.statuses.iter().all(|status| status.is_leader != Some(true))
        && status.and_then(|status| status.is_leader).is_none()
    {
        warn!(
            node = %node.name,
            cl_rpc = %node.cl_rpc,
            "conductor leadership is unknown; deferring start leader validation to server-side RPC"
        );
    }
    let (unsafe_head, unsafe_head_source) =
        resolve_start_hash(&snapshot, node, args.unsafe_head.as_deref())?;
    ensure_start_request_matches_observed_head(status, unsafe_head, unsafe_head_source)?;
    let prompt =
        format!("Start sequencer on {} ({}) at {}? [y/N] ", node.name, node.cl_rpc, unsafe_head);
    if !confirm_or_abort(&prompt, args.yes)? {
        return Ok(());
    }

    start_sequencer(&node.cl_rpc, unsafe_head)
        .await
        .with_context(|| format!("starting sequencer on {} via {}", node.name, node.cl_rpc))?;
    wait_for_expected_state(node, SequencerAction::Start, Some(unsafe_head)).await?;

    let message = format!("sequencer started on {} at {}", node.name, unsafe_head);
    print_action(
        &SequencerActionJson::start(&config.name, node, unsafe_head, unsafe_head_source, message),
        args.json,
    )
}

async fn run_stop(
    config: MonitoringConfig,
    source: ConductorSource,
    args: SequencerNodeActionArgs,
) -> Result<()> {
    let snapshot = ConductorControl::snapshot(source).await?;
    let node = find_node(&snapshot.nodes, &args.node)?;
    let status = snapshot_node_status(&snapshot, &node.name);
    ensure_stop_allowed(node, status)?;
    let prompt = format!("Stop sequencer on {} ({})? [y/N] ", node.name, node.cl_rpc);
    if !confirm_or_abort(&prompt, args.yes)? {
        return Ok(());
    }

    let unsafe_head = stop_sequencer(&node.cl_rpc)
        .await
        .with_context(|| format!("stopping sequencer on {} via {}", node.name, node.cl_rpc))?;
    wait_for_expected_state(node, SequencerAction::Stop, Some(unsafe_head)).await?;

    let message = format!("sequencer stopped on {} at {}", node.name, unsafe_head);
    print_action(&SequencerActionJson::stop(&config.name, node, unsafe_head, message), args.json)
}

fn resolve_source(
    config: &MonitoringConfig,
    conductor_rpc: Option<Url>,
) -> Result<ConductorSource> {
    config.conductor_source(conductor_rpc).ok_or_else(|| {
        anyhow!(
            "sequencer commands need conductor config or a bootstrap RPC URL for '{}'. Set `conductors` or `discovery.bootstrap_rpc` in config, or pass `--conductor-rpc <url>`.",
            config.name
        )
    })
}

fn find_node<'a>(nodes: &'a [ConductorNodeConfig], name: &str) -> Result<&'a ConductorNodeConfig> {
    nodes.iter().find(|node| node.name == name).ok_or_else(|| {
        anyhow!(
            "sequencer node {name} not found. Available nodes: {}",
            nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>().join(", ")
        )
    })
}

fn resolve_start_hash(
    snapshot: &ConductorClusterSnapshot,
    node: &ConductorNodeConfig,
    unsafe_head: Option<&str>,
) -> Result<(B256, UnsafeHeadSource)> {
    match unsafe_head {
        Some(unsafe_head) => Ok((parse_unsafe_head(unsafe_head)?, UnsafeHeadSource::Explicit)),
        None => {
            let hash = snapshot_node_status(snapshot, &node.name)
                .and_then(|status| status.unsafe_l2_hash)
                .filter(|hash| *hash != B256::ZERO)
                .ok_or_else(|| {
                    anyhow!(
                        "could not determine unsafe head for {}; pass an explicit 32-byte hash or restore CL reachability",
                        node.name
                    )
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
) -> Result<()> {
    if status.and_then(|status| status.sequencer_active) == Some(true) {
        bail!("sequencer already active on {}; stop it before starting again", node.name,);
    }
    ensure_leader_target(snapshot, node, status, SequencerAction::Start)
}

fn ensure_stop_allowed(
    node: &ConductorNodeConfig,
    status: Option<&ConductorNodeStatus>,
) -> Result<()> {
    if status.and_then(|status| status.sequencer_active) == Some(false) {
        bail!("sequencer already stopped on {}", node.name);
    }
    Ok(())
}

fn ensure_leader_target(
    snapshot: &ConductorClusterSnapshot,
    node: &ConductorNodeConfig,
    status: Option<&ConductorNodeStatus>,
    action: SequencerAction,
) -> Result<()> {
    let leader = snapshot
        .statuses
        .iter()
        .find(|status| status.is_leader == Some(true))
        .map(|status| status.name.as_str());
    if let Some(leader) = leader {
        if leader == node.name {
            return Ok(());
        }
        bail!(
            "Node is not the conductor leader. Current leader: {leader}. `basectl sequencer {}` must target the leader instead of {}.",
            action.infinitive(),
            node.name,
        );
    }
    if status.and_then(|status| status.is_leader) == Some(false) {
        bail!(
            "Node is not the conductor leader. `basectl sequencer {}` must target the current leader instead of {}.",
            action.infinitive(),
            node.name,
        );
    }
    Ok(())
}

fn ensure_start_request_matches_observed_head(
    status: Option<&ConductorNodeStatus>,
    unsafe_head: B256,
    unsafe_head_source: UnsafeHeadSource,
) -> Result<()> {
    if !matches!(unsafe_head_source, UnsafeHeadSource::Explicit) {
        return Ok(());
    }

    let Some(observed_head) = status.and_then(|status| status.unsafe_l2_hash) else {
        return Ok(());
    };
    if observed_head == B256::ZERO {
        bail!("no prestate: engine unsafe head is uninitialized, cannot safely start sequencer");
    }
    if observed_head != unsafe_head {
        bail!(
            "block hash mismatch: engine unsafe head is {observed_head}, caller requested {unsafe_head}"
        );
    }
    Ok(())
}

fn parse_unsafe_head(raw: &str) -> Result<B256> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("unsafe head hash cannot be empty");
    }

    let normalized = if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        trimmed.to_string()
    } else if trimmed.len() == 64 && trimmed.chars().all(|char| char.is_ascii_hexdigit()) {
        format!("0x{trimmed}")
    } else {
        trimmed.to_string()
    };
    let hash = B256::from_str(&normalized)
        .with_context(|| format!("parsing unsafe head hash `{trimmed}`"))?;
    if hash == B256::ZERO {
        bail!("unsafe head hash must not be zero");
    }
    Ok(hash)
}

async fn wait_for_expected_state(
    node: &ConductorNodeConfig,
    action: SequencerAction,
    unsafe_head: Option<B256>,
) -> Result<()> {
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
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let expected_active = action.expected_active();
    let deadline = Instant::now() + observation_timeout;
    let mut matching_observations = 0usize;
    let mut last_observed = None;
    let mut last_error = None;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, fetch()).await {
            Ok(Ok(is_active)) => {
                last_observed = Some(is_active);
                if last_error.is_some() {
                    last_error = None;
                }
                if is_active == expected_active {
                    matching_observations += 1;
                    if matching_observations >= REQUIRED_OBSERVATIONS {
                        return Ok(());
                    }
                } else {
                    matching_observations = 0;
                }
            }
            Ok(Err(error)) => {
                matching_observations = 0;
                last_error = Some(error.to_string());
            }
            Err(_) => {
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

    bail!(action.timeout_message(node, unsafe_head, last_observed, last_error.as_deref()))
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

fn print_action(action: &SequencerActionJson, json: bool) -> Result<()> {
    if json {
        JsonOutput::print(action)?;
    } else {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "OK {}", action.message)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize)]
enum SequencerAction {
    #[serde(rename = "start")]
    Start,
    #[serde(rename = "stop")]
    Stop,
}

impl SequencerAction {
    const fn expected_active(self) -> bool {
        match self {
            Self::Start => true,
            Self::Stop => false,
        }
    }

    const fn infinitive(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }

    fn timeout_message(
        self,
        node: &ConductorNodeConfig,
        unsafe_head: Option<B256>,
        last_observed: Option<bool>,
        last_error: Option<&str>,
    ) -> String {
        let unsafe_head_suffix =
            unsafe_head.map_or_else(String::new, |unsafe_head| format!(" at {unsafe_head}"));
        let observed_suffix = match (last_observed, last_error) {
            (Some(observed), Some(error)) => {
                format!(" (last observed `sequencer_active={observed}`; last poll error: {error})")
            }
            (Some(observed), None) => format!(" (last observed `sequencer_active={observed}`)"),
            (None, Some(error)) => format!(" (last poll error: {error})"),
            (None, None) => String::new(),
        };

        format!(
            "{} RPC succeeded on {} ({}){}, but `sequencer_active={}` was not observed within {}s{}",
            self.infinitive(),
            node.name,
            node.cl_rpc,
            unsafe_head_suffix,
            self.expected_active(),
            OBSERVATION_TIMEOUT.as_secs(),
            observed_suffix,
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum UnsafeHeadSource {
    Explicit,
    Observed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SequencerActionJson {
    network: String,
    action: SequencerAction,
    node: String,
    cl_rpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unsafe_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unsafe_head_source: Option<UnsafeHeadSource>,
    message: String,
}

impl SequencerActionJson {
    fn start(
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

    fn stop(network: &str, node: &ConductorNodeConfig, unsafe_head: B256, message: String) -> Self {
        Self {
            network: network.to_string(),
            action: SequencerAction::Stop,
            node: node.name.clone(),
            cl_rpc: node.cl_rpc.to_string(),
            unsafe_head: Some(unsafe_head.to_string()),
            unsafe_head_source: None,
            message,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SequencerStatusJson {
    network: String,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    membership_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    membership_error: Option<String>,
    leader: Option<String>,
    nodes: Vec<SequencerNodeJson>,
}

impl SequencerStatusJson {
    fn from_snapshot(
        network: &str,
        snapshot: &ConductorClusterSnapshot,
        selected_node: Option<&str>,
    ) -> Result<Self> {
        if let Some(selected_node) = selected_node {
            find_node(&snapshot.nodes, selected_node)?;
        }

        let nodes = snapshot
            .nodes
            .iter()
            .filter(|node| selected_node.is_none_or(|selected_node| node.name == selected_node))
            .map(|node| {
                let status = snapshot.statuses.iter().find(|status| status.name == node.name);
                SequencerNodeJson::from_node_status(node, status)
            })
            .collect::<Vec<_>>();

        Ok(Self {
            network: network.to_string(),
            source: if snapshot.discovered { "discovered" } else { "static" },
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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SequencerRole {
    Leader,
    Follower,
    Unknown,
}

impl SequencerRole {
    const fn from_is_leader(is_leader: Option<bool>) -> Self {
        match is_leader {
            Some(true) => Self::Leader,
            Some(false) => Self::Follower,
            None => Self::Unknown,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Leader => "leader",
            Self::Follower => "follower",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SequencerNodeJson {
    name: String,
    cl_rpc: String,
    role: SequencerRole,
    is_leader: Option<bool>,
    sequencer_active: Option<bool>,
    sequencer_healthy: Option<bool>,
    conductor_paused: Option<bool>,
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
    discovered: bool,
}

impl SequencerNodeJson {
    fn from_node_status(node: &ConductorNodeConfig, status: Option<&ConductorNodeStatus>) -> Self {
        let is_leader = status.and_then(|status| status.is_leader);
        Self {
            name: node.name.clone(),
            cl_rpc: node.cl_rpc.to_string(),
            role: SequencerRole::from_is_leader(is_leader),
            is_leader,
            sequencer_active: status.and_then(|status| status.sequencer_active),
            sequencer_healthy: status.and_then(|status| status.sequencer_healthy),
            conductor_paused: status.and_then(|status| status.conductor_paused),
            unsafe_l2_block: status.and_then(|status| status.unsafe_l2_block),
            unsafe_l2_hash: status
                .and_then(|status| status.unsafe_l2_hash)
                .map(|unsafe_l2_hash| unsafe_l2_hash.to_string()),
            safe_l2_block: status.and_then(|status| status.safe_l2_block),
            safe_l2_hash: status
                .and_then(|status| status.safe_l2_hash)
                .map(|safe_l2_hash| safe_l2_hash.to_string()),
            finalized_l2_block: status.and_then(|status| status.finalized_l2_block),
            current_l1_block: status.and_then(|status| status.current_l1_block),
            head_l1_block: status.and_then(|status| status.head_l1_block),
            cl_peer_count: status.and_then(|status| status.cl_peer_count),
            el_block: status.and_then(|status| status.el_block),
            el_syncing: status.and_then(|status| status.el_syncing),
            el_peer_count: status.and_then(|status| status.el_peer_count),
            discovered: status.is_some_and(|status| status.discovered),
        }
    }

    fn compact_status(&self) -> String {
        format!(
            "role={} active={} healthy={} paused={} unsafe={} safe={} finalized={} current_l1={} head_l1={} cl_peers={} el_peers={}",
            self.role.as_str(),
            fmt_bool(self.sequencer_active),
            fmt_bool(self.sequencer_healthy),
            fmt_bool(self.conductor_paused),
            fmt_u64(self.unsafe_l2_block),
            fmt_u64(self.safe_l2_block),
            fmt_u64(self.finalized_l2_block),
            fmt_u64(self.current_l1_block),
            fmt_u64(self.head_l1_block),
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
    use basectl_cli::{ConductorClusterSnapshot, ConductorNodeConfig, ConductorNodeStatus};
    use serde_json::json;
    use tokio::time::{Duration, Instant};
    use url::Url;

    use super::{
        SequencerAction, SequencerActionJson, SequencerStatusJson, UnsafeHeadSource,
        ensure_leader_target, ensure_start_allowed, ensure_start_request_matches_observed_head,
        ensure_stop_allowed, find_node, parse_unsafe_head, wait_for_expected_state_with_fetch,
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

        assert!(err.to_string().contains("must not be zero"));
    }

    #[test]
    fn explicit_start_hash_must_match_observed_head() {
        let err = ensure_start_request_matches_observed_head(
            Some(&status("op-conductor-0", true, false)),
            B256::with_last_byte(9),
            UnsafeHeadSource::Explicit,
        )
        .expect_err("mismatched explicit hash should error");

        assert_eq!(
            err.to_string(),
            "block hash mismatch: engine unsafe head is 0x0000000000000000000000000000000000000000000000000000000000000001, caller requested 0x0000000000000000000000000000000000000000000000000000000000000009"
        );
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

        assert_eq!(
            err.to_string(),
            "sequencer already active on op-conductor-0; stop it before starting again"
        );
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

        assert_eq!(
            err.to_string(),
            "Node is not the conductor leader. Current leader: op-conductor-0. `basectl sequencer start` must target the leader instead of op-conductor-1."
        );
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

        assert_eq!(
            err.to_string(),
            "Node is not the conductor leader. Current leader: op-conductor-0. `basectl sequencer start` must target the leader instead of op-conductor-1."
        );
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

        assert_eq!(err.to_string(), "sequencer already stopped on op-conductor-0");
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
                Ok(false)
            },
        )
        .await
        .expect_err("hung fetch should time out");

        assert!(start.elapsed() < Duration::from_millis(120));
        assert!(err.to_string().contains("timed out waiting for admin_sequencerActive"));
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
    fn find_node_reports_missing_name() {
        let nodes = vec![node("op-conductor-0")];

        let err = find_node(&nodes, "op-conductor-1").expect_err("missing node should error");

        assert!(err.to_string().contains("op-conductor-1"));
        assert!(err.to_string().contains("op-conductor-0"));
    }
}
