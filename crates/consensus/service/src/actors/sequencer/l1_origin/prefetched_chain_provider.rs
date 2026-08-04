//! The [`PrefetchedChainProvider`] adapter that serves the attributes builder from the selected
//! L1 origin published by the [`L1OriginSelector`](super::L1OriginSelector).

use std::{future::Future, time::Duration};

use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{ChainProvider, PipelineError, PipelineErrorKind};
use base_consensus_providers::{AlloyChainProvider, AlloyChainProviderError};
use base_protocol::BlockInfo;
use tokio::sync::watch;

use super::PreparedL1Origin;
use crate::Metrics;

/// A [`ChainProvider`] that serves the sequencer's attributes builder from the origin the
/// [`L1OriginSelector`](super::L1OriginSelector) most recently selected, falling back to a bounded
/// direct RPC on a miss.
///
/// The origin selector publishes the selected origin (header + receipts) on a one-slot [`watch`]
/// channel; the attributes builder only ever reads the origin it was just handed, so a single slot
/// suffices. In steady state `header_by_hash` and `receipts_by_hash` issue no inline L1 I/O. On a
/// miss (cold start, recovery, or a multi-epoch jump) the call is served by the
/// [`AlloyChainProvider`] fallback under a deadline anchored to the block time.
#[derive(Debug)]
pub struct PrefetchedChainProvider {
    /// The one-slot published-origin read handle.
    origin: watch::Receiver<Option<PreparedL1Origin>>,
    /// The direct-RPC fallback used on a miss.
    fallback: AlloyChainProvider,
    /// Per-request deadline applied to every fallback lookup.
    fallback_timeout: Duration,
}

impl PrefetchedChainProvider {
    /// Creates a new [`PrefetchedChainProvider`].
    pub const fn new(
        origin: watch::Receiver<Option<PreparedL1Origin>>,
        fallback: AlloyChainProvider,
        fallback_timeout: Duration,
    ) -> Self {
        Self { origin, fallback, fallback_timeout }
    }

    /// Returns the published origin if it matches `hash`, cloned so the [`watch`] borrow is not held
    /// across an `await`.
    fn prepared_if(&self, hash: B256) -> Option<PreparedL1Origin> {
        self.origin.borrow().as_ref().filter(|prepared| prepared.hash == hash).cloned()
    }

    /// Bounds a fallback lookup with `timeout_dur`, recording a timeout under `kind`.
    async fn bounded_fallback<T>(
        timeout_dur: Duration,
        kind: &'static str,
        fut: impl Future<Output = Result<T, AlloyChainProviderError>>,
    ) -> Result<T, PrefetchedChainProviderError> {
        tokio::time::timeout(timeout_dur, fut).await.map_or_else(
            |_| {
                Metrics::sequencer_l1_origin_fetch_timeouts_total(kind).increment(1);
                Err(PrefetchedChainProviderError::Timeout)
            },
            |result| result.map_err(PrefetchedChainProviderError::Fallback),
        )
    }
}

#[async_trait]
impl ChainProvider for PrefetchedChainProvider {
    type Error = PrefetchedChainProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        if let Some(prepared) = self.prepared_if(hash) {
            Metrics::sequencer_l1_origin_buffer_hits_total("header").increment(1);
            return Ok(prepared.header);
        }
        Metrics::sequencer_l1_origin_buffer_misses_total("header").increment(1);
        Self::bounded_fallback(self.fallback_timeout, "by_hash", self.fallback.header_by_hash(hash))
            .await
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        // Not used by the sequencer's attributes builder; delegate to the bounded fallback.
        Self::bounded_fallback(
            self.fallback_timeout,
            "by_number",
            self.fallback.block_info_by_number(number),
        )
        .await
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        if let Some(prepared) = self.prepared_if(hash) {
            Metrics::sequencer_l1_origin_buffer_hits_total("receipts").increment(1);
            return Ok((*prepared.receipts).clone());
        }
        Metrics::sequencer_l1_origin_buffer_misses_total("receipts").increment(1);
        Self::bounded_fallback(
            self.fallback_timeout,
            "receipts",
            self.fallback.receipts_by_hash(hash),
        )
        .await
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        // Not used by the sequencer's attributes builder; delegate to the bounded fallback.
        Self::bounded_fallback(
            self.fallback_timeout,
            "by_hash",
            self.fallback.block_info_and_transactions_by_hash(hash),
        )
        .await
    }
}

/// An error produced by the [`PrefetchedChainProvider`].
#[derive(Debug, thiserror::Error)]
pub enum PrefetchedChainProviderError {
    /// The fallback [`AlloyChainProvider`] returned an error.
    #[error(transparent)]
    Fallback(#[from] AlloyChainProviderError),
    /// A fallback lookup exceeded the block-time-anchored deadline. Treated as a temporary error so
    /// the build path retries on the next tick.
    #[error("timed out fetching L1 data from fallback provider")]
    Timeout,
}

impl From<PrefetchedChainProviderError> for PipelineErrorKind {
    fn from(err: PrefetchedChainProviderError) -> Self {
        match err {
            PrefetchedChainProviderError::Fallback(inner) => inner.into(),
            PrefetchedChainProviderError::Timeout => {
                Self::Temporary(PipelineError::Provider("L1 fallback lookup timed out".to_string()))
            }
        }
    }
}
