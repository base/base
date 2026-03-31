use std::{sync::Arc, time::Duration};

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, ProviderBuilder, network::TransactionResponse};
use alloy_rpc_types_eth::BlockNumberOrTag;
use anyhow::Result;
use base_alloy_consensus::OpTxEnvelope;
use base_alloy_flashblocks::Flashblock;
use base_alloy_network::Base;
use base_consensus_rpc::{ConductorApiClient, OpP2PApiClient, RollupNodeApiClient};
use futures::{StreamExt, stream};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::connect_async;
use tracing::warn;

use crate::{
    config::{ConductorNodeConfig, ValidatorNodeConfig},
    tui::Toast,
};

const CONCURRENT_BLOCK_FETCHES: usize = 16;
const WS_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
const WS_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

/// Fetches the safe and latest L2 block numbers.
pub(crate) async fn fetch_safe_and_latest(l2_rpc: &str) -> Result<(u64, u64)> {
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

struct RawBlockInfo {
    da_bytes: u64,
    timestamp: u64,
}

async fn fetch_raw_block_info<P: Provider<Base>>(
    provider: &P,
    block_num: u64,
) -> Option<RawBlockInfo> {
    let block =
        provider.get_block_by_number(BlockNumberOrTag::Number(block_num)).full().await.ok()??;

    let da_bytes: u64 =
        block.transactions.txns().map(|tx| tx.inner.inner.encode_2718_len() as u64).sum();

    Some(RawBlockInfo { da_bytes, timestamp: block.header.timestamp })
}

/// Polls the L2 safe head block number at regular intervals.
pub(crate) async fn run_safe_head_poller(
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

/// Connects to the URL in `url_rx`, forwarding decoded flashblocks to `tx`.
///
/// Reconnects automatically on disconnection or error (exponential backoff).
/// If `url_rx` emits a new value while connected, the current connection is
/// dropped immediately and a fresh connection is opened to the new URL with
/// no backoff delay.  This is the mechanism used to follow conductor leader
/// changes: when a new Raft leader is elected, the caller pushes the new
/// leader's flashblocks endpoint into the watch channel and the loop here
/// switches over without waiting for the old socket to time out.
async fn run_flashblock_ws_inner<T: Send + 'static>(
    url_rx: &mut watch::Receiver<String>,
    tx: &mpsc::Sender<T>,
    toast_tx: &mpsc::Sender<Toast>,
    map_fb: impl Fn(Flashblock) -> T,
) {
    let mut delay = WS_RECONNECT_INITIAL_DELAY;

    loop {
        let url = url_rx.borrow_and_update().clone();

        // Wrap connect_async in a select so a second leader change that
        // arrives while a TCP handshake is already in progress (e.g. rapid
        // successive transfers, or non-localhost endpoints that stall rather
        // than immediately refuse) is acted on without waiting for the
        // handshake to resolve.
        tokio::select! {
            result = connect_async(url.as_str()) => {
                match result {
                    Ok((ws_stream, _)) => {
                        delay = WS_RECONNECT_INITIAL_DELAY;
                        let (_, mut read) = ws_stream.split();
                        let mut leader_changed = false;

                        loop {
                            tokio::select! {
                                msg_opt = read.next() => {
                                    let msg = match msg_opt {
                                        Some(Ok(m)) => m,
                                        Some(Err(e)) => {
                                            warn!(error = %e, "Flashblock WebSocket connection error");
                                            let _ = toast_tx.try_send(Toast::warning("WebSocket disconnected"));
                                            break;
                                        }
                                        None => break,
                                    };
                                    if !msg.is_binary() && !msg.is_text() {
                                        continue;
                                    }
                                    let fb = match Flashblock::try_decode_message(msg.into_data()) {
                                        Ok(fb) => fb,
                                        Err(_) => continue,
                                    };
                                    if tx.send(map_fb(fb)).await.is_err() {
                                        return;
                                    }
                                }
                                Ok(()) = url_rx.changed() => {
                                    leader_changed = true;
                                    break;
                                }
                            }
                        }

                        if leader_changed {
                            // Skip backoff: reconnect immediately to the new leader.
                            delay = WS_RECONNECT_INITIAL_DELAY;
                            continue;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, url = %url, "Failed to connect to flashblock WebSocket");
                        let _ = toast_tx.try_send(Toast::warning(format!(
                            "WebSocket connection failed, retrying in {}s",
                            delay.as_secs()
                        )));
                    }
                }
            }
            Ok(()) = url_rx.changed() => {
                // URL changed while connecting; abandon this attempt and
                // reconnect to the new leader immediately, without backoff.
                delay = WS_RECONNECT_INITIAL_DELAY;
                continue;
            }
        }

        // Exponential backoff, but skip the remainder if the URL changes.
        tokio::select! {
            _ = tokio::time::sleep(delay) => {
                delay = (delay * 2).min(WS_RECONNECT_MAX_DELAY);
            }
            Ok(()) = url_rx.changed() => {
                delay = WS_RECONNECT_INITIAL_DELAY;
            }
        }
    }
}

/// Subscribes to flashblocks via WebSocket and forwards raw flashblocks.
pub(crate) async fn run_flashblock_ws(
    mut url_rx: watch::Receiver<String>,
    tx: mpsc::Sender<Flashblock>,
    toast_tx: mpsc::Sender<Toast>,
) {
    run_flashblock_ws_inner(&mut url_rx, &tx, &toast_tx, |fb| fb).await;
}

/// A flashblock paired with its local receive timestamp.
#[derive(Debug)]
pub(crate) struct TimestampedFlashblock {
    /// The decoded flashblock.
    pub flashblock: Flashblock,
    /// Local time when this flashblock was received.
    pub received_at: chrono::DateTime<chrono::Local>,
}

/// Subscribes to flashblocks via WebSocket and forwards timestamped flashblocks.
pub(crate) async fn run_flashblock_ws_timestamped(
    mut url_rx: watch::Receiver<String>,
    tx: mpsc::Sender<TimestampedFlashblock>,
    toast_tx: mpsc::Sender<Toast>,
) {
    run_flashblock_ws_inner(&mut url_rx, &tx, &toast_tx, |fb| TimestampedFlashblock {
        flashblock: fb,
        received_at: chrono::Local::now(),
    })
    .await;
}

/// Polls the conductor cluster and pushes the current Raft leader's flashblocks
/// WebSocket URL into `url_tx` whenever leadership changes.
///
/// Only conductor nodes that have a `flashblocks_ws` configured are considered.
/// The task exits immediately if no such nodes exist.
/// Summary of the initial DA backlog between safe and latest blocks.
#[derive(Debug, Clone)]
pub(crate) struct InitialBacklog {
    /// Safe L2 block number.
    pub safe_block: u64,
    /// Total DA bytes across all backlog blocks.
    pub da_bytes: u64,
}

/// Progress update during initial backlog fetch.
#[derive(Debug, Clone)]
pub(crate) struct BacklogProgress {
    /// Number of blocks fetched so far.
    pub current_block: u64,
    /// Total number of blocks to fetch.
    pub total_blocks: u64,
}

/// Individual block data from backlog fetch.
#[derive(Debug, Clone)]
pub(crate) struct BacklogBlock {
    /// L2 block number.
    pub block_number: u64,
    /// DA bytes contributed by this block.
    pub da_bytes: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
}

/// Result of initial backlog fetch - either progress or complete.
#[derive(Debug, Clone)]
pub(crate) enum BacklogFetchResult {
    /// Incremental progress update.
    Progress(BacklogProgress),
    /// A single fetched block.
    Block(BacklogBlock),
    /// Backlog fetch completed successfully.
    Complete(InitialBacklog),
    /// Backlog fetch failed.
    Error,
}

/// Fetches the initial DA backlog, sending progress updates and block data.
pub(crate) async fn fetch_initial_backlog_with_progress(
    l2_rpc: String,
    progress_tx: tokio::sync::mpsc::Sender<BacklogFetchResult>,
) {
    let result = async {
        let (safe_block, unsafe_block) = fetch_safe_and_latest(&l2_rpc).await?;

        if unsafe_block <= safe_block {
            return Ok(InitialBacklog { safe_block, da_bytes: 0 });
        }

        let total_blocks = unsafe_block - safe_block;
        let provider = Arc::new(
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<Base>()
                .connect(&l2_rpc)
                .await?,
        );

        let block_numbers: Vec<u64> = ((safe_block + 1)..=unsafe_block).collect();

        let mut total_da_bytes: u64 = 0;
        let mut blocks_fetched: u64 = 0;
        let mut blocks: Vec<BacklogBlock> = Vec::with_capacity(block_numbers.len());

        let mut fetch_stream = stream::iter(block_numbers)
            .map(|block_num| {
                let provider = Arc::clone(&provider);
                async move {
                    let info = fetch_raw_block_info(&*provider, block_num).await;
                    BacklogBlock {
                        block_number: block_num,
                        da_bytes: info.as_ref().map(|i| i.da_bytes).unwrap_or(0),
                        timestamp: info.as_ref().map(|i| i.timestamp).unwrap_or(0),
                    }
                }
            })
            .buffer_unordered(CONCURRENT_BLOCK_FETCHES);

        while let Some(block) = fetch_stream.next().await {
            total_da_bytes = total_da_bytes.saturating_add(block.da_bytes);
            blocks.push(block);
            blocks_fetched += 1;

            if blocks_fetched.is_multiple_of(10) {
                let _ = progress_tx
                    .send(BacklogFetchResult::Progress(BacklogProgress {
                        current_block: blocks_fetched,
                        total_blocks,
                    }))
                    .await;
            }
        }

        blocks.sort_by_key(|b| b.block_number);
        for block in blocks {
            let _ = progress_tx.send(BacklogFetchResult::Block(block)).await;
        }

        Ok::<_, anyhow::Error>(InitialBacklog { safe_block, da_bytes: total_da_bytes })
    }
    .await;

    match result {
        Ok(backlog) => {
            let _ = progress_tx.send(BacklogFetchResult::Complete(backlog)).await;
        }
        Err(e) => {
            warn!(error = %e, "Backlog fetch failed");
            let _ = progress_tx.send(BacklogFetchResult::Error).await;
        }
    }
}

/// DA and gas information for a single L2 block.
#[derive(Debug, Clone)]
pub(crate) struct BlockDaInfo {
    /// L2 block number.
    pub block_number: u64,
    /// Total DA bytes from all transactions.
    pub da_bytes: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
}

/// Fetches DA info for requested block numbers and sends results back.
pub(crate) async fn run_block_fetcher(
    l2_rpc: String,
    mut request_rx: mpsc::Receiver<u64>,
    result_tx: mpsc::Sender<BlockDaInfo>,
    toast_tx: mpsc::Sender<Toast>,
) {
    let provider = match ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect(&l2_rpc)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to connect to L2 RPC for block fetcher");
            let _ = toast_tx.try_send(Toast::warning("Block fetcher connection failed"));
            return;
        }
    };

    while let Some(block_num) = request_rx.recv().await {
        if let Some(info) = fetch_raw_block_info(&provider, block_num).await {
            let block_info = BlockDaInfo {
                block_number: block_num,
                da_bytes: info.da_bytes,
                timestamp: info.timestamp,
            };

            if result_tx.send(block_info).await.is_err() {
                break;
            }
        }
    }
}

