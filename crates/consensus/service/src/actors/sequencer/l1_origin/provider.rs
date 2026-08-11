//! L1 provider interfaces and implementations for origin selection.

use std::fmt::Debug;

use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_protocol::BlockInfo;
use tokio::sync::watch;

use super::L1OriginSelectorError;

/// L1 [`BlockInfo`] provider interface for the [`super::L1OriginSelector`].
#[async_trait]
pub trait L1OriginSelectorProvider: Debug + Sync {
    /// Returns the latest observed L1 head hash, used to identify the canonical chain view.
    fn chain_view(&self) -> Option<B256>;

    /// Returns a [`BlockInfo`] by its hash.
    async fn get_block_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError>;

    /// Returns a [`BlockInfo`] by its number.
    async fn get_block_by_number(
        &self,
        number: u64,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError>;
}

/// A wrapper around the [`RootProvider`] that delays the view of the L1 chain by a configurable
/// amount of blocks.
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
}

#[async_trait]
impl L1OriginSelectorProvider for DelayedL1OriginSelectorProvider {
    fn chain_view(&self) -> Option<B256> {
        self.l1_head.borrow().as_ref().map(|head| head.hash)
    }

    async fn get_block_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError> {
        // By-hash lookups are not delayed, as they're direct indexes.
        Ok(Provider::get_block_by_hash(&self.inner, hash).await?.map(Into::into))
    }

    async fn get_block_by_number(
        &self,
        number: u64,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError> {
        let Some(l1_head) = *self.l1_head.borrow() else {
            // Without an observed head, a by-number result cannot be tied to a canonical chain
            // view or checked against the confirmation delay.
            return Ok(None);
        };

        if number == 0
            || self.confirmation_depth == 0
            || number + self.confirmation_depth <= l1_head.number
        {
            Ok(Provider::get_block_by_number(&self.inner, number.into()).await?.map(Into::into))
        } else {
            Ok(None)
        }
    }
}
