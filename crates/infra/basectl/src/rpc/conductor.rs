use std::{collections::BTreeSet, sync::Arc, time::Duration};

use base_consensus_rpc::{
    AdminApiClient, BaseP2PApiClient, ClusterMembership, ConductorApiClient, RollupNodeApiClient,
    ServerSuffrage,
};
use futures::{StreamExt, stream, stream::FuturesUnordered};
use jsonrpsee::{
    core::client::{ClientT, Error as JsonRpcClientError},
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::{ConductorNodeConfig, ConductorSource};

/// Live status snapshot for a single node in an HA conductor cluster.
#[derive(Debug, Clone)]
pub struct ConductorNodeStatus {
    /// Human-readable name for this node.
    pub name: String,

    // ── Conductor ────────────────────────────────────────────────────────
    /// Whether this node is the Raft leader. `None` means the node is unreachable.
    pub is_leader: Option<bool>,
    /// Whether the conductor's sequencer is actively sequencing (`conductor_active`).
    /// Expected to be `false` for followers. `None` means unreachable.
    pub conductor_active: Option<bool>,
    /// Whether op-conductor's control loop is paused (`conductor_paused`). When paused,
    /// the conductor stops driving leader election and health checks. `None` means
    /// unreachable.
    pub conductor_paused: Option<bool>,
    /// Whether op-conductor has been fully stopped (`conductor_stopped`). `None` means
    /// unreachable.
    pub conductor_stopped: Option<bool>,
    /// Whether the sequencer is reporting healthy via `conductor_sequencerHealthy`.
    /// `None` means unreachable.
    pub sequencer_healthy: Option<bool>,
    /// Whether the sequencer is currently producing blocks (`admin_sequencerActive`).
    /// Sourced from the consensus node's admin namespace on `cl_rpc`. `None` means
    /// unreachable.
    pub sequencer_active: Option<bool>,

    // ── CL (consensus layer) ─────────────────────────────────────────────
    /// Unsafe L2 block number from `optimism_syncStatus`.
    pub unsafe_l2_block: Option<u64>,
    /// Unsafe L2 block hash from `optimism_syncStatus`.
    pub unsafe_l2_hash: Option<alloy_primitives::B256>,
    /// Safe L2 block number from `optimism_syncStatus`.
    pub safe_l2_block: Option<u64>,
    /// Safe L2 block hash from `optimism_syncStatus`.
    pub safe_l2_hash: Option<alloy_primitives::B256>,
    /// Finalized L2 block number from `optimism_syncStatus`.
    pub finalized_l2_block: Option<u64>,
    /// L1 derivation cursor block number (`current_l1`).
    pub current_l1_block: Option<u64>,
    /// L1 chain head block number (`head_l1`). Compared with `current_l1_block` to show lag.
    pub head_l1_block: Option<u64>,
    /// Number of connected CL libp2p peers from `opp2p_peerStats`.
    pub cl_peer_count: Option<u32>,

    // ── EL (execution layer) ─────────────────────────────────────────────
    /// Latest block number from `eth_blockNumber`. `None` if `el_rpc` not configured.
    pub el_block: Option<u64>,
    /// Whether the EL is snap-syncing (`eth_syncing` returns non-false). `None` if not
    /// configured.
    pub el_syncing: Option<bool>,
    /// Number of connected EL devp2p peers from `net_peerCount`. `None` if not configured.
    pub el_peer_count: Option<u32>,

    // ── Cluster membership ───────────────────────────────────────────────
    /// Raft suffrage (Voter/Nonvoter) reported for this node by the most recent
    /// `conductor_clusterMembership` snapshot, looked up by `server_id`. `None`
    /// when membership has not yet been observed or this node is not present.
    pub suffrage: Option<ServerSuffrage>,
    /// Whether this node was synthesised from `conductor_clusterMembership`
    /// (i.e. the active source is `Discover`). Used by the UI to gate actions
    /// like "Restart containers" that only make sense when basectl runs on the
    /// same host as the docker daemon.
    pub discovered: bool,
}

/// Typed conductor RPC helper surface for non-TUI commands.
#[derive(Debug)]
pub struct ConductorControl;

/// One-shot conductor cluster snapshot.
#[derive(Debug, Clone)]
pub struct ConductorClusterSnapshot {
    /// Effective node configs used for this snapshot.
    ///
    /// For static sources, this is filtered to the live raft membership when
    /// that membership can be both fetched and reconciled with the configured
    /// nodes; otherwise it falls back to the full configured node list.
    pub nodes: Vec<ConductorNodeConfig>,
    /// Per-node status rows fetched from the cluster.
    pub statuses: Vec<ConductorNodeStatus>,
    /// Raft membership observed while fetching the snapshot.
    pub membership: Option<ClusterMembership>,
    /// Error from the best-effort membership lookup or reconciliation for static
    /// snapshots. Set when membership could not be fetched, or when a fetched
    /// membership referenced servers missing from the configured node list and
    /// the snapshot fell back to the configured nodes.
    pub membership_error: Option<String>,
    /// Whether this snapshot was built from discovered raft membership.
    pub discovered: bool,
}

/// Result of running a conductor control RPC across several nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConductorFanoutReport {
    /// Total nodes targeted.
    pub total: usize,
    /// Node names whose RPC succeeded.
    pub successes: Vec<String>,
    /// Node names and errors for failed RPCs.
    pub failures: Vec<ConductorNodeFailure>,
}

/// Per-node fanout failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConductorNodeFailure {
    /// Node name.
    pub name: String,
    /// Error returned while mutating the node.
    pub error: String,
}

impl ConductorFanoutReport {
    /// Returns true only when at least one node was targeted and every RPC succeeded.
    pub const fn is_success(&self) -> bool {
        self.total > 0 && self.failures.is_empty()
    }