/// Information about an L1 block and its blob counts.
#[derive(Debug, Clone)]
pub(crate) struct L1BlockInfo {
    /// L1 block number.
    pub block_number: u64,
    /// Unix timestamp of the L1 block.
    pub timestamp: u64,
    /// Total number of blobs in this L1 block.
    pub total_blobs: u64,
    /// Number of blobs from the Base batcher.
    pub base_blobs: u64,
}

/// How the L1 watcher connects to the L1 node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum L1ConnectionMode {
    /// Connected via WebSocket subscription.
    WebSocket,
    /// Connected via HTTP polling.
    Polling,
}

fn http_to_ws(url: &str) -> String {
    url.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1)
}

/// Watches L1 blocks for blob transactions, preferring WebSocket with polling fallback.
pub(crate) async fn run_l1_blob_watcher(
    l1_rpc: String,
    batcher_address: Address,
    result_tx: mpsc::Sender<L1BlockInfo>,
    mode_tx: mpsc::Sender<L1ConnectionMode>,
    toast_tx: mpsc::Sender<Toast>,
) {
    let ws_url = http_to_ws(&l1_rpc);

    if let Err(()) =
        run_l1_blob_watcher_ws(&ws_url, batcher_address, result_tx.clone(), &mode_tx, &toast_tx)
            .await
    {
        let _ = mode_tx.send(L1ConnectionMode::Polling).await;
        let _ = toast_tx.try_send(Toast::info("L1 watcher fell back to HTTP polling"));
        run_l1_blob_watcher_poll(&l1_rpc, batcher_address, result_tx, &toast_tx).await;
    }
}

