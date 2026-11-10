use std::{
    ops::{ControlFlow, RangeInclusive},
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_consensus::Transaction;
use alloy_eips::{BlockId, eip2718::Encodable2718};
use alloy_primitives::{Address, B256};
use alloy_provider::{Network, Provider, ProviderBuilder, network::TransactionResponse};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_transport_http::Http;
use anyhow::{Context, Result, anyhow};
use base_common_genesis::DenimTimestampSchedule;
use base_common_network::Base;
use futures::{StreamExt, stream};
use tokio::{sync::mpsc, time::Instant};
use tracing::{debug, warn};
use url::Url;

use super::{ConnectionMode, fetch_safe_and_latest};
use crate::tui::Toast;

const CONCURRENT_BLOCK_FETCHES: usize = 16;
const STALLED_FETCH_TOAST_INTERVAL: Duration = Duration::from_secs(30);
const BACKLOG_SETUP_RETRY_INTERVAL: Duration = Duration::from_secs(5);

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

/// Fetches the L2 chain ID via `eth_chainId` with an explicit request timeout.
pub async fn fetch_l2_chain_id(rpc: &Url) -> Result<u64> {
    connect_l2_provider(rpc)
        .await?
        .get_chain_id()
        .await
        .with_context(|| format!("fetching eth_chainId from {rpc}"))
}

/// Fetches the latest L2 block number via `eth_blockNumber` with an explicit request timeout.
pub async fn fetch_l2_block_number(rpc: &Url) -> Result<u64> {
    connect_l2_provider(rpc)
        .await?
        .get_block_number()
        .await
        .with_context(|| format!("fetching eth_blockNumber from {rpc}"))
}

async fn connect_l2_provider(rpc: &Url) -> Result<impl Provider<Base>> {
    let http_client = alloy_transport_http::reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .with_context(|| format!("building L2 HTTP client for {rpc}"))?;
    let transport = Http::with_client(http_client, rpc.clone());
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect_client(RpcClient::new(transport, false)))
}

/// DA and gas information for a single L2 block.
#[derive(Debug, Clone, Default)]
pub struct BlockDaInfo {
    /// L2 block number.
    pub block_number: u64,
    /// EIP-2718 encoded bytes of the non-deposit transactions the batcher must post.
    pub da_bytes: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
    /// Number of transactions in the block.
    pub tx_count: usize,
    /// Gas used by the block.
    pub gas_used: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Base fee per gas in wei, if the block has one.
    pub base_fee_per_gas: Option<u64>,
}

impl BlockDaInfo {
    /// Returns the age of this block in seconds since its timestamp.
    pub fn age_seconds(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.timestamp)
    }
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

/// Result of initial backlog fetch - either progress or complete.
#[derive(Debug, Clone)]
pub enum BacklogFetchResult {
    /// Incremental progress update.
    Progress(BacklogProgress),
    /// A single fetched block.
    Block(BlockDaInfo),
    /// Backlog fetch completed with every block in the range.
    Complete(InitialBacklog),
}

/// First delay before retrying an L2 block fetch; it doubles up to [`BLOCK_FETCH_MAX_DELAY`].
/// Public RPCs rate limit bursts, and a load-balanced node can briefly lag the
/// head that was announced.
const BLOCK_FETCH_MIN_DELAY: Duration = Duration::from_millis(250);
/// Longest delay between attempts to fetch one L2 block.
const BLOCK_FETCH_MAX_DELAY: Duration = Duration::from_secs(4);
/// Failed attempts after which a block fetch counts as stalled (about 8 seconds).
const BLOCK_FETCH_STALL_ATTEMPTS: u32 = 5;

