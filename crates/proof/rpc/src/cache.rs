//! LRU cache wrapper with optional Prometheus hit/miss counters.

use std::{hash::Hash, sync::Arc};

use moka::future::Cache;

use super::config::DEFAULT_CACHE_SIZE;

/// An LRU cache that optionally emits Prometheus hit/miss counters.
#[derive(Debug)]
pub struct MeteredCache<K, V>
where
    K: Hash + Eq + Send + Sync + Clone + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// The underlying cache.
    cache: Cache<K, V>,
    /// Name of this cache for identification.
    name: Arc<str>,
    /// Precomputed Prometheus counter name for cache hits.
    hits_metric_name: Option<Arc<str>>,
    /// Precomputed Prometheus counter name for cache misses.
    misses_metric_name: Option<Arc<str>>,
}

impl<K, V> MeteredCache<K, V>
where
    K: Hash + Eq + Send + Sync + Clone + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Creates a new metered cache with the given name and default size.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self::with_capacity(name, DEFAULT_CACHE_SIZE)
    }

    /// Creates a new metered cache with the given name and capacity.
    pub fn with_capacity(name: impl Into<Arc<str>>, capacity: usize) -> Self {
        Self {
            cache: Cache::new(capacity as u64),
            name: name.into(),
            hits_metric_name: None,
            misses_metric_name: None,
        }
    }

    /// Sets a Prometheus metrics prefix on this cache.
    ///
    /// When set, cache hits and misses are emitted as Prometheus counters:
    /// `{prefix}_cache_hits_total` and `{prefix}_cache_misses_total`.
    pub fn with_metrics_prefix(mut self, prefix: &str) -> Self {
        self.hits_metric_name = Some(format!("{prefix}_cache_hits_total").into());
        self.misses_metric_name = Some(format!("{prefix}_cache_misses_total").into());
        self
    }

    /// Gets a value from the cache, returning `None` if not present.
    pub async fn get(&self, key: &K) -> Option<V> {
        let value = self.cache.get(key).await;
        if value.is_some() {
            if let Some(name) = &self.hits_metric_name {
                metrics::counter!(
                    Arc::clone(name),
                    "cache_name" => Arc::clone(&self.name),
                )
                .increment(1);
            }
        } else if let Some(name) = &self.misses_metric_name {
            metrics::counter!(
                Arc::clone(name),
                "cache_name" => Arc::clone(&self.name),
            )
            .increment(1);
        }
        value
    }

    /// Inserts a value into the cache.
    pub async fn insert(&self, key: K, value: V) {
        self.cache.insert(key, value).await;
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

        assert!(cache.get(&"key1".to_string()).await.is_none());

        cache.insert("key1".to_string(), 42).await;
        assert_eq!(cache.get(&"key1".to_string()).await, Some(42));
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
}
