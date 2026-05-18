use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use alloy_primitives::B256;

/// Number of [`RecentlySent::was_recently_sent`] calls between pruning sweeps.
const PRUNE_INTERVAL: u64 = 1000;

/// Hash-based deduplication tracker with a configurable time-to-live.
///
/// Tracks transaction hashes that have already been forwarded through the
/// consumer broadcast channel. A transaction whose hash appears in the map
/// and whose entry is younger than `resend_after` will be skipped. Expired
/// entries are pruned periodically (every [`PRUNE_INTERVAL`] lookups) to
/// bound memory.
pub struct RecentlySent {
    seen: HashMap<B256, Instant>,
    resend_after: Duration,
    check_count: u64,
}

impl RecentlySent {
    /// Creates a new tracker.
    pub fn new(resend_after: Duration) -> Self {
        Self { seen: HashMap::new(), resend_after, check_count: 0 }
    }

    /// Returns `true` if the hash was sent within the `resend_after` window.
    ///
    /// Triggers a pruning sweep every [`PRUNE_INTERVAL`] calls.
    pub fn was_recently_sent(&mut self, hash: &B256) -> bool {
        self.check_count += 1;
        if self.check_count.is_multiple_of(PRUNE_INTERVAL) {
            self.prune_expired();
        }

        self.seen.get(hash).is_some_and(|instant| instant.elapsed() < self.resend_after)
    }

    /// Records a hash as sent at the current instant.
    pub fn mark_sent(&mut self, hash: B256) {
        self.seen.insert(hash, Instant::now());
    }

    /// Returns the current cache size.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Returns `true` when the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        self.seen.retain(|_, instant| now.duration_since(*instant) < self.resend_after);
    }
}

impl std::fmt::Debug for RecentlySent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecentlySent")
            .field("resend_after", &self.resend_after)
            .field("entries", &self.seen.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_hash_is_not_recent() {
        let mut tracker = RecentlySent::new(Duration::from_secs(5));
        assert!(!tracker.was_recently_sent(&B256::random()));
    }

    #[test]
    fn sent_hash_is_recent() {
        let mut tracker = RecentlySent::new(Duration::from_secs(5));
        let hash = B256::random();

        tracker.mark_sent(hash);
        assert!(tracker.was_recently_sent(&hash));
    }

    #[test]
    fn expired_hash_is_not_recent() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10));
        let hash = B256::random();

        tracker.mark_sent(hash);
        std::thread::sleep(Duration::from_millis(15));
        assert!(!tracker.was_recently_sent(&hash));
    }

    #[test]
    fn len_tracks_entries() {
        let mut tracker = RecentlySent::new(Duration::from_secs(5));
        assert!(tracker.is_empty());

        tracker.mark_sent(B256::random());
        tracker.mark_sent(B256::random());
        tracker.mark_sent(B256::random());

        assert_eq!(tracker.len(), 3);
    }

    #[test]
    fn prune_removes_expired() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10));

        for _ in 0..5 {
            tracker.mark_sent(B256::random());
        }
        assert_eq!(tracker.len(), 5);

        std::thread::sleep(Duration::from_millis(15));
        tracker.prune_expired();

        assert_eq!(tracker.len(), 0);
    }
}