/// Fetches a single L2 block and summarizes its DA and gas usage.
///
/// DA totals are only correct when no block is skipped, so errors and
/// not-yet-visible blocks are retried with backoff until the fetch succeeds.
/// `on_stall` runs once if the block is still failing after
/// [`BLOCK_FETCH_STALL_ATTEMPTS`] attempts.
async fn fetch_raw_block_info<P: Provider<Base>>(
    provider: &P,
    block_num: u64,
    on_stall: &(dyn Fn() + Sync),
) -> BlockDaInfo {
    let mut delay = BLOCK_FETCH_MIN_DELAY;
    let mut attempts = 0u32;
    let block = loop {
        match provider.get_block_by_number(BlockNumberOrTag::Number(block_num)).full().await {
            Ok(Some(block)) => break block,
            Ok(None) => debug!(block = block_num, "L2 block not found yet"),
            Err(error) => debug!(error = %error, block = block_num, "L2 block fetch failed"),
        }
        attempts += 1;
        if attempts == BLOCK_FETCH_STALL_ATTEMPTS {
            warn!(block = block_num, "L2 block fetch still failing; retrying");
            on_stall();
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(BLOCK_FETCH_MAX_DELAY);
    };

    // Deposits are derived from L1, so the batcher never posts them.
    let da_bytes: u64 = block
        .transactions
        .txns()
        .filter(|tx| !tx.inner.inner.is_deposit())
        .map(|tx| tx.inner.inner.encode_2718_len() as u64)
        .sum();

    BlockDaInfo {
        block_number: block_num,
        da_bytes,
        timestamp: block.header.timestamp,
        tx_count: block.transactions.len(),
        gas_used: block.header.gas_used,
        gas_limit: block.header.gas_limit,
        base_fee_per_gas: block.header.base_fee_per_gas,
    }
}

/// Tracks the newest L2 head seen so each block number is requested once, in order.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeadCursor {
    last_seen: Option<u64>,
}

impl HeadCursor {
    /// Returns the block numbers to request for a newly observed head and advances past it.
    ///
    /// The first head requests only itself, a head that skips ahead also requests the
    /// skipped blocks, and a head at or below the newest seen requests nothing.
    pub fn advance(&mut self, head: u64) -> RangeInclusive<u64> {
        let first = self.last_seen.map_or(head, |seen| seen + 1);
        self.last_seen = Some(self.last_seen.map_or(head, |seen| seen.max(head)));
        first..=head
    }
}

/// Follows new L2 heads and requests each new block number from [`run_block_fetcher`].
///
/// Prefers an `eth_subscribe("newHeads")` WebSocket subscription. When the
/// WebSocket is unavailable or drops, it polls `eth_blockNumber` over HTTP at
/// the block interval and retries the WebSocket every [`Self::WS_RETRY_INTERVAL`].
/// A shared [`HeadCursor`] backfills any blocks missed across reconnects.
#[derive(Debug)]
pub struct L2HeadFeed {
    /// WebSocket endpoint to subscribe on, if any.
    pub ws_url: Option<Url>,
    /// HTTP endpoint polled while the WebSocket is unavailable.
    pub http_url: Url,
    /// Receives each new block number.
    pub request_tx: mpsc::Sender<u64>,
    /// Receives the connection mode whenever it changes.
    pub mode_tx: mpsc::Sender<ConnectionMode>,
    /// Receives user-facing connection notices.
    pub toast_tx: mpsc::Sender<Toast>,
    cursor: HeadCursor,
    mode: Option<ConnectionMode>,
}

impl L2HeadFeed {
    /// HTTP polling cadence, matching the Denim block interval.
    pub const POLL_INTERVAL: Duration =
        Duration::from_millis(DenimTimestampSchedule::DENIM_BLOCK_INTERVAL_MILLIS);
    /// Longest delay between HTTP polls after consecutive failures.
    pub const MAX_POLL_BACKOFF: Duration = Duration::from_secs(5);
    /// Upper bound on establishing the WebSocket before falling back to polling.
    pub const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    /// How long to poll over HTTP before trying the WebSocket again.
    pub const WS_RETRY_INTERVAL: Duration = Duration::from_secs(30);

    /// Creates a feed that has not seen any head yet.
    pub fn new(
        ws_url: Option<Url>,
        http_url: Url,
        request_tx: mpsc::Sender<u64>,
        mode_tx: mpsc::Sender<ConnectionMode>,
        toast_tx: mpsc::Sender<Toast>,
    ) -> Self {
        Self {
            ws_url,
            http_url,
            request_tx,
            mode_tx,
            toast_tx,
            cursor: HeadCursor::default(),
            mode: None,
        }
    }

