use std::{sync::Arc, time::Duration};

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use anyhow::Result;
use base_alloy_consensus::OpTxEnvelope;
use base_alloy_flashblocks::Flashblock;
use base_alloy_network::{Base, ReceiptResponse, TransactionResponse};
use futures::{StreamExt, stream};
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
                            warn!(error = %e, "Flashblock WebSocket connection error");
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
                warn!(error = %e, "Failed to connect to flashblock WebSocket");
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
    /// Effective tip per gas (effective gas price minus base fee), in wei.
    pub effective_tip_per_gas: Option<u128>,
    /// Effective tip paid by this transaction, in wei.
    pub effective_tip_paid: Option<u128>,
    /// Block base fee per gas, in wei.
    pub base_fee_per_gas: Option<u64>,
    /// Gas used by this transaction, if available.
    pub gas_used: Option<u64>,
    /// Whether this transaction is known to have reverted.
    pub reverted: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TxReceiptFields {
    /// Gas used by this transaction.
    pub gas_used: u64,
    /// Effective gas price paid by this transaction, in wei.
    pub effective_gas_price: u128,
    /// Whether the transaction succeeded.
    pub success: bool,
}

fn effective_tip_per_gas(base_fee_per_gas: Option<u64>, effective_gas_price: u128) -> Option<u128> {
    base_fee_per_gas.map(|base_fee| effective_gas_price.saturating_sub(u128::from(base_fee)))
}

fn effective_tip_paid(
    base_fee_per_gas: Option<u64>,
    effective_gas_price: u128,
    gas_used: u64,
) -> Option<u128> {
    effective_tip_per_gas(base_fee_per_gas, effective_gas_price)
        .map(|tip_per_gas| tip_per_gas.saturating_mul(u128::from(gas_used)))
}

fn ordered_receipt_fields_from_receipt_pairs(
    tx_hashes: &[B256],
    receipt_pairs: Vec<(B256, TxReceiptFields)>,
) -> Option<Vec<Option<TxReceiptFields>>> {
    (receipt_pairs.len() == tx_hashes.len()).then(|| {
        receipt_pairs.into_iter().map(|(_, receipt_fields)| Some(receipt_fields)).collect()
    })
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
            let effective_tip_per_gas = effective_tip_per_gas(
                base_fee_per_gas,
                envelope.effective_gas_price(base_fee_per_gas),
            );
            let recovered = envelope
                .try_into_recovered()
                .inspect_err(|e| warn!(error = %e, "failed to recover signer"))
                .ok()?;
            Some(TxSummary {
                hash,
                from: recovered.signer(),
                to,
                effective_tip_per_gas,
                effective_tip_paid: None,
                base_fee_per_gas,
                gas_used: None,
                reverted: None,
            })
        })
        .collect()
}

async fn fetch_receipt_fields_by_hashes<P: Provider<Base>>(
    provider: Arc<P>,
    block_number: u64,
    block_hash: B256,
    tx_hashes: Vec<B256>,
) -> std::collections::HashMap<B256, TxReceiptFields> {
    match provider.get_block_receipts(block_hash.into()).await {
        Ok(Some(receipts)) => {
            return receipts
                .into_iter()
                .map(|receipt| {
                    (
                        receipt.transaction_hash(),
                        TxReceiptFields {
                            gas_used: receipt.gas_used(),
                            effective_gas_price: receipt.effective_gas_price(),
                            success: receipt.status(),
                        },
                    )
                })
                .collect();
        }
        Ok(None) => {
            warn!(
                block = block_number,
                "block receipts not found; falling back to per-transaction receipts"
            );
        }
        Err(e) => {
            warn!(
                error = %e,
                block = block_number,
                "failed to fetch block receipts; falling back to per-transaction receipts"
            );
        }
    }

    let mut receipt_fields_by_hash = std::collections::HashMap::with_capacity(tx_hashes.len());

    let mut receipt_stream = stream::iter(tx_hashes.into_iter().map(|hash| {
        let provider = Arc::clone(&provider);
        async move { (hash, provider.get_transaction_receipt(hash).await) }
    }))
    .buffer_unordered(CONCURRENT_BLOCK_FETCHES);

    while let Some((hash, receipt_result)) = receipt_stream.next().await {
        match receipt_result {
            Ok(Some(receipt)) => {
                receipt_fields_by_hash.insert(
                    hash,
                    TxReceiptFields {
                        gas_used: receipt.gas_used(),
                        effective_gas_price: receipt.effective_gas_price(),
                        success: receipt.status(),
                    },
                );
            }
            Ok(None) => {}
            Err(e) => {
                warn!(
                    error = %e,
                    block = block_number,
                    tx_hash = %format_args!("{hash:#x}"),
                    "failed to fetch transaction receipt"
                );
            }
        }
    }

    receipt_fields_by_hash
}

