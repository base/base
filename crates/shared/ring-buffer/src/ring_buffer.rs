use std::collections::VecDeque;

/// A single entry in the ring buffer.
///
/// `position` is `None` for sentinel entries (those whose position could not
/// be determined), which are always included when replaying to reconnecting
/// clients.
#[derive(Debug)]
pub struct RingBufferEntry<I, V> {
    /// Position of this entry, or `None` if the position could not be parsed.
    pub position: Option<I>,
    /// The raw payload for this entry.
    pub payload: V,
}

/// A fixed-capacity ring buffer of sequenced entries.
///
/// Each entry is associated with an optional position of type `I`. Entries
/// pushed with `position = None` are treated as sentinels and are always
/// returned by [`RingBuffer::entries_after`], regardless of the requested
/// cutoff. This handles the case where an entry's position cannot be
/// determined.
///
/// When the capacity is exceeded, the oldest entry is evicted.
#[derive(Debug)]
pub struct RingBuffer<I, V> {
    capacity: usize,
    entries: VecDeque<RingBufferEntry<I, V>>,
}

impl<I: PartialOrd, V> RingBuffer<I, V> {
    /// Creates a new ring buffer with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be at least 1");
        Self { capacity, entries: VecDeque::with_capacity(capacity) }
    }

    /// Pushes a new entry, evicting the oldest if at capacity.
    ///
    /// `position` should be the ordered index for this entry. Passing `None`
    /// marks the entry as a sentinel, ensuring it is always included in replay.
    pub fn push(&mut self, position: Option<I>, payload: V) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(RingBufferEntry { position, payload });
    }

    /// Returns an iterator over payloads for all entries whose position is
    /// strictly after `position`, plus all sentinel entries (those with
    /// `position = None`).
    pub fn entries_after<'a>(&'a self, position: &'a I) -> impl Iterator<Item = &'a V> + 'a {
        self.entries
            .iter()
            .filter(move |e| e.position.as_ref().is_none_or(|p| p > position))
            .map(|e| &e.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_evict_at_capacity() {
        let mut buf: RingBuffer<(u64, u64), Vec<u8>> = RingBuffer::new(2);
        buf.push(Some((1, 0)), b"1:0".to_vec());
        buf.push(Some((1, 1)), b"1:1".to_vec());
        assert_eq!(buf.entries.len(), 2);

        buf.push(Some((2, 0)), b"2:0".to_vec()); // evicts (1,0)
        assert_eq!(buf.entries.len(), 2);
        assert_eq!(buf.entries.front().unwrap().position, Some((1, 1)));
    }

    #[test]
    fn entries_after_correct_subset() {
        let mut buf: RingBuffer<(u64, u64), Vec<u8>> = RingBuffer::new(10);
        for (bn, fi) in [(1u64, 0u64), (1, 1), (1, 2), (2, 0), (2, 1)] {
            buf.push(Some((bn, fi)), format!("{bn}:{fi}").into_bytes());
        }

        assert_eq!(
            buf.entries_after(&(1, 1)).cloned().collect::<Vec<_>>(),
            vec![b"1:2".to_vec(), b"2:0".to_vec(), b"2:1".to_vec()],
        );

        assert_eq!(buf.entries_after(&(2, 0)).cloned().collect::<Vec<_>>(), vec![b"2:1".to_vec()]);

        assert!(buf.entries_after(&(2, 1)).next().is_none());

        assert_eq!(
            buf.entries_after(&(0, 0)).cloned().collect::<Vec<_>>(),
            vec![
                b"1:0".to_vec(),
                b"1:1".to_vec(),
                b"1:2".to_vec(),
                b"2:0".to_vec(),
                b"2:1".to_vec(),
            ],
        );
    }

    #[test]
    fn empty_buffer_returns_empty() {
        let buf: RingBuffer<(u64, u64), Vec<u8>> = RingBuffer::new(16);
        assert!(buf.entries_after(&(0, 0)).next().is_none());
    }

    #[test]
    fn boundary_same_block_different_index() {
        let mut buf: RingBuffer<(u64, u64), Vec<u8>> = RingBuffer::new(10);
        buf.push(Some((5, 3)), b"5:3".to_vec());
        buf.push(Some((5, 4)), b"5:4".to_vec());
        assert_eq!(buf.entries_after(&(5, 3)).cloned().collect::<Vec<_>>(), vec![b"5:4".to_vec()]);
    }

    #[test]
    fn sentinel_entries_always_included() {
        let mut buf: RingBuffer<(u64, u64), Vec<u8>> = RingBuffer::new(10);
        buf.push(Some((1, 0)), b"1:0".to_vec());
        buf.push(None, b"unparseable".to_vec());
        buf.push(Some((2, 0)), b"2:0".to_vec());

        assert!(buf.entries.iter().any(|e| e.position.is_none() && e.payload == b"unparseable"));

        // Sentinel is always included regardless of the requested cutoff.
        let after_1_0: Vec<Vec<u8>> = buf.entries_after(&(1, 0)).cloned().collect();
        assert!(after_1_0.contains(&b"unparseable".to_vec()));
        assert!(after_1_0.contains(&b"2:0".to_vec()));

        // Even a maximum position still includes the sentinel.
        let after_max: Vec<Vec<u8>> = buf.entries_after(&(u64::MAX, u64::MAX)).cloned().collect();
        assert_eq!(after_max, vec![b"unparseable".to_vec()]);
    }
}
