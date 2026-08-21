//! Watch-backed chain provider for prefetched sequencer origins.

use std::sync::Arc;

use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{ChainProvider, PipelineErrorKind};
use base_consensus_providers::{AlloyChainProvider, AlloyChainProviderError};
use base_protocol::BlockInfo;
use tokio::sync::watch;

use super::PreparedL1Origin;
use crate::Metrics;

/// Serves matching header and receipt requests from the selected prepared origin.
///
/// Requests that do not match the buffered origin fall back to L1 RPC. This preserves cold-start,
/// stale-buffer, and missing-receipts behavior without making RPC the normal block-building path.
#[derive(Debug)]
pub struct PrefetchedChainProvider {
    origin: watch::Receiver<Option<PreparedL1Origin>>,
    fallback: AlloyChainProvider,
}

impl PrefetchedChainProvider {
    /// Creates a provider with an RPC fallback for requests the buffered origin cannot serve.
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
            .and_then(|origin| origin.receipts.as_ref().map(Arc::clone));
        if let Some(receipts) = receipts {
            Metrics::sequencer_l1_origin_buffer_hits_total("receipts").increment(1);
            return Ok((*receipts).clone());
        }
        Metrics::sequencer_l1_origin_buffer_misses_total("receipts").increment(1);
        self.fallback.receipts_by_hash(hash).await.map_err(PrefetchedChainProviderError::Fallback)
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
}

impl From<PrefetchedChainProviderError> for PipelineErrorKind {
    fn from(error: PrefetchedChainProviderError) -> Self {
        match error {
            PrefetchedChainProviderError::Fallback(error) => error.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_provider::RootProvider;
    use httpmock::prelude::*;

    use super::*;

    fn fallback(url: &str) -> AlloyChainProvider {
        AlloyChainProvider::new(RootProvider::new_http(url.parse().expect("valid URL")), 1)
    }

    #[tokio::test]
    async fn test_matching_origin_serves_header_and_receipts() {
        let header = Header { number: 7, timestamp: 84, ..Default::default() };
        let hash = header.hash_slow();
        let receipts = Arc::new(vec![Receipt::default()]);
        let (_tx, rx) = watch::channel(Some(PreparedL1Origin {
            hash,
            header: header.clone(),
            receipts: Some(Arc::clone(&receipts)),
        }));
        let mut provider = PrefetchedChainProvider::new(rx, fallback("http://localhost:1"));

        assert_eq!(provider.header_by_hash(hash).await.unwrap(), header);
        assert_eq!(provider.receipts_by_hash(hash).await.unwrap(), *receipts);
    }

    #[tokio::test]
    async fn test_origin_without_receipts_uses_fallback() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":0,"result":[]}"#);
            })
            .await;
        let header = Header::default();
        let hash = header.hash_slow();
        let (_tx, rx) = watch::channel(Some(PreparedL1Origin { hash, header, receipts: None }));
        let mut provider = PrefetchedChainProvider::new(rx, fallback(&server.url("/")));

        assert!(provider.receipts_by_hash(hash).await.unwrap().is_empty());
        mock.assert_calls_async(1).await;
    }

    #[tokio::test]
    async fn test_missing_fallback_receipts_remain_block_not_found() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":0,"result":null}"#);
            })
            .await;
        let (_tx, rx) = watch::channel(None);
        let mut provider = PrefetchedChainProvider::new(rx, fallback(&server.url("/")));

        let error = provider.receipts_by_hash(B256::ZERO).await.unwrap_err();
        assert!(matches!(
            &error,
            PrefetchedChainProviderError::Fallback(AlloyChainProviderError::BlockNotFound(_))
        ));
        let kind: PipelineErrorKind = error.into();

        assert!(matches!(kind, PipelineErrorKind::Reset(_)));
        mock.assert_calls_async(1).await;
    }
}
