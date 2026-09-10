use std::{sync::Arc, time::Instant};

use alloy_primitives::B256;
use base_common_observability_metrics::Metrics;
use metrics::{Counter, Histogram};
use parking_lot::Mutex;
use tracing::{debug, instrument, warn};

use crate::SavedCache;

/// A guarded, thread-safe cache of execution state that tracks the most recent block's caches.
///
/// This is the cross-block cache used to accelerate sequential payload processing.
/// When a new block arrives, its parent's cached state can be reused to avoid
/// redundant database lookups.
///
/// This process assumes that payloads are received sequentially.
///
/// ## Cache Safety
///
/// **CRITICAL**: Cache update operations require exclusive access. All concurrent cache users
/// (such as prewarming tasks) must be terminated before calling
/// [`PayloadExecutionCache::update_with_guard`], otherwise the cache may be corrupted or cleared.
#[derive(Clone, Debug, Default)]
pub struct PayloadExecutionCache {
    /// Guarded cloneable cache identified by a block hash.
    inner: Arc<Mutex<Option<SavedCache>>>,
    /// Metrics for cache operations.
    metrics: PayloadExecutionCacheMetrics,
}

impl PayloadExecutionCache {
    /// Returns the cache for `parent_hash` if it's available for use.
    ///
    /// A cache is considered available when:
    /// - It exists and matches the requested parent hash
    /// - No other tasks are currently using it (checked via Arc reference count)
    #[instrument(level = "debug", target = "engine::tree::payload_processor", skip(self))]
    pub fn get_cache_for(&self, parent_hash: B256) -> Option<SavedCache> {
        let start = Instant::now();
        let mut cache = self.inner.lock();

        let elapsed = start.elapsed();
        self.metrics.execution_cache_wait_duration.record(elapsed.as_secs_f64());
        if elapsed.as_millis() > 5 {
            warn!(blocked_for=?elapsed, "Blocked waiting for execution cache mutex");
        }

        if let Some(c) = cache.as_mut() {
            let cached_hash = c.executed_block_hash();
            // Check that the cache hash matches the parent hash of the current block. It won't
            // match in case it's a fork block.
            let hash_matches = cached_hash == parent_hash;
            // Check `is_available()` to ensure no other tasks (e.g., prewarming) currently hold
            // a reference to this cache. We can only reuse it when we have exclusive access.
            let available = c.is_available();
            let usage_count = c.usage_count();

            debug!(
                target: "engine::caching",
                %cached_hash,
                %parent_hash,
                hash_matches,
                available,
                usage_count,
                "Existing cache found"
            );

            if available {
                if !hash_matches {
                    // Fork block: clear and update the hash on the ORIGINAL before cloning.
                    // This prevents the canonical chain from matching on the stale hash
                    // and picking up polluted data if the fork block fails.
                    c.clear_with_hash(parent_hash);
                }
                return Some(c.clone());
            } else if hash_matches {
                self.metrics.execution_cache_in_use.increment(1);
            }
        } else {
            debug!(target: "engine::caching", %parent_hash, "No cache found");
        }

        None
    }

    /// Updates the cache with a closure that has exclusive access to the guard.
    /// This ensures that all cache operations happen atomically.
    ///
    /// ## CRITICAL SAFETY REQUIREMENT
    ///
    /// **Before calling this method, you MUST ensure there are no other active cache users.**
    /// This includes:
    /// - No running prewarming task instances that could write to the cache
    /// - No concurrent transactions that might access the cached state
    /// - All prewarming operations must be completed or cancelled
    ///
    /// Violating this requirement can result in cache corruption, incorrect state data,
    /// and potential consensus failures.
    pub fn update_with_guard<F>(&self, update_fn: F)
    where
        F: FnOnce(&mut Option<SavedCache>),
    {
        let mut guard = self.inner.lock();
        update_fn(&mut guard);
    }
}

/// Metrics for [`PayloadExecutionCache`] operations.
#[derive(Metrics, Clone)]
#[metrics(scope = "consensus.engine.beacon")]
struct PayloadExecutionCacheMetrics {
    /// Counter for when the execution cache was unavailable because other threads
    /// (e.g., prewarming) are still using it.
    execution_cache_in_use: Counter,
    /// Time spent waiting for execution cache mutex to become available.
    execution_cache_wait_duration: Histogram,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecutionCache;

    #[test]
    fn single_checkout_blocks_second() {
        let cache = PayloadExecutionCache::default();
        let hash = B256::from([1u8; 32]);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash, ExecutionCache::new(1_000)))
        });

        let first = cache.get_cache_for(hash);
        assert!(first.is_some());

        let second = cache.get_cache_for(hash);
        assert!(second.is_none());
    }

    #[test]
    fn checkout_available_after_drop() {
        let cache = PayloadExecutionCache::default();
        let hash = B256::from([2u8; 32]);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash, ExecutionCache::new(1_000)))
        });

        let checked_out = cache.get_cache_for(hash);
        assert!(checked_out.is_some());
        drop(checked_out);

        let second = cache.get_cache_for(hash);
        assert!(second.is_some());
    }

    #[test]
    fn raw_cache_handle_blocks_checkout_until_drop() {
        let cache = PayloadExecutionCache::default();
        let hash = B256::from([3u8; 32]);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash, ExecutionCache::new(1_000)))
        });

        let checked_out = cache.get_cache_for(hash).expect("checkout should succeed");
        let cache_handle = checked_out.cache().clone();
        drop(checked_out);

        let blocked = cache.get_cache_for(hash);
        assert!(blocked.is_none(), "raw ExecutionCache handle should keep slot in use");

        drop(cache_handle);

        let available = cache.get_cache_for(hash);
        assert!(available.is_some(), "checkout should succeed after raw handle is dropped");
    }

    #[test]
    fn hash_mismatch_clears_and_retags() {
        let cache = PayloadExecutionCache::default();
        let hash_a = B256::from([0xAA; 32]);
        let hash_b = B256::from([0xBB; 32]);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash_a, ExecutionCache::new(1_000)))
        });

        let checked_out = cache.get_cache_for(hash_b);
        assert!(checked_out.is_some());
        assert_eq!(checked_out.unwrap().executed_block_hash(), hash_b);
    }

    #[test]
    fn empty_cache_returns_none() {
        let cache = PayloadExecutionCache::default();
        assert!(cache.get_cache_for(B256::ZERO).is_none());
    }
}
