use std::{sync::Arc, time::Duration};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::{BlockNumberOrTag, TransactionTrait};
use anyhow::Result;
use base_flashtypes::Flashblock;
use futures_util::{StreamExt, stream};
use op_alloy_network::Optimism;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::warn;

use crate::{config::ChainConfig, l1_client::fetch_system_config_params, tui::Toast};

const CONCURRENT_BLOCK_FETCHES: usize = 16;
const DEFAULT_ELASTICITY: u64 = 6;
const WS_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
const WS_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

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

struct RawBlockInfo {
    da_bytes: u64,
    tx_count: usize,
    gas_used: u64,
    timestamp: u64,
}

async fn fetch_raw_block_info<P: Provider<Optimism>>(
    provider: &P,
    block_num: u64,
) -> Option<RawBlockInfo> {
    let block =
        provider.get_block_by_number(BlockNumberOrTag::Number(block_num)).full().await.ok()??;

    let tx_count = block.transactions.len();
    let da_bytes: u64 =
        block.transactions.txns().map(|tx| tx.inner.inner.encode_2718_len() as u64).sum();

    Some(RawBlockInfo {
        da_bytes,
        tx_count,
        gas_used: block.header.gas_used,
        timestamp: block.header.timestamp,
    })
}

pub async fn run_safe_head_poller(
    l2_rpc: String,
    tx: mpsc::Sender<u64>,
    toast_tx: mpsc::Sender<Toast>,
) {
    let provider = match ProviderBuilder::new().connect(&l2_rpc).await {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to connect to L2 RPC for safe head polling: {e}");
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

async fn run_flashblock_ws_inner<T: Send + 'static>(
    url: &str,
    tx: &mpsc::Sender<T>,
    toast_tx: &mpsc::Sender<Toast>,
    map_fb: impl Fn(Flashblock) -> T,
) {
    let mut delay = WS_RECONNECT_INITIAL_DELAY;

    loop {
        match connect_async(url).await {
            Ok((ws_stream, _)) => {
                delay = WS_RECONNECT_INITIAL_DELAY;
                let (_, mut read) = ws_stream.split();

                while let Some(msg) = read.next().await {
                    let msg = match msg {
                        Ok(m) => m,
                        Err(e) => {
                            warn!("Flashblock WebSocket connection error: {e}");
                            let _ = toast_tx.try_send(Toast::warning("WebSocket disconnected"));
                            break;
                        }
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
            }
            Err(e) => {
                warn!("Failed to connect to flashblock WebSocket: {e}");
                let _ = toast_tx.try_send(Toast::warning(format!(
                    "WebSocket connection failed, retrying in {}s",
                    delay.as_secs()
                )));
            }
        }

        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(WS_RECONNECT_MAX_DELAY);
    }
}

pub async fn run_flashblock_ws(
    url: String,
    tx: mpsc::Sender<Flashblock>,
    toast_tx: mpsc::Sender<Toast>,
) {
    run_flashblock_ws_inner(&url, &tx, &toast_tx, |fb| fb).await;
}

#[derive(Debug)]
pub struct TimestampedFlashblock {
    pub flashblock: Flashblock,
    pub received_at: chrono::DateTime<chrono::Local>,
}

pub async fn run_flashblock_ws_timestamped(
    url: String,
    tx: mpsc::Sender<TimestampedFlashblock>,
    toast_tx: mpsc::Sender<Toast>,
) {
    run_flashblock_ws_inner(&url, &tx, &toast_tx, |fb| TimestampedFlashblock {
        flashblock: fb,
        received_at: chrono::Local::now(),
    })
    .await;
}

#[derive(Debug, Clone)]
pub struct InitialBacklog {
    pub safe_block: u64,
    pub unsafe_block: u64,
    pub da_bytes: u64,
}

/// Progress update during initial backlog fetch
#[derive(Debug, Clone)]
pub struct BacklogProgress {
    pub current_block: u64,
    pub total_blocks: u64,
    pub da_bytes_so_far: u64,
}

/// Individual block data from backlog fetch
#[derive(Debug, Clone)]
pub struct BacklogBlock {
    pub block_number: u64,
    pub da_bytes: u64,
    pub timestamp: u64,
}

/// Result of initial backlog fetch - either progress or complete
#[derive(Debug, Clone)]
pub enum BacklogFetchResult {
    Progress(BacklogProgress),
    Block(BacklogBlock),
    Complete(InitialBacklog),
    Error(String),
}

pub async fn fetch_initial_backlog_with_progress(
    l2_rpc: String,
    progress_tx: tokio::sync::mpsc::Sender<BacklogFetchResult>,
) {
    let result = async {
        let (safe_block, unsafe_block) = fetch_safe_and_latest(&l2_rpc).await?;

        if unsafe_block <= safe_block {
            return Ok(InitialBacklog { safe_block, unsafe_block, da_bytes: 0 });
        }

        let total_blocks = unsafe_block - safe_block;
        let provider = Arc::new(
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<Optimism>()
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
                        da_bytes_so_far: total_da_bytes,
                    }))
                    .await;
            }
        }

        blocks.sort_by_key(|b| b.block_number);
        for block in blocks {
            let _ = progress_tx.send(BacklogFetchResult::Block(block)).await;
        }

        Ok::<_, anyhow::Error>(InitialBacklog {
            safe_block,
            unsafe_block,
            da_bytes: total_da_bytes,
        })
    }
    .await;

    match result {
        Ok(backlog) => {
            let _ = progress_tx.send(BacklogFetchResult::Complete(backlog)).await;
        }
        Err(e) => {
            let _ = progress_tx.send(BacklogFetchResult::Error(e.to_string())).await;
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlockDaInfo {
    pub block_number: u64,
    pub da_bytes: u64,
    pub tx_count: usize,
    pub gas_used: u64,
    pub timestamp: u64,
}

pub async fn run_block_fetcher(
    l2_rpc: String,
    mut request_rx: mpsc::Receiver<u64>,
    result_tx: mpsc::Sender<BlockDaInfo>,
    toast_tx: mpsc::Sender<Toast>,
) {
    let provider = match ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Optimism>()
        .connect(&l2_rpc)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to connect to L2 RPC for block fetcher: {e}");
            let _ = toast_tx.try_send(Toast::warning("Block fetcher connection failed"));
            return;
        }
    };

    while let Some(block_num) = request_rx.recv().await {
        if let Some(info) = fetch_raw_block_info(&provider, block_num).await {
            let block_info = BlockDaInfo {
                block_number: block_num,
                da_bytes: info.da_bytes,
                tx_count: info.tx_count,
                gas_used: info.gas_used,
                timestamp: info.timestamp,
            };

            if result_tx.send(block_info).await.is_err() {
                break;
            }
        }
    }
}

