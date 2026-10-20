//! Trusted L2 source that fills the startup catch-up gap left by blocks gossiped while the
//! sequencer was down.

use std::{
    fmt::Debug,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_protocol::L2BlockInfo;
use futures::{StreamExt, TryStreamExt, stream};
use thiserror::Error;

use crate::{RemoteClient, RemoteL2ClientError};

/// Canonical blocks missing between the engine's unsafe head and the oldest buffered gossip
/// payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchupGap {
    /// First missing block number, one above the unsafe head.
    pub first: u64,
    /// Last missing block number, one below the oldest buffered payload.
    pub last: u64,
    /// Parent hash of the oldest buffered payload, which the fetched range must end at.
    pub tip_hash: B256,
}

impl CatchupGap {
    /// Returns the number of missing blocks.
    pub const fn block_count(&self) -> u64 {
        self.last - self.first + 1
    }

    /// Checks that `payloads` form exactly this gap and chain from `anchor` into `tip_hash`.
    ///
    /// The tip hash comes from an authenticated gossip payload, and the execution engine
    /// recomputes every block hash on insert, so a linked range is the canonical one no matter
    /// which source served it.
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

/// Error produced while filling a catch-up gap from the trusted L2 source.
#[derive(Debug, Error)]
pub enum CatchupSourceError {
    /// The source failed to serve a block.
    #[error(transparent)]
    Fetch(#[from] RemoteL2ClientError),
    /// The source did not serve the range within [`CanonicalCatchupSource::FETCH_TIMEOUT`].
    #[error("catch-up source timed out")]
    Timeout,
    /// The served range does not chain from the unsafe head into the buffered gossip payload.
    #[error("catch-up source block {number} does not link to the canonical chain")]
    Unlinked {
        /// Block number at which the range stopped linking.
        number: u64,
    },
}

/// Fetches missing canonical blocks from a trusted L2 execution node during startup catch-up.
#[derive(Debug)]
pub struct CanonicalCatchupSource {
    client: Arc<dyn RemoteClient>,
    retry_at: Option<Instant>,
}

impl CanonicalCatchupSource {
    /// Largest gap filled from the source. Larger gaps close through derivation until they fit.
    pub const MAX_GAP_BLOCKS: u64 = 300;
    /// Bound on one gap fetch so a stalled source cannot hold the engine loop.
    pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
    /// Delay before retrying after a failed fetch.
    pub const RETRY_DELAY: Duration = Duration::from_secs(1);
    /// Concurrent block requests per fetch.
    pub const CONCURRENCY: usize = 16;

    /// Creates a source backed by `client`.
    pub fn new(client: Arc<dyn RemoteClient>) -> Self {
        Self { client, retry_at: None }
    }

    /// Returns whether a fetch may be attempted now.
    pub fn is_ready(&self) -> bool {
        self.retry_at.is_none_or(|retry_at| Instant::now() >= retry_at)
    }

    /// Fetches and verifies `gap` above `anchor`, delaying the next attempt on failure.
    pub async fn fetch(
        &mut self,
        anchor: L2BlockInfo,
        gap: CatchupGap,
    ) -> Result<Vec<BaseExecutionPayloadEnvelope>, CatchupSourceError> {
        let result = self.fetch_linked(anchor, gap).await;
        self.retry_at = result.is_err().then(|| Instant::now() + Self::RETRY_DELAY);
        result
    }

    async fn fetch_linked(
        &self,
        anchor: L2BlockInfo,
        gap: CatchupGap,
    ) -> Result<Vec<BaseExecutionPayloadEnvelope>, CatchupSourceError> {
        let fetch = stream::iter(gap.first..=gap.last)
            .map(|number| self.client.get_payload_by_number(number))
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
    use std::sync::Arc;

    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::{CanonicalCatchupSource, CatchupGap, CatchupSourceError};
    use crate::{MockRemoteClient, RemoteL2ClientError};

    fn head(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, hash: hash(number), ..Default::default() },
            ..Default::default()
        }
    }

    fn hash(number: u64) -> B256 {
        B256::from(U256::from(number))
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

    fn gap() -> CatchupGap {
        CatchupGap { first: 11, last: 13, tip_hash: hash(13) }
    }

    #[tokio::test]
    async fn fetches_linked_gap() {
        let mut source = source(|number| Some(hash(number - 1)));

        let payloads = source.fetch(head(10), gap()).await.expect("linked gap");

        let numbers: Vec<_> =
            payloads.iter().map(|payload| payload.execution_payload.block_number()).collect();
        assert_eq!(numbers, vec![11, 12, 13]);
        assert!(source.is_ready());
    }

    #[tokio::test]
    async fn rejects_range_that_forks_from_anchor() {
        let mut source = source(|number| Some(hash(number - 1)));
        let forked_anchor = L2BlockInfo {
            block_info: BlockInfo {
                number: 10,
                hash: B256::with_last_byte(0xff),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = source.fetch(forked_anchor, gap()).await.expect_err("forked anchor");

        assert!(matches!(error, CatchupSourceError::Unlinked { number: 11 }));
        assert!(!source.is_ready());
    }

    #[tokio::test]
    async fn rejects_range_that_misses_gossip_parent() {
        let mut source = source(|number| Some(hash(number - 1)));
        let gap = CatchupGap { tip_hash: B256::with_last_byte(0xee), ..gap() };

        let error = source.fetch(head(10), gap).await.expect_err("wrong tip");

        assert!(matches!(error, CatchupSourceError::Unlinked { number: 13 }));
    }

    #[tokio::test]
    async fn rejects_broken_internal_link() {
        let mut source = source(|number| {
            Some(if number == 12 { B256::with_last_byte(0xdd) } else { hash(number - 1) })
        });

        let error = source.fetch(head(10), gap()).await.expect_err("broken link");

        assert!(matches!(error, CatchupSourceError::Unlinked { number: 12 }));
    }

    #[tokio::test]
    async fn source_error_delays_retry() {
        let mut source = source(|number| (number != 12).then(|| hash(number - 1)));

        let error = source.fetch(head(10), gap()).await.expect_err("missing block");

        assert!(matches!(error, CatchupSourceError::Fetch(RemoteL2ClientError::BlockNotFound(_))));
        assert!(!source.is_ready());
    }
}