async fn run_l1_blob_watcher_ws(
    ws_url: &str,
    batcher_address: Address,
    result_tx: mpsc::Sender<L1BlockInfo>,
    mode_tx: &mpsc::Sender<L1ConnectionMode>,
    toast_tx: &mpsc::Sender<Toast>,
) -> Result<(), ()> {
    let provider = ProviderBuilder::new().connect(ws_url).await.map_err(|e| {
        warn!(error = %e, "Failed to connect to L1 WebSocket");
        let _ = toast_tx.try_send(Toast::warning("L1 WebSocket connection failed"));
    })?;

    let sub = provider.subscribe_blocks().await.map_err(|e| {
        warn!(error = %e, "Failed to subscribe to L1 blocks");
        let _ = toast_tx.try_send(Toast::warning("L1 block subscription failed"));
    })?;
    let mut stream = sub.into_stream();

    let _ = mode_tx.send(L1ConnectionMode::WebSocket).await;

    let mut last_block: Option<u64> = None;

    if let Ok(Some(block)) = provider.get_block_by_number(BlockNumberOrTag::Latest).full().await {
        let info = extract_l1_block_info(&block, batcher_address);
        last_block = Some(block.header.number);
        let _ = result_tx.send(info).await;
    }

    while let Some(header) = stream.next().await {
        let block_num = header.number;

        let start = last_block.map_or(block_num, |last| last + 1);
        for gap_num in start..block_num {
            if let Ok(Some(block)) =
                provider.get_block_by_number(BlockNumberOrTag::Number(gap_num)).full().await
            {
                let info = extract_l1_block_info(&block, batcher_address);
                if result_tx.send(info).await.is_err() {
                    return Ok(());
                }
            }
        }

        if let Ok(Some(block)) =
            provider.get_block_by_number(BlockNumberOrTag::Number(block_num)).full().await
        {
            let info = extract_l1_block_info(&block, batcher_address);
            if result_tx.send(info).await.is_err() {
                return Ok(());
            }
        }

        last_block = Some(block_num);
    }

    warn!("L1 WebSocket stream ended");
    let _ = toast_tx.try_send(Toast::warning("L1 WebSocket disconnected"));

    Err(())
}