/// Chain parameters needed for flashblocks display
#[derive(Debug, Clone, Copy)]
pub struct ChainParams {
    pub gas_limit: u64,
    pub elasticity: u64,
}

/// Fetch chain parameters, trying L1 first and falling back to L2.
///
/// This fetches `gas_limit` and elasticity from the L1 `SystemConfig` contract.
/// If elasticity is not available on L1 (older `SystemConfig`), it falls back
/// to fetching from L2 `extraData`.
pub async fn fetch_chain_params(config: &ChainConfig) -> Result<ChainParams> {
    let l1_params =
        fetch_system_config_params(config.l1_rpc.as_str(), config.system_config).await?;

    let elasticity = match l1_params.elasticity {
        Some(e) => e,
        None => fetch_elasticity(config.rpc.as_str()).await?,
    };

    Ok(ChainParams { gas_limit: l1_params.gas_limit, elasticity })
}

/// Fetch the EIP-1559 elasticity multiplier from the L2 block extraData.
/// Falls back to default (6) if extraData is not in Holocene format.
pub async fn fetch_elasticity(rpc_url: &str) -> Result<u64> {
    let provider = ProviderBuilder::new().connect(rpc_url).await?;

    let block = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("No block found"))?;

    let extra_data = &block.header.extra_data;

    // Holocene format: version(1) + denominator(4) + elasticity(4) = 9 bytes
    if extra_data.len() >= 9 && extra_data[0] == 0 {
        let elasticity =
            u32::from_be_bytes([extra_data[5], extra_data[6], extra_data[7], extra_data[8]]);
        Ok(elasticity as u64)
    } else {
        // Pre-Holocene or invalid format, use default
        Ok(DEFAULT_ELASTICITY)
    }
}

#[derive(Debug, Clone)]
pub struct L1BlockInfo {
    pub block_number: u64,
    pub block_hash: B256,
    pub timestamp: u64,
    pub total_blobs: u64,
    pub base_blobs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L1ConnectionMode {
    WebSocket,
    Polling,
}

fn http_to_ws(url: &str) -> String {
    url.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1)
}

pub async fn run_l1_blob_watcher(
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
        warn!("Failed to connect to L1 WebSocket: {e}");
        let _ = toast_tx.try_send(Toast::warning("L1 WebSocket connection failed"));
    })?;

    let sub = provider.subscribe_blocks().await.map_err(|e| {
        warn!("Failed to subscribe to L1 blocks: {e}");
        let _ = toast_tx.try_send(Toast::warning("L1 block subscription failed"));
    })?;
    let mut stream = sub.into_stream();

    let _ = mode_tx.send(L1ConnectionMode::WebSocket).await;

    if let Ok(Some(block)) = provider.get_block_by_number(BlockNumberOrTag::Latest).full().await {
        let info = extract_l1_block_info(&block, batcher_address);
        let _ = result_tx.send(info).await;
    }

    while let Some(header) = stream.next().await {
        let block_num = header.number;

        if let Ok(Some(block)) =
            provider.get_block_by_number(BlockNumberOrTag::Number(block_num)).full().await
        {
            let info = extract_l1_block_info(&block, batcher_address);
            if result_tx.send(info).await.is_err() {
                return Ok(());
            }
        }
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
            warn!("Failed to connect to L1 RPC for polling: {e}");
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
        block_hash: block.header.hash,
        timestamp: block.header.timestamp,
        total_blobs,
        base_blobs,
    }
}