    /// Formats the same summary string used by the TUI toast path.
    ///
    /// `verb` is the past-tense action used for success and partial-failure
    /// summaries. Add new verbs to [`empty_fanout_verb`] so the zero-target
    /// branch can render the infinitive form.
    pub fn summary(&self, verb: &str) -> String {
        if self.total == 0 {
            return format!("no conductor nodes to {}", empty_fanout_verb(verb));
        }
        let ok_count = self.successes.len();
        if self.failures.is_empty() {
            format!("conductor {verb} on {ok_count}/{} nodes", self.total)
        } else {
            let detail = self
                .failures
                .iter()
                .map(|failure| format!("{}: {}", failure.name, failure.error))
                .collect::<Vec<_>>()
                .join("; ");
            format!("conductor {verb} on {ok_count}/{} nodes; failures: {detail}", self.total)
        }
    }

    /// Converts the report into the TUI's success-or-warning result shape.
    pub fn to_result(&self, verb: &str) -> Result<String, String> {
        if self.is_success() { Ok(self.summary(verb)) } else { Err(self.summary(verb)) }
    }
}

fn empty_fanout_verb(verb: &str) -> &str {
    match verb {
        "paused" => "pause",
        "resumed" => "resume",
        other => other,
    }
}

impl ConductorControl {
    /// Fetches a one-shot conductor cluster snapshot.
    pub async fn snapshot(source: ConductorSource) -> anyhow::Result<ConductorClusterSnapshot> {
        const RPC_TIMEOUT: Duration = Duration::from_millis(500);

        match &source {
            ConductorSource::Static(static_nodes) => {
                let membership_result = Self::current_membership(&source).await;
                let (membership, mut membership_error) = match membership_result {
                    Ok(membership) => (Some(membership), None),
                    Err(error) => {
                        warn!(error = %error, "failed to fetch conductor cluster membership for static snapshot");
                        (None, Some(error.to_string()))
                    }
                };
                // A fetched membership that references servers missing from the
                // static config cannot be reconciled, but the configured node
                // list is still usable, so degrade to it instead of failing the
                // whole snapshot.
                let nodes = match Self::snapshot_nodes(&source, membership.as_ref()) {
                    Ok(nodes) => nodes,
                    Err(error) => {
                        warn!(error = %error, "failed to resolve conductor nodes from membership for static snapshot; falling back to configured node list");
                        membership_error.get_or_insert_with(|| error.to_string());
                        static_nodes.clone()
                    }
                };
                let clients = build_conductor_clients(&nodes, RPC_TIMEOUT);
                let statuses = fetch_conductor_statuses(&clients, membership.as_ref(), false).await;
                Ok(ConductorClusterSnapshot {
                    nodes,
                    statuses,
                    membership,
                    membership_error,
                    discovered: false,
                })
            }
            ConductorSource::Discover { .. } => {
                let membership = Self::current_membership(&source).await?;
                let nodes = Self::snapshot_nodes(&source, Some(&membership))?;
                let clients = build_conductor_clients(&nodes, RPC_TIMEOUT);
                let statuses = fetch_conductor_statuses(&clients, Some(&membership), true).await;
                Ok(ConductorClusterSnapshot {
                    nodes,
                    statuses,
                    membership: Some(membership),
                    membership_error: None,
                    discovered: true,
                })
            }
        }
    }

    fn snapshot_nodes(
        source: &ConductorSource,
        membership: Option<&ClusterMembership>,
    ) -> anyhow::Result<Vec<ConductorNodeConfig>> {
        match membership {
            Some(membership) => Self::nodes_from_membership(source, membership),
            None => match source {
                ConductorSource::Static(nodes) => Ok(nodes.clone()),
                ConductorSource::Discover { .. } => {
                    anyhow::bail!(
                        "conductor cluster membership is required for discovered snapshots"
                    )
                }
            },
        }
    }

    /// Fetches live raft membership from the configured conductor source.
    pub async fn current_membership(source: &ConductorSource) -> anyhow::Result<ClusterMembership> {
        const TIMEOUT: Duration = Duration::from_millis(500);

        let nodes: Vec<ConductorNodeConfig> = match source {
            ConductorSource::Static(nodes) => nodes.clone(),
            ConductorSource::Discover { .. } => source.bootstrap_node().into_iter().collect(),
        };
        if nodes.is_empty() {
            anyhow::bail!("no conductor nodes configured");
        }

        let mut probes: FuturesUnordered<_> = nodes
            .iter()
            .map(|node| async move {
                let client = HttpClientBuilder::default()
                    .request_timeout(TIMEOUT)
                    .build(node.conductor_rpc.as_str())
                    .map_err(|error| format!("{}: {error}", node.name))?;
                ConductorApiClient::conductor_cluster_membership(&client)
                    .await
                    .map_err(|error| format!("{}: {error}", node.name))
            })
            .collect();

        let mut failures = Vec::new();
        while let Some(result) = probes.next().await {
            match result {
                Ok(membership) => return Ok(membership),
                Err(error) => failures.push(error),
            }
        }

        anyhow::bail!(
            "failed to fetch conductor cluster membership from any node: {}",
            failures.join("; ")
        )
    }