async fn run_l1_blob_watcher_poll(
    l1_rpc: &str,
    batcher_address: Address,
    result_tx: mpsc::Sender<L1BlockInfo>,
    toast_tx: &mpsc::Sender<Toast>,
) {
    let provider = match ProviderBuilder::new().connect(l1_rpc).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to connect to L1 RPC for polling");
            let _ = toast_tx.try_send(Toast::warning("L1 poller connection failed"));
            return;
        }
    };

    let mut last_block: Option<u64> = None;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        interval.tick().await;

        let latest = match provider.get_block_number().await {
            Ok(n) => n,
            Err(_) => continue,
        };

        let start_block = last_block.map_or(latest, |b| b + 1);

        for block_num in start_block..=latest {
            if let Ok(Some(block)) =
                provider.get_block_by_number(BlockNumberOrTag::Number(block_num)).full().await
            {
                let info = extract_l1_block_info(&block, batcher_address);
                if result_tx.send(info).await.is_err() {
                    return;
                }
            }
        }

        last_block = Some(latest);
    }
}

fn extract_l1_block_info(
    block: &alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction>,
    batcher_address: Address,
) -> L1BlockInfo {
    let mut total_blobs: u64 = 0;
    let mut base_blobs: u64 = 0;

    for tx in block.transactions.txns() {
        if let Some(blob_hashes) = tx.blob_versioned_hashes() {
            let blob_count = blob_hashes.len() as u64;
            total_blobs += blob_count;
            if tx.inner.signer() == batcher_address {
                base_blobs += blob_count;
            }
        }
    }

    L1BlockInfo {
        block_number: block.header.number,
        timestamp: block.header.timestamp,
        total_blobs,
        base_blobs,
    }
}

