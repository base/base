use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_sol_types::sol;
use anyhow::Result;
use base_consensus_rpc::{BaseP2PApiClient, RollupNodeApiClient};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use tokio::sync::mpsc;
use tracing::warn;
use url::Url;

use crate::{
    config::{ProofsConfig, ValidatorNodeConfig},
    tui::Toast,
};

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

// =============================================================================
// Proof system contract interfaces
// =============================================================================

sol! {
    #[sol(rpc)]
    interface IAnchorStateRegistry {
        function getAnchorRoot() external view returns (bytes32 root, uint256 l2SequenceNumber);
        function respectedGameType() external view returns (uint32);
        function paused() external view returns (bool);
    }

    #[sol(rpc)]
    interface IDisputeGameFactory {
        function gameCount() external view returns (uint256);
        function gameAtIndex(uint256 index) external view returns (
            uint32 gameType, uint64 timestamp, address proxy
        );
    }

    #[sol(rpc)]
    interface IAggregateVerifier {
        function rootClaim() external pure returns (bytes32);
        function l2SequenceNumber() external pure returns (uint256);
        function status() external view returns (uint8);
    }
}

/// Snapshot of proof system state, fetched periodically.
#[derive(Debug, Clone)]
pub struct ProofsSnapshot {
    /// Current L1 block number.
    pub l1_block: Option<u64>,
    /// Current L2 latest (unsafe) block number.
    pub l2_latest_block: Option<u64>,
    /// Current L2 safe block number.
    pub l2_safe_block: Option<u64>,
    /// Current L2 finalized block number.
    pub l2_finalized_block: Option<u64>,
    /// Respected game type from the `AnchorStateRegistry`.
    pub respected_game_type: Option<u32>,
    /// Whether the proof system is paused.
    pub system_paused: Option<bool>,
    /// Total number of dispute games created.
    pub total_games: Option<u64>,
    /// Anchor L2 block number (latest finalized anchor).
    pub anchor_l2_block: Option<u64>,
    /// Anchor output root hash.
    pub anchor_root: Option<B256>,
    /// Most recent dispute game proposal.
    pub latest_proposal: Option<LatestProposal>,
}

/// Information about the most recent dispute game proposal.
#[derive(Debug, Clone)]
pub struct LatestProposal {
    /// Address of the dispute game proxy contract.
    pub game_address: Address,
    /// L2 block number proposed.
    pub l2_block: u64,
    /// Output root claimed by the proposal.
    pub root_claim: B256,
    /// Game status: `0`=`IN_PROGRESS`, `1`=`CHALLENGER_WINS`, `2`=`DEFENDER_WINS`.
    pub status: u8,
    /// L1 timestamp when the game was created.
    pub created_at: u64,
}

/// Polls proof system state (anchor state, dispute games, chain heads) at regular
/// intervals and sends snapshots to the TUI.
pub async fn run_proofs_poller(
    proofs_config: ProofsConfig,
    l1_rpc: Url,
    l2_rpc: Url,
    tx: mpsc::Sender<ProofsSnapshot>,
    toast_tx: mpsc::Sender<Toast>,
) {
    let l1_provider = match ProviderBuilder::new().connect(l1_rpc.as_str()).await {
        Ok(p) => Arc::new(p),
        Err(e) => {
            warn!(error = %e, "Failed to connect to L1 RPC for proofs poller");
            let _ = toast_tx.try_send(Toast::warning("Proofs: L1 connection failed"));
            return;
        }
    };

    let l2_provider = match ProviderBuilder::new().connect(l2_rpc.as_str()).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to connect to L2 RPC for proofs poller");
            let _ = toast_tx.try_send(Toast::warning("Proofs: L2 connection failed"));
            return;
        }
    };

    let asr = IAnchorStateRegistry::new(proofs_config.anchor_state_registry, &*l1_provider);
    let factory = IDisputeGameFactory::new(proofs_config.dispute_game_factory, &*l1_provider);

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;

        let snapshot = fetch_proofs_snapshot(&asr, &factory, &l1_provider, &l2_provider).await;

        if tx.send(snapshot).await.is_err() {
            break;
        }
    }
}

