use std::time::Duration;

use alloy_primitives::B256;
use anyhow::{Context, Result, ensure};
use base_consensus_rpc::{AdminApiClient, BaseP2PApiClient};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use tokio::sync::mpsc;
use tracing::warn;
use url::Url;

use super::PausedPeers;
use crate::config::ConductorNodeConfig;

/// Timeout used when polling `admin_sequencerActive` during convergence checks.
pub const SEQUENCER_ACTIVE_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Returns whether the consensus node reports the sequencer as active.
pub async fn fetch_sequencer_active(cl_rpc: &Url) -> Result<bool> {
    let client = HttpClientBuilder::default()
        .request_timeout(SEQUENCER_ACTIVE_RPC_TIMEOUT)
        .build(cl_rpc.as_str())
        .with_context(|| format!("building consensus admin client for {cl_rpc}"))?;
    AdminApiClient::admin_sequencer_active(&client)
        .await
        .with_context(|| format!("calling admin_sequencerActive on {cl_rpc}"))
}

/// Starts the sequencer via the consensus node's `admin_startSequencer` RPC.
pub async fn start_sequencer(cl_rpc: &Url, unsafe_head: B256) -> Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(5);

    ensure!(unsafe_head != B256::ZERO, "unsafe head must not be zero");
    let client = HttpClientBuilder::default()
        .request_timeout(TIMEOUT)
        .build(cl_rpc.as_str())
        .with_context(|| format!("building consensus admin client for {cl_rpc}"))?;
    AdminApiClient::admin_start_sequencer(&client, unsafe_head)
        .await
        .with_context(|| format!("calling admin_startSequencer on {cl_rpc}"))
}

/// Stops the sequencer via the consensus node's `admin_stopSequencer` RPC.
///
/// Returns the unsafe head hash captured at the moment the sequencer stopped.
/// A returned [`B256::ZERO`] means the sequencer was stopped but the captured
/// head is unavailable; it is not a valid restart point and must not be reused.
/// Because the RPC has already taken effect by the time this returns, an
/// unexpected zero head is surfaced as a warning rather than an error so callers
/// do not treat a successful stop as a failure and retry it.
pub async fn stop_sequencer(cl_rpc: &Url) -> Result<B256> {
    // `admin_stopSequencer` may defer its response until the seal pipeline
    // finishes and the final unsafe head is known.
    const TIMEOUT: Duration = Duration::from_secs(60);

    let client = HttpClientBuilder::default()
        .request_timeout(TIMEOUT)
        .build(cl_rpc.as_str())
        .with_context(|| format!("building consensus admin client for {cl_rpc}"))?;
    let unsafe_head = AdminApiClient::admin_stop_sequencer(&client)
        .await
        .with_context(|| format!("calling admin_stopSequencer on {cl_rpc}"))?;
    if unsafe_head == B256::ZERO {
        warn!(
            cl_rpc = %cl_rpc,
            "admin_stopSequencer returned a zero unsafe head; sequencer stopped but the captured head is unavailable"
        );
    }
    Ok(unsafe_head)
}

/// Starts the sequencer on a single node via `admin_startSequencer`.
///
/// The `unsafe_head` hash must match the node's current engine unsafe head; the
/// server rejects mismatches and `B256::ZERO`. When op-conductor is enabled,
/// this only succeeds if the target node is the Raft leader.
pub async fn start_sequencer_node(
    node: ConductorNodeConfig,
    unsafe_head: B256,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    let outcome = start_sequencer(&node.cl_rpc, unsafe_head)
        .await
        .with_context(|| format!("starting sequencer on {} via {}", node.name, node.cl_rpc))
        .map(|()| format!("sequencer started on {} at {unsafe_head}", node.name));

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}

/// Stops the sequencer on a single node via `admin_stopSequencer`.
///
/// Returns the unsafe head hash captured at the moment the sequencer was
/// stopped, suitable for passing back into [`start_sequencer_node`] later.
pub async fn stop_sequencer_node(
    node: ConductorNodeConfig,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    let outcome = stop_sequencer(&node.cl_rpc)
        .await
        .with_context(|| format!("stopping sequencer on {} via {}", node.name, node.cl_rpc))
        .map(|head| {
            if head == B256::ZERO {
                format!("sequencer stopped on {} (unsafe head unavailable)", node.name)
            } else {
                format!("sequencer stopped on {} at {head}", node.name)
            }
        });

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}

