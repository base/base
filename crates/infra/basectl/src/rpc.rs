use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::{BlockNumberOrTag, TransactionTrait};
use anyhow::Result;
use base_flashtypes::Flashblock;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;

const CONCURRENT_BLOCK_FETCHES: usize = 16;

use crate::{config::ChainConfig, l1_client::fetch_system_config_params};

const DEFAULT_ELASTICITY: u64 = 6;

#[derive(Debug, Clone, Deserialize)]
pub struct L2BlockRef {
    pub hash: B256,
    pub number: u64,
    #[serde(rename = "parentHash")]
    pub parent_hash: B256,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncStatus {
    pub unsafe_l2: L2BlockRef,
    pub safe_l2: L2BlockRef,
    pub finalized_l2: L2BlockRef,
}

pub async fn fetch_sync_status(op_node_rpc: &str) -> Result<SyncStatus> {
    let provider = ProviderBuilder::new().connect(op_node_rpc).await?;
    let status: SyncStatus = provider.raw_request("optimism_syncStatus".into(), ()).await?;
    Ok(status)
}

pub async fn run_flashblock_ws(url: String, tx: mpsc::Sender<Flashblock>) -> Result<()> {
    let (ws_stream, _) = connect_async(&url).await?;
    let (_, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if !msg.is_binary() && !msg.is_text() {
            continue;
        }
        let fb = Flashblock::try_decode_message(msg.into_data())?;
        if tx.send(fb).await.is_err() {
            break;
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct TimestampedFlashblock {
    pub flashblock: Flashblock,
    pub received_at: chrono::DateTime<chrono::Local>,
}

pub async fn run_flashblock_ws_timestamped(
    url: String,
    tx: mpsc::Sender<TimestampedFlashblock>,
) -> Result<()> {
    let (ws_stream, _) = connect_async(&url).await?;
    let (_, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if !msg.is_binary() && !msg.is_text() {
            continue;
        }
        let fb = Flashblock::try_decode_message(msg.into_data())?;
        let timestamped =
            TimestampedFlashblock { flashblock: fb, received_at: chrono::Local::now() };
        if tx.send(timestamped).await.is_err() {
            break;
        }
    }
    Ok(())
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

/// Result of initial backlog fetch - either progress or complete
#[derive(Debug, Clone)]
pub enum BacklogFetchResult {
    Progress(BacklogProgress),
    Complete(InitialBacklog),
    Error(String),
}

pub async fn fetch_initial_backlog(l2_rpc: &str, op_node_rpc: &str) -> Result<InitialBacklog> {
    let status = fetch_sync_status(op_node_rpc).await?;
    let safe_block = status.safe_l2.number;
    let unsafe_block = status.unsafe_l2.number;

    if unsafe_block <= safe_block {
        return Ok(InitialBacklog { safe_block, unsafe_block, da_bytes: 0 });
    }

    let provider = Arc::new(ProviderBuilder::new().connect(l2_rpc).await?);

    let block_numbers: Vec<u64> = ((safe_block + 1)..=unsafe_block).collect();
    let total_da_bytes: u64 = stream::iter(block_numbers)
        .map(|block_num| {
            let provider = Arc::clone(&provider);
            async move {
                provider
                    .get_block_by_number(BlockNumberOrTag::Number(block_num))
                    .full()
                    .await
                    .ok()
                    .flatten()
                    .map(|block| {
                        block
                            .transactions
                            .txns()
                            .map(|tx| tx.inner.input().len() as u64)
                            .sum::<u64>()
                    })
                    .unwrap_or(0)
            }
        })
        .buffer_unordered(CONCURRENT_BLOCK_FETCHES)
        .fold(0u64, |acc, bytes| async move { acc.saturating_add(bytes) })
        .await;

    Ok(InitialBacklog { safe_block, unsafe_block, da_bytes: total_da_bytes })
}

/// Fetch initial backlog with progress updates via channel
pub async fn fetch_initial_backlog_with_progress(
    l2_rpc: String,
    op_node_rpc: String,
    progress_tx: tokio::sync::mpsc::Sender<BacklogFetchResult>,
) {
    let result = async {
        let status = fetch_sync_status(&op_node_rpc).await?;
        let safe_block = status.safe_l2.number;
        let unsafe_block = status.unsafe_l2.number;

        if unsafe_block <= safe_block {
            return Ok(InitialBacklog { safe_block, unsafe_block, da_bytes: 0 });
        }

        let total_blocks = unsafe_block - safe_block;
        let provider = Arc::new(ProviderBuilder::new().connect(&l2_rpc).await?);
        let mut total_da_bytes: u64 = 0;
        let mut blocks_processed: u64 = 0;

        let block_numbers: Vec<u64> = ((safe_block + 1)..=unsafe_block).collect();
        for chunk in block_numbers.chunks(CONCURRENT_BLOCK_FETCHES) {
            let chunk_results: Vec<u64> = stream::iter(chunk.iter().copied())
                .map(|block_num| {
                    let provider = Arc::clone(&provider);
                    async move {
                        provider
                            .get_block_by_number(BlockNumberOrTag::Number(block_num))
                            .full()
                            .await
                            .ok()
                            .flatten()
                            .map(|block| {
                                block
                                    .transactions
                                    .txns()
                                    .map(|tx| tx.inner.input().len() as u64)
                                    .sum::<u64>()
                            })
                            .unwrap_or(0)
                    }
                })
                .buffer_unordered(CONCURRENT_BLOCK_FETCHES)
                .collect()
                .await;

            for bytes in chunk_results {
                total_da_bytes = total_da_bytes.saturating_add(bytes);
                blocks_processed += 1;
            }

            let _ = progress_tx
                .send(BacklogFetchResult::Progress(BacklogProgress {
                    current_block: blocks_processed,
                    total_blocks,
                    da_bytes_so_far: total_da_bytes,
                }))
                .await;
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
}

pub async fn run_block_fetcher(
    l2_rpc: String,
    mut request_rx: mpsc::Receiver<u64>,
    result_tx: mpsc::Sender<BlockDaInfo>,
) {
    let provider = match ProviderBuilder::new().connect(&l2_rpc).await {
        Ok(p) => p,
        Err(_) => return,
    };

    while let Some(block_num) = request_rx.recv().await {
        if let Ok(Some(block)) =
            provider.get_block_by_number(BlockNumberOrTag::Number(block_num)).full().await
        {
            let da_bytes: u64 =
                block.transactions.txns().map(|tx| tx.inner.input().len() as u64).sum();

            let info = BlockDaInfo {
                block_number: block_num,
                da_bytes,
                tx_count: block.transactions.len(),
                gas_used: block.header.gas_used,
            };

            if result_tx.send(info).await.is_err() {
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

pub const BYTES_PER_BLOB: u64 = 131_072;

#[derive(Debug, Clone)]
pub struct BlobSubmission {
    pub block_number: u64,
    pub block_hash: B256,
    pub blob_count: u64,
    pub l1_blob_bytes: u64,
}

pub async fn run_l1_batcher_watcher(
    l1_rpc: String,
    batcher_address: Address,
    result_tx: mpsc::Sender<BlobSubmission>,
) {
    let provider = match ProviderBuilder::new().connect(&l1_rpc).await {
        Ok(p) => p,
        Err(_) => return,
    };

    let mut last_block: Option<u64> = None;
    let mut interval = tokio::time::interval(Duration::from_secs(12));

    loop {
        interval.tick().await;

        let latest = match provider.get_block_number().await {
            Ok(n) => n,
            Err(_) => continue,
        };

        let start_block = last_block.map(|b| b + 1).unwrap_or_else(|| latest.saturating_sub(5));

        for block_num in start_block..=latest {
            if let Ok(Some(block)) =
                provider.get_block_by_number(BlockNumberOrTag::Number(block_num)).full().await
            {
                let block_hash = block.header.hash;
                for tx in block.transactions.txns() {
                    if tx.inner.signer() == batcher_address
                        && let Some(blob_hashes) = tx.blob_versioned_hashes()
                    {
                        let blob_count = blob_hashes.len() as u64;
                        if blob_count > 0 {
                            let submission = BlobSubmission {
                                block_number: block_num,
                                block_hash,
                                blob_count,
                                l1_blob_bytes: blob_count * BYTES_PER_BLOB,
                            };
                            if result_tx.send(submission).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }

        last_block = Some(latest);
    }
}
