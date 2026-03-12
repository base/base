use std::{collections::VecDeque, num::NonZeroUsize};

/// A bounded ring buffer that maps optional positions to values.
///
/// Each entry is stored as `(Option<I>, V)`. When the buffer reaches
/// capacity, the oldest entry is evicted on [`push`](Self::push).
///
/// Entries with `None` positions are sentinels — they are always
/// yielded by the `entries_after` and `positioned_entries_after`
/// iterators regardless of the cutoff.
#[derive(Debug, Clone)]
pub struct RingBuffer<I, V> {
    entries: VecDeque<(Option<I>, V)>,
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
    pub fn push(&mut self, position: Option<I>, value: V) {
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
}

impl<I, V> RingBuffer<I, V>
where
    I: Ord,
{
    /// Returns an iterator over values for entries whose position is
    /// strictly greater than `cutoff`, plus all sentinel (`None`-position)
    /// entries.
    pub fn entries_after<'a>(&'a self, cutoff: &'a I) -> impl Iterator<Item = &'a V> {
        self.entries.iter().filter_map(move |(pos, val)| match pos {
            Some(p) if p > cutoff => Some(val),
            None => Some(val),
            _ => None,
        })
    }

    /// Returns an iterator over `(Option<&I>, &V)` tuples for entries
    /// whose position is strictly greater than `cutoff`, plus all sentinel
    /// entries.
    pub fn positioned_entries_after<'a>(
        &'a self,
        cutoff: &'a I,
    ) -> impl Iterator<Item = (Option<&'a I>, &'a V)> {
        self.entries.iter().filter_map(move |(pos, val)| match pos {
            Some(p) if p > cutoff => Some((Some(p), val)),
            None => Some((None, val)),
            _ => None,
        })
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
        rb.push(Some(1u64), "a");
        rb.push(Some(2), "b");
        rb.push(Some(3), "c");

        let vals: Vec<_> = rb.entries_after(&1).collect();
        assert_eq!(vals, vec![&"b", &"c"]);
    }

    #[test]
    fn eviction_at_capacity() {
        let mut rb = RingBuffer::new(cap(3));
        rb.push(Some(1u64), "a");
        rb.push(Some(2), "b");
        rb.push(Some(3), "c");
        rb.push(Some(4), "d");

        assert_eq!(rb.len(), 3);
        let vals: Vec<_> = rb.entries_after(&0).collect();
        assert_eq!(vals, vec![&"b", &"c", &"d"]);
    }

    #[test]
    fn sentinels_always_included() {
        let mut rb = RingBuffer::new(cap(4));
        rb.push(Some(1u64), "a");
        rb.push(None, "sentinel");
        rb.push(Some(3), "c");

        let vals: Vec<_> = rb.entries_after(&5).collect();
        assert_eq!(vals, vec![&"sentinel"]);
    }

    #[test]
    fn positioned_entries_after() {
        let mut rb = RingBuffer::new(cap(4));
        rb.push(Some(1u64), "a");
        rb.push(None, "sentinel");
        rb.push(Some(3), "c");

        let entries: Vec<_> = rb.positioned_entries_after(&1).collect();
        assert_eq!(entries, vec![(None, &"sentinel"), (Some(&3), &"c")]);
    }

    #[test]
    fn tuple_positions() {
        let mut rb = RingBuffer::<(u64, u64), &str>::new(cap(4));
        rb.push(Some((1, 0)), "a");
        rb.push(Some((1, 1)), "b");
        rb.push(Some((2, 0)), "c");
        rb.push(None, "sentinel");

        let vals: Vec<_> = rb.entries_after(&(1, 0)).collect();
        assert_eq!(vals, vec![&"b", &"c", &"sentinel"]);
    }

    #[test]
    fn empty_buffer() {
        let rb = RingBuffer::<u64, &str>::new(cap(4));
        assert!(rb.is_empty());
        let vals: Vec<_> = rb.entries_after(&0).collect();
        assert!(vals.is_empty());
    }

    #[test]
    fn cutoff_at_exact_boundary() {
        let mut rb = RingBuffer::new(cap(4));
        rb.push(Some(5u64), "a");
        rb.push(Some(10), "b");
        rb.push(Some(15), "c");

        // cutoff == existing position → should NOT include that position
        let vals: Vec<_> = rb.entries_after(&10).collect();
        assert_eq!(vals, vec![&"c"]);
    }

    #[test]
    fn full_eviction_cycle() {
        let mut rb = RingBuffer::new(cap(2));
        rb.push(Some(1u64), "a");
        rb.push(Some(2), "b");
        rb.push(Some(3), "c");
        rb.push(Some(4), "d");

        assert_eq!(rb.len(), 2);
        let vals: Vec<_> = rb.entries_after(&0).collect();
        assert_eq!(vals, vec![&"c", &"d"]);
    }
}