    /// Resolves node configs for every server in a live raft membership snapshot.
    pub fn nodes_from_membership(
        source: &ConductorSource,
        membership: &ClusterMembership,
    ) -> anyhow::Result<Vec<ConductorNodeConfig>> {
        match source {
            ConductorSource::Discover { .. } => {
                source.synthesize_nodes(membership).filter(|nodes| !nodes.is_empty()).ok_or_else(
                    || anyhow::anyhow!("failed to synthesize conductor nodes from membership"),
                )
            }
            ConductorSource::Static(configured) => {
                let mut nodes = Vec::new();
                let mut missing = Vec::new();
                for server in &membership.servers {
                    if let Some(node) = configured.iter().find(|node| node.server_id == server.id) {
                        nodes.push(node.clone());
                    } else {
                        missing.push(server.id.clone());
                    }
                }
                if !missing.is_empty() {
                    anyhow::bail!(
                        "raft membership contains server(s) missing from conductor config: {}",
                        missing.join(", ")
                    );
                }
                if nodes.is_empty() {
                    anyhow::bail!("conductor cluster membership is empty");
                }
                Ok(nodes)
            }
        }
    }

    /// Finds the current Raft leader and transfers leadership.
    pub async fn transfer_leader(
        nodes: &[ConductorNodeConfig],
        target_name: Option<&str>,
    ) -> anyhow::Result<String> {
        const TIMEOUT: Duration = Duration::from_millis(500);
        const SUBMIT_ATTEMPTS: usize = 3;

        let target_node = match target_name {
            Some(target) => Some(
                nodes
                    .iter()
                    .find(|n| n.name == target)
                    .ok_or_else(|| anyhow::anyhow!("target node {target} not found"))?,
            ),
            None => None,
        };
        let mut last_error = None;

        for _ in 0..SUBMIT_ATTEMPTS {
            let leader_lookup = find_conductor_leader(nodes, TIMEOUT).await;
            let failure_summary = leader_lookup.failure_summary();
            let Some((leader_name, leader)) = leader_lookup.leader else {
                last_error = Some(
                    failure_summary.unwrap_or_else(|| "no leader found in cluster".to_string()),
                );
                tokio::time::sleep(TIMEOUT).await;
                continue;
            };

            match target_node {
                None => match ConductorApiClient::conductor_transfer_leader(&leader).await {
                    Ok(()) => {
                        let observed = wait_for_stable_leader(
                            nodes,
                            StableLeaderGoal::ReplacementFor(&leader_name),
                        )
                        .await?;
                        return Ok(format!(
                            "leadership transferred from {leader_name} to {observed}"
                        ));
                    }
                    Err(error) if is_stale_leader_error(&error) => {
                        last_error = Some(error.to_string());
                        tokio::time::sleep(TIMEOUT).await;
                    }
                    Err(error) => return Err(anyhow::anyhow!("{error}")),
                },
                Some(target_node) => {
                    if leader_name == target_node.name.as_str() {
                        return Ok(format!("leadership already on {}", target_node.name));
                    }
                    match ConductorApiClient::conductor_transfer_leader_to_server(
                        &leader,
                        target_node.server_id.clone(),
                        target_node.raft_addr.clone(),
                    )
                    .await
                    {
                        Ok(()) => {
                            let observed = wait_for_stable_leader(
                                nodes,
                                StableLeaderGoal::Specific(target_node.name.as_str()),
                            )
                            .await?;
                            return Ok(format!("leadership transferred to {observed}"));
                        }
                        Err(error) if is_stale_leader_error(&error) => {
                            last_error = Some(error.to_string());
                            tokio::time::sleep(TIMEOUT).await;
                        }
                        Err(error) => return Err(anyhow::anyhow!("{error}")),
                    }
                }
            }
        }

        anyhow::bail!(
            "failed to submit leadership transfer after {SUBMIT_ATTEMPTS} attempts: {}",
            last_error.unwrap_or_else(|| "no leader found in cluster".to_string())
        )
    }

    /// Pauses op-conductor's control loop on a single node.
    pub async fn pause_node(node: &ConductorNodeConfig) -> anyhow::Result<String> {
        const TIMEOUT: Duration = Duration::from_secs(5);

        let client = HttpClientBuilder::default()
            .request_timeout(TIMEOUT)
            .build(node.conductor_rpc.as_str())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        ConductorApiClient::conductor_pause(&client).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(format!("conductor paused on {}", node.name))
    }

    /// Resumes op-conductor's control loop on a single node.
    pub async fn resume_node(node: &ConductorNodeConfig) -> anyhow::Result<String> {
        const TIMEOUT: Duration = Duration::from_secs(5);

        let client = HttpClientBuilder::default()
            .request_timeout(TIMEOUT)
            .build(node.conductor_rpc.as_str())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        ConductorApiClient::conductor_resume(&client).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(format!("conductor resumed on {}", node.name))
    }

