//! Watch-backed chain provider for prefetched sequencer origins.

use std::sync::Arc;

use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{ChainProvider, PipelineError, PipelineErrorKind};
use base_consensus_providers::{AlloyChainProvider, AlloyChainProviderError};
use base_protocol::BlockInfo;
use tokio::sync::watch;

use super::PreparedL1Origin;
use crate::Metrics;

/// Serves matching header and receipt requests from the selected prepared origin.
#[derive(Debug)]
pub struct PrefetchedChainProvider {
    origin: watch::Receiver<Option<PreparedL1Origin>>,
    fallback: AlloyChainProvider,
}

impl PrefetchedChainProvider {
    /// Creates a provider with an RPC fallback.
    pub const fn new(
        origin: watch::Receiver<Option<PreparedL1Origin>>,
        fallback: AlloyChainProvider,
    ) -> Self {
        Self { origin, fallback }
    }
}

#[async_trait]
impl ChainProvider for PrefetchedChainProvider {
    type Error = PrefetchedChainProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        let header = self
            .origin
            .borrow()
            .as_ref()
            .filter(|origin| origin.hash == hash)
            .map(|origin| origin.header.clone());
        if let Some(header) = header {
            Metrics::sequencer_l1_origin_buffer_hits_total("header").increment(1);
            return Ok(header);
        }
        Metrics::sequencer_l1_origin_buffer_misses_total("header").increment(1);
        self.fallback.header_by_hash(hash).await.map_err(PrefetchedChainProviderError::Fallback)
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        self.fallback
            .block_info_by_number(number)
            .await
            .map_err(PrefetchedChainProviderError::Fallback)
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        let receipts = self
            .origin
            .borrow()
            .as_ref()
            .filter(|origin| origin.hash == hash)
            .map(|origin| Arc::clone(&origin.receipts));
        if let Some(receipts) = receipts {
            Metrics::sequencer_l1_origin_buffer_hits_total("receipts").increment(1);
            return Ok((*receipts).clone());
        }
        Metrics::sequencer_l1_origin_buffer_misses_total("receipts").increment(1);
        match self
            .fallback
            .receipts_by_hash(hash)
            .await
            .map_err(PrefetchedChainProviderError::Fallback)
        {
            Err(PrefetchedChainProviderError::Fallback(
                AlloyChainProviderError::BlockNotFound(_),
            )) => Err(PrefetchedChainProviderError::ReceiptsUnavailable(hash)),
            result => result,
        }
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        self.fallback
            .block_info_and_transactions_by_hash(hash)
            .await
            .map_err(PrefetchedChainProviderError::Fallback)
    }
}

/// Error returned by [`PrefetchedChainProvider`].
#[derive(Debug, thiserror::Error)]
pub enum PrefetchedChainProviderError {
    /// The RPC fallback failed.
    #[error(transparent)]
    Fallback(#[from] AlloyChainProviderError),
    /// The requested block exists but its receipts are not available yet.
    #[error("receipts unavailable for L1 origin: {0}")]
    ReceiptsUnavailable(B256),
}

impl From<PrefetchedChainProviderError> for PipelineErrorKind {
    fn from(error: PrefetchedChainProviderError) -> Self {
        match error {
            PrefetchedChainProviderError::Fallback(error) => error.into(),
            PrefetchedChainProviderError::ReceiptsUnavailable(hash) => Self::Temporary(
                PipelineError::Provider(format!("L1 origin receipts unavailable: {hash}")),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_provider::RootProvider;

    use super::*;

    fn fallback() -> AlloyChainProvider {
        AlloyChainProvider::new(
            RootProvider::new_http("http://localhost:1".parse().expect("valid URL")),
            1,
        )
    }

    #[tokio::test]
    async fn test_matching_origin_serves_header_and_receipts() {
        let header = Header { number: 7, timestamp: 84, ..Default::default() };
        let hash = header.hash_slow();
        let receipts = Arc::new(vec![Receipt::default()]);
        let (_tx, rx) = watch::channel(Some(PreparedL1Origin {
            hash,
            header: header.clone(),
            receipts: Arc::clone(&receipts),
        }));
        let mut provider = PrefetchedChainProvider::new(rx, fallback());

        assert_eq!(provider.header_by_hash(hash).await.unwrap(), header);
        assert_eq!(provider.receipts_by_hash(hash).await.unwrap(), *receipts);
    }

    #[test]
    fn test_missing_fallback_receipts_are_temporary() {
        let kind: PipelineErrorKind =
            PrefetchedChainProviderError::ReceiptsUnavailable(B256::ZERO).into();

        assert!(matches!(kind, PipelineErrorKind::Temporary(_)));
    }
}