/// Disconnects all p2p peers from the CL and EL of a node so that neither layer
/// can advance.  Returns the saved peer addresses so they can be restored later
/// via [`unpause_sequencer_node`].
pub async fn pause_sequencer_node(
    node: ConductorNodeConfig,
    result_tx: mpsc::Sender<Result<(String, PausedPeers), String>>,
) {
    const TIMEOUT: Duration = Duration::from_secs(5);

    let outcome: anyhow::Result<(String, PausedPeers)> = async {
        let cl_client = HttpClientBuilder::default()
            .request_timeout(TIMEOUT)
            .build(node.cl_rpc.as_str())
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Snapshot connected CL peers before disconnecting so we can restore them.
        let dump = BaseP2PApiClient::opp2p_peers(&cl_client, true)
            .await
            .map_err(|e| anyhow::anyhow!("opp2p_peers: {e}"))?;

        let mut cl_addrs = Vec::new();
        for (peer_id, info) in dump.peers {
            let _ = BaseP2PApiClient::opp2p_disconnect_peer(&cl_client, peer_id).await;
            if let Some(addr) = info.addresses.into_iter().next() {
                cl_addrs.push(addr);
            }
        }

        // Remove EL peers (best-effort; skip if EL not configured).
        let mut el_enodes = Vec::new();
        if let Some(ref el_rpc) = node.el_rpc {
            let el_client = HttpClientBuilder::default()
                .request_timeout(TIMEOUT)
                .build(el_rpc.as_str())
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            let peers: Vec<serde_json::Value> =
                ClientT::request(&el_client, "admin_peers", rpc_params![])
                    .await
                    .unwrap_or_default();

            for peer in &peers {
                if let Some(enode) = peer.get("enode").and_then(|v| v.as_str()) {
                    let _: Result<bool, _> =
                        ClientT::request(&el_client, "admin_removePeer", rpc_params![enode]).await;
                    el_enodes.push(enode.to_string());
                }
            }
        }

        let msg = format!(
            "paused {} — disconnected {} CL peer(s), {} EL peer(s)",
            node.name,
            cl_addrs.len(),
            el_enodes.len()
        );
        Ok((msg, PausedPeers { cl_addrs, el_enodes }))
    }
    .await;

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}

/// Reconnects the CL and EL peers that were saved by [`pause_sequencer_node`],
/// allowing the node to resume syncing to tip.
pub async fn unpause_sequencer_node(
    node: ConductorNodeConfig,
    peers: PausedPeers,
    result_tx: mpsc::Sender<Result<String, String>>,
) {
    const TIMEOUT: Duration = Duration::from_secs(5);

    let outcome: anyhow::Result<String> = async {
        let cl_client = HttpClientBuilder::default()
            .request_timeout(TIMEOUT)
            .build(node.cl_rpc.as_str())
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut cl_ok = 0usize;
        for addr in &peers.cl_addrs {
            if BaseP2PApiClient::opp2p_connect_peer(&cl_client, addr.clone()).await.is_ok() {
                cl_ok += 1;
            }
        }

        let mut el_ok = 0usize;
        if let Some(ref el_rpc) = node.el_rpc {
            let el_client = HttpClientBuilder::default()
                .request_timeout(TIMEOUT)
                .build(el_rpc.as_str())
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            for enode in &peers.el_enodes {
                let r: Result<bool, _> =
                    ClientT::request(&el_client, "admin_addPeer", rpc_params![enode]).await;
                if matches!(r, Ok(true)) {
                    el_ok += 1;
                }
            }
        }

        if cl_ok != peers.cl_addrs.len() || (node.el_rpc.is_some() && el_ok != peers.el_enodes.len()) {
            anyhow::bail!(
                "unpaused {} — reconnected {cl_ok}/{} CL peer(s), {el_ok}/{} EL peer(s); saved peers kept for retry",
                node.name,
                peers.cl_addrs.len(),
                peers.el_enodes.len()
            );
        }

        Ok(format!(
            "unpaused {} — reconnected {cl_ok}/{} CL peer(s), {el_ok}/{} EL peer(s)",
            node.name,
            peers.cl_addrs.len(),
            peers.el_enodes.len()
        ))
    }
    .await;

    let _ = result_tx.send(outcome.map_err(|e| e.to_string())).await;
}