/// Summary of a single transaction within a block.
#[derive(Debug, Clone)]
pub(crate) struct TxSummary {
    /// Transaction hash.
    pub hash: B256,
    /// Sender address.
    pub from: Address,
    /// Recipient address (None for contract creations).
    pub to: Option<Address>,
    /// Effective priority fee per gas (tip), in wei.
    pub effective_priority_fee_per_gas: Option<u128>,
    /// Block base fee per gas, in wei.
    pub base_fee_per_gas: Option<u64>,
}

fn effective_priority_fee_per_gas(
    base_fee_per_gas: Option<u64>,
    effective_gas_price: u128,
    max_priority_fee_per_gas: Option<u128>,
) -> Option<u128> {
    base_fee_per_gas
        .map(|base_fee| effective_gas_price.saturating_sub(u128::from(base_fee)))
        .or(max_priority_fee_per_gas)
}

/// Decodes raw EIP-2718 encoded transaction bytes into summaries.
///
/// Used to extract transaction details from flashblock stream data without RPC calls.
pub(crate) fn decode_flashblock_transactions(
    raw_txs: &[Bytes],
    base_fee_per_gas: Option<u64>,
) -> Vec<TxSummary> {
    raw_txs
        .iter()
        .filter_map(|tx_bytes| {
            let envelope = OpTxEnvelope::decode_2718(&mut tx_bytes.as_ref())
                .inspect_err(|e| warn!(error = %e, "failed to decode transaction"))
                .ok()?;
            let hash = envelope.tx_hash();
            let to = envelope.to();
            let effective_priority_fee_per_gas = effective_priority_fee_per_gas(
                base_fee_per_gas,
                envelope.effective_gas_price(base_fee_per_gas),
                envelope.max_priority_fee_per_gas(),
            );
            let recovered = envelope
                .try_into_recovered()
                .inspect_err(|e| warn!(error = %e, "failed to recover signer"))
                .ok()?;
            Some(TxSummary {
                hash,
                from: recovered.signer(),
                to,
                effective_priority_fee_per_gas,
                base_fee_per_gas,
            })
        })
        .collect()
}

