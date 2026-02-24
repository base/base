use std::{sync::Arc, time::Duration};

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use anyhow::Result;
use base_alloy_consensus::OpTxEnvelope;
use base_alloy_network::{Base, TransactionResponse};
use base_primitives::Flashblock;
use futures_util::{StreamExt, stream};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::warn;

use crate::tui::Toast;

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

/// Subscribes to flashblocks via WebSocket and forwards raw flashblocks.
pub(crate) async fn run_flashblock_ws(
    url: String,
    tx: mpsc::Sender<Flashblock>,
    toast_tx: mpsc::Sender<Toast>,
) {
    run_flashblock_ws_inner(&url, &tx, &toast_tx, |fb| fb).await;
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
            warn!("Backlog fetch failed: {e}");
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
    /// Max priority fee per gas (tip), in wei.
    pub max_priority_fee_per_gas: Option<u128>,
    /// Block base fee per gas, in wei.
    pub base_fee_per_gas: Option<u64>,
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
                .inspect_err(|e| warn!("Failed to decode transaction: {e}"))
                .ok()?;
            let hash = envelope.tx_hash();
            let to = envelope.to();
            let max_priority_fee = envelope.max_priority_fee_per_gas();
            let recovered = envelope
                .try_into_recovered()
                .inspect_err(|e| warn!("Failed to recover signer: {e}"))
                .ok()?;
            Some(TxSummary {
                hash,
                from: recovered.signer(),
                to,
                max_priority_fee_per_gas: max_priority_fee,
                base_fee_per_gas,
            })
        })
        .collect()
}

/// Fetches all transactions for a given block and sends summaries through the channel.
pub(crate) async fn fetch_block_transactions(
    l2_rpc: String,
    block_number: u64,
    tx: mpsc::Sender<Vec<TxSummary>>,
) {
    let result = async {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect(&l2_rpc)
            .await?;

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
                max_priority_fee_per_gas: tx_obj.inner.max_priority_fee_per_gas(),
                base_fee_per_gas: base_fee,
            })
            .collect();

        Ok::<_, anyhow::Error>(summaries)
    }
    .await;

    match result {
        Ok(summaries) => {
            let _ = tx.send(summaries).await;
        }
        Err(e) => {
            warn!("Failed to fetch block transactions for block {block_number}: {e}");
            let _ = tx.send(Vec::new()).await;
        }
    }
}
