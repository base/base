use std::collections::VecDeque;

use tokio_tungstenite::tungstenite::Utf8Bytes;

/// A single entry in the publisher ring buffer.
#[derive(Debug)]
pub struct RingBufferEntry {
    /// The block number this flashblock belongs to.
    pub block_number: u64,
    /// The index of this flashblock within its block.
    pub flashblock_index: u64,
    /// The serialized JSON payload.
    pub payload: Utf8Bytes,
}

/// A fixed-capacity ring buffer of flashblock entries.
///
/// When the capacity is exceeded, the oldest entry is evicted to make room.
#[derive(Debug)]
pub struct RingBuffer {
    capacity: usize,
    entries: VecDeque<RingBufferEntry>,
}

impl RingBuffer {
    /// Creates a new ring buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self { capacity, entries: VecDeque::with_capacity(capacity) }
    }

    /// Pushes a new entry, evicting the oldest if at capacity.
    pub fn push(&mut self, entry: RingBufferEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Returns payloads for all entries that come after `(block_number, flashblock_index)`.
    ///
    /// An entry is "after" if its block number is greater, or if it has the same block number
    /// and a higher flashblock index.
    pub fn entries_after(&self, block_number: u64, flashblock_index: u64) -> Vec<Utf8Bytes> {
        self.entries
            .iter()
            .filter(|e| {
                e.block_number > block_number
                    || (e.block_number == block_number && e.flashblock_index > flashblock_index)
            })
            .map(|e| e.payload.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(block_number: u64, flashblock_index: u64) -> RingBufferEntry {
        RingBufferEntry {
            block_number,
            flashblock_index,
            payload: Utf8Bytes::from(format!("{block_number}:{flashblock_index}")),
        }
    }

    #[test]
    fn push_and_evict_at_capacity() {
        let mut buf = RingBuffer::new(3);
        buf.push(entry(1, 0));
        buf.push(entry(1, 1));
        buf.push(entry(1, 2));
        assert_eq!(buf.entries.len(), 3);
        buf.push(entry(2, 0)); // evicts (1, 0)
        assert_eq!(buf.entries.len(), 3);
        let front = buf.entries.front().unwrap();
        assert_eq!(front.block_number, 1);
        assert_eq!(front.flashblock_index, 1);
    }

    #[test]
    fn entries_after_correct_subset() {
        let mut buf = RingBuffer::new(10);
        buf.push(entry(1, 0));
        buf.push(entry(1, 1));
        buf.push(entry(1, 2));
        buf.push(entry(2, 0));
        buf.push(entry(2, 1));

        let result = buf.entries_after(1, 1);
        assert_eq!(result.len(), 3); // (1,2), (2,0), (2,1)

        let result = buf.entries_after(2, 0);
        assert_eq!(result.len(), 1); // (2,1)

        let result = buf.entries_after(2, 1);
        assert_eq!(result.len(), 0);

        let result = buf.entries_after(0, 0);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn empty_buffer_returns_empty() {
        let buf = RingBuffer::new(16);
        assert_eq!(buf.entries_after(0, 0).len(), 0);
    }

    #[test]
    fn boundary_same_block_different_index() {
        let mut buf = RingBuffer::new(10);
        buf.push(entry(5, 3));
        buf.push(entry(5, 4));
        let result = buf.entries_after(5, 3);
        assert_eq!(result.len(), 1); // only (5,4)
    }
}