/// Fetches all transactions for a given block and sends summaries through the channel.
pub(crate) async fn fetch_block_transactions(
    l2_rpc: String,
    block_number: u64,
    tx: mpsc::Sender<Result<Vec<TxSummary>, String>>,
) {
    let result = async {
        let provider = Arc::new(
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<Base>()
                .connect(&l2_rpc)
                .await?,
        );

        let block = provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .full()
            .await?
            .ok_or_else(|| anyhow::anyhow!("Block {block_number} not found"))?;

        let base_fee = block.header.base_fee_per_gas;

        let summaries: Vec<TxSummary> = block
            .transactions
            .txns()
            .map(|tx_obj| TxSummary {
                hash: tx_obj.inner.tx_hash(),
                from: tx_obj.inner.inner.signer(),
                to: tx_obj.inner.to(),
                effective_priority_fee_per_gas: effective_priority_fee_per_gas(
                    base_fee,
                    tx_obj.inner.effective_gas_price(base_fee),
                    tx_obj.max_priority_fee_per_gas(),
                ),
                base_fee_per_gas: base_fee,
            })
            .collect();

        Ok::<_, anyhow::Error>(summaries)
    }
    .await;

    match result {
        Ok(summaries) => {
            let _ = tx.send(Ok(summaries)).await;
        }
        Err(e) => {
            warn!(error = %e, block = block_number, "failed to fetch block transactions");
            let _ = tx.send(Err(e.to_string())).await;
        }
    }
}

/// Live status snapshot for a single node in an HA conductor cluster.
#[derive(Debug, Clone)]
pub(crate) struct ConductorNodeStatus {
    /// Human-readable name for this node.
    pub name: String,

    // ── Conductor ────────────────────────────────────────────────────────
    /// Whether this node is the Raft leader. `None` means the node is unreachable.
    pub is_leader: Option<bool>,
    /// Whether the conductor's sequencer is actively sequencing (`conductor_active`).
    /// Expected to be `false` for followers. `None` means unreachable.
    pub conductor_active: Option<bool>,

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
}