    /// Pauses op-conductor's control loop on every node in parallel.
    pub async fn pause_all(nodes: Vec<ConductorNodeConfig>) -> ConductorFanoutReport {
        fan_out_conductor_control(nodes, |client| async move {
            ConductorApiClient::conductor_pause(&client).await.map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await
    }

    /// Resumes op-conductor's control loop on every node in parallel.
    pub async fn resume_all(nodes: Vec<ConductorNodeConfig>) -> ConductorFanoutReport {
        fan_out_conductor_control(nodes, |client| async move {
            ConductorApiClient::conductor_resume(&client).await.map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await
    }
}

#[derive(Debug)]
struct LeaderLookup {
    leader: Option<(String, HttpClient)>,
    failures: Vec<ConductorNodeFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StableLeaderGoal<'a> {
    Specific(&'a str),
    ReplacementFor(&'a str),
}

impl StableLeaderGoal<'_> {
    fn matches(self, observed: &str) -> bool {
        match self {
            Self::Specific(target) => observed == target,
            Self::ReplacementFor(original) => observed != original,
        }
    }

    fn timeout_message(
        self,
        observation_timeout: Duration,
        last_observed: &str,
        probe_suffix: &str,
    ) -> String {
        match self {
            Self::Specific(target) => format!(
                "leadership transfer submitted, but target {target} was not observed as stable leader after {}s (last observed leader: {last_observed}{probe_suffix})",
                observation_timeout.as_secs(),
            ),
            Self::ReplacementFor(original) => format!(
                "leadership transfer submitted, but no stable replacement leader for {original} was observed after {}s (last observed leader: {last_observed}{probe_suffix})",
                observation_timeout.as_secs(),
            ),
        }
    }
}

impl LeaderLookup {
    fn failure_summary(&self) -> Option<String> {
        (!self.failures.is_empty()).then(|| {
            self.failures
                .iter()
                .map(|failure| format!("{}: {}", failure.name, failure.error))
                .collect::<Vec<_>>()
                .join("; ")
        })
    }
}

async fn find_conductor_leader(nodes: &[ConductorNodeConfig], timeout: Duration) -> LeaderLookup {
    let results = futures::future::join_all(nodes.iter().map(|node| async move {
        let client = HttpClientBuilder::default()
            .request_timeout(timeout)
            .build(node.conductor_rpc.as_str())
            .map_err(|error| {
                debug!(error = %error, node = %node.name, rpc = %node.conductor_rpc, "failed to build conductor client for leader probe");
                ConductorNodeFailure { name: node.name.clone(), error: error.to_string() }
            })?;
        let is_leader = ConductorApiClient::conductor_leader(&client).await.map_err(|error| {
            debug!(error = %error, node = %node.name, rpc = %node.conductor_rpc, "failed to probe conductor leader");
            ConductorNodeFailure { name: node.name.clone(), error: error.to_string() }
        })?;
        Ok::<_, ConductorNodeFailure>((node.name.clone(), client, is_leader))
    }))
    .await;

    let mut leader = None;
    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok((name, client, true)) if leader.is_none() => leader = Some((name, client)),
            Ok((name, _, true)) => {
                let first_leader =
                    leader.as_ref().map_or("unknown", |(leader_name, _)| leader_name.as_str());
                warn!(first_leader = %first_leader, node = %name, "multiple nodes report is_leader=true; possible split-brain");
            }
            Ok(_) => {}
            Err(failure) => failures.push(failure),
        }
    }

    LeaderLookup { leader, failures }
}

async fn wait_for_stable_leader(
    nodes: &[ConductorNodeConfig],
    goal: StableLeaderGoal<'_>,
) -> anyhow::Result<String> {
    const LEADER_RPC_TIMEOUT: Duration = Duration::from_millis(500);
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    // Keep a short stabilization barrier so back-to-back conductor actions do
    // not race leader churn, while still returning quickly once leadership settles.
    const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(6);
    const STABLE_OBSERVATIONS: usize = 2;

    let deadline = tokio::time::sleep(OBSERVATION_TIMEOUT);
    tokio::pin!(deadline);
    let mut last_observed = None;
    let mut last_probe_error = None;
    let mut stable_name: Option<String> = None;
    let mut stable_observations = 0usize;

    loop {
        let leader_lookup = tokio::select! {
            _ = &mut deadline => break,
            leader_lookup = find_conductor_leader(nodes, LEADER_RPC_TIMEOUT) => leader_lookup,
        };
        let failure_summary = leader_lookup.failure_summary();
        let observed = leader_lookup.leader.map(|(name, _)| name);
        if let Some(name) = observed {
            last_observed = Some(name.clone());
            let _ = last_probe_error.take();
            if goal.matches(name.as_str()) {
                if stable_name.as_deref() == Some(name.as_str()) {
                    stable_observations += 1;
                } else {
                    stable_name = Some(name.clone());
                    stable_observations = 1;
                }
                if stable_observations >= STABLE_OBSERVATIONS {
                    return Ok(name);
                }
            } else {
                stable_name = None;
                stable_observations = 0;
            }
        } else {
            last_probe_error = failure_summary;
            stable_name = None;
            stable_observations = 0;
        }

        tokio::select! {
            _ = &mut deadline => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }

    let last = last_observed.as_deref().unwrap_or("none");
    let probe_suffix = last_probe_error
        .as_deref()
        .map_or_else(String::new, |error| format!("; last probe errors: {error}"));
    anyhow::bail!(goal.timeout_message(OBSERVATION_TIMEOUT, last, &probe_suffix))
}

/// Returns whether `error` means the targeted conductor is no longer the raft
/// leader, used to drive bounded leadership-transfer retries.
///
/// Classification is by message content rather than JSON-RPC error code on
/// purpose. This runs against op-conductor's leader-transfer RPCs, which surface
/// the not-leader condition under several codes (e.g. `-32000` directly and the
/// generic `-32603` "internal error" when wrapped), so the code is not a
/// reliable signal and gating on it would miss legitimate stale-leader errors.
/// [`is_stale_leader_message`] instead keeps false positives in check with a
/// strict filler-word allowlist (so e.g. "not authorized as cluster leader" does
/// not match), and the call site only retries a bounded number of times.
fn is_stale_leader_error(error: &JsonRpcClientError) -> bool {
    match error {
        JsonRpcClientError::Call(payload) => is_stale_leader_message(payload.message()),
        JsonRpcClientError::Custom(message) => is_stale_leader_message(message),
        JsonRpcClientError::RestartNeeded(inner) => is_stale_leader_error(inner),
        _ => false,
    }
}

fn is_stale_leader_message(message: &str) -> bool {
    let normalized =
        message
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { ' ' }
            })
            .collect::<String>();
    let words = normalized.split_whitespace().collect::<Vec<_>>();

    words.windows(2).any(|window| window == ["stale", "leader"])
        || words.windows(4).any(|window| window == ["no", "longer", "the", "leader"])
        || words.iter().enumerate().any(|(index, word)| {
            if *word != "not" {
                return false;
            }

            let tail = &words[index + 1..];
            let Some(leader_offset) = tail.iter().position(|candidate| *candidate == "leader")
            else {
                return false;
            };
            let fillers = &tail[..leader_offset];
            fillers.len() <= 3
                && fillers.iter().all(|candidate| {
                    matches!(
                        *candidate,
                        "the"
                            | "a"
                            | "an"
                            | "current"
                            | "currently"
                            | "conductor"
                            | "raft"
                            | "cluster"
                    )
                })
        })
}

/// Finds the current Raft leader and transfers leadership.
///
/// If `target_name` is `None`, leadership is transferred to any available peer
/// (`conductor_transferLeader`). If `target_name` is `Some(name)`, leadership
/// is transferred to the named node via `conductor_transferLeaderToServer`.
///
/// The result — `Ok(description)` or `Err(message)` — is sent to `result_tx`.
pub async fn transfer_conductor_leader(
    nodes: Vec<ConductorNodeConfig>,
    target_name: Option<String>,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    let outcome = ConductorControl::transfer_leader(&nodes, target_name.as_deref()).await;

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}

/// Pauses op-conductor's control loop on a single node via `conductor_pause`.
///
/// While paused, the conductor stops driving leader election and sequencer
/// health checks, but the underlying Raft membership is preserved. Paired with
/// [`conductor_resume_node`].
pub async fn conductor_pause_node(
    node: ConductorNodeConfig,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    let outcome = ConductorControl::pause_node(&node).await;

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}

/// Resumes op-conductor's control loop on a single node via `conductor_resume`.
pub async fn conductor_resume_node(
    node: ConductorNodeConfig,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    let outcome = ConductorControl::resume_node(&node).await;

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}

/// Pauses op-conductor's control loop on every node in `nodes` in parallel.
///
/// Returns a single summary string suitable for a toast. Per-node errors are
/// collated into the summary so an operator sees both the success count and
/// the names of any nodes that failed. Returns `Ok` only when every node
/// succeeded; otherwise returns `Err` with the same summary text so the TUI
/// surfaces it as a warning.
pub async fn conductor_pause_all_nodes(
    nodes: Vec<ConductorNodeConfig>,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    let summary = ConductorControl::pause_all(nodes).await.to_result("paused");
    let _ = result_tx.send(summary).await;
}

/// Resumes op-conductor's control loop on every node in `nodes` in parallel.
///
/// Mirrors [`conductor_pause_all_nodes`] in error handling and summary format.
pub async fn conductor_resume_all_nodes(
    nodes: Vec<ConductorNodeConfig>,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    let summary = ConductorControl::resume_all(nodes).await.to_result("resumed");
    let _ = result_tx.send(summary).await;
}

/// Runs a per-node conductor control RPC against every node concurrently.
async fn fan_out_conductor_control<F, Fut>(
    nodes: Vec<ConductorNodeConfig>,
    call: F,
) -> ConductorFanoutReport
where
    F: Fn(jsonrpsee::http_client::HttpClient) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
{
    const TIMEOUT: Duration = Duration::from_secs(5);

    if nodes.is_empty() {
        return ConductorFanoutReport { total: 0, successes: Vec::new(), failures: Vec::new() };
    }
    let total = nodes.len();

    let results: Vec<(String, anyhow::Result<()>)> = stream::iter(nodes)
        .map(|node| {
            let call = call.clone();
            async move {
                let outcome: anyhow::Result<()> = async {
                    let client = HttpClientBuilder::default()
                        .request_timeout(TIMEOUT)
                        .build(node.conductor_rpc.as_str())
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    call(client).await
                }
                .await;
                (node.name, outcome)
            }
        })
        .buffer_unordered(total.max(1))
        .collect()
        .await;

    let mut successes = Vec::new();
    let mut failures = Vec::new();
    for (name, result) in results {
        match result {
            Ok(()) => successes.push(name),
            Err(error) => failures.push(ConductorNodeFailure { name, error: error.to_string() }),
        }
    }
    successes.sort();
    failures.sort_by(|a, b| a.name.cmp(&b.name));

    ConductorFanoutReport { total, successes, failures }
}

/// Restarts the docker containers for a single conductor cluster node.
///
/// Containers are restarted in dependency order — EL → CL → conductor —
/// waiting for each to become healthy before starting the next. This prevents
/// op-conductor from crashing on startup because it tries to connect to the EL
/// before the EL has bound its port.
pub async fn restart_conductor_node(
    node: ConductorNodeConfig,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    // Dependency order: EL must be healthy before CL starts, CL before conductor.
    let ordered: &[Option<&str>] =
        &[node.docker_el.as_deref(), node.docker_cl.as_deref(), node.docker_conductor.as_deref()];
    let containers: Vec<&str> = ordered.iter().filter_map(|c| *c).collect();

    let outcome: anyhow::Result<String> = async {
        if containers.is_empty() {
            return Err(anyhow::anyhow!("no docker containers configured for {}", node.name));
        }

        for container in &containers {
            // Restart this container.
            let out = tokio::process::Command::new("docker")
                .args(["restart", container])
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("docker restart {container}: {e}"))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                return Err(anyhow::anyhow!(
                    "docker restart {container} failed: {}",
                    stderr.trim()
                ));
            }

            // Wait until Docker reports the container as healthy (or running if
            // no healthcheck is defined) before moving to the next dependency.
            for _ in 0..60u32 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let status = tokio::process::Command::new("docker")
                    .args(["inspect", "--format", "{{.State.Health.Status}}", container])
                    .output()
                    .await
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok());
                match status.as_deref().map(str::trim) {
                    Some("healthy") => break,
                    // Container has no healthcheck — treat "running" as ready.
                    Some("") | None => {
                        let running = tokio::process::Command::new("docker")
                            .args(["inspect", "--format", "{{.State.Running}}", container])
                            .output()
                            .await
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok());
                        if running.as_deref().map(str::trim) == Some("true") {
                            break;
                        }
                    }
                    _ => {} // starting / unhealthy — keep waiting
                }
            }
        }

        Ok(format!("restarted {} ({})", node.name, containers.join(" → ")))
    }
    .await;

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}

