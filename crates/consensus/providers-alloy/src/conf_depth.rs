//! Confirmation-depth-filtered L1 chain provider.
//!
//! Wraps an [`AlloyChainProvider`] to enforce an L1 confirmation depth cutoff on
//! `block_info_by_number` lookups. This prevents the derivation pipeline from reading
//! L1 blocks that are too close to the chain tip, matching the behaviour of op-node's
//! `ConfDepth` wrapper.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::ChainProvider;
use base_protocol::BlockInfo;

use crate::{AlloyChainProvider, AlloyChainProviderError};

/// Shared atomic holding the current L1 head block number.
///
/// The L1 watcher updates this whenever a new L1 head is observed.
/// The [`ConfDepthProvider`] reads it to enforce the confirmation depth cutoff.
pub type L1HeadNumber = Arc<AtomicU64>;

/// A [`ChainProvider`] wrapper that enforces an L1 confirmation depth.
///
/// When `conf_depth > 0`, `block_info_by_number(n)` returns a temporary `BlockNotFound`
/// error for any block `n` where `n + conf_depth > l1_head`. This causes the derivation
/// pipeline to yield and retry later, matching the behavior of op-node's `ConfDepth` wrapper.
///
/// All other methods delegate to the inner [`AlloyChainProvider`] unchanged.
#[derive(Debug, Clone)]
pub struct ConfDepthProvider {
    /// The inner chain provider.
    inner: AlloyChainProvider,
    /// The current L1 head block number, updated atomically by the L1 watcher.
    l1_head_number: L1HeadNumber,
    /// The number of L1 blocks to keep distance from the head.
    conf_depth: u64,
}

impl ConfDepthProvider {
    /// Creates a new [`ConfDepthProvider`].
    pub const fn new(
        inner: AlloyChainProvider,
        l1_head_number: L1HeadNumber,
        conf_depth: u64,
    ) -> Self {
        Self { inner, l1_head_number, conf_depth }
    }
}

#[async_trait]
impl ChainProvider for ConfDepthProvider {
    type Error = AlloyChainProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        self.inner.header_by_hash(hash).await
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        if self.conf_depth > 0 {
            let l1_head = self.l1_head_number.load(Ordering::Relaxed);
            // Only filter when the L1 head is known (non-zero). A number-based
            // BlockNotFound maps to a Temporary pipeline error, causing the pipeline
            // to yield and retry once the chain advances.
            if l1_head > 0 && l1_head.saturating_sub(number) < self.conf_depth {
                return Err(AlloyChainProviderError::BlockNotFound(number.into()));
            }
        }
        self.inner.block_info_by_number(number).await
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        self.inner.receipts_by_hash(hash).await
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        self.inner.block_info_and_transactions_by_hash(hash).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::AlloyChainProviderError;

    /// Returns a provider whose L1 head is pinned at the given block number.
    fn provider_with_head(head: u64, conf_depth: u64) -> ConfDepthProvider {
        // We cannot issue real RPC calls in a unit test, so we only exercise the
        // conf-depth gating logic — not the inner provider's responses.
        let dummy_inner = AlloyChainProvider::new(
            alloy_provider::RootProvider::new_http("http://localhost:1".parse().unwrap()),
            1,
        );
        let l1_head = Arc::new(AtomicU64::new(head));
        ConfDepthProvider::new(dummy_inner, l1_head, conf_depth)
    }

    #[tokio::test]
    async fn zero_conf_depth_passes_through() {
        let mut p = provider_with_head(100, 0);
        // conf_depth == 0 ⇒ no gating; the call reaches the inner provider which will
        // fail because there is no real RPC, but it must NOT be a BlockNotFound error
        // from the conf-depth check.
        let err = p.block_info_by_number(200).await.unwrap_err();
        assert!(
            !matches!(err, AlloyChainProviderError::BlockNotFound(_)),
            "zero conf depth must not produce BlockNotFound"
        );
    }

    #[tokio::test]
    async fn block_beyond_conf_depth_is_rejected() {
        let mut p = provider_with_head(100, 4);
        // number (97) + conf_depth (4) = 101 > 100 → rejected
        let err = p.block_info_by_number(97).await.unwrap_err();
        assert!(matches!(err, AlloyChainProviderError::BlockNotFound(_)));
    }

    #[tokio::test]
    async fn block_within_conf_depth_passes_through() {
        let mut p = provider_with_head(100, 4);
        // number (96) + conf_depth (4) = 100 ≤ 100 → allowed (hits inner, fails because
        // no real RPC, but not a BlockNotFound).
        let err = p.block_info_by_number(96).await.unwrap_err();
        assert!(
            !matches!(err, AlloyChainProviderError::BlockNotFound(_)),
            "block within conf depth must not be gated"
        );
    }

    #[tokio::test]
    async fn unknown_l1_head_passes_through() {
        let mut p = provider_with_head(0, 4);
        // L1 head is 0 (not yet known) → no gating
        let err = p.block_info_by_number(200).await.unwrap_err();
        assert!(
            !matches!(err, AlloyChainProviderError::BlockNotFound(_)),
            "unknown L1 head must not gate"
        );
    }
}
