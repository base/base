//! Pending trie caching for efficient bundle metering.
//!
//! When metering bundles on top of pending flashblocks, we cache the pre-computed
//! trie input so that subsequent bundle simulations can reuse it rather than
//! recomputing the pending state root each time.

use std::sync::Arc;

use alloy_rpc_types_engine::PayloadId;
use arc_swap::ArcSwap;
use eyre::Result as EyreResult;
use reth_provider::StateProvider;

use crate::{PendingState, PendingTrieInput, meter::compute_pending_trie_input, metrics::Metrics};

/// Internal cache entry for a single flashblock's pending trie input.
#[derive(Debug, Clone)]
struct CachedEntry {
    payload_id: PayloadId,
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
    /// If the cache already contains an entry for the provided `payload_id` and
    /// `flashblock_index`, the cached data is returned immediately. Otherwise the trie
    /// input is computed, cached (replacing any previous entry), and returned.
    pub fn ensure_cached(
        &self,
        payload_id: PayloadId,
        flashblock_index: u64,
        pending_state: &PendingState,
        canonical_state_provider: &dyn StateProvider,
    ) -> EyreResult<PendingTrieInput> {
        let cached_entry = self.cache.load();
        if let Some(cached) = cached_entry.as_ref()
            && cached.payload_id == payload_id
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
            payload_id,
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

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use base_client_node::test_utils::{Account, TestHarness};
    use reth_provider::StateProviderFactory;
    use reth_revm::{bytecode::Bytecode, primitives::KECCAK_EMPTY, state::AccountInfo};
    use revm_database::states::BundleState;

    use super::*;

    fn bundle_with_nonce(who: Address, from_nonce: u64, to_nonce: u64) -> BundleState {
        BundleState::new(
            [(
                who,
                Some(AccountInfo {
                    balance: U256::from(1_000_000_000u64),
                    nonce: from_nonce,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                }),
                Some(AccountInfo {
                    balance: U256::from(1_000_000_000u64),
                    nonce: to_nonce,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                }),
                Default::default(),
            )],
            Vec::<Vec<(Address, Option<Option<AccountInfo>>, Vec<(U256, U256)>)>>::new(),
            Vec::<(B256, Bytecode)>::new(),
        )
    }

    #[tokio::test]
    async fn ensure_cached_misses_on_different_payload_id() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let state_provider =
            harness.blockchain_provider().state_by_block_hash(latest.hash())?;

        let alice = Account::Alice.address();
        let flashblock_index = 0;

        let payload_a = PayloadId::new([1; 8]);
        let payload_b = PayloadId::new([2; 8]);

        let state_a =
            PendingState { bundle_state: bundle_with_nonce(alice, 0, 1), trie_input: None };
        let state_b =
            PendingState { bundle_state: bundle_with_nonce(alice, 0, 2), trie_input: None };

        let expected = crate::meter::compute_pending_trie_input(
            &*state_provider,
            &state_b.bundle_state,
            &Metrics::default(),
        )?;

        let cache = PendingTrieCache::new();
        cache.ensure_cached(payload_a, flashblock_index, &state_a, &*state_provider)?;

        let result =
            cache.ensure_cached(payload_b, flashblock_index, &state_b, &*state_provider)?;

        assert_eq!(
            result.hashed_state, expected.hashed_state,
            "cache must return freshly computed trie input for the new build attempt, \
             not stale data from the previous one"
        );

        Ok(())
    }
}
