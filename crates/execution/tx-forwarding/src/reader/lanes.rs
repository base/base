use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    hash::Hash,
    time::Instant,
};

/// Lane a destination reader serves its next pick from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lane {
    /// Oldest pending transaction first.
    Fifo,
    /// Highest bid first.
    Priority,
}

/// Interleaves the two lanes so that a fixed percentage of picks goes to the FIFO lane.
///
/// At 20%, every fifth pick is FIFO and the other four are by priority, so each forwarded batch
/// carries the same mix. The interleaving state lives across reader passes, which keeps the
/// long-run ratio exact even when a pass sends only a handful of transactions.
///
/// Percentages above 100 behave as 100.
#[derive(Debug, Clone)]
pub(super) struct LaneMix {
    fifo_percent: u8,
    credit: u8,
}

impl LaneMix {
    /// Creates a mix that sends `fifo_percent` of picks through the FIFO lane.
    pub(super) const fn new(fifo_percent: u8) -> Self {
        Self { fifo_percent: if fifo_percent > 100 { 100 } else { fifo_percent }, credit: 0 }
    }

    /// Returns the lane for the next pick and advances the interleaving.
    pub(super) const fn next_lane(&mut self) -> Lane {
        self.credit += self.fifo_percent;
        if self.credit >= 100 {
            self.credit -= 100;
            Lane::Fifo
        } else {
            Lane::Priority
        }
    }
}

/// One snapshot's worth of forwardable transactions, indexed for both lanes.
///
/// Built from a `best_transactions()` snapshot, which yields the transactions of each nonce
/// sequence (a sender, or a sender and EIP-8130 nonce key) in nonce order and only after their
/// predecessors. Only a sequence's earliest unpicked transaction (its head) is eligible in either
/// lane, so neither lane forwards a transaction ahead of its predecessor and the builder never
/// receives a nonce gap from this reader. Transactions without a sequence are always eligible.
///
/// Every head sits in both heaps. Picking it from one leaves a stale entry in the other, which is
/// skipped when it surfaces, so both lanes always see the same set of eligible transactions and
/// one lane is empty exactly when the other is.
#[derive(Debug)]
pub(super) struct LaneScheduler<T, P, K> {
    entries: Vec<Entry<T, P>>,
    by_age: BinaryHeap<Reverse<(Instant, usize)>>,
    by_priority: BinaryHeap<(P, Reverse<Instant>, Reverse<usize>)>,
    last_of_sequence: HashMap<K, usize>,
    remaining: usize,
}

#[derive(Debug)]
struct Entry<T, P> {
    /// `None` once picked.
    item: Option<T>,
    arrived: Instant,
    priority: P,
    /// The same sequence's next transaction in snapshot order.
    next_in_sequence: Option<usize>,
}

