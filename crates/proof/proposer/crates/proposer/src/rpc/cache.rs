//! LRU cache wrapper with metrics.

use std::{
    hash::Hash,
    sync::atomic::{AtomicU64, Ordering},
};

use moka::future::Cache;

use crate::constants::DEFAULT_CACHE_SIZE;
use crate::metrics as proposer_metrics;

/// Cache metrics for tracking hit/miss rates.
#[derive(Debug, Default)]
pub struct CacheMetrics {
    /// Number of cache hits.
    hits: AtomicU64,
    /// Number of cache misses.
    misses: AtomicU64,
}

impl CacheMetrics {
    /// Creates a new cache metrics instance.
    pub const fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Records a cache hit.
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a cache miss.
    pub fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the number of cache hits.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Returns the number of cache misses.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Returns the total number of cache accesses.
    pub fn total(&self) -> u64 {
        self.hits() + self.misses()
    }

    /// Returns the cache hit rate as a percentage (0.0 to 1.0).
    pub fn hit_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        self.hits() as f64 / total as f64
    }
}

/// A metered LRU cache that tracks hit/miss statistics.
#[derive(Debug)]
pub struct MeteredCache<K, V>
where
    K: Hash + Eq + Send + Sync + Clone + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// The underlying cache.
    cache: Cache<K, V>,
    /// Metrics for this cache.
    metrics: CacheMetrics,
    /// Name of this cache for identification.
    name: String,
}

impl<K, V> MeteredCache<K, V>
where
    K: Hash + Eq + Send + Sync + Clone + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Creates a new metered cache with the given name and default size.
    pub fn new(name: impl Into<String>) -> Self {
        Self::with_capacity(name, DEFAULT_CACHE_SIZE)
    }

    /// Creates a new metered cache with the given name and capacity.
    pub fn with_capacity(name: impl Into<String>, capacity: usize) -> Self {
        Self {
            cache: Cache::new(capacity as u64),
            metrics: CacheMetrics::new(),
            name: name.into(),
        }
    }

    /// Gets a value from the cache, returning `None` if not present.
    pub async fn get(&self, key: &K) -> Option<V> {
        let value = self.cache.get(key).await;
        if value.is_some() {
            self.metrics.record_hit();
            metrics::counter!(proposer_metrics::CACHE_HITS_TOTAL, proposer_metrics::LABEL_CACHE_NAME => self.name.clone()).increment(1);
        } else {
            self.metrics.record_miss();
            metrics::counter!(proposer_metrics::CACHE_MISSES_TOTAL, proposer_metrics::LABEL_CACHE_NAME => self.name.clone()).increment(1);
        }
        value
    }

    /// Inserts a value into the cache.
    pub async fn insert(&self, key: K, value: V) {
        self.cache.insert(key, value).await;
    }

    /// Returns the cache metrics.
    pub const fn metrics(&self) -> &CacheMetrics {
        &self.metrics
    }

    /// Returns the cache name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the current number of entries in the cache.
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cache_hit_miss() {
        let cache: MeteredCache<String, i32> = MeteredCache::new("test");

        // Initial miss
        assert!(cache.get(&"key1".to_string()).await.is_none());
        assert_eq!(cache.metrics().misses(), 1);
        assert_eq!(cache.metrics().hits(), 0);

        // Insert and hit
        cache.insert("key1".to_string(), 42).await;
        assert_eq!(cache.get(&"key1".to_string()).await, Some(42));
        assert_eq!(cache.metrics().hits(), 1);
        assert_eq!(cache.metrics().misses(), 1);

        // Hit rate should be 50%
        assert!((cache.metrics().hit_rate() - 0.5).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_cache_capacity() {
        let cache: MeteredCache<i32, i32> = MeteredCache::with_capacity("test", 2);

        cache.insert(1, 100).await;
        cache.insert(2, 200).await;

        // Both should be present
        assert_eq!(cache.get(&1).await, Some(100));
        assert_eq!(cache.get(&2).await, Some(200));
    }

    #[test]
    fn test_cache_metrics_initial() {
        let metrics = CacheMetrics::new();
        assert_eq!(metrics.hits(), 0);
        assert_eq!(metrics.misses(), 0);
        assert_eq!(metrics.total(), 0);
        assert!((metrics.hit_rate() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_cache_metrics_hit_rate() {
        let metrics = CacheMetrics::new();

        // 3 hits, 1 miss = 75% hit rate
        metrics.record_hit();
        metrics.record_hit();
        metrics.record_hit();
        metrics.record_miss();

        assert_eq!(metrics.hits(), 3);
        assert_eq!(metrics.misses(), 1);
        assert_eq!(metrics.total(), 4);
        assert!((metrics.hit_rate() - 0.75).abs() < 0.001);
    }
}
