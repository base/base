use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use alloy_primitives::B256;

/// Number of [`RecentlySent::was_recently_sent`] calls between pruning sweeps.
const PRUNE_INTERVAL: u64 = 1000;

/// Per-destination deduplication tracker with a configurable time-to-live.
///
/// Tracks transaction hashes accepted by one destination's queue. A transaction whose hash appears
/// in the map and whose entry is younger than `resend_after` will be skipped. Entries are kept for
/// at least twice `resend_after`, so a transaction due for a resend can be told apart from one
/// never sent. Older entries are pruned every [`PRUNE_INTERVAL`] lookups to bound memory, so one
/// can outlive twice `resend_after` until the next sweep.
///
/// Once the destination confirms it holds a transaction, its entry is dated ahead so the resend
/// comes [`Self::DELIVERED_RESEND_AFTER`] after the confirmation: resending a transaction the
/// destination holds only earns an `already imported` rejection, and a resting pool would
/// otherwise be resent in full every `resend_after`.
pub(super) struct RecentlySent {
    seen: HashMap<B256, Instant>,
    resend_after: Duration,
    check_count: u64,
}

impl RecentlySent {
    /// How long after a confirmed delivery a still-pending transaction is resent, unless the
    /// destination is lost first (see [`Self::forget_deliveries`]).
    pub(super) const DELIVERED_RESEND_AFTER: Duration = Duration::from_secs(30);

    /// Creates a new tracker.
    pub(super) fn new(resend_after: Duration) -> Self {
        Self { seen: HashMap::new(), resend_after, check_count: 0 }
    }

    /// Returns `true` if the hash was sent within the `resend_after` window.
    ///
    /// Triggers a pruning sweep every [`PRUNE_INTERVAL`] calls.
    pub(super) fn was_recently_sent(&mut self, hash: &B256) -> bool {
        self.check_count += 1;
        if self.check_count.is_multiple_of(PRUNE_INTERVAL) {
            self.prune_expired();
        }

        self.seen.get(hash).is_some_and(|instant| instant.elapsed() < self.resend_after)
    }

    /// Returns when a transaction sent earlier is or was due for a resend, or `None` if it has no
    /// entry. An entry lasts at least two resend windows; see [`RecentlySent`].
    pub(super) fn resend_due(&self, hash: &B256) -> Option<Instant> {
        self.seen.get(hash).map(|sent| *sent + self.resend_after)
    }

    /// Returns `true` if the hash has an entry, which lasts at least two resend windows; see
    /// [`RecentlySent`].
    pub(super) fn was_sent(&self, hash: &B256) -> bool {
        self.seen.contains_key(hash)
    }

    /// Records a hash as sent at the current instant.
    pub(super) fn mark_sent(&mut self, hash: B256) {
        self.seen.insert(hash, Instant::now());
    }

    /// Records that the destination holds the transaction, deferring its resend to
    /// [`Self::DELIVERED_RESEND_AFTER`] from now, or to `resend_after` if that is longer.
    ///
    /// The entry is dated in the future, `resend_after` before the resend is due.
    pub(super) fn mark_delivered(&mut self, hash: B256) {
        let deferral = Self::DELIVERED_RESEND_AFTER.saturating_sub(self.resend_after);
        let dated = Instant::now() + deferral;
        self.seen.entry(hash).and_modify(|at| *at = (*at).max(dated)).or_insert(dated);
    }

    /// Puts every confirmed transaction back on the regular resend schedule, from now.
    ///
    /// Called when the destination failed a send, as when it restarts: it may have lost the
    /// transactions it confirmed before.
    pub(super) fn forget_deliveries(&mut self) {
        let now = Instant::now();
        self.seen.values_mut().for_each(|at| *at = (*at).min(now));
    }

    /// Returns the current cache size.
    pub(super) fn len(&self) -> usize {
        self.seen.len()
    }

    /// Returns `true` when the cache is empty.
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        let keep = self.resend_after * 2;
        self.seen.retain(|_, instant| now.duration_since(*instant) < keep);
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
    fn trackers_deduplicate_independently() {
        let mut first_destination = RecentlySent::new(Duration::from_secs(5));
        let mut second_destination = RecentlySent::new(Duration::from_secs(5));
        let hash = B256::random();

        first_destination.mark_sent(hash);

        assert!(first_destination.was_recently_sent(&hash));
        assert!(!second_destination.was_recently_sent(&hash));
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
    fn delivered_hash_outlives_the_resend_window() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10));
        let (delivered, undelivered) = (B256::random(), B256::random());

        tracker.mark_sent(delivered);
        tracker.mark_sent(undelivered);
        tracker.mark_delivered(delivered);
        std::thread::sleep(Duration::from_millis(15));
        tracker.prune_expired();

        assert!(tracker.was_recently_sent(&delivered));
        assert!(!tracker.was_recently_sent(&undelivered));
    }

    #[test]
    fn forgotten_delivery_is_resent_after_the_resend_window() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10));
        let hash = B256::random();

        tracker.mark_sent(hash);
        tracker.mark_delivered(hash);
        tracker.forget_deliveries();

        assert!(tracker.was_recently_sent(&hash));
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

        std::thread::sleep(Duration::from_millis(25));
        tracker.prune_expired();

        assert_eq!(tracker.len(), 0);
    }
}
