use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use alloy_primitives::B256;

/// Number of [`RecentlySent::was_recently_sent`] calls between pruning sweeps.
const PRUNE_INTERVAL: u64 = 1000;

/// One transaction accepted by a destination's queue.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// When the transaction was queued.
    sent: Instant,
    /// When the pool admitted the copy that was queued, its [`ValidPoolTransaction::timestamp`].
    ///
    /// [`ValidPoolTransaction::timestamp`]: reth_transaction_pool::ValidPoolTransaction::timestamp
    pooled_at: Instant,
    /// Whether the transaction waits out the validity hold instead of the resend window.
    held: bool,
}

/// Per-destination deduplication tracker.
///
/// Tracks transaction hashes accepted by one destination's queue. A standard transaction becomes
/// due for a resend `resend_after` after it was queued. A validity transaction is held for
/// `validity_hold` instead, long enough to outlive any validity transaction, so it is never resent
/// on a timer: staying in the pool until its predicates hold is its expected state, and the
/// destination keeps it parked until its block expiry. Only a failed forward, reported through
/// [`Self::mark_failed`], puts it back on the standard schedule.
///
/// Entries are kept for at least `resend_after` past their due time, so a transaction due for a
/// resend can be told apart from one never sent. Older entries are pruned every
/// [`PRUNE_INTERVAL`] lookups to bound memory, so one can outlive that until the next sweep.
pub(super) struct RecentlySent {
    seen: HashMap<B256, Entry>,
    resend_after: Duration,
    validity_hold: Duration,
    check_count: u64,
}

impl RecentlySent {
    /// Creates a new tracker.
    pub(super) fn new(resend_after: Duration, validity_hold: Duration) -> Self {
        Self { seen: HashMap::new(), resend_after, validity_hold, check_count: 0 }
    }

    /// Returns `true` if the copy of `hash` the pool admitted at `pooled_at` was sent and is not
    /// yet due for a resend.
    ///
    /// Triggers a pruning sweep every [`PRUNE_INTERVAL`] calls.
    pub(super) fn was_recently_sent(&mut self, hash: &B256, pooled_at: Instant) -> bool {
        self.check_count += 1;
        if self.check_count.is_multiple_of(PRUNE_INTERVAL) {
            self.prune_expired();
        }

        self.resend_due(hash, pooled_at).is_some_and(|due| Instant::now() < due)
    }

    /// Returns when the copy of `hash` the pool admitted at `pooled_at` is or was due for a
    /// resend, or `None` if that copy has no entry. An entry lasts at least `resend_after` past its
    /// due time; see [`RecentlySent`].
    pub(super) fn resend_due(&self, hash: &B256, pooled_at: Instant) -> Option<Instant> {
        self.entry(hash, pooled_at).map(|entry| self.due(entry))
    }

    /// Returns `true` if the copy of `hash` the pool admitted at `pooled_at` has an entry, which
    /// lasts at least `resend_after` past its due time; see [`RecentlySent`].
    pub(super) fn was_sent(&self, hash: &B256, pooled_at: Instant) -> bool {
        self.entry(hash, pooled_at).is_some()
    }

    /// Records the copy of `hash` the pool admitted at `pooled_at` as sent now. A validity
    /// transaction is held for the validity hold rather than resent after the standard window.
    pub(super) fn mark_sent(&mut self, hash: B256, pooled_at: Instant, is_validity: bool) {
        self.seen.insert(hash, Entry { sent: Instant::now(), pooled_at, held: is_validity });
    }

