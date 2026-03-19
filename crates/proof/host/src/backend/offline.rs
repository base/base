//! Contains the implementations of the [`HintRouter`] and [`PreimageFetcher`] traits.

use std::sync::Arc;

use async_trait::async_trait;
use base_proof_preimage::{
    HintRouter, PreimageFetcher, PreimageKey,
    errors::{PreimageOracleError, PreimageOracleResult},
};
use tokio::sync::RwLock;

use crate::kv::KeyValueStore;

/// A [`KeyValueStore`]-backed implementation of the [`PreimageFetcher`] trait.
#[derive(Debug)]
pub struct OfflineHostBackend<KV>
where
    KV: KeyValueStore + ?Sized,
{
    inner: Arc<RwLock<KV>>,
}

impl<KV> OfflineHostBackend<KV>
where
    KV: KeyValueStore + ?Sized,
{
    /// Create a new [`OfflineHostBackend`] from the given [`KeyValueStore`].
    pub const fn new(kv_store: Arc<RwLock<KV>>) -> Self {
        Self { inner: kv_store }
    }
}

#[async_trait]
impl<KV> PreimageFetcher for OfflineHostBackend<KV>
where
    KV: KeyValueStore + Send + Sync + ?Sized,
{
    async fn get_preimage(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        let kv_store = self.inner.read().await;
        let result = kv_store.get(key.into());
        if result.is_none() {
            base_macros::inc!(counter, crate::Metrics::OFFLINE_MISSES_TOTAL);
        }
        result.ok_or(PreimageOracleError::KeyNotFound)
    }
}

#[async_trait]
impl<KV> HintRouter for OfflineHostBackend<KV>
where
    KV: KeyValueStore + Send + Sync + ?Sized,
{
    async fn route_hint(&self, _hint: String) -> PreimageOracleResult<()> {
        Ok(())
    }
}
