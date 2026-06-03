use std::time::Duration;

use alloy_consensus::Transaction;
use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder, layers::CallBatchLayer};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_sol_types::sol;
use anyhow::Result;
use base_common_genesis::SystemConfig;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::warn;

use crate::tui::Toast;

sol! {
    #[sol(rpc)]
    interface ISystemConfig {
        function gasLimit() external view returns (uint64);
        function eip1559Elasticity() external view returns (uint32);
        function eip1559Denominator() external view returns (uint32);
        function batcherHash() external view returns (bytes32);
        function overhead() external view returns (uint256);
        function scalar() external view returns (uint256);
        function basefeeScalar() external view returns (uint32);
        function blobbasefeeScalar() external view returns (uint32);
    }
}

/// Fetch all available `SystemConfig` values from the L1 contract.
///
/// Uses Multicall3 via `CallBatchLayer` to batch all calls into a single RPC request.
/// Fields that are absent on older contract versions fall back to their defaults.
pub async fn fetch_full_system_config(
    l1_rpc_url: &str,
    system_config_address: Address,
) -> Result<SystemConfig> {
    // Use CallBatchLayer to batch all concurrent calls into a single Multicall3 call
    let provider = ProviderBuilder::new()
        .layer(CallBatchLayer::new().wait(Duration::from_millis(10)))
        .connect(l1_rpc_url)
        .await?;
    let contract = ISystemConfig::new(system_config_address, provider);

    // Create call builders first to avoid temporary borrow issues
    let gas_limit_call = contract.gasLimit();
    let eip1559_elasticity_call = contract.eip1559Elasticity();
    let eip1559_denominator_call = contract.eip1559Denominator();
    let batcher_hash_call = contract.batcherHash();
    let overhead_call = contract.overhead();
    let scalar_call = contract.scalar();
    let basefee_scalar_call = contract.basefeeScalar();
    let blobbasefee_scalar_call = contract.blobbasefeeScalar();

    // Fetch all values concurrently - each may fail on older versions
    let (
        gas_limit,
        eip1559_elasticity,
        eip1559_denominator,
        batcher_hash,
        overhead,
        scalar,
        basefee_scalar,
        blobbasefee_scalar,
    ) = tokio::join!(
        gas_limit_call.call(),
        eip1559_elasticity_call.call(),
        eip1559_denominator_call.call(),
        batcher_hash_call.call(),
        overhead_call.call(),
        scalar_call.call(),
        basefee_scalar_call.call(),
        blobbasefee_scalar_call.call(),
    );

    Ok(SystemConfig {
        batcher_address: batcher_hash
            .ok()
            .map(|h| Address::from_slice(&h.0[12..]))
            .unwrap_or_default(),
        overhead: overhead.ok().unwrap_or_default(),
        scalar: scalar.ok().unwrap_or_default(),
        gas_limit: gas_limit.ok().unwrap_or_default(),
        eip1559_elasticity: eip1559_elasticity.ok(),
        eip1559_denominator: eip1559_denominator.ok(),
        base_fee_scalar: basefee_scalar.ok().map(|v| v as u64),
        blob_base_fee_scalar: blobbasefee_scalar.ok().map(|v| v as u64),
        ..Default::default()
    })
}

/// Information about an L1 block and its blob counts.
#[derive(Debug, Clone)]
pub struct L1BlockInfo {
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
pub enum L1ConnectionMode {
    /// Connected via WebSocket subscription.
    WebSocket,
    /// Connected via HTTP polling.
    Polling,
}

fn http_to_ws(url: &str) -> String {
    url.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1)
}

/// Watches L1 blocks for blob transactions, preferring WebSocket with polling fallback.
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
        if result_tx.send(info).await.is_err() {
            return Ok(());
        }
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

        if block_num > last_block.unwrap_or(0)
            && let Ok(Some(block)) =
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
