//! Trusted L2 source for canonical blocks a restarted sequencer missed.

use std::{
    future,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::Block;
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::B256;
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_protocol::{BaseTimeUpdateTx, L2BlockInfo};
use futures::{FutureExt, StreamExt, TryStreamExt, stream};
use thiserror::Error;
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};

use crate::{RemoteClient, RemoteL2ClientError};

/// Result of one background gap fetch.
pub type CatchupFetchResult = Result<Vec<BaseExecutionPayloadEnvelope>, CatchupSourceError>;

/// Canonical blocks missing between the unsafe head and the oldest buffered gossip payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchupGap {
    /// First missing block number.
    pub first: u64,
    /// Last missing block number.
    pub last: u64,
    /// Parent hash of the oldest buffered gossip payload.
    pub tip_hash: B256,
}

impl CatchupGap {
    /// Number of missing blocks.
    pub const fn block_count(&self) -> u64 {
        self.last - self.first + 1
    }

    /// Checks that `payloads` are well-formed and chain from `anchor` into `tip_hash`.
    ///
    /// Block hashes are recomputed from contents and `tip_hash` comes from signed gossip, so a
    /// range that passes is canonical whoever served it.
    pub fn verify(
        &self,
        rollup: &RollupConfig,
        anchor: L2BlockInfo,
        payloads: &[BaseExecutionPayloadEnvelope],
    ) -> Result<(), CatchupSourceError> {
        let mut parent = anchor.block_info.hash;
        let mut number = self.first;
        for envelope in payloads {
            let payload = &envelope.execution_payload;
            if payload.block_number() != number || payload.parent_hash() != parent {
                return Err(CatchupSourceError::Unlinked { number });
            }
            Self::validate_block(rollup, envelope)
                .map_err(|reason| CatchupSourceError::InvalidBlock { number, reason })?;
            parent = payload.block_hash();
            number += 1;
        }
        if number != self.last + 1 || parent != self.tip_hash {
            return Err(CatchupSourceError::Unlinked { number: self.last });
        }
        Ok(())
    }
}

