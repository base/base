use std::{collections::BTreeSet, sync::Arc, time::Duration};

use base_consensus_rpc::{
    AdminApiClient, BaseP2PApiClient, ClusterMembership, ConductorApiClient, RollupNodeApiClient,
    ServerSuffrage,
};
use futures::{StreamExt, stream};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use tokio::sync::mpsc;
use tracing::warn;

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
    const TIMEOUT: Duration = Duration::from_millis(500);

    let outcome: anyhow::Result<String> = async {
        let mut leader_client = None;
        let mut leader_name = String::new();

        for node in &nodes {
            let client = HttpClientBuilder::default()
                .request_timeout(TIMEOUT)
                .build(node.conductor_rpc.as_str())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if ConductorApiClient::conductor_leader(&client).await.unwrap_or(false) {
                leader_client = Some(client);
                leader_name = node.name.clone();
                break;
            }
        }

        let leader = leader_client.ok_or_else(|| anyhow::anyhow!("no leader found in cluster"))?;

        match target_name {
            None => {
                ConductorApiClient::conductor_transfer_leader(&leader)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(format!("leadership transferred from {leader_name}"))
            }
            Some(ref target) => {
                let target_node = nodes
                    .iter()
                    .find(|n| n.name == *target)
                    .ok_or_else(|| anyhow::anyhow!("target node {target} not found"))?;
                ConductorApiClient::conductor_transfer_leader_to_server(
                    &leader,
                    target_node.server_id.clone(),
                    target_node.raft_addr.clone(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(format!("leadership transferred to {target}"))
            }
        }
    }
    .await;

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
    const TIMEOUT: Duration = Duration::from_secs(5);

    let outcome: anyhow::Result<String> = async {
        let client = HttpClientBuilder::default()
            .request_timeout(TIMEOUT)
            .build(node.conductor_rpc.as_str())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        ConductorApiClient::conductor_pause(&client).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(format!("conductor paused on {}", node.name))
    }
    .await;

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}

/// Resumes op-conductor's control loop on a single node via `conductor_resume`.
pub async fn conductor_resume_node(
    node: ConductorNodeConfig,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    const TIMEOUT: Duration = Duration::from_secs(5);

    let outcome: anyhow::Result<String> = async {
        let client = HttpClientBuilder::default()
            .request_timeout(TIMEOUT)
            .build(node.conductor_rpc.as_str())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        ConductorApiClient::conductor_resume(&client).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(format!("conductor resumed on {}", node.name))
    }
    .await;

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
    let summary = fan_out_conductor_control(nodes, "paused", |client| async move {
        ConductorApiClient::conductor_pause(&client).await.map_err(|e| anyhow::anyhow!("{e}"))
    })
    .await;
    let _ = result_tx.send(summary).await;
}

/// Resumes op-conductor's control loop on every node in `nodes` in parallel.
///
/// Mirrors [`conductor_pause_all_nodes`] in error handling and summary format.
pub async fn conductor_resume_all_nodes(
    nodes: Vec<ConductorNodeConfig>,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    let summary = fan_out_conductor_control(nodes, "resumed", |client| async move {
        ConductorApiClient::conductor_resume(&client).await.map_err(|e| anyhow::anyhow!("{e}"))
    })
    .await;
    let _ = result_tx.send(summary).await;
}

/// Runs a per-node conductor control RPC against every node concurrently and
/// builds a single summary toast string.
///
/// `verb` is the past-tense action ("paused" / "resumed") used in the message.
async fn fan_out_conductor_control<F, Fut>(
    nodes: Vec<ConductorNodeConfig>,
    verb: &'static str,
    call: F,
) -> Result<String, String>
where
    F: Fn(jsonrpsee::http_client::HttpClient) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
{
    const TIMEOUT: Duration = Duration::from_secs(5);

    if nodes.is_empty() {
        return Err(format!("no conductor nodes to {verb}"));
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

    let (ok, failures): (Vec<_>, Vec<_>) = results.into_iter().partition(|(_, r)| r.is_ok());
    let ok_count = ok.len();

    if failures.is_empty() {
        Ok(format!("conductor {verb} on {ok_count}/{total} nodes"))
    } else {
        let detail = failures
            .iter()
            .map(|(name, r)| {
                let err = r.as_ref().err().map_or_else(String::new, ToString::to_string);
                format!("{name}: {err}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        Err(format!("conductor {verb} on {ok_count}/{total} nodes; failures: {detail}"))
    }
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

        let membership_for_lookup = last_membership.as_ref();
        let statuses_fut = futures::future::join_all(clients.iter().map(
            |(name, server_id, conductor_client, cl_client, el_client)| async move {
                // Fire all RPCs concurrently so a single timed-out node does not
                // stall the poll for the full sum of all call timeouts (11 × 500 ms).
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
        ));

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