async fn fetch_ordered_receipt_fields<P: Provider<Base>>(
    provider: Arc<P>,
    block_number: u64,
    block_hash: B256,
    tx_hashes: Vec<B256>,
) -> Vec<Option<TxReceiptFields>> {
    match provider.get_block_receipts(block_hash.into()).await {
        Ok(Some(receipts)) => {
            let receipt_count = receipts.len();
            let receipt_pairs: Vec<_> = receipts
                .into_iter()
                .map(|receipt| {
                    (
                        receipt.transaction_hash(),
                        TxReceiptFields {
                            gas_used: receipt.gas_used(),
                            effective_gas_price: receipt.effective_gas_price(),
                            success: receipt.status(),
                        },
                    )
                })
                .collect();
            if let Some(ordered_receipt_fields) =
                ordered_receipt_fields_from_receipt_pairs(&tx_hashes, receipt_pairs)
            {
                return ordered_receipt_fields;
            }

            warn!(
                block = block_number,
                receipt_count,
                tx_count = tx_hashes.len(),
                "block receipts count mismatch; falling back to hash-based receipt matching"
            );
        }
        Ok(None) => {
            warn!(
                block = block_number,
                "block receipts not found; falling back to hash-based receipt matching"
            );
        }
        Err(e) => {
            warn!(
                error = %e,
                block = block_number,
                "failed to fetch block receipts; falling back to hash-based receipt matching"
            );
        }
    }

    let receipt_fields_by_hash =
        fetch_receipt_fields_by_hashes(provider, block_number, block_hash, tx_hashes.clone()).await;
    tx_hashes.into_iter().map(|hash| receipt_fields_by_hash.get(&hash).copied()).collect()
}

/// Fetches per-transaction gas used for a block keyed by transaction hash.
pub(crate) async fn fetch_block_transaction_gas(
    l2_rpc: String,
    block_number: u64,
    tx: mpsc::Sender<Vec<(B256, TxReceiptFields)>>,
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

        let tx_hashes = block.transactions.txns().map(|tx| tx.inner.tx_hash()).collect();
        let receipt_fields_by_hash =
            fetch_receipt_fields_by_hashes(provider, block_number, block.header.hash, tx_hashes)
                .await;

        Ok::<_, anyhow::Error>(receipt_fields_by_hash)
    }
    .await;

    match result {
        Ok(receipt_fields_by_hash) => {
            let _ = tx.send(receipt_fields_by_hash.into_iter().collect()).await;
        }
        Err(e) => {
            warn!(error = %e, block = block_number, "failed to fetch block transaction gas");
            let _ = tx.send(Vec::new()).await;
        }
    }
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
        let tx_hashes = block.transactions.txns().map(|tx| tx.inner.tx_hash()).collect();
        let ordered_receipt_fields =
            fetch_ordered_receipt_fields(provider, block_number, block.header.hash, tx_hashes)
                .await;

        let summaries: Vec<TxSummary> = block
            .transactions
            .txns()
            .zip(ordered_receipt_fields.into_iter())
            .map(|tx_obj| {
                let (tx_obj, receipt_fields) = tx_obj;
                let hash = tx_obj.inner.tx_hash();
                TxSummary {
                    hash,
                    from: tx_obj.inner.inner.signer(),
                    to: tx_obj.inner.to(),
                    effective_tip_per_gas: receipt_fields.and_then(|receipt| {
                        effective_tip_per_gas(base_fee, receipt.effective_gas_price)
                    }),
                    effective_tip_paid: receipt_fields.and_then(|receipt| {
                        effective_tip_paid(base_fee, receipt.effective_gas_price, receipt.gas_used)
                    }),
                    base_fee_per_gas: base_fee,
                    gas_used: receipt_fields.map(|receipt| receipt.gas_used),
                    reverted: receipt_fields.map(|receipt| !receipt.success),
                }
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

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn ordered_receipt_matching_prefers_position_when_counts_match() {
        let tx_hashes = vec![
            b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            b256!("0000000000000000000000000000000000000000000000000000000000000002"),
        ];
        let unrelated_receipt_hashes = vec![
            b256!("00000000000000000000000000000000000000000000000000000000000000aa"),
            b256!("00000000000000000000000000000000000000000000000000000000000000bb"),
        ];
        let receipt_pairs = unrelated_receipt_hashes
            .into_iter()
            .zip([
                TxReceiptFields { gas_used: 21_000, effective_gas_price: 7, success: true },
                TxReceiptFields { gas_used: 42_000, effective_gas_price: 9, success: false },
            ])
            .collect();

        let ordered =
            ordered_receipt_fields_from_receipt_pairs(&tx_hashes, receipt_pairs).expect("matched");

        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].expect("first").gas_used, 21_000);
        assert_eq!(ordered[1].expect("second").gas_used, 42_000);
    }

    #[test]
    fn ordered_receipt_matching_rejects_count_mismatch() {
        let tx_hashes = vec![
            b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            b256!("0000000000000000000000000000000000000000000000000000000000000002"),
        ];
        let receipt_pairs = vec![(
            b256!("00000000000000000000000000000000000000000000000000000000000000aa"),
            TxReceiptFields { gas_used: 21_000, effective_gas_price: 7, success: true },
        )];

        assert!(ordered_receipt_fields_from_receipt_pairs(&tx_hashes, receipt_pairs).is_none());
    }

    #[tokio::test]
    #[ignore = "debug network test"]
    async fn debug_block_43312151_receipt_population() {
        let (tx, mut rx) = mpsc::channel(1);
        fetch_block_transactions("https://mainnet.base.org".to_string(), 43_312_151, tx).await;
        let summaries = rx.recv().await.expect("result").expect("fetch ok");
        let missing = summaries
            .iter()
            .enumerate()
            .filter(|(_, summary)| {
                summary.gas_used.is_none()
                    || summary.effective_tip_per_gas.is_none()
                    || summary.effective_tip_paid.is_none()
            })
            .map(|(idx, summary)| (idx, summary.hash))
            .collect::<Vec<_>>();

        eprintln!("tx_count={} missing_count={}", summaries.len(), missing.len());
        for (idx, hash) in missing.iter().take(25) {
            eprintln!("missing idx={idx} hash={hash:#x}");
        }

        assert!(missing.is_empty(), "missing {:?}", missing);
    }
}