/// Finds the current Raft leader and transfers leadership.
///
/// If `target_name` is `None`, leadership is transferred to any available peer
/// (`conductor_transferLeader`). If `target_name` is `Some(name)`, leadership
/// is transferred to the named node via `conductor_transferLeaderToServer`.
///
/// The result — `Ok(description)` or `Err(message)` — is sent to `result_tx`.
pub(crate) async fn transfer_conductor_leader(
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

/// Restarts the docker containers for a single conductor cluster node.
///
/// Containers are restarted in dependency order — EL → CL → conductor —
/// waiting for each to become healthy before starting the next. This prevents
/// op-conductor from crashing on startup because it tries to connect to the EL
/// before the EL has bound its port.
pub(crate) async fn restart_conductor_node(
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
pub(crate) struct PausedPeers {
    /// Multiaddrs of the CL peers that were connected before pausing.
    /// Used to reconnect them on unpause via `opp2p_connectPeer`.
    pub cl_addrs: Vec<String>,
    /// Enode URLs of the EL peers that were connected before pausing.
    /// Used to re-add them on unpause via `admin_addPeer`.
    pub el_enodes: Vec<String>,
}

/// Disconnects all p2p peers from the CL and EL of a node so that neither layer
/// can advance.  Returns the saved peer addresses so they can be restored later
/// via [`unpause_sequencer_node`].
pub(crate) async fn pause_sequencer_node(
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
        let dump = OpP2PApiClient::opp2p_peers(&cl_client, true)
            .await
            .map_err(|e| anyhow::anyhow!("opp2p_peers: {e}"))?;

        let mut cl_addrs = Vec::new();
        for (peer_id, info) in dump.peers {
            let _ = OpP2PApiClient::opp2p_disconnect_peer(&cl_client, peer_id).await;
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
pub(crate) async fn unpause_sequencer_node(
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
            if OpP2PApiClient::opp2p_connect_peer(&cl_client, addr.clone()).await.is_ok() {
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
                if r.is_ok() {
                    el_ok += 1;
                }
            }
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

/// Polls all conductor nodes every 200 ms and forwards status snapshots.
///
/// Builds one pair of HTTP clients per node (conductor RPC + CL RPC) before
/// entering the loop so connection setup cost is paid only once. Each poll
/// fires all per-node requests concurrently via [`futures::future::join_all`].
/// Any individual RPC that times out or errors yields `None` for that field —
/// the node is shown as offline when `is_leader` is `None`.
pub(crate) async fn run_conductor_poller(
    nodes: Vec<ConductorNodeConfig>,
    tx: mpsc::Sender<Vec<ConductorNodeStatus>>,
) {
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    const RPC_TIMEOUT: Duration = Duration::from_millis(500);

    let clients: Vec<(String, _, _, _)> = nodes
        .into_iter()
        .filter_map(|node| {
            let conductor_client = HttpClientBuilder::default()
                .request_timeout(RPC_TIMEOUT)
                .build(node.conductor_rpc.as_str())
                .inspect_err(|e| {
                    warn!(error = %e, node = %node.name, "failed to build conductor HTTP client");
                })
                .ok()?;
            let cl_client = HttpClientBuilder::default()
                .request_timeout(RPC_TIMEOUT)
                .build(node.cl_rpc.as_str())
                .inspect_err(|e| {
                    warn!(error = %e, node = %node.name, "failed to build CL HTTP client");
                })
                .ok()?;
            let el_client = node.el_rpc.as_ref().and_then(|url| {
                HttpClientBuilder::default()
                    .request_timeout(RPC_TIMEOUT)
                    .build(url.as_str())
                    .inspect_err(|e| {
                        warn!(error = %e, node = %node.name, "failed to build EL HTTP client");
                    })
                    .ok()
            });
            Some((node.name, conductor_client, cl_client, el_client))
        })
        .collect();

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let statuses = futures::future::join_all(clients.iter().map(
            |(name, conductor_client, cl_client, el_client)| async move {
                // Fire all RPCs concurrently so a single timed-out node does not
                // stall the poll for the full sum of all call timeouts (7 × 500 ms).
                let (
                    is_leader,
                    conductor_active,
                    sync,
                    cl_peer_stats,
                    el_block_r,
                    el_syncing_r,
                    el_peers_r,
                ) = tokio::join!(
                    ConductorApiClient::conductor_leader(conductor_client),
                    ConductorApiClient::conductor_active(conductor_client),
                    RollupNodeApiClient::op_sync_status(cl_client),
                    OpP2PApiClient::opp2p_peer_stats(cl_client),
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
                ConductorNodeStatus {
                    name: name.clone(),
                    is_leader: is_leader.ok(),
                    conductor_active: conductor_active.ok(),
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

/// Live status snapshot for a single validator (non-sequencing) node.
#[derive(Debug, Clone)]
pub(crate) struct ValidatorNodeStatus {
    /// Human-readable name for this node.
    pub name: String,

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
pub(crate) async fn run_validator_poller(
    nodes: Vec<ValidatorNodeConfig>,
    tx: mpsc::Sender<Vec<ValidatorNodeStatus>>,
) {
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    const RPC_TIMEOUT: Duration = Duration::from_millis(500);

    let clients: Vec<(String, _, _)> = nodes
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
            Some((node.name, cl_client, el_client))
        })
        .collect();

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let statuses = futures::future::join_all(clients.iter().map(
            |(name, cl_client, el_client)| async move {
                let (sync, cl_peer_stats, el_block_r, el_syncing_r, el_peers_r) = tokio::join!(
                    RollupNodeApiClient::op_sync_status(cl_client),
                    OpP2PApiClient::opp2p_peer_stats(cl_client),
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

#[cfg(test)]
mod tests {
    use super::effective_priority_fee_per_gas;

    #[test]
    fn priority_fee_uses_effective_gas_price_when_base_fee_known() {
        assert_eq!(effective_priority_fee_per_gas(Some(100), 125, Some(50)), Some(25));
    }

    #[test]
    fn priority_fee_falls_back_to_declared_max_priority_fee_when_base_fee_unknown() {
        assert_eq!(effective_priority_fee_per_gas(None, 125, Some(50)), Some(50));
    }

    #[test]
    fn priority_fee_is_unknown_for_legacy_txs_when_base_fee_unknown() {
        assert_eq!(effective_priority_fee_per_gas(None, 125, None), None);
    }
}