    /// Puts a transaction whose forward failed back on the standard resend schedule. Has no
    /// effect on a transaction without an entry or already on that schedule.
    pub(super) fn mark_failed(&mut self, hash: &B256) {
        if let Some(entry) = self.seen.get_mut(hash) {
            entry.held = false;
        }
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

    fn entry(&self, hash: &B256, pooled_at: Instant) -> Option<&Entry> {
        self.seen.get(hash).filter(|entry| entry.pooled_at == pooled_at)
    }

    fn due(&self, entry: &Entry) -> Instant {
        entry.sent + if entry.held { self.validity_hold } else { self.resend_after }
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        let (resend_after, validity_hold) = (self.resend_after, self.validity_hold);
        self.seen.retain(|_, entry| {
            let window = if entry.held { validity_hold } else { resend_after };
            now < entry.sent + window + resend_after
        });
    }
}

impl std::fmt::Debug for RecentlySent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecentlySent")
            .field("resend_after", &self.resend_after)
            .field("validity_hold", &self.validity_hold)
            .field("entries", &self.seen.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    const WINDOW: Duration = Duration::from_secs(5);
    const HOLD: Duration = Duration::from_secs(60);

    /// One pool admission time shared by every copy the tests treat as the same pooled
    /// transaction.
    fn pooled() -> Instant {
        static POOLED: OnceLock<Instant> = OnceLock::new();
        *POOLED.get_or_init(Instant::now)
    }

    /// A held validity transaction that left the pool and was admitted again under the same hash
    /// is due at once, since the destination may have dropped the first copy at its expiry.
    #[test]
    fn a_readmitted_copy_of_a_held_transaction_is_not_recent() {
        let mut tracker = RecentlySent::new(WINDOW, HOLD);
        let hash = B256::random();
        tracker.mark_sent(hash, pooled(), true);

        let readmitted = pooled() + Duration::from_secs(1);

        assert!(tracker.was_recently_sent(&hash, pooled()), "the queued copy is still held");
        assert!(!tracker.was_recently_sent(&hash, readmitted));
        assert!(!tracker.was_sent(&hash, readmitted));
        assert_eq!(tracker.resend_due(&hash, readmitted), None);
    }

    #[test]
    fn unseen_hash_is_not_recent() {
        let mut tracker = RecentlySent::new(WINDOW, HOLD);
        assert!(!tracker.was_recently_sent(&B256::random(), pooled()));
    }

    #[test]
    fn sent_hash_is_recent() {
        let mut tracker = RecentlySent::new(WINDOW, HOLD);
        let hash = B256::random();

        tracker.mark_sent(hash, pooled(), false);
        assert!(tracker.was_recently_sent(&hash, pooled()));
    }

    #[test]
    fn trackers_deduplicate_independently() {
        let mut first_destination = RecentlySent::new(WINDOW, HOLD);
        let mut second_destination = RecentlySent::new(WINDOW, HOLD);
        let hash = B256::random();

        first_destination.mark_sent(hash, pooled(), false);

        assert!(first_destination.was_recently_sent(&hash, pooled()));
        assert!(!second_destination.was_recently_sent(&hash, pooled()));
    }

    #[test]
    fn expired_hash_is_not_recent() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10), HOLD);
        let hash = B256::random();

        tracker.mark_sent(hash, pooled(), false);
        std::thread::sleep(Duration::from_millis(15));
        assert!(!tracker.was_recently_sent(&hash, pooled()));
        assert!(tracker.was_sent(&hash, pooled()), "a due entry is kept to mark the resend");
    }

    /// A validity transaction is held after a standard transaction sent at the same time has
    /// become due.
    #[test]
    fn a_validity_transaction_is_held_past_the_resend_window() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10), HOLD);
        let (standard, validity) = (B256::random(), B256::random());

        tracker.mark_sent(standard, pooled(), false);
        tracker.mark_sent(validity, pooled(), true);
        std::thread::sleep(Duration::from_millis(15));

        assert!(!tracker.was_recently_sent(&standard, pooled()));
        assert!(tracker.was_recently_sent(&validity, pooled()));
    }

    /// A failed forward releases a held validity transaction onto the standard schedule, counted
    /// from when it was sent.
    #[test]
    fn a_failed_validity_transaction_follows_the_resend_window() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10), HOLD);
        let hash = B256::random();

        tracker.mark_sent(hash, pooled(), true);
        tracker.mark_failed(&hash);
        assert!(tracker.was_recently_sent(&hash, pooled()), "still inside the standard window");

        std::thread::sleep(Duration::from_millis(15));
        assert!(!tracker.was_recently_sent(&hash, pooled()));
    }

    #[test]
    fn marking_an_unsent_hash_failed_adds_nothing() {
        let mut tracker = RecentlySent::new(WINDOW, HOLD);

        tracker.mark_failed(&B256::random());

        assert!(tracker.is_empty());
    }

    #[test]
    fn resend_due_follows_the_entry_schedule() {
        let mut tracker = RecentlySent::new(WINDOW, HOLD);
        let (standard, validity) = (B256::random(), B256::random());
        let before = Instant::now();

        tracker.mark_sent(standard, pooled(), false);
        tracker.mark_sent(validity, pooled(), true);

        let after = Instant::now();
        let due = tracker.resend_due(&standard, pooled()).unwrap();
        assert!(due >= before + WINDOW && due <= after + WINDOW);
        let due = tracker.resend_due(&validity, pooled()).unwrap();
        assert!(due >= before + HOLD && due <= after + HOLD);
    }

    #[test]
    fn len_tracks_entries() {
        let mut tracker = RecentlySent::new(WINDOW, HOLD);
        assert!(tracker.is_empty());

        tracker.mark_sent(B256::random(), pooled(), false);
        tracker.mark_sent(B256::random(), pooled(), false);
        tracker.mark_sent(B256::random(), pooled(), true);

        assert_eq!(tracker.len(), 3);
    }

    #[test]
    fn prune_removes_expired() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10), HOLD);

        for _ in 0..5 {
            tracker.mark_sent(B256::random(), pooled(), false);
        }
        assert_eq!(tracker.len(), 5);

        std::thread::sleep(Duration::from_millis(25));
        tracker.prune_expired();

        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn prune_keeps_held_entries() {
        let mut tracker = RecentlySent::new(Duration::from_millis(10), HOLD);
        tracker.mark_sent(B256::random(), pooled(), true);

        std::thread::sleep(Duration::from_millis(25));
        tracker.prune_expired();

        assert_eq!(tracker.len(), 1);
    }
}