impl<T, P: Ord + Clone, K: Hash + Eq> LaneScheduler<T, P, K> {
    /// Creates an empty scheduler.
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
            by_age: BinaryHeap::new(),
            by_priority: BinaryHeap::new(),
            last_of_sequence: HashMap::new(),
            remaining: 0,
        }
    }

    /// Adds a transaction. Must be called in snapshot order.
    ///
    /// `arrived` orders the FIFO lane and `priority` orders the priority lane; ties in the
    /// priority lane go to the earlier arrival. A transaction with a `sequence` is held back until
    /// every earlier transaction pushed with the same sequence has been picked.
    pub(super) fn push(&mut self, item: T, sequence: Option<K>, arrived: Instant, priority: P) {
        let index = self.entries.len();
        self.entries.push(Entry { item: Some(item), arrived, priority, next_in_sequence: None });
        self.remaining += 1;
        let previous = sequence.and_then(|sequence| self.last_of_sequence.insert(sequence, index));
        match previous {
            Some(previous) => self.entries[previous].next_in_sequence = Some(index),
            None => self.make_eligible(index),
        }
    }

    /// Returns `true` once every pushed transaction has been picked.
    pub(super) const fn is_empty(&self) -> bool {
        self.remaining == 0
    }

    /// Removes and returns the best eligible transaction in `lane`, or `None` when nothing is left.
    pub(super) fn pop(&mut self, lane: Lane) -> Option<T> {
        loop {
            let index = match lane {
                Lane::Fifo => self.by_age.pop()?.0.1,
                Lane::Priority => self.by_priority.pop()?.2.0,
            };
            let entry = &mut self.entries[index];
            let Some(item) = entry.item.take() else { continue };
            if let Some(next) = entry.next_in_sequence {
                self.make_eligible(next);
            }
            self.remaining -= 1;
            return Some(item);
        }
    }

    fn make_eligible(&mut self, index: usize) {
        let entry = &self.entries[index];
        self.by_age.push(Reverse((entry.arrived, index)));
        self.by_priority.push((entry.priority.clone(), Reverse(entry.arrived), Reverse(index)));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn sequence(id: u8) -> Option<u8> {
        Some(id)
    }

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn drain(
        scheduler: &mut LaneScheduler<&'static str, u64, u8>,
        mix: &mut LaneMix,
    ) -> Vec<&'static str> {
        std::iter::from_fn(|| scheduler.pop(mix.next_lane())).collect()
    }

    #[test]
    fn mix_sends_the_configured_share_through_fifo() {
        let mut mix = LaneMix::new(20);

        let lanes: Vec<_> = (0..10).map(|_| mix.next_lane()).collect();

        let fifo = lanes.iter().filter(|lane| **lane == Lane::Fifo).count();
        assert_eq!(fifo, 2);
        assert_eq!(lanes[4], Lane::Fifo, "FIFO picks are spread out, not bunched at the start");
    }

    #[test]
    fn mix_extremes_use_a_single_lane() {
        let mut fifo_only = LaneMix::new(100);
        let mut over = LaneMix::new(u8::MAX);
        let mut priority_only = LaneMix::new(0);

        for _ in 0..50 {
            assert_eq!(fifo_only.next_lane(), Lane::Fifo);
            assert_eq!(over.next_lane(), Lane::Fifo);
            assert_eq!(priority_only.next_lane(), Lane::Priority);
        }
    }

    #[test]
    fn priority_lane_sends_the_highest_bid_first_and_fifo_lane_the_oldest() {
        let base = Instant::now();
        let mut scheduler = LaneScheduler::new();
        scheduler.push("old-cheap", sequence(1), at(base, 0), 1);
        scheduler.push("new-rich", sequence(2), at(base, 10), 100);
        scheduler.push("mid", sequence(3), at(base, 5), 50);

        assert_eq!(scheduler.pop(Lane::Priority), Some("new-rich"));
        assert_eq!(scheduler.pop(Lane::Fifo), Some("old-cheap"));
        assert_eq!(scheduler.pop(Lane::Fifo), Some("mid"));
        assert_eq!(scheduler.pop(Lane::Priority), None);
        assert_eq!(scheduler.pop(Lane::Fifo), None);
    }

    #[test]
    fn equal_bids_go_to_the_earlier_arrival() {
        let base = Instant::now();
        let mut scheduler = LaneScheduler::new();
        scheduler.push("later", sequence(1), at(base, 10), 7);
        scheduler.push("earlier", sequence(2), at(base, 0), 7);

        assert_eq!(scheduler.pop(Lane::Priority), Some("earlier"));
    }

    /// A high bid on a sender's second nonce must not overtake that sender's first nonce: the
    /// builder would park it as a gapped transaction instead of making it executable.
    #[test]
    fn a_sequence_is_never_forwarded_out_of_nonce_order() {
        let base = Instant::now();
        let mut scheduler = LaneScheduler::new();
        scheduler.push("a0-cheap", sequence(0xa), at(base, 0), 1);
        scheduler.push("a1-rich", sequence(0xa), at(base, 1), 1_000);
        scheduler.push("b0", sequence(0xb), at(base, 2), 10);

        assert_eq!(scheduler.pop(Lane::Priority), Some("b0"));
        assert_eq!(scheduler.pop(Lane::Priority), Some("a0-cheap"));
        assert_eq!(scheduler.pop(Lane::Priority), Some("a1-rich"));
    }

    #[test]
    fn transactions_without_a_sequence_are_never_held_back() {
        let base = Instant::now();
        let mut scheduler = LaneScheduler::<_, u64, u8>::new();
        scheduler.push("cheap", None, at(base, 0), 1);
        scheduler.push("rich", None, at(base, 1), 100);

        assert_eq!(scheduler.pop(Lane::Priority), Some("rich"));
        assert_eq!(scheduler.pop(Lane::Priority), Some("cheap"));
        assert!(scheduler.is_empty());
    }

    #[test]
    fn a_transaction_picked_by_one_lane_is_not_sent_again_by_the_other() {
        let base = Instant::now();
        let mut scheduler = LaneScheduler::new();
        scheduler.push("only", sequence(1), at(base, 0), 1);

        assert_eq!(scheduler.pop(Lane::Fifo), Some("only"));
        assert_eq!(scheduler.pop(Lane::Priority), None);
    }

    /// During a burst of cheap transactions, high bids arriving last still go out first, while the
    /// FIFO share keeps the oldest cheap transactions moving.
    #[test]
    fn burst_sends_late_high_bids_first_and_keeps_the_oldest_moving() {
        let base = Instant::now();
        let mut scheduler = LaneScheduler::new();
        let spam = ["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"];
        for (i, name) in spam.iter().enumerate() {
            // Later spam bids slightly more, so the priority lane would never pick `s0`.
            scheduler.push(*name, sequence(i as u8), at(base, i as u64), 1 + i as u64);
        }
        scheduler.push("searcher-a", sequence(0xa0), at(base, 100), 500);
        scheduler.push("searcher-b", sequence(0xb0), at(base, 101), 400);
        let mut mix = LaneMix::new(25);

        let order = drain(&mut scheduler, &mut mix);

        assert_eq!(&order[..3], &["searcher-a", "searcher-b", "s7"]);
        assert_eq!(order[3], "s0", "the fourth pick is FIFO and takes the oldest transaction");
        assert_eq!(order.len(), spam.len() + 2);
    }
}
