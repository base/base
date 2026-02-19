//! Pending trie caching for efficient bundle metering.
//!
//! When metering bundles on top of pending flashblocks, we cache the pre-computed
//! trie input so that subsequent bundle simulations can reuse it rather than
//! recomputing the pending state root each time.

use std::sync::Arc;

use arc_swap::ArcSwap;
use eyre::Result as EyreResult;
use reth_provider::StateProvider;

use crate::{PendingState, PendingTrieInput, meter::compute_pending_trie_input, metrics::Metrics};

/// Internal cache entry for a single flashblock's pending trie input.
#[derive(Debug, Clone)]
struct CachedEntry {
    block_number: u64,
    flashblock_index: u64,
    trie_input: PendingTrieInput,
}

/// Thread-safe single-entry cache for pending trie input.
///
/// This cache stores the pre-computed trie input (trie updates and hashed state)
/// from the latest pending flashblock. Subsequent bundle metering operations
/// on the same flashblock can reuse this cached input instead of recomputing it,
/// significantly improving performance.
///
/// **Important**: This cache holds only ONE entry at a time.
/// When a new flashblock is cached, it replaces any previously cached entry.
#[derive(Debug, Clone)]
pub struct PendingTrieCache {
    cache: Arc<ArcSwap<Option<CachedEntry>>>,
    metrics: Metrics,
}

impl PendingTrieCache {
    /// Creates a new empty pending trie cache.
    pub fn new() -> Self {
        Self { cache: Arc::new(ArcSwap::from_pointee(None)), metrics: Metrics::default() }
    }

    /// Ensures the trie input for the given flashblock is cached and returns it.
    ///
    /// If the cache already contains an entry for the provided `block_number` and
    /// `flashblock_index`, the cached data is returned immediately. Otherwise the trie
    /// input is computed, cached (replacing any previous entry), and returned.
    pub fn ensure_cached(
        &self,
        block_number: u64,
        flashblock_index: u64,
        pending_state: &PendingState,
        canonical_state_provider: &dyn StateProvider,
    ) -> EyreResult<PendingTrieInput> {
        let cached_entry = self.cache.load();
        if let Some(cached) = cached_entry.as_ref()
            && cached.block_number == block_number
            && cached.flashblock_index == flashblock_index
        {
            self.metrics.pending_trie_cache_hits.increment(1);
            return Ok(cached.trie_input.clone());
        }

        // Cache miss - compute the trie input with metrics
        let trie_input = compute_pending_trie_input(
            canonical_state_provider,
            &pending_state.bundle_state,
            &self.metrics,
        )?;

        // Store the new entry, replacing any previous cached entry
        self.cache.store(Arc::new(Some(CachedEntry {
            block_number,
            flashblock_index,
            trie_input: trie_input.clone(),
        })));

        Ok(trie_input)
    }
}

impl Default for PendingTrieCache {
    fn default() -> Self {
        Self::new()
    }
}
