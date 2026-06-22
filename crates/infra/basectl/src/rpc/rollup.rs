use std::time::Duration;

use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockNumberOrTag, SyncStatus as EthSyncStatus};
use alloy_transport_http::Http;
use anyhow::{Context, Result};
use base_common_network::Base;
use base_consensus_rpc::{BaseP2PApiClient, RollupNodeApiClient};
use base_protocol::SyncStatus;
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use tokio::sync::mpsc;
use tracing::warn;
use url::Url;

use crate::{config::ValidatorNodeConfig, tui::Toast};

/// Combined CL `optimism_syncStatus` + EL `eth_syncing` snapshot.
///
/// The CL `SyncStatus` carries every L1/L2 head ref the rollup node knows
/// about, including timestamps on `unsafe_l2`/`safe_l2`/`finalized_l2` via
/// `L2BlockInfo.block_info.timestamp`. The EL `EthSyncStatus` is alloy's
/// typed `eth_syncing` response (`Info(SyncInfo)` while syncing, `None`
/// otherwise).
///
/// Doctor (Phase 2.8) reuses this to compute `unsafe.timestamp -
/// safe.timestamp` for its safe-head-recency check, gated on whether the
/// EL is actively syncing.
#[derive(Debug, Clone)]
pub struct SyncStatusReport {
    /// Rollup node `optimism_syncStatus` response.
    pub cl: SyncStatus,
    /// Execution-layer `eth_syncing` response.
    pub el: EthSyncStatus,
}

/// Fetches a combined CL + EL sync-status snapshot.
///
/// Calls `optimism_syncStatus` against the consensus-node RPC and
/// `eth_syncing` against the execution-layer RPC in parallel; either error
/// short-circuits the call.
pub async fn fetch_sync_status(rpc: &Url, cl_rpc: &Url) -> Result<SyncStatusReport> {
    let cl_client = HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(10))
        .build(cl_rpc.as_str())
        .with_context(|| format!("connecting to consensus node RPC at {cl_rpc}"))?;
    let http_client = alloy_transport_http::reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .with_context(|| format!("building L2 EL HTTP client for {rpc}"))?;
    let transport = Http::with_client(http_client, rpc.clone());
    let el_provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect_client(RpcClient::new(transport, false));
    let (cl, el) = tokio::try_join!(
        async {
            RollupNodeApiClient::sync_status(&cl_client)
                .await
                .with_context(|| format!("fetching optimism_syncStatus from {cl_rpc}"))
        },
        async {
            el_provider.syncing().await.with_context(|| format!("fetching eth_syncing from {rpc}"))
        },
    )?;
    Ok(SyncStatusReport { cl, el })
}

/// Fetches the safe and latest L2 block numbers.
pub async fn fetch_safe_and_latest(l2_rpc: &str) -> Result<(u64, u64)> {
    let provider = ProviderBuilder::new().connect(l2_rpc).await?;

    let safe_block = provider
        .get_block_by_number(BlockNumberOrTag::Safe)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Safe block not found"))?;

    let latest_block = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Latest block not found"))?;

    Ok((safe_block.header.number, latest_block.header.number))
}

/// Polls the L2 safe head block number at regular intervals.
pub async fn run_safe_head_poller(
    l2_rpc: String,
    tx: mpsc::Sender<u64>,
    toast_tx: mpsc::Sender<Toast>,
) {
    let provider = match ProviderBuilder::new().connect(&l2_rpc).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to connect to L2 RPC for safe head polling");
            let _ = toast_tx.try_send(Toast::warning("Safe head poller connection failed"));
            return;
        }
    };

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        if let Ok(Some(block)) = provider.get_block_by_number(BlockNumberOrTag::Safe).await
            && tx.send(block.header.number).await.is_err()
        {
            break;
        }
    }
}

/// Live status snapshot for a single validator (non-sequencing) node.
#[derive(Debug, Clone)]
pub struct ValidatorNodeStatus {
    /// Human-readable name for this node.
    pub name: String,
    /// Human-readable binary/process description shown in the TUI.
    pub binary: Option<String>,

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
    /// L1 chain head block number (`head_l1`).
    pub head_l1_block: Option<u64>,
    /// Number of connected CL libp2p peers from `opp2p_peerStats`.
    pub cl_peer_count: Option<u32>,

    // ── EL (execution layer) ─────────────────────────────────────────────
    /// Latest block number from `eth_blockNumber`. `None` if `el_rpc` not configured.
    pub el_block: Option<u64>,
    /// Whether the EL is snap-syncing. `None` if not configured.
    pub el_syncing: Option<bool>,
    /// Number of connected EL devp2p peers from `net_peerCount`. `None` if not configured.
    pub el_peer_count: Option<u32>,
}

/// Polls all validator nodes every 200 ms and forwards status snapshots.
pub async fn run_validator_poller(
    nodes: Vec<ValidatorNodeConfig>,
    tx: mpsc::Sender<Vec<ValidatorNodeStatus>>,
) {
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    const RPC_TIMEOUT: Duration = Duration::from_millis(500);

    let clients: Vec<(String, Option<String>, _, _)> = nodes
        .into_iter()
        .filter_map(|node| {
            let cl_client = HttpClientBuilder::default()
                .request_timeout(RPC_TIMEOUT)
                .build(node.cl_rpc.as_str())
                .inspect_err(|e| {
                    warn!(error = %e, node = %node.name, "failed to build validator CL HTTP client");
                })
                .ok()?;
            let el_client = node.el_rpc.as_ref().and_then(|url| {
                HttpClientBuilder::default()
                    .request_timeout(RPC_TIMEOUT)
                    .build(url.as_str())
                    .inspect_err(|e| {
                        warn!(error = %e, node = %node.name, "failed to build validator EL HTTP client");
                    })
                    .ok()
            });
            Some((node.name, node.binary, cl_client, el_client))
        })
        .collect();

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let statuses = futures::future::join_all(clients.iter().map(
            |(name, binary, cl_client, el_client)| async move {
                let (sync, cl_peer_stats, el_block_r, el_syncing_r, el_peers_r) = tokio::join!(
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
                ValidatorNodeStatus {
                    name: name.clone(),
                    binary: binary.clone(),
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
                }
            },
        ))
        .await;

        if tx.send(statuses).await.is_err() {
            break;
        }
    }
}