/// Peers saved when a sequencer node is paused, used to restore connectivity on unpause.
#[derive(Debug, Clone, Default)]
pub struct PausedPeers {
    /// Multiaddrs of the CL peers that were connected before pausing.
    /// Used to reconnect them on unpause via `opp2p_connectPeer`.
    pub cl_addrs: Vec<String>,
    /// Enode URLs of the EL peers that were connected before pausing.
    /// Used to re-add them on unpause via `admin_addPeer`.
    pub el_enodes: Vec<String>,
}

/// Updates emitted by [`run_conductor_poller`] on every poll cycle.
#[derive(Debug, Clone)]
pub enum ConductorPollUpdate {
    /// Latest per-node status snapshot.
    Status(Vec<ConductorNodeStatus>),
    /// Raft cluster membership reported by one of the polled nodes. Emitted
    /// only when the membership `version` advances. Wrapped in `Arc` so the
    /// poller and the UI state share the snapshot without deep-copying the
    /// server list on every change.
    Membership(Arc<ClusterMembership>),
    /// New peer list synthesised from a `Discover` source after a membership
    /// change. Subscribers may use this to update displayed config (e.g.
    /// flashblocks URL routing) without restarting the poller.
    NodeListRefreshed(Vec<ConductorNodeConfig>),
}

