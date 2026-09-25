//! Follows the L2 tip and fills the witness cache.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::debug::ExecutionWitness;
use alloy_transport::TransportError;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types_engine::BasePayloadAttributes;
use tracing::{Instrument, error, info, info_span, warn};

use crate::{Metrics, PayloadAttributes, WitnessCache, WitnessKey};

/// Same bound the proof host uses for `debug_executePayload`.
const EXECUTE_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Failures of one block before the follower moves on.
///
/// A transient error is still retried. After this many failures the block is left for the proof
/// node and later blocks are cached.
const MAX_INGEST_ATTEMPTS: u32 = 3;

/// Calls `debug_executePayload` for each new L2 block and stores the response.
///
/// Starts at the head observed on startup. Older blocks are left for the prover to fetch from the
/// proof node. One block is requested at a time so the follower does not fill the proof node's
/// execute-payload semaphore.
#[derive(Debug)]
pub struct WitnessFollower {
    provider: RootProvider<Base>,
    rollup_config: RollupConfig,
    cache: Arc<WitnessCache>,
}

impl WitnessFollower {
    /// Creates a follower that writes into `cache`.
    pub const fn new(
        provider: RootProvider<Base>,
        rollup_config: RollupConfig,
        cache: Arc<WitnessCache>,
    ) -> Self {
        Self { provider, rollup_config, cache }
    }

    /// Follows the canonical head until the future is dropped.
    ///
    /// A failed head read, including the first one, is retried. The process keeps running so a
    /// proof node that is briefly unreachable does not look like a clean shutdown. One block that
    /// keeps failing `debug_executePayload` is skipped after [`MAX_INGEST_ATTEMPTS`] so later
    /// blocks are still cached. `tip_lag_blocks` stays elevated while a block is being retried.
    pub async fn run(self) {
        let mut next = None;
        let mut attempts = IngestAttempts::default();
        // A down proof node would otherwise log once per poll.
        let mut head_down = false;
        loop {
            match self.provider.get_block_number().await {
                Ok(head) => {
                    if head_down {
                        info!(block_number = head, "read L2 head again");
                        head_down = false;
                    }
                    let mut block_number = next.unwrap_or(head);
                    if next.is_none() {
                        info!(block_number, "witness follower started");
                    }
                    while block_number <= head {
                        let lag_blocks = head.saturating_sub(block_number);
                        Metrics::tip_lag_blocks().set(lag_blocks as f64);
                        let ingested = self
                            .ingest(block_number, lag_blocks)
                            .instrument(info_span!("payload_witness", block_number, lag_blocks))
                            .await;
                        if !attempts.advance(block_number, ingested) {
                            break;
                        }
                        if !ingested {
                            Metrics::ingest_attempts_total(Metrics::INGEST_SKIPPED).increment(1);
                            error!(
                                block_number,
                                attempts = MAX_INGEST_ATTEMPTS,
                                "skipping block after repeated payload witness failures"
                            );
                        }
                        block_number += 1;
                    }
                    Metrics::tip_lag_blocks().set(head.saturating_sub(block_number) as f64);
                    next = Some(block_number);
                }
                Err(error) => {
                    if !head_down {
                        warn!(error = %error, "failed to read L2 head");
                        head_down = true;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    /// Returns true when `block_number` is done (cached or permanently skipped).
    async fn ingest(&self, block_number: u64, lag_blocks: u64) -> bool {
        info!(block_number, lag_blocks, "requesting payload witness");

        let block = match self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .full()
            .await
        {
            Ok(Some(block)) => block,
            Ok(None) => {
                Metrics::ingest_attempts_total(Metrics::INGEST_RETRY).increment(1);
                warn!(block_number, "payload witness retry: block not found");
                return false;
            }
            Err(error) => {
                Metrics::ingest_attempts_total(Metrics::INGEST_RETRY).increment(1);
                error!(block_number, error = %error, "failed to fetch L2 block");
                return false;
            }
        };
        let parent_hash = block.header.inner.parent_hash;
        let payload_attributes = match PayloadAttributes::from_l2_block(&self.rollup_config, block)
        {
            Ok(payload_attributes) => payload_attributes,
            Err(error) => {
                Metrics::ingest_attempts_total(Metrics::INGEST_SKIPPED).increment(1);
                error!(
                    block_number,
                    error = %error,
                    "skipping block: failed to reconstruct payload attributes"
                );
                return true;
            }
        };
        let attributes_digest = match PayloadAttributes::digest(&payload_attributes) {
            Ok(digest) => digest,
            Err(error) => {
                Metrics::ingest_attempts_total(Metrics::INGEST_SKIPPED).increment(1);
                error!(
                    block_number,
                    error = %error,
                    "skipping block: failed to digest payload attributes"
                );
                return true;
            }
        };

        let started = Instant::now();
        let result = base_metrics::time!(Metrics::execute_payload_duration_seconds(), {
            tokio::time::timeout(
                EXECUTE_PAYLOAD_TIMEOUT,
                execute_payload(&self.provider, parent_hash, payload_attributes).instrument(
                    info_span!("debug_executePayload", block_number, parent_hash = %parent_hash,),
                ),
            )
            .await
        });
        let elapsed_ms = started.elapsed().as_millis();
        match result {
            Ok(Ok(witness)) => {
                self.cache.insert(WitnessKey { parent_hash, attributes_digest }, witness);
                Metrics::cached_blocks().set(self.cache.len() as f64);
                Metrics::ingest_attempts_total(Metrics::INGEST_CACHED).increment(1);
                info!(
                    block_number,
                    parent_hash = %parent_hash,
                    elapsed_ms,
                    lag_blocks,
                    "cached payload witness"
                );
                true
            }
            Ok(Err(error)) => {
                Metrics::ingest_attempts_total(Metrics::INGEST_RETRY).increment(1);
                error!(
                    block_number,
                    parent_hash = %parent_hash,
                    elapsed_ms,
                    lag_blocks,
                    error = %error,
                    "debug_executePayload failed"
                );
                false
            }
            Err(_) => {
                Metrics::ingest_attempts_total(Metrics::INGEST_RETRY).increment(1);
                error!(
                    block_number,
                    parent_hash = %parent_hash,
                    elapsed_ms,
                    lag_blocks,
                    timeout_secs = EXECUTE_PAYLOAD_TIMEOUT.as_secs(),
                    "debug_executePayload timed out"
                );
                false
            }
        }
    }
}

/// Consecutive ingest failures for one L2 block.
#[derive(Debug, Default)]
struct IngestAttempts {
    block_number: Option<u64>,
    attempts: u32,
}

impl IngestAttempts {
    /// Returns true when the follower should move to the next block.
    ///
    /// A successful ingest advances immediately. A failing block advances only after
    /// [`MAX_INGEST_ATTEMPTS`]. Switching blocks resets the count.
    fn advance(&mut self, block_number: u64, ingested: bool) -> bool {
        if self.block_number != Some(block_number) {
            self.block_number = Some(block_number);
            self.attempts = 0;
        }
        self.attempts = self.attempts.saturating_add(1);
        ingested || self.attempts >= MAX_INGEST_ATTEMPTS
    }
}

async fn execute_payload(
    provider: &RootProvider<Base>,
    parent_hash: B256,
    payload_attributes: BasePayloadAttributes,
) -> Result<ExecutionWitness, TransportError> {
    provider
        .client()
        .request::<(B256, BasePayloadAttributes), ExecutionWitness>(
            "debug_executePayload",
            (parent_hash, payload_attributes),
        )
        .await
}
