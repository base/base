use std::sync::Arc;

use alloy_primitives::B256;
use arc_swap::ArcSwap;
use eyre::Result as EyreResult;
use reth_provider::StateProvider;
use reth_trie_common::{HashedPostState, updates::TrieUpdates};

use crate::FlashblocksState;

/// Trie nodes and hashed state from computing a flashblock state root.
///
/// When metering bundles, we want each state root calculation to measure only
/// the bundle's incremental I/O, not I/O from previous flashblocks. By caching
/// the flashblock trie once and reusing it for all bundle simulations, we ensure
/// each bundle's state root time reflects only its own I/O cost.
#[derive(Debug, Clone)]
pub struct FlashblockTrieData {
    /// The trie updates from computing the state root.
    pub trie_updates: TrieUpdates,
    /// The hashed post state used for state root computation.
    pub hashed_state: HashedPostState,
}

/// Internal cache entry for a single flashblock.
#[derive(Debug, Clone)]
struct CachedFlashblockTrie {
    block_hash: B256,
    flashblock_index: u64,
    trie_data: FlashblockTrieData,
}

/// Thread-safe single-entry cache for the latest flashblock's trie nodes.
///
/// This cache stores the intermediate trie nodes computed when calculating
/// the latest flashblock's state root. Subsequent bundle metering operations
/// on the same flashblock can reuse these cached nodes instead of recalculating
/// them, significantly improving performance.
///
/// **Important**: This cache holds only ONE flashblock's trie at a time.
/// When a new flashblock is cached, it replaces any previously cached flashblock.
#[derive(Debug, Clone)]
pub struct FlashblockTrieCache {
    cache: Arc<ArcSwap<Option<CachedFlashblockTrie>>>,
}

impl FlashblockTrieCache {
    /// Creates a new empty flashblock trie cache.
    pub fn new() -> Self {
        Self { cache: Arc::new(ArcSwap::from_pointee(None)) }
    }

    /// Ensures the trie for the given flashblock is cached and returns it.
    ///
    /// If the cache already contains an entry for the provided `block_hash` and
    /// `flashblock_index`, the cached data is returned immediately. Otherwise the trie is
    /// recomputed, cached (replacing any previous entry), and returned.
    pub fn ensure_cached(
        &self,
        block_hash: B256,
        flashblock_index: u64,
        flashblocks_state: &FlashblocksState,
        canonical_state_provider: &dyn StateProvider,
    ) -> EyreResult<FlashblockTrieData> {
        let cached_entry = self.cache.load();
        if let Some(cached) = cached_entry.as_ref()
            && cached.block_hash == block_hash
            && cached.flashblock_index == flashblock_index
        {
            return Ok(cached.trie_data.clone());
        }

        let hashed_state =
            canonical_state_provider.hashed_post_state(&flashblocks_state.bundle_state);
        let (_state_root, trie_updates) =
            canonical_state_provider.state_root_with_updates(hashed_state.clone())?;

        let trie_data = FlashblockTrieData { trie_updates, hashed_state };

        // Store the new entry, replacing any previous flashblock's cached trie
        self.cache.store(Arc::new(Some(CachedFlashblockTrie {
            block_hash,
            flashblock_index,
            trie_data: trie_data.clone(),
        })));

        Ok(trie_data)
    }
}

impl Default for FlashblockTrieCache {
    fn default() -> Self {
        Self::new()
    }
}