type ConductorClientTuple = (
    String,
    String,
    jsonrpsee::http_client::HttpClient,
    jsonrpsee::http_client::HttpClient,
    Option<jsonrpsee::http_client::HttpClient>,
);

fn build_conductor_clients(
    nodes: &[ConductorNodeConfig],
    timeout: Duration,
) -> Vec<ConductorClientTuple> {
    nodes
        .iter()
        .filter_map(|node| {
            let conductor_client = HttpClientBuilder::default()
                .request_timeout(timeout)
                .build(node.conductor_rpc.as_str())
                .inspect_err(|e| {
                    warn!(error = %e, node = %node.name, "failed to build conductor HTTP client");
                })
                .ok()?;
            let cl_client = HttpClientBuilder::default()
                .request_timeout(timeout)
                .build(node.cl_rpc.as_str())
                .inspect_err(|e| {
                    warn!(error = %e, node = %node.name, "failed to build CL HTTP client");
                })
                .ok()?;
            let el_client = node.el_rpc.as_ref().and_then(|url| {
                HttpClientBuilder::default()
                    .request_timeout(timeout)
                    .build(url.as_str())
                    .inspect_err(|e| {
                        warn!(error = %e, node = %node.name, "failed to build EL HTTP client");
                    })
                    .ok()
            });
            Some((
                node.name.clone(),
                node.server_id.clone(),
                conductor_client,
                cl_client,
                el_client,
            ))
        })
        .collect()
}

async fn fetch_conductor_statuses(
    clients: &[ConductorClientTuple],
    membership_for_lookup: Option<&ClusterMembership>,
    discovered: bool,
) -> Vec<ConductorNodeStatus> {
    futures::future::join_all(clients.iter().map(
        |(name, server_id, conductor_client, cl_client, el_client)| async move {
            // Fire all RPCs concurrently so a single timed-out node does not
            // stall the poll for the full sum of all call timeouts.
            let (
                is_leader,
                conductor_active,
                conductor_paused,
                conductor_stopped,
                sequencer_healthy,
                sequencer_active,
                sync,
                cl_peer_stats,
                el_block_r,
                el_syncing_r,
                el_peers_r,
            ) = tokio::join!(
                ConductorApiClient::conductor_leader(conductor_client),
                ConductorApiClient::conductor_active(conductor_client),
                ConductorApiClient::conductor_paused(conductor_client),
                ConductorApiClient::conductor_stopped(conductor_client),
                ConductorApiClient::conductor_sequencer_healthy(conductor_client),
                AdminApiClient::admin_sequencer_active(cl_client),
                RollupNodeApiClient::sync_status(cl_client),
                BaseP2PApiClient::opp2p_peer_stats(cl_client),
                async {
                    if let Some(el) = el_client {
                        let r: Result<alloy_primitives::U64, _> =
                            ClientT::request(el, "eth_blockNumber", rpc_params![]).await;
                        r.ok().map(|v| v.to::<u64>())
                    } else {
                        None
                    }
                },
                async {
                    if let Some(el) = el_client {
                        let r: Result<serde_json::Value, _> =
                            ClientT::request(el, "eth_syncing", rpc_params![]).await;
                        r.ok().map(|v| !matches!(v, serde_json::Value::Bool(false)))
                    } else {
                        None
                    }
                },
                async {
                    if let Some(el) = el_client {
                        let r: Result<alloy_primitives::U64, _> =
                            ClientT::request(el, "net_peerCount", rpc_params![]).await;
                        r.ok().map(|v| v.to::<u32>())
                    } else {
                        None
                    }
                },
            );

            let sync = sync.ok();
            let suffrage = membership_for_lookup
                .and_then(|m| m.servers.iter().find(|s| s.id == *server_id))
                .map(|s| s.suffrage);
            ConductorNodeStatus {
                name: name.clone(),
                is_leader: is_leader.ok(),
                conductor_active: conductor_active.ok(),
                conductor_paused: conductor_paused.ok(),
                conductor_stopped: conductor_stopped.ok(),
                sequencer_healthy: sequencer_healthy.ok(),
                sequencer_active: sequencer_active.ok(),
                unsafe_l2_block: sync.as_ref().map(|s| s.unsafe_l2.block_info.number),
                unsafe_l2_hash: sync.as_ref().map(|s| s.unsafe_l2.block_info.hash),
                safe_l2_block: sync.as_ref().map(|s| s.safe_l2.block_info.number),
                safe_l2_hash: sync.as_ref().map(|s| s.safe_l2.block_info.hash),
                finalized_l2_block: sync.as_ref().map(|s| s.finalized_l2.block_info.number),
                current_l1_block: sync.as_ref().map(|s| s.current_l1.number),
                head_l1_block: sync.as_ref().map(|s| s.head_l1.number),
                cl_peer_count: cl_peer_stats.ok().map(|s| s.connected),
                el_block: el_block_r,
                el_syncing: el_syncing_r,
                el_peer_count: el_peers_r,
                suffrage,
                discovered,
            }
        },
    ))
    .await
}

