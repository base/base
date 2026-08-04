//! The prepared-origin provider interface and its delayed, RPC-backed implementation.

use std::fmt::Debug;

use alloy_consensus::{Header, Receipt};
use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_transport::TransportErrorKind;
use async_trait::async_trait;
use base_protocol::BlockInfo;
use tokio::sync::watch;

use super::{L1OriginSelectorError, PreparedL1Origin};

/// Prepared-L1-origin provider interface for the [`L1OriginSelector`](super::L1OriginSelector).
///
/// Returns the full origin state (header + receipts) so the selector can publish it to the
/// attributes builder. Implementations apply the confirmation delay to by-number lookups and verify
/// the returned header hash for by-hash lookups.
///
/// Tests hand-roll a fake rather than use `automock`: the double must apply a per-call delay (to
/// exercise the fetch timeout) and return scripted per-number failures, neither of which
/// `automock`'s synchronous `returning` closure expresses cleanly.
#[async_trait]
pub trait L1OriginSelectorProvider: Debug + Send + Sync + 'static {
    /// Returns the prepared origin with the given hash, or `None` if it is unavailable.
    async fn prepared_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError>;

    /// Returns the prepared origin at the given number, already gated by the confirmation delay
    /// (`None` when the block has not cleared the delay or does not yet exist).
    async fn prepared_by_number(
        &self,
        number: u64,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError>;
}

/// A wrapper around the [`RootProvider`] that delays the view of the L1 chain by a configurable
/// amount of blocks and prepares the full origin state (header + receipts) each lookup.
#[derive(Debug)]
pub struct DelayedL1OriginSelectorProvider {
    /// The inner [`RootProvider`].
    inner: RootProvider,
    /// The L1 head watch channel.
    l1_head: watch::Receiver<Option<BlockInfo>>,
    /// The confirmation depth to delay the view of the L1 chain.
    confirmation_depth: u64,
}

impl DelayedL1OriginSelectorProvider {
    /// Creates a new [`DelayedL1OriginSelectorProvider`].
    pub const fn new(
        inner: RootProvider,
        l1_head: watch::Receiver<Option<BlockInfo>>,
        confirmation_depth: u64,
    ) -> Self {
        Self { inner, l1_head, confirmation_depth }
    }

    /// Returns whether the block at `number` has cleared the confirmation delay relative to the L1
    /// head. With no L1 head yet, the delay is not enforced.
    fn next_eligible(&self, number: u64) -> bool {
        let Some(head) = *self.l1_head.borrow() else {
            return true;
        };
        number == 0
            || self.confirmation_depth == 0
            || number.saturating_add(self.confirmation_depth) <= head.number
    }

    /// Fetches the L1 header with the given hash.
    async fn header_by_hash(&self, hash: B256) -> Result<Option<Header>, L1OriginSelectorError> {
        Ok(Provider::get_block_by_hash(&self.inner, hash)
            .await?
            .map(|block| block.header.into_consensus()))
    }

    /// Fetches the L1 header at the given number.
    async fn header_by_number(&self, number: u64) -> Result<Option<Header>, L1OriginSelectorError> {
        Ok(Provider::get_block_by_number(&self.inner, number.into())
            .await?
            .map(|block| block.header.into_consensus()))
    }

    /// Fetches all receipts in the block with the given hash.
    async fn receipts_by_hash(&self, hash: B256) -> Result<Vec<Receipt>, L1OriginSelectorError> {
        let receipts = Provider::get_block_receipts(&self.inner, hash.into())
            .await?
            .ok_or(L1OriginSelectorError::OriginNotFound(hash))?;
        receipts
            .into_iter()
            .map(|r| r.inner.into_primitives_receipt().as_receipt().cloned())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                L1OriginSelectorError::Provider(TransportErrorKind::custom_str(
                    "failed to convert RPC receipts",
                ))
            })
    }
}

#[async_trait]
impl L1OriginSelectorProvider for DelayedL1OriginSelectorProvider {
    async fn prepared_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        // By-hash lookups are not delayed, as they're direct indexes.
        let Some(header) = self.header_by_hash(hash).await? else {
            return Ok(None);
        };
        // The `RootProvider` does not verify the returned header hashes to the requested hash
        // (unlike `AlloyChainProvider`'s `trust_rpc` path), so a misbehaving L1 RPC could otherwise
        // return a mismatched hash->header pair. Treat a mismatch as unavailable.
        let returned_hash = header.hash_slow();
        if returned_hash != hash {
            warn!(
                target: "l1_origin_selector",
                requested = %hash,
                returned = %returned_hash,
                "L1 RPC returned header with mismatched hash; treating origin as unavailable"
            );
            return Ok(None);
        }
        let receipts = self.receipts_by_hash(hash).await?;
        Ok(Some(PreparedL1Origin { hash, header, receipts: receipts.into() }))
    }

    async fn prepared_by_number(
        &self,
        number: u64,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        if !self.next_eligible(number) {
            return Ok(None);
        }
        let Some(header) = self.header_by_number(number).await? else {
            return Ok(None);
        };
        let hash = header.hash_slow();
        let receipts = self.receipts_by_hash(hash).await?;
        Ok(Some(PreparedL1Origin { hash, header, receipts: receipts.into() }))
    }
}