    /// Runs until the block request receiver is dropped.
    pub async fn run(mut self) {
        loop {
            if let Some(ws_url) = self.ws_url.clone()
                && self.follow_ws(&ws_url).await.is_break()
            {
                return;
            }
            let retry_ws_at =
                self.ws_url.is_some().then(|| Instant::now() + Self::WS_RETRY_INTERVAL);
            if self.poll_http(retry_ws_at).await.is_break() {
                return;
            }
        }
    }

    async fn follow_ws(&mut self, ws_url: &Url) -> ControlFlow<()> {
        let connect = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect(ws_url.as_str());
        let provider = match tokio::time::timeout(Self::WS_CONNECT_TIMEOUT, connect).await {
            Ok(Ok(provider)) => provider,
            Ok(Err(error)) => {
                debug!(error = %error, url = %ws_url, "L2 WebSocket connection failed");
                return ControlFlow::Continue(());
            }
            Err(_) => {
                debug!(url = %ws_url, "L2 WebSocket connection timed out");
                return ControlFlow::Continue(());
            }
        };
        let subscription = match provider.subscribe_blocks().await {
            Ok(subscription) => subscription,
            Err(error) => {
                warn!(error = %error, url = %ws_url, "L2 newHeads subscription failed");
                return ControlFlow::Continue(());
            }
        };

        self.set_mode(ConnectionMode::WebSocket).await;
        let mut heads = subscription.into_stream();
        while let Some(header) = heads.next().await {
            self.request(header.number).await?;
        }

        warn!(url = %ws_url, "L2 WebSocket stream ended");
        let _ = self.toast_tx.try_send(Toast::warning("L2 WebSocket disconnected"));
        ControlFlow::Continue(())
    }

    /// Polls until `until`, or forever when it is `None`.
    async fn poll_http(&mut self, until: Option<Instant>) -> ControlFlow<()> {
        let provider = match ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect(self.http_url.as_str())
            .await
        {
            Ok(provider) => provider,
            Err(error) => {
                warn!(error = %error, url = %self.http_url, "L2 RPC connection failed");
                let _ = self.toast_tx.try_send(Toast::warning("L2 head feed connection failed"));
                tokio::time::sleep(Self::WS_RETRY_INTERVAL).await;
                return ControlFlow::Continue(());
            }
        };

        self.set_mode(ConnectionMode::Polling).await;
        let mut delay = Self::POLL_INTERVAL;
        while until.is_none_or(|until| Instant::now() < until) {
            tokio::time::sleep(delay).await;
            match provider.get_block_number().await {
                Ok(head) => {
                    delay = Self::POLL_INTERVAL;
                    self.request(head).await?;
                }
                // Back off so a rate-limited endpoint can recover instead of being hammered.
                Err(error) => {
                    debug!(error = %error, "L2 head poll failed");
                    delay = (delay * 2).min(Self::MAX_POLL_BACKOFF);
                }
            }
        }
        ControlFlow::Continue(())
    }

