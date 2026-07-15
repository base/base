use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use tokio::{sync::mpsc, time};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::follow::{error::FollowError, recovery::ReplayBlock, source::RemoteClient};

/// Number of source L2 payloads to keep prefetched ahead of the insert loop.
pub(super) const PREFETCH_WINDOW: usize = 50;
const SOURCE_HEAD_BACKOFF: Duration = Duration::from_secs(1);
const PREFETCH_FAILURE_WARN_INTERVAL: u64 = 5;

/// A fetched source payload.
pub(super) type PrefetchedPayload = BaseExecutionPayloadEnvelope;

/// Fetches source L2 payloads ahead of the insert loop.
#[derive(Debug)]
pub(super) struct PayloadPrefetcher<Remote> {
    source: Arc<Remote>,
    cancellation: CancellationToken,
    blocks_to_insert_tx: mpsc::Sender<PrefetchedPayload>,
}

impl<Remote> PayloadPrefetcher<Remote>
where
    Remote: RemoteClient + 'static,
{
    /// Creates a payload prefetcher.
    pub(super) const fn new(
        source: Arc<Remote>,
        cancellation: CancellationToken,
        blocks_to_insert_tx: mpsc::Sender<PrefetchedPayload>,
    ) -> Self {
        Self { source, cancellation, blocks_to_insert_tx }
    }

    /// Starts fetching from the local node head and pushes payloads through a bounded channel.
    ///
    /// Recovery payloads are fetched by hash so every replay request stays on the source branch
    /// captured during ancestor discovery. Once the captured path is exhausted, live follow mode
    /// resumes with the normal latest/by-number polling.
    pub(super) async fn run(
        self,
        start_from_local_head: u64,
        replay: Vec<ReplayBlock>,
    ) -> Result<(), FollowError> {
        let mut next_fetch = start_from_local_head.saturating_add(1);
        let mut source_latest = start_from_local_head;
        let mut consecutive_payload_failures = 0;

        for replay_block in replay {
            if replay_block.number != next_fetch {
                return Err(FollowError::OutOfOrderPayload {
                    actual: replay_block.number,
                    expected: next_fetch,
                });
            }

            loop {
                if self.cancellation.is_cancelled() {
                    return Ok(());
                }

                match self.source.get_payload_by_hash(replay_block.hash).await {
                    Ok(payload)
                        if payload.execution_payload.block_number() == replay_block.number
                            && payload.execution_payload.block_hash() == replay_block.hash
                            && payload.execution_payload.parent_hash()
                                == replay_block.parent_hash =>
                    {
                        if self.blocks_to_insert_tx.send(payload).await.is_err() {
                            return Ok(());
                        }
                        consecutive_payload_failures = 0;
                        next_fetch = next_fetch.saturating_add(1);
                        break;
                    }
                    Ok(payload) => {
                        consecutive_payload_failures += 1;
                        let actual_number = payload.execution_payload.block_number();
                        let actual_hash = payload.execution_payload.block_hash();
                        let actual_parent = payload.execution_payload.parent_hash();
                        if consecutive_payload_failures % PREFETCH_FAILURE_WARN_INTERVAL == 0 {
                            warn!(
                                target: "follow",
                                block = replay_block.number,
                                attempts = consecutive_payload_failures,
                                expected_hash = %replay_block.hash,
                                actual_number,
                                actual_hash = %actual_hash,
                                actual_parent = %actual_parent,
                                "Repeatedly received a source payload outside the captured replay branch"
                            );
                        } else {
                            debug!(
                                target: "follow",
                                block = replay_block.number,
                                attempts = consecutive_payload_failures,
                                expected_hash = %replay_block.hash,
                                actual_number,
                                actual_hash = %actual_hash,
                                actual_parent = %actual_parent,
                                "Received a source payload outside the captured replay branch"
                            );
                        }
                        self.backoff_at_source_head().await;
                    }
                    Err(e) => {
                        consecutive_payload_failures += 1;
                        if consecutive_payload_failures % PREFETCH_FAILURE_WARN_INTERVAL == 0 {
                            warn!(
                                target: "follow",
                                block = replay_block.number,
                                attempts = consecutive_payload_failures,
                                error = %e,
                                "Repeatedly failed to prefetch captured source payload"
                            );
                        } else {
                            debug!(
                                target: "follow",
                                block = replay_block.number,
                                attempts = consecutive_payload_failures,
                                error = %e,
                                "Failed to prefetch captured source payload"
                            );
                        }
                        self.backoff_at_source_head().await;
                    }
                }
            }
        }

        if next_fetch > start_from_local_head.saturating_add(1) {
            source_latest = next_fetch.saturating_sub(1);
        }

        loop {
            if self.cancellation.is_cancelled() {
                return Ok(());
            }

            if next_fetch > source_latest {
                source_latest = self.refresh_source_latest(source_latest).await;
                if next_fetch > source_latest {
                    self.backoff_at_source_head().await;
                    continue;
                }
            }

            let payload = self.source.get_payload_by_number(next_fetch).await;

            match payload {
                Ok(payload) => {
                    if self.blocks_to_insert_tx.send(payload).await.is_err() {
                        return Ok(());
                    }
                    consecutive_payload_failures = 0;
                    next_fetch = next_fetch.saturating_add(1);
                }
                Err(e) => {
                    consecutive_payload_failures += 1;
                    if consecutive_payload_failures % PREFETCH_FAILURE_WARN_INTERVAL == 0 {
                        warn!(
                            target: "follow",
                            block = next_fetch,
                            attempts = consecutive_payload_failures,
                            error = %e,
                            "Repeatedly failed to prefetch source payload"
                        );
                    } else {
                        debug!(
                            target: "follow",
                            block = next_fetch,
                            attempts = consecutive_payload_failures,
                            error = %e,
                            "Failed to prefetch source payload"
                        );
                    }
                    self.backoff_at_source_head().await;
                }
            }
        }
    }

    async fn refresh_source_latest(&self, current: u64) -> u64 {
        match self.source.get_block_number(BlockNumberOrTag::Latest).await {
            Ok(latest) => latest,
            Err(e) => {
                debug!(target: "follow", error = %e, "Failed to fetch source latest head");
                current
            }
        }
    }

    async fn backoff_at_source_head(&self) {
        time::sleep(SOURCE_HEAD_BACKOFF).await;
    }
}