async fn fetch_proofs_snapshot<P: Provider + Clone>(
    asr: &IAnchorStateRegistry::IAnchorStateRegistryInstance<&P>,
    factory: &IDisputeGameFactory::IDisputeGameFactoryInstance<&P>,
    l1_provider: &P,
    l2_provider: &impl Provider,
) -> ProofsSnapshot {
    // Fetch chain state and contract state concurrently.
    let (chain, anchor, game_type, paused, game_count) = tokio::join!(
        fetch_chain_heads(l1_provider, l2_provider),
        async { asr.getAnchorRoot().call().await.ok() },
        async { asr.respectedGameType().call().await.ok() },
        async { asr.paused().call().await.ok() },
        async { factory.gameCount().call().await.ok() },
    );

    let (l1_block, l2_latest, l2_safe, l2_finalized) = chain;

    let total_games: Option<u64> = game_count.and_then(|c| c.try_into().ok());
    let respected_type = game_type;

    // Find and query the latest proposal for the respected game type.
    let latest_proposal =
        find_latest_proposal(factory, l1_provider, respected_type, total_games).await;

    ProofsSnapshot {
        l1_block,
        l2_latest_block: l2_latest,
        l2_safe_block: l2_safe,
        l2_finalized_block: l2_finalized,
        respected_game_type: respected_type,
        system_paused: paused,
        total_games,
        anchor_l2_block: anchor.as_ref().map(|a| a.l2SequenceNumber.try_into().unwrap_or(0)),
        anchor_root: anchor.map(|a| a.root),
        latest_proposal,
    }
}

async fn fetch_chain_heads(
    l1: &impl Provider,
    l2: &impl Provider,
) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let (l1_block, l2_latest, l2_safe, l2_finalized) = tokio::join!(
        async { l1.get_block_number().await.ok() },
        async {
            l2.get_block_by_number(BlockNumberOrTag::Latest)
                .await
                .ok()
                .flatten()
                .map(|b| b.header.number)
        },
        async {
            l2.get_block_by_number(BlockNumberOrTag::Safe)
                .await
                .ok()
                .flatten()
                .map(|b| b.header.number)
        },
        async {
            l2.get_block_by_number(BlockNumberOrTag::Finalized)
                .await
                .ok()
                .flatten()
                .map(|b| b.header.number)
        },
    );
    (l1_block, l2_latest, l2_safe, l2_finalized)
}

/// Scans the most recent games in the factory to find the latest one matching the
/// respected game type, then queries its details from the `AggregateVerifier`.
async fn find_latest_proposal<P: Provider + Clone>(
    factory: &IDisputeGameFactory::IDisputeGameFactoryInstance<&P>,
    l1_provider: &P,
    respected_type: Option<u32>,
    total_games: Option<u64>,
) -> Option<LatestProposal> {
    let game_type = respected_type?;
    let count = total_games.filter(|&c| c > 0)?;

    // Scan backwards from the most recent game (max 50 games).
    let scan_start = count - 1;
    let scan_end = count.saturating_sub(50);

    for idx in (scan_end..=scan_start).rev() {
        let Ok(game) = factory.gameAtIndex(alloy_primitives::U256::from(idx)).call().await else {
            continue;
        };

        if game.gameType != game_type {
            continue;
        }

        // Found a matching game — query its details.
        let verifier = IAggregateVerifier::new(game.proxy, l1_provider);

        let (root_claim, l2_seq, status) = tokio::join!(
            async { verifier.rootClaim().call().await.ok() },
            async { verifier.l2SequenceNumber().call().await.ok() },
            async { verifier.status().call().await.ok() },
        );

        return Some(LatestProposal {
            game_address: game.proxy,
            l2_block: l2_seq.and_then(|s| s.try_into().ok()).unwrap_or(0),
            root_claim: root_claim.unwrap_or_default(),
            status: status.unwrap_or(0),
            created_at: game.timestamp,
        });
    }

    None
}