    async fn request(&mut self, head: u64) -> ControlFlow<()> {
        for block_number in self.cursor.advance(head) {
            if self.request_tx.send(block_number).await.is_err() {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    async fn set_mode(&mut self, mode: ConnectionMode) {
        if self.mode == Some(mode) {
            return;
        }
        if mode == ConnectionMode::Polling && self.ws_url.is_some() {
            let _ = self.toast_tx.try_send(Toast::info("L2 head feed fell back to HTTP polling"));
        }
        self.mode = Some(mode);
        let _ = self.mode_tx.send(mode).await;
    }
}

/// Fetches DA info for requested block numbers and sends results back in request order.
///
/// Up to [`CONCURRENT_BLOCK_FETCHES`] fetches run at once so a remote RPC keeps up
/// with sub-second blocks.
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

    let last_stall_toast: Mutex<Option<Instant>> = Mutex::new(None);
    let on_stall = || {
        let mut last = last_stall_toast.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.is_none_or(|at| at.elapsed() >= STALLED_FETCH_TOAST_INTERVAL) {
            *last = Some(Instant::now());
            let _ = toast_tx.try_send(Toast::warning("L2 RPC failing or rate limited"));
        }
    };
    let mut fetched = stream::poll_fn(|cx| request_rx.poll_recv(cx))
        .map(|block_num| fetch_raw_block_info(&provider, block_num, &on_stall))
        .buffered(CONCURRENT_BLOCK_FETCHES);
    while let Some(info) = fetched.next().await {
        if result_tx.send(info).await.is_err() {
            break;
        }
    }
}

/// Fetches every block between the safe and latest heads, sending progress updates
/// and block data, then [`BacklogFetchResult::Complete`].
///
/// Failures are retried rather than reported, so `Complete` always means the
/// backlog total includes every block.
pub async fn fetch_initial_backlog_with_progress(
    l2_rpc: String,
    progress_tx: mpsc::Sender<BacklogFetchResult>,
) {
    let (provider, safe_block, unsafe_block) = loop {
        let setup = async {
            let (safe_block, unsafe_block) = fetch_safe_and_latest(&l2_rpc).await?;
            let provider = ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<Base>()
                .connect(&l2_rpc)
                .await?;
            Ok::<_, anyhow::Error>((provider, safe_block, unsafe_block))
        };
        match setup.await {
            Ok(setup) => break setup,
            Err(error) => {
                warn!(error = %error, "Backlog fetch setup failed; retrying");
                tokio::time::sleep(BACKLOG_SETUP_RETRY_INTERVAL).await;
            }
        }
    };

    let total_blocks = unsafe_block.saturating_sub(safe_block);
    let mut total_da_bytes: u64 = 0;
    let mut blocks: Vec<BlockDaInfo> = Vec::with_capacity(total_blocks as usize);

    let mut fetch_stream = stream::iter((safe_block + 1)..=unsafe_block)
        .map(|block_num| fetch_raw_block_info(&provider, block_num, &|| {}))
        .buffer_unordered(CONCURRENT_BLOCK_FETCHES);

    while let Some(block) = fetch_stream.next().await {
        total_da_bytes = total_da_bytes.saturating_add(block.da_bytes);
        blocks.push(block);

        if (blocks.len() as u64).is_multiple_of(10) {
            let _ = progress_tx
                .send(BacklogFetchResult::Progress(BacklogProgress {
                    current_block: blocks.len() as u64,
                    total_blocks,
                }))
                .await;
        }
    }

    blocks.sort_by_key(|b| b.block_number);
    for block in blocks {
        let _ = progress_tx.send(BacklogFetchResult::Block(block)).await;
    }
    let _ = progress_tx
        .send(BacklogFetchResult::Complete(InitialBacklog { safe_block, da_bytes: total_da_bytes }))
        .await;
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use alloy_provider::{Network, ProviderBuilder, mock::Asserter};
    use base_common_network::Base;

    use super::{HeadCursor, effective_priority_fee_per_gas, fetch_raw_block_info};

    #[tokio::test]
    async fn block_fetch_retries_errors_and_missing_blocks_until_it_succeeds() {
        let asserter = Asserter::new();
        let provider =
            ProviderBuilder::new().network::<Base>().connect_mocked_client(asserter.clone());
        let mut block = <Base as Network>::BlockResponse::default();
        block.header.inner.inner.number = 42;
        block.header.inner.inner.gas_used = 21_000;
        asserter.push_failure_msg("rate limited");
        asserter.push_success(&Option::<<Base as Network>::BlockResponse>::None);
        asserter.push_success(&block);

        let stalls = AtomicU32::new(0);
        let info = fetch_raw_block_info(&provider, 42, &|| {
            stalls.fetch_add(1, Ordering::Relaxed);
        })
        .await;

        assert_eq!(info.block_number, 42);
        assert_eq!(info.gas_used, 21_000);
        assert_eq!(stalls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn head_cursor_requests_each_new_block_once_in_order() {
        let mut cursor = HeadCursor::default();
        assert_eq!(cursor.advance(100).collect::<Vec<_>>(), [100]);
        assert_eq!(cursor.advance(101).collect::<Vec<_>>(), [101]);
        // A skipped head backfills the gap.
        assert_eq!(cursor.advance(104).collect::<Vec<_>>(), [102, 103, 104]);
        // Repeated or older heads (reorgs, overlapping sources) request nothing.
        assert_eq!(cursor.advance(104).count(), 0);
        assert_eq!(cursor.advance(103).count(), 0);
        assert_eq!(cursor.advance(105).collect::<Vec<_>>(), [105]);
    }

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
