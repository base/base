//! Ordered source L2 block prefetching for follow mode.

use std::{sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use tokio::{sync::mpsc, time};
use tokio_util::sync::CancellationToken;

use crate::actors::derivation::{
    DerivationError,
    delegate_l2::{DelegateL2ClientError, L2SourceClient},
};

/// Default number of fetched source blocks to buffer ahead of insertion.
pub const DEFAULT_SOURCE_PREFETCH_BUFFER_BLOCKS: usize = 256;

/// Configuration for source L2 block prefetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceBlockFetcherConfig {
    /// Number of fetched source blocks to buffer ahead of insertion.
    pub buffer_blocks: usize,
    /// How long to wait before polling source latest again when caught up.
    pub head_poll_interval: Duration,
    /// How long to wait before retrying a failed source RPC request.
    pub retry_backoff: Duration,
}

impl SourceBlockFetcherConfig {
    /// The default source block fetcher configuration.
    pub const DEFAULT: Self = Self {
        buffer_blocks: DEFAULT_SOURCE_PREFETCH_BUFFER_BLOCKS,
        head_poll_interval: Duration::from_secs(2),
        retry_backoff: Duration::from_millis(500),
    };
}

impl Default for SourceBlockFetcherConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A source L2 block fetched ahead of local insertion.
#[derive(Debug, Clone)]
pub struct PrefetchedL2Block {
    /// The expected L2 block number.
    pub number: u64,
    /// The source execution payload envelope.
    pub envelope: BaseExecutionPayloadEnvelope,
}

impl PrefetchedL2Block {
    /// Returns the payload block hash.
    pub const fn hash(&self) -> B256 {
        self.envelope.execution_payload.block_hash()
    }
}

/// Fetches source L2 blocks in order and pushes them into a bounded buffer.
#[derive(Debug)]
pub struct SourceBlockFetcher<L2Source>
where
    L2Source: L2SourceClient,
{
    l2_source: Arc<L2Source>,
    next_fetch_number: u64,
    remote_head: u64,
    output_tx: mpsc::Sender<PrefetchedL2Block>,
    cancellation_token: CancellationToken,
    config: SourceBlockFetcherConfig,
}

impl<L2Source> SourceBlockFetcher<L2Source>
where
    L2Source: L2SourceClient,
{
    /// Creates a source block fetcher starting at `start_number`.
    pub const fn new(
        l2_source: Arc<L2Source>,
        start_number: u64,
        output_tx: mpsc::Sender<PrefetchedL2Block>,
        cancellation_token: CancellationToken,
        config: SourceBlockFetcherConfig,
    ) -> Self {
        Self {
            l2_source,
            next_fetch_number: start_number,
            remote_head: 0,
            output_tx,
            cancellation_token,
            config,
        }
    }

    /// Runs the fetch loop until cancelled.
    pub async fn run(mut self) -> Result<(), DerivationError> {
        self.refresh_remote_head().await?;
        info!(
            target: "derivation",
            start_number = self.next_fetch_number,
            remote_head = self.remote_head,
            "Starting source block prefetcher"
        );

        loop {
            if self.cancellation_token.is_cancelled() {
                info!(
                    target: "derivation",
                    next_fetch_number = self.next_fetch_number,
                    "Source block prefetcher stopped"
                );
                return Ok(());
            }

            if self.next_fetch_number > self.remote_head {
                debug!(
                    target: "derivation",
                    next_fetch_number = self.next_fetch_number,
                    remote_head = self.remote_head,
                    "Source block prefetcher caught up"
                );
                self.sleep_or_cancel(self.config.head_poll_interval).await;
                self.refresh_remote_head().await?;
                continue;
            }

            match self.l2_source.get_payload_by_number(self.next_fetch_number).await {
                Ok(envelope) => {
                    let block = PrefetchedL2Block { number: self.next_fetch_number, envelope };
                    debug!(
                        target: "derivation",
                        block_number = block.number,
                        block_hash = %block.hash(),
                        "Prefetched source L2 block"
                    );
                    if self.output_tx.send(block).await.is_err() {
                        info!(
                            target: "derivation",
                            next_fetch_number = self.next_fetch_number,
                            "Source block prefetch receiver closed"
                        );
                        return Ok(());
                    }
                    self.next_fetch_number = self.next_fetch_number.saturating_add(1);
                }
                Err(DelegateL2ClientError::BlockNotFound(tag)) => {
                    warn!(
                        target: "derivation",
                        block_number = self.next_fetch_number,
                        tag,
                        "Source block not found during prefetch"
                    );
                    self.sleep_or_cancel(self.config.retry_backoff).await;
                    self.refresh_remote_head().await?;
                }
                Err(err) => {
                    warn!(
                        target: "derivation",
                        block_number = self.next_fetch_number,
                        error = %err,
                        "Source block prefetch failed"
                    );
                    self.sleep_or_cancel(self.config.retry_backoff).await;
                }
            }
        }
    }

    async fn refresh_remote_head(&mut self) -> Result<(), DerivationError> {
        loop {
            if self.cancellation_token.is_cancelled() {
                return Ok(());
            }

            match self.l2_source.get_block_number(BlockNumberOrTag::Latest).await {
                Ok(remote_head) => {
                    self.remote_head = remote_head;
                    return Ok(());
                }
                Err(err) => {
                    warn!(
                        target: "derivation",
                        error = %err,
                        "Failed to refresh source latest head for prefetch"
                    );
                    self.sleep_or_cancel(self.config.retry_backoff).await;
                }
            }
        }
    }

    async fn sleep_or_cancel(&self, duration: Duration) {
        tokio::select! {
            _ = self.cancellation_token.cancelled() => {}
            _ = time::sleep(duration) => {}
        }
    }
}
