//! Trusted L2 source for canonical blocks a restarted sequencer missed.

use std::{
    future,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_protocol::L2BlockInfo;
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

    /// Checks that `payloads` chain from `anchor` into `tip_hash`.
    ///
    /// `tip_hash` comes from signed gossip, so a linked range is canonical whoever served it.
    pub fn verify(
        &self,
        anchor: L2BlockInfo,
        payloads: &[BaseExecutionPayloadEnvelope],
    ) -> Result<(), CatchupSourceError> {
        let mut parent = anchor.block_info.hash;
        let mut number = self.first;
        for payload in payloads {
            let payload = &payload.execution_payload;
            if payload.block_number() != number || payload.parent_hash() != parent {
                return Err(CatchupSourceError::Unlinked { number });
            }
            parent = payload.block_hash();
            number += 1;
        }
        if number != self.last + 1 || parent != self.tip_hash {
            return Err(CatchupSourceError::Unlinked { number: self.last });
        }
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
    in_flight: Option<(L2BlockInfo, AbortOnDropHandle<CatchupFetchResult>)>,
    completed: Option<(L2BlockInfo, CatchupFetchResult)>,
    retry_at: Option<Instant>,
}

impl CanonicalCatchupSource {
    /// Largest gap fetched; larger gaps close through derivation until they fit.
    pub const MAX_GAP_BLOCKS: u64 = 300;
    /// Bound on one gap fetch.
    pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
    /// Delay before the next fetch after a failed fetch or insert.
    pub const RETRY_DELAY: Duration = Duration::from_secs(5);
    /// Concurrent block requests per fetch.
    pub const CONCURRENCY: usize = 16;

    /// Creates a source backed by `client`.
    pub fn new(client: Arc<dyn RemoteClient>) -> Self {
        Self { client, in_flight: None, completed: None, retry_at: None }
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
        let handle = tokio::spawn(Self::fetch(client, anchor, gap));
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
            Ok(payloads) if fetched_anchor == anchor => {
                info!(
                    target: "engine",
                    first = fetched_anchor.block_info.number + 1,
                    count = payloads.len(),
                    "Fetched canonical catch-up gap from source"
                );
                Some(payloads)
            }
            Ok(_) => None,
            Err(error) => {
                warn!(target: "engine", error = %error, "Canonical catch-up gap fetch failed");
                self.back_off();
                None
            }
        }
    }

    /// Delays the next fetch by [`Self::RETRY_DELAY`].
    pub fn back_off(&mut self) {
        self.retry_at = Some(Instant::now() + Self::RETRY_DELAY);
    }

    async fn fetch(
        client: Arc<dyn RemoteClient>,
        anchor: L2BlockInfo,
        gap: CatchupGap,
    ) -> CatchupFetchResult {
        let fetch = stream::iter(gap.first..=gap.last)
            .map(|number| client.get_payload_by_number(number))
            .buffered(Self::CONCURRENCY)
            .try_collect::<Vec<_>>();
        let payloads = tokio::time::timeout(Self::FETCH_TIMEOUT, fetch)
            .await
            .map_err(|_| CatchupSourceError::Timeout)??;
        gap.verify(anchor, &payloads)?;
        Ok(payloads)
    }
}

#[cfg(test)]
mod tests {
    use std::{future, sync::Arc};

    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use async_trait::async_trait;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::{CanonicalCatchupSource, CatchupGap};
    use crate::{MockRemoteClient, RemoteClient, RemoteL2ClientError};

    fn hash(number: u64) -> B256 {
        B256::from(U256::from(number))
    }

    fn head(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, hash: hash(number), ..Default::default() },
            ..Default::default()
        }
    }

    fn gap() -> CatchupGap {
        CatchupGap { first: 11, last: 13, tip_hash: hash(13) }
    }

    fn payload(number: u64, parent_hash: B256) -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: number,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash: hash(number),
                transactions: vec![],
            }),
        }
    }

    fn source(
        serve: impl Fn(u64) -> Option<B256> + Send + Sync + 'static,
    ) -> CanonicalCatchupSource {
        let mut client = MockRemoteClient::new();
        client.expect_get_payload_by_number().returning(move |number| {
            serve(number)
                .map(|parent| payload(number, parent))
                .ok_or_else(|| RemoteL2ClientError::BlockNotFound(number.to_string()))
        });
        CanonicalCatchupSource::new(Arc::new(client))
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

    fn numbers(payloads: &[BaseExecutionPayloadEnvelope]) -> Vec<u64> {
        payloads.iter().map(|payload| payload.execution_payload.block_number()).collect()
    }

    #[tokio::test]
    async fn fetches_linked_gap() {
        let mut source = source(|number| Some(hash(number - 1)));

        let payloads = fetch(&mut source, head(10), gap()).await.expect("linked gap");

        assert_eq!(numbers(&payloads), vec![11, 12, 13]);
    }

    #[tokio::test]
    async fn discards_fetch_when_anchor_moved() {
        let mut source = source(|number| Some(hash(number - 1)));
        source.start(head(10), gap());
        source.wait().await;

        assert!(source.take(head(11)).is_none());
        source.start(head(11), CatchupGap { first: 12, ..gap() });
        assert!(source.is_fetching(), "a stale result must not back off");
    }

    #[tokio::test]
    async fn rejects_unlinked_ranges_and_backs_off() {
        let forked_anchor = L2BlockInfo {
            block_info: BlockInfo {
                number: 10,
                hash: B256::with_last_byte(0xff),
                ..Default::default()
            },
            ..Default::default()
        };
        let cases = [
            (forked_anchor, gap(), source(|number| Some(hash(number - 1)))),
            (
                head(10),
                CatchupGap { tip_hash: B256::with_last_byte(0xee), ..gap() },
                source(|number| Some(hash(number - 1))),
            ),
            (
                head(10),
                gap(),
                source(|number| Some(if number == 12 { B256::ZERO } else { hash(number - 1) })),
            ),
            (head(10), gap(), source(|number| (number != 12).then(|| hash(number - 1)))),
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
        let mut source = CanonicalCatchupSource::new(Arc::new(StalledClient));
        source.start(head(10), gap());

        assert!(source.take(head(10)).is_none());
        assert!(source.is_fetching());

        source.wait().await;
        assert!(source.take(head(10)).is_none());
        source.start(head(10), gap());
        assert!(!source.is_fetching(), "timed-out fetch must back off");
    }
}