/// Polls every conductor in the active source every 200 ms and forwards updates.
///
/// Builds one pair of HTTP clients per node (conductor RPC + CL RPC) so connection
/// setup cost is paid only once per node lifetime. Each poll fires all per-node
/// requests concurrently via [`futures::future::join_all`]; any individual RPC that
/// times out or errors yields `None` for that field — the node is shown as offline
/// when `is_leader` is `None`.
///
/// On every tick the poller also calls `conductor_clusterMembership` on one node
/// (round-robin) and, when the membership version advances, emits a `Membership`
/// update. For `Discover` sources, the synthesised peer list is rebuilt from the
/// new membership and the per-node clients are recreated in place.
pub async fn run_conductor_poller(source: ConductorSource, tx: mpsc::Sender<ConductorPollUpdate>) {
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    const RPC_TIMEOUT: Duration = Duration::from_millis(500);

    let discovered = source.is_discover();

    let mut current_nodes: Vec<ConductorNodeConfig> = match &source {
        ConductorSource::Static(nodes) => nodes.clone(),
        ConductorSource::Discover { .. } => source.bootstrap_node().into_iter().collect(),
    };
    let mut clients = build_conductor_clients(&current_nodes, RPC_TIMEOUT);
    let mut last_membership: Option<Arc<ClusterMembership>> = None;
    let mut membership_round_robin: usize = 0;

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let membership_target = if clients.is_empty() {
            None
        } else {
            let idx = membership_round_robin % clients.len();
            membership_round_robin = membership_round_robin.wrapping_add(1);
            Some(idx)
        };

        let membership_fut = async {
            let idx = membership_target?;
            let (_, _, conductor_client, _, _) = &clients[idx];
            ConductorApiClient::conductor_cluster_membership(conductor_client).await.ok()
        };

        let statuses_fut =
            fetch_conductor_statuses(&clients, last_membership.as_deref(), discovered);

        let (statuses, new_membership) = tokio::join!(statuses_fut, membership_fut);

        // Send Status first so the UI flushes the statuses keyed to the
        // current node set before we potentially swap that set out below.
        if tx.send(ConductorPollUpdate::Status(statuses)).await.is_err() {
            break;
        }

        if let Some(membership) = new_membership {
            let changed =
                last_membership.as_ref().is_none_or(|prev| prev.version != membership.version);
            if changed {
                let membership = Arc::new(membership);
                if tx.send(ConductorPollUpdate::Membership(Arc::clone(&membership))).await.is_err()
                {
                    break;
                }
                if let Some(synthesized) = source.synthesize_nodes(&membership) {
                    let old_ids: BTreeSet<_> = current_nodes.iter().map(|n| &n.server_id).collect();
                    let new_ids: BTreeSet<_> = synthesized.iter().map(|n| &n.server_id).collect();
                    if old_ids != new_ids && !synthesized.is_empty() {
                        current_nodes = synthesized.clone();
                        clients = build_conductor_clients(&current_nodes, RPC_TIMEOUT);
                        if tx
                            .send(ConductorPollUpdate::NodeListRefreshed(synthesized))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                last_membership = Some(membership);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use base_consensus_rpc::{ClusterMembership, ServerInfo, ServerSuffrage};
    use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorObjectOwned};
    use url::Url;

    use super::{
        ConductorControl, ConductorFanoutReport, ConductorNodeFailure, LeaderLookup,
        StableLeaderGoal, is_stale_leader_error,
    };
    use crate::config::{ConductorNodeConfig, ConductorSource};

    fn node(name: &str, server_id: &str) -> ConductorNodeConfig {
        ConductorNodeConfig {
            name: name.to_string(),
            conductor_rpc: Url::parse("http://127.0.0.1:6545").unwrap(),
            cl_rpc: Url::parse("http://127.0.0.1:7545").unwrap(),
            server_id: server_id.to_string(),
            raft_addr: format!("{name}:5050"),
            el_rpc: None,
            docker_conductor: None,
            docker_el: None,
            docker_cl: None,
            flashblocks_ws: None,
        }
    }

    fn membership(ids: &[&str]) -> ClusterMembership {
        ClusterMembership {
            version: 7,
            servers: ids
                .iter()
                .map(|id| ServerInfo {
                    id: (*id).to_string(),
                    addr: format!("{id}:5050"),
                    suffrage: ServerSuffrage::Voter,
                })
                .collect(),
        }
    }

    #[test]
    fn fanout_summary_formats_success() {
        let report = ConductorFanoutReport {
            total: 2,
            successes: vec!["a".to_string(), "b".to_string()],
            failures: Vec::new(),
        };

        assert!(report.is_success());
        assert_eq!(report.summary("paused"), "conductor paused on 2/2 nodes");
        assert_eq!(report.to_result("paused").unwrap(), "conductor paused on 2/2 nodes");
    }

    #[test]
    fn fanout_summary_formats_partial_failure() {
        let report = ConductorFanoutReport {
            total: 2,
            successes: vec!["a".to_string()],
            failures: vec![ConductorNodeFailure {
                name: "b".to_string(),
                error: "request timed out".to_string(),
            }],
        };

        assert!(!report.is_success());
        assert_eq!(
            report.summary("paused"),
            "conductor paused on 1/2 nodes; failures: b: request timed out"
        );
        assert!(report.to_result("paused").is_err());
    }

    #[test]
    fn fanout_summary_formats_empty_nodes() {
        let report =
            ConductorFanoutReport { total: 0, successes: Vec::new(), failures: Vec::new() };

        assert!(!report.is_success());
        assert_eq!(report.summary("paused"), "no conductor nodes to pause");
    }

    #[test]
    fn nodes_from_membership_maps_static_nodes_by_server_id() {
        let source = ConductorSource::Static(vec![node("op-conductor-0", "sequencer-0")]);
        let membership = membership(&["sequencer-0"]);

        let nodes = ConductorControl::nodes_from_membership(&source, &membership).unwrap();

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "op-conductor-0");
    }

    #[test]
    fn nodes_from_membership_rejects_unknown_static_member() {
        let source = ConductorSource::Static(vec![node("op-conductor-0", "sequencer-0")]);
        let membership = membership(&["sequencer-1"]);

        let err = ConductorControl::nodes_from_membership(&source, &membership)
            .expect_err("unknown member should fail");

        assert!(err.to_string().contains("sequencer-1"));
    }

    #[test]
    fn snapshot_nodes_filter_static_source_to_live_membership() {
        let source = ConductorSource::Static(vec![
            node("op-conductor-0", "sequencer-0"),
            node("op-conductor-1", "sequencer-1"),
        ]);
        let membership = membership(&["sequencer-1"]);

        let nodes = ConductorControl::snapshot_nodes(&source, Some(&membership)).unwrap();

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "op-conductor-1");
        assert_eq!(nodes[0].server_id, "sequencer-1");
    }

    #[test]
    fn snapshot_nodes_fall_back_to_static_config_when_membership_is_unavailable() {
        let source = ConductorSource::Static(vec![
            node("op-conductor-0", "sequencer-0"),
            node("op-conductor-1", "sequencer-1"),
        ]);

        let nodes = ConductorControl::snapshot_nodes(&source, None).unwrap();

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "op-conductor-0");
        assert_eq!(nodes[1].name, "op-conductor-1");
    }

    #[test]
    fn leader_lookup_failure_summary_formats_node_errors() {
        let lookup = LeaderLookup {
            leader: None,
            failures: vec![
                ConductorNodeFailure { name: "a".to_string(), error: "timeout".to_string() },
                ConductorNodeFailure {
                    name: "b".to_string(),
                    error: "connection refused".to_string(),
                },
            ],
        };

        assert_eq!(lookup.failure_summary().as_deref(), Some("a: timeout; b: connection refused"));
    }

    #[test]
    fn stable_leader_goal_matches_expected_observations() {
        assert!(StableLeaderGoal::Specific("op-conductor-1").matches("op-conductor-1"));
        assert!(!StableLeaderGoal::Specific("op-conductor-1").matches("op-conductor-0"));
        assert!(StableLeaderGoal::ReplacementFor("op-conductor-0").matches("op-conductor-1"));
        assert!(!StableLeaderGoal::ReplacementFor("op-conductor-0").matches("op-conductor-0"));
    }

    #[test]
    fn stable_leader_goal_formats_timeout_messages() {
        let targeted = StableLeaderGoal::Specific("op-conductor-1").timeout_message(
            Duration::from_secs(6),
            "op-conductor-0",
            "; last probe errors: timeout",
        );
        let replacement = StableLeaderGoal::ReplacementFor("op-conductor-0").timeout_message(
            Duration::from_secs(6),
            "op-conductor-0",
            "",
        );

        assert!(
            targeted.contains("target op-conductor-1 was not observed as stable leader after 6s")
        );
        assert!(
            targeted.contains("last observed leader: op-conductor-0; last probe errors: timeout")
        );
        assert!(
            replacement
                .contains("no stable replacement leader for op-conductor-0 was observed after 6s")
        );
    }

    #[test]
    fn stale_leader_error_matches_message_variants() {
        let stale = JsonRpcClientError::Call(ErrorObjectOwned::owned(
            -32000,
            "node is not the leader",
            None::<()>,
        ));
        let capitalized = JsonRpcClientError::Call(ErrorObjectOwned::owned(
            -32000,
            "Node is not the leader.",
            None::<()>,
        ));
        let wrapped = JsonRpcClientError::RestartNeeded(Arc::new(JsonRpcClientError::Call(
            ErrorObjectOwned::owned(
                -32603,
                "node is not currently the conductor leader",
                None::<()>,
            ),
        )));
        let stale_phrase = JsonRpcClientError::Custom(
            "leadership transfer failed because this node is no longer the leader".to_string(),
        );
        let unrelated = JsonRpcClientError::Call(ErrorObjectOwned::owned(
            -32603,
            "leader could not be contacted",
            None::<()>,
        ));
        // Even under the canonical stale-leader code, an unrelated "not ... leader"
        // phrase is rejected by the filler-word allowlist (no code-based fallback).
        let unrelated_role = JsonRpcClientError::Call(ErrorObjectOwned::owned(
            -32000,
            "not authorized as cluster leader",
            None::<()>,
        ));

        assert!(is_stale_leader_error(&stale));
        assert!(is_stale_leader_error(&capitalized));
        assert!(is_stale_leader_error(&wrapped));
        assert!(is_stale_leader_error(&stale_phrase));
        assert!(!is_stale_leader_error(&unrelated));
        assert!(!is_stale_leader_error(&unrelated_role));
    }
}
