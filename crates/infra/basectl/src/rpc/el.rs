use std::sync::Arc;

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_eips::{
    BlockId,
    eip2718::{Decodable2718, Encodable2718},
};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Network, Provider, ProviderBuilder, network::TransactionResponse};
use alloy_rpc_types_eth::BlockNumberOrTag;
use anyhow::{Context, Result, anyhow};
use base_common_consensus::BaseTxEnvelope;
use base_common_network::Base;
use futures::{StreamExt, stream};
use tokio::sync::mpsc;
use tracing::warn;
use url::Url;

use super::fetch_safe_and_latest;
use crate::tui::Toast;

const CONCURRENT_BLOCK_FETCHES: usize = 16;

/// Fetches a single L2 block via `eth_getBlockByHash` or `eth_getBlockByNumber`.
///
/// `reference` selects the block by hash, number, or tag (alloy's `BlockId`
/// dispatches between the two RPC methods internally). The `pending` tag is
/// not supported because alloy's typed `Block` does not accept a null
/// number/hash; pass a number, hash, or `latest` / `safe` / `finalized` /
/// `earliest`.
pub async fn fetch_block(
    rpc: &Url,
    reference: BlockId,
) -> Result<<Base as Network>::BlockResponse> {
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect(rpc.as_str())
        .await
        .with_context(|| format!("connecting to L2 RPC at {rpc}"))?;
    provider
        .get_block(reference)
        .await
        .with_context(|| format!("fetching block {reference}"))?
        .ok_or_else(|| anyhow!("block {reference} not found"))
}

/// DA and gas information for a single L2 block.
#[derive(Debug, Clone)]
pub struct BlockDaInfo {
    /// L2 block number.
    pub block_number: u64,
    /// Total DA bytes from all transactions.
    pub da_bytes: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
}

/// Summary of the initial DA backlog between safe and latest blocks.
#[derive(Debug, Clone)]
pub struct InitialBacklog {
    /// Safe L2 block number.
    pub safe_block: u64,
    /// Total DA bytes across all backlog blocks.
    pub da_bytes: u64,
}

/// Progress update during initial backlog fetch.
#[derive(Debug, Clone)]
pub struct BacklogProgress {
    /// Number of blocks fetched so far.
    pub current_block: u64,
    /// Total number of blocks to fetch.
    pub total_blocks: u64,
}

/// Individual block data from backlog fetch.
#[derive(Debug, Clone)]
pub struct BacklogBlock {
    /// L2 block number.
    pub block_number: u64,
    /// DA bytes contributed by this block.
    pub da_bytes: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
}

/// Result of initial backlog fetch - either progress or complete.
#[derive(Debug, Clone)]
pub enum BacklogFetchResult {
    /// Incremental progress update.
    Progress(BacklogProgress),
    /// A single fetched block.
    Block(BacklogBlock),
    /// Backlog fetch completed successfully.
    Complete(InitialBacklog),
    /// Backlog fetch failed.
    Error,
}

/// Raw DA bytes and timestamp for a single L2 block, decoupled from the higher-level
/// shapes (`BlockDaInfo`, `BacklogBlock`) that wrap it for different consumers.
struct RawBlockInfo {
    da_bytes: u64,
    timestamp: u64,
}

/// Fetches a single L2 block and computes its DA bytes.
///
/// Returns `None` if the RPC call fails or the block does not exist.
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

/// Fetches DA info for requested block numbers and sends results back.
pub async fn run_block_fetcher(
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

/// Fetches the initial DA backlog, sending progress updates and block data.
pub async fn fetch_initial_backlog_with_progress(
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
                    fetch_raw_block_info(&*provider, block_num).await.map_or(
                        BacklogBlock { block_number: block_num, da_bytes: 0, timestamp: 0 },
                        |info| BacklogBlock {
                            block_number: block_num,
                            da_bytes: info.da_bytes,
                            timestamp: info.timestamp,
                        },
                    )
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

/// Summary of a single transaction within a block.
#[derive(Debug, Clone)]
pub struct TxSummary {
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
pub fn decode_flashblock_transactions(
    raw_txs: &[Bytes],
    base_fee_per_gas: Option<u64>,
) -> Vec<TxSummary> {
    raw_txs
        .iter()
        .filter_map(|tx_bytes| {
            let envelope = BaseTxEnvelope::decode_2718_exact(tx_bytes.as_ref())
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
pub async fn fetch_block_transactions(
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
