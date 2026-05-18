use std::{collections::VecDeque, num::NonZeroUsize};

/// A bounded ring buffer that maps positions to values.
///
/// Each entry is stored as `(I, V)`. When the buffer reaches
/// capacity, the oldest entry is evicted on [`push`](Self::push).
///
/// # Ordering invariant
///
/// Callers must push entries in non-decreasing position order.
/// The lookup methods ([`entries_after`](Self::entries_after),
/// [`positioned_entries_after`](Self::positioned_entries_after)) use binary
/// search and will return incorrect results if this invariant is violated.
#[derive(Debug, Clone)]
pub struct RingBuffer<I, V> {
    entries: VecDeque<(I, V)>,
    capacity: usize,
}

impl<I, V> RingBuffer<I, V> {
    /// Creates a new ring buffer with the given capacity.
    pub fn new(capacity: NonZeroUsize) -> Self {
        let cap = capacity.get();
        Self { entries: VecDeque::with_capacity(cap), capacity: cap }
    }

    /// Appends an entry to the buffer.
    ///
    /// If the buffer is at capacity, the oldest entry is evicted first.
    pub fn push(&mut self, position: I, value: V) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((position, value));
    }

    /// Returns the number of entries currently stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the buffer contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the position of the oldest (front) entry, or `None` if empty.
    pub fn oldest_position(&self) -> Option<&I> {
        self.entries.front().map(|(pos, _)| pos)
    }
}

impl<I, V> RingBuffer<I, V>
where
    I: Ord,
{
    /// Returns an iterator over values for entries whose position is
    /// strictly greater than `cutoff`.
    ///
    /// Uses binary search — O(log n) — relying on the ordering invariant.
    pub fn entries_after<'a>(&'a self, cutoff: &'a I) -> impl Iterator<Item = &'a V> {
        let start = self.entries.partition_point(|(pos, _)| pos <= cutoff);
        self.entries.range(start..).map(|(_, val)| val)
    }

    /// Returns an iterator over `(&I, &V)` tuples for entries whose
    /// position is strictly greater than `cutoff`.
    ///
    /// Uses binary search — O(log n) — relying on the ordering invariant.
    pub fn positioned_entries_after<'a>(
        &'a self,
        cutoff: &'a I,
    ) -> impl Iterator<Item = (&'a I, &'a V)> {
        let start = self.entries.partition_point(|(pos, _)| pos <= cutoff);
        self.entries.range(start..).map(|(pos, val)| (pos, val))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;

    fn cap(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn push_and_iterate() {
        let mut rb = RingBuffer::new(cap(4));
        rb.push(1u64, "a");
        rb.push(2, "b");
        rb.push(3, "c");

        let vals: Vec<_> = rb.entries_after(&1).collect();
        assert_eq!(vals, vec![&"b", &"c"]);
    }

    #[test]
    fn eviction_at_capacity() {
        let mut rb = RingBuffer::new(cap(3));
        rb.push(1u64, "a");
        rb.push(2, "b");
        rb.push(3, "c");
        rb.push(4, "d");

        assert_eq!(rb.len(), 3);
        let vals: Vec<_> = rb.entries_after(&0).collect();
        assert_eq!(vals, vec![&"b", &"c", &"d"]);
    }

    #[test]
    fn positioned_entries_after() {
        let mut rb = RingBuffer::new(cap(4));
        rb.push(1u64, "a");
        rb.push(2, "b");
        rb.push(3, "c");

        let entries: Vec<_> = rb.positioned_entries_after(&1).collect();
        assert_eq!(entries, vec![(&2, &"b"), (&3, &"c")]);
    }

    #[test]
    fn tuple_positions() {
        let mut rb = RingBuffer::<(u64, u64), &str>::new(cap(4));
        rb.push((1, 0), "a");
        rb.push((1, 1), "b");
        rb.push((2, 0), "c");

        let vals: Vec<_> = rb.entries_after(&(1, 0)).collect();
        assert_eq!(vals, vec![&"b", &"c"]);
    }

    #[test]
    fn empty_buffer() {
        let rb = RingBuffer::<u64, &str>::new(cap(4));
        assert!(rb.is_empty());
        let vals: Vec<_> = rb.entries_after(&0).collect();
        assert!(vals.is_empty());
    }

    #[test]
    fn oldest_position_empty() {
        let rb = RingBuffer::<u64, &str>::new(cap(4));
        assert_eq!(rb.oldest_position(), None);
    }

    #[test]
    fn oldest_position_after_push() {
        let mut rb = RingBuffer::new(cap(4));
        rb.push(10u64, "a");
        rb.push(20, "b");
        rb.push(30, "c");
        assert_eq!(rb.oldest_position(), Some(&10));
    }

    #[test]
    fn oldest_position_after_eviction() {
        let mut rb = RingBuffer::new(cap(2));
        rb.push(10u64, "a");
        rb.push(20, "b");
        rb.push(30, "c"); // evicts 10
        assert_eq!(rb.oldest_position(), Some(&20));
    }

    #[test]
    fn cutoff_at_exact_boundary() {
        let mut rb = RingBuffer::new(cap(4));
        rb.push(5u64, "a");
        rb.push(10, "b");
        rb.push(15, "c");

        // cutoff == existing position → should NOT include that position
        let vals: Vec<_> = rb.entries_after(&10).collect();
        assert_eq!(vals, vec![&"c"]);
    }

    #[test]
    fn full_eviction_cycle() {
        let mut rb = RingBuffer::new(cap(2));
        rb.push(1u64, "a");
        rb.push(2, "b");
        rb.push(3, "c");
        rb.push(4, "d");

        assert_eq!(rb.len(), 2);
        let vals: Vec<_> = rb.entries_after(&0).collect();
        assert_eq!(vals, vec![&"c", &"d"]);
    }
}