impl CatchupGap {
    /// Rejects what `InsertTask` would treat as a critical engine error.
    fn validate_block(
        rollup: &RollupConfig,
        envelope: &BaseExecutionPayloadEnvelope,
    ) -> Result<(), String> {
        let payload = &envelope.execution_payload;
        let mut block: Block<BaseTxEnvelope> =
            payload.clone().try_into_block().map_err(|error| error.to_string())?;
        block.header.parent_beacon_block_root = envelope.parent_beacon_block_root;
        if matches!(payload, BaseExecutionPayload::V4(_)) {
            block.header.requests_hash = Some(EMPTY_REQUESTS_HASH);
        }
        if block.header.hash_slow() != payload.block_hash() {
            return Err("block hash does not match contents".to_string());
        }
        BaseTimeUpdateTx::validate_block_timestamp(
            rollup,
            &block.body.transactions,
            block.header.number,
            block.header.timestamp,
        )
        .map_err(|error| error.to_string())?;
        L2BlockInfo::from_block_and_genesis(&block, &rollup.genesis)
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// Error filling a catch-up gap from the trusted source.
#[derive(Debug, Error)]
pub enum CatchupSourceError {
    /// The source failed to serve a block.
    #[error(transparent)]
    Fetch(#[from] RemoteL2ClientError),
    /// The source did not serve the range in time.
    #[error("catch-up source timed out")]
    Timeout,
    /// The fetch task panicked or was aborted.
    #[error("catch-up fetch task failed")]
    Task,
    /// A served block is malformed or its hash does not match its contents.
    #[error("catch-up source block {number} is invalid: {reason}")]
    InvalidBlock {
        /// Block number of the invalid block.
        number: u64,
        /// Why the block was rejected.
        reason: String,
    },
    /// The range does not chain from the unsafe head into the buffered gossip payload.
    #[error("catch-up source block {number} does not link to the canonical chain")]
    Unlinked {
        /// Block number where the range stopped linking.
        number: u64,
    },
}

/// Fetches missed canonical blocks in the background so the engine loop never waits on the
/// source.
#[derive(Debug)]
pub struct CanonicalCatchupSource {
    client: Arc<dyn RemoteClient>,
    rollup: Arc<RollupConfig>,
    in_flight: Option<(L2BlockInfo, AbortOnDropHandle<CatchupFetchResult>)>,
    completed: Option<(L2BlockInfo, CatchupFetchResult)>,
    retry_at: Option<Instant>,
}

impl CanonicalCatchupSource {
    /// Largest gap fetched; larger gaps close through derivation until they fit.
    pub const MAX_GAP_BLOCKS: u64 = 300;
    /// Minimum bound on one gap fetch.
    pub const MIN_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
    /// Fetch time allowed per block on top of [`Self::MIN_FETCH_TIMEOUT`].
    ///
    /// The whole range must arrive: a partial prefix does not reach the signed gossip tip.
    pub const FETCH_TIMEOUT_PER_BLOCK: Duration = Duration::from_millis(100);
    /// Delay before the next fetch after a failed fetch or insert.
    pub const RETRY_DELAY: Duration = Duration::from_secs(5);
    /// Concurrent block requests per fetch.
    pub const CONCURRENCY: usize = 16;

    /// Creates a source backed by `client`.
    pub fn new(client: Arc<dyn RemoteClient>, rollup: Arc<RollupConfig>) -> Self {
        Self { client, rollup, in_flight: None, completed: None, retry_at: None }
    }

    /// Whether a fetch is running.
    pub const fn is_fetching(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Starts fetching `gap` above `anchor` unless a fetch is pending or backing off.
    pub fn start(&mut self, anchor: L2BlockInfo, gap: CatchupGap) {
        let backing_off = self.retry_at.is_some_and(|retry_at| Instant::now() < retry_at);
        if self.in_flight.is_some() || self.completed.is_some() || backing_off {
            return;
        }
        let client = Arc::clone(&self.client);
        let rollup = Arc::clone(&self.rollup);
        let handle = tokio::spawn(Self::fetch(client, rollup, anchor, gap));
        self.in_flight = Some((anchor, AbortOnDropHandle::new(handle)));
    }

    /// Resolves once the running fetch finishes; never resolves when idle.
    pub async fn wait(&mut self) {
        let Some((anchor, handle)) = &mut self.in_flight else {
            return future::pending().await;
        };
        let result = handle.await.unwrap_or(Err(CatchupSourceError::Task));
        self.completed = Some((*anchor, result));
        self.in_flight = None;
    }

    /// Returns a finished fetch that still extends `anchor`.
    pub fn take(&mut self, anchor: L2BlockInfo) -> Option<Vec<BaseExecutionPayloadEnvelope>> {
        if self.in_flight.as_ref().is_some_and(|(_, handle)| handle.is_finished()) {
            self.wait().now_or_never();
        }
        let (fetched_anchor, result) = self.completed.take()?;
        match result {
            Ok(mut payloads) => {
                if fetched_anchor != anchor {
                    let applied = payloads.iter().position(|payload| {
                        payload.execution_payload.block_hash() == anchor.block_info.hash
                    })?;
                    payloads.drain(..=applied);
                }
                info!(
                    target: "engine",
                    first = anchor.block_info.number + 1,
                    count = payloads.len(),
                    "Fetched canonical catch-up gap from source"
                );
                Some(payloads)
            }
            Err(error) => {
                warn!(target: "engine", error = %error, "Canonical catch-up gap fetch failed");
                self.back_off();
                None
            }
        }
    }

    /// Drops the running fetch and any finished result, e.g. when the gossip they were validated
    /// against is cleared by a reset.
    pub fn discard(&mut self) {
        self.in_flight = None;
        self.completed = None;
    }

    /// Delays the next fetch by [`Self::RETRY_DELAY`].
    pub fn back_off(&mut self) {
        self.retry_at = Some(Instant::now() + Self::RETRY_DELAY);
    }

    async fn fetch(
        client: Arc<dyn RemoteClient>,
        rollup: Arc<RollupConfig>,
        anchor: L2BlockInfo,
        gap: CatchupGap,
    ) -> CatchupFetchResult {
        let fetch = stream::iter(gap.first..=gap.last)
            .map(|number| client.get_payload_by_number(number))
            .buffered(Self::CONCURRENCY)
            .try_collect::<Vec<_>>();
        let timeout = Self::MIN_FETCH_TIMEOUT
            + Self::FETCH_TIMEOUT_PER_BLOCK * u32::try_from(gap.block_count()).unwrap_or(u32::MAX);
        let payloads = tokio::time::timeout(timeout, fetch)
            .await
            .map_err(|_| CatchupSourceError::Timeout)??;
        gap.verify(&rollup, anchor, &payloads)?;
        Ok(payloads)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, future, sync::Arc, time::Duration};

    use alloy_consensus::{Block, BlockBody, Header};
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::B256;
    use async_trait::async_trait;
    use base_common_consensus::BaseTxEnvelope;
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::test_utils::l1_info_deposit_tx;
    use base_protocol::{BlockInfo, L1BlockInfoBedrock, L2BlockInfo};

    use super::{CanonicalCatchupSource, CatchupGap, CatchupSourceError};
    use crate::{MockRemoteClient, RemoteClient, RemoteL2ClientError};

    fn envelope(number: u64, parent_hash: B256) -> BaseExecutionPayloadEnvelope {
        let block: Block<BaseTxEnvelope> = Block {
            header: Header { number, parent_hash, timestamp: number, ..Default::default() },
            body: BlockBody {
                transactions: vec![l1_info_deposit_tx(L1BlockInfoBedrock::default())],
                ..Default::default()
            },
        };
        // Round-trip so derived header fields such as the transactions root are populated.
        let block: Block<BaseTxEnvelope> =
            BaseExecutionPayload::from_block_unchecked(B256::ZERO, &block)
                .0
                .try_into_block()
                .expect("valid block");
        let (execution_payload, _) =
            BaseExecutionPayload::from_block_unchecked(block.header.hash_slow(), &block);
        BaseExecutionPayloadEnvelope { parent_beacon_block_root: None, execution_payload }
    }

    /// Canonical blocks 10..=13; block 10 is the anchor.
    fn chain() -> BTreeMap<u64, BaseExecutionPayloadEnvelope> {
        chain_to(13)
    }

    fn chain_to(last: u64) -> BTreeMap<u64, BaseExecutionPayloadEnvelope> {
        let mut chain = BTreeMap::new();
        let mut parent = B256::ZERO;
        for number in 10..=last {
            let block = envelope(number, parent);
            parent = block.execution_payload.block_hash();
            chain.insert(number, block);
        }
        chain
    }

    fn info(block: &BaseExecutionPayloadEnvelope) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                number: block.execution_payload.block_number(),
                hash: block.execution_payload.block_hash(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn gap(chain: &BTreeMap<u64, BaseExecutionPayloadEnvelope>) -> CatchupGap {
        CatchupGap { first: 11, last: 13, tip_hash: chain[&13].execution_payload.block_hash() }
    }

    fn source(
        serve: impl Fn(u64) -> Result<BaseExecutionPayloadEnvelope, RemoteL2ClientError>
        + Send
        + Sync
        + 'static,
    ) -> CanonicalCatchupSource {
        let mut client = MockRemoteClient::new();
        client.expect_get_payload_by_number().returning(serve);
        CanonicalCatchupSource::new(Arc::new(client), Arc::new(RollupConfig::default()))
    }

    fn serving(chain: BTreeMap<u64, BaseExecutionPayloadEnvelope>) -> CanonicalCatchupSource {
        source(move |number| Ok(chain[&number].clone()))
    }

    async fn fetch(
        source: &mut CanonicalCatchupSource,
        anchor: L2BlockInfo,
        gap: CatchupGap,
    ) -> Option<Vec<BaseExecutionPayloadEnvelope>> {
        source.start(anchor, gap);
        source.wait().await;
        source.take(anchor)
    }

    #[tokio::test]
    async fn fetches_linked_gap() {
        let chain = chain();
        let (anchor, gap) = (info(&chain[&10]), gap(&chain));
        let mut source = serving(chain.clone());

        let payloads = fetch(&mut source, anchor, gap).await.expect("linked gap");

        assert_eq!(payloads, chain.range(11..).map(|(_, block)| block.clone()).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn keeps_suffix_when_anchor_advanced_within_fetch() {
        let chain = chain();
        let (anchor, gap) = (info(&chain[&10]), gap(&chain));
        let mut source = serving(chain.clone());
        source.start(anchor, gap);
        source.wait().await;

        let payloads = source.take(info(&chain[&11])).expect("suffix above new anchor");

        assert_eq!(payloads, chain.range(12..).map(|(_, block)| block.clone()).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn discards_fetch_when_anchor_left_fetched_chain() {
        let chain = chain();
        let (anchor, gap) = (info(&chain[&10]), gap(&chain));
        let mut source = serving(chain.clone());
        source.start(anchor, gap);
        source.wait().await;
        let reorged = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::with_last_byte(0xaa),
                ..info(&chain[&11]).block_info
            },
            ..anchor
        };

        assert!(source.take(reorged).is_none());
        source.start(reorged, CatchupGap { first: 12, ..gap });
        assert!(source.is_fetching(), "a stale result must not back off");
    }

    #[tokio::test]
    async fn rejects_block_whose_hash_does_not_match_contents() {
        let chain = chain();
        let (anchor, gap) = (info(&chain[&10]), gap(&chain));
        let mut forged = chain.clone();
        if let BaseExecutionPayload::V1(payload) =
            &mut forged.get_mut(&12).unwrap().execution_payload
        {
            payload.gas_used += 1;
        }
        let mut source = serving(forged);

        source.start(anchor, gap);
        source.wait().await;
        let (_, result) = source.completed.take().expect("finished");

        assert!(matches!(result, Err(CatchupSourceError::InvalidBlock { number: 12, .. })));
    }

    #[tokio::test]
    async fn rejects_unlinked_ranges_and_backs_off() {
        let chain = chain();
        let (anchor, gap) = (info(&chain[&10]), gap(&chain));
        let forked_anchor = L2BlockInfo {
            block_info: BlockInfo { hash: B256::with_last_byte(0xff), ..anchor.block_info },
            ..anchor
        };
        let missing = chain.clone();
        let cases = [
            (forked_anchor, gap, serving(chain.clone())),
            (
                anchor,
                CatchupGap { tip_hash: B256::with_last_byte(0xee), ..gap },
                serving(chain.clone()),
            ),
            (
                anchor,
                gap,
                source(move |number| {
                    (number != 12)
                        .then(|| missing[&number].clone())
                        .ok_or_else(|| RemoteL2ClientError::BlockNotFound(number.to_string()))
                }),
            ),
        ];
        for (anchor, gap, mut source) in cases {
            assert!(fetch(&mut source, anchor, gap).await.is_none());
            source.start(anchor, gap);
            assert!(!source.is_fetching(), "failed fetch must back off");
        }
    }

    /// Hand-rolled because a mockall expectation cannot return a future that never resolves.
    #[derive(Debug)]
    struct StalledClient;

    #[async_trait]
    impl RemoteClient for StalledClient {
        async fn get_block_number(&self, _: BlockNumberOrTag) -> Result<u64, RemoteL2ClientError> {
            future::pending().await
        }

        async fn get_block_info(
            &self,
            _: BlockNumberOrTag,
        ) -> Result<BlockInfo, RemoteL2ClientError> {
            future::pending().await
        }

        async fn get_payload_by_number(
            &self,
            _: u64,
        ) -> Result<BaseExecutionPayloadEnvelope, RemoteL2ClientError> {
            future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_source_never_blocks_caller_and_times_out() {
        let chain = chain();
        let (anchor, gap) = (info(&chain[&10]), gap(&chain));
        let mut source =
            CanonicalCatchupSource::new(Arc::new(StalledClient), Arc::new(RollupConfig::default()));
        source.start(anchor, gap);

        assert!(source.take(anchor).is_none());
        assert!(source.is_fetching());

        source.wait().await;
        assert!(source.take(anchor).is_none());
        source.start(anchor, gap);
        assert!(!source.is_fetching(), "timed-out fetch must back off");
    }

    /// Hand-rolled because mockall returns values synchronously and cannot delay a response.
    #[derive(Debug)]
    struct SlowClient(BTreeMap<u64, BaseExecutionPayloadEnvelope>);

    #[async_trait]
    impl RemoteClient for SlowClient {
        async fn get_block_number(&self, _: BlockNumberOrTag) -> Result<u64, RemoteL2ClientError> {
            unimplemented!()
        }

        async fn get_block_info(
            &self,
            _: BlockNumberOrTag,
        ) -> Result<BlockInfo, RemoteL2ClientError> {
            unimplemented!()
        }

        async fn get_payload_by_number(
            &self,
            number: u64,
        ) -> Result<BaseExecutionPayloadEnvelope, RemoteL2ClientError> {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok(self.0[&number].clone())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn large_gap_gets_more_than_minimum_fetch_time() {
        let chain = chain_to(210);
        let anchor = info(&chain[&10]);
        let gap = CatchupGap {
            first: 11,
            last: 210,
            tip_hash: chain[&210].execution_payload.block_hash(),
        };
        let mut source = CanonicalCatchupSource::new(
            Arc::new(SlowClient(chain)),
            Arc::new(RollupConfig::default()),
        );

        let payloads = fetch(&mut source, anchor, gap).await.expect("slow but complete range");

        assert_eq!(payloads.len(), 200);
    }
}
