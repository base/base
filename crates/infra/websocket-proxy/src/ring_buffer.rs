use std::collections::VecDeque;

/// Parses `(block_number, flashblock_index)` position from a flashblock JSON payload.
///
/// Returns `None` if the JSON cannot be parsed or the required fields are absent.
fn extract_position(json: &str) -> Option<(u64, u64)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let index = v.get("index")?.as_u64()?;
    let block_number = v.get("metadata")?.get("block_number")?.as_u64()?;
    Some((block_number, index))
}

/// A single entry in the proxy ring buffer.
#[derive(Debug)]
pub struct FlashblocksRingBufferEntry {
    /// Block number, or `u64::MAX` if position could not be parsed.
    pub block_number: u64,
    /// Flashblock index within the block, or `u64::MAX` if position could not be parsed.
    pub flashblock_index: u64,
    /// The raw bytes of this flashblock, ready to send to downstream clients.
    pub payload: Vec<u8>,
}

/// A fixed-capacity ring buffer of raw flashblock payloads for the WebSocket proxy.
///
/// Entries are inserted via [`FlashblocksRingBuffer::push`], which extracts the
/// `(block_number, flashblock_index)` position from the JSON automatically.
/// Entries whose position cannot be parsed are assigned `(u64::MAX, u64::MAX)`
/// and are always included when replaying to reconnecting clients.
#[derive(Debug)]
pub struct FlashblocksRingBuffer {
    capacity: usize,
    entries: VecDeque<FlashblocksRingBufferEntry>,
}

impl FlashblocksRingBuffer {
    /// Creates a new ring buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self { capacity, entries: VecDeque::with_capacity(capacity) }
    }

    /// Pushes a raw JSON string and its encoded byte representation.
    ///
    /// The `(block_number, flashblock_index)` position is parsed from `json`.
    /// If parsing fails, the entry is stored with `(u64::MAX, u64::MAX)` and
    /// will always be included in replay output.
    ///
    /// Evicts the oldest entry if the buffer is at capacity.
    pub fn push(&mut self, json: &str, raw_bytes: Vec<u8>) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        let (block_number, flashblock_index) =
            extract_position(json).unwrap_or((u64::MAX, u64::MAX));
        self.entries.push_back(FlashblocksRingBufferEntry {
            block_number,
            flashblock_index,
            payload: raw_bytes,
        });
    }

    /// Returns payloads for all entries that come after `(block_number, flashblock_index)`.
    ///
    /// An entry is "after" if its block number is greater, or if it has the same block number
    /// and a higher flashblock index. Entries with `(u64::MAX, u64::MAX)` are always included.
    pub fn entries_after(&self, block_number: u64, flashblock_index: u64) -> Vec<Vec<u8>> {
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

    fn make_json(block_number: u64, index: u64) -> String {
        format!(r#"{{"index":{index},"metadata":{{"block_number":{block_number}}}}}"#)
    }

    #[test]
    fn push_and_evict_at_capacity() {
        let mut buf = FlashblocksRingBuffer::new(2);
        let json1 = make_json(1, 0);
        let json2 = make_json(1, 1);
        let json3 = make_json(2, 0);

        buf.push(&json1, json1.as_bytes().to_vec());
        buf.push(&json2, json2.as_bytes().to_vec());
        assert_eq!(buf.entries.len(), 2);

        buf.push(&json3, json3.as_bytes().to_vec()); // evicts (1,0)
        assert_eq!(buf.entries.len(), 2);
        let front = buf.entries.front().unwrap();
        assert_eq!(front.block_number, 1);
        assert_eq!(front.flashblock_index, 1);
    }

    #[test]
    fn entries_after_correct_subset() {
        let mut buf = FlashblocksRingBuffer::new(10);
        for (bn, fi) in [(1u64, 0u64), (1, 1), (1, 2), (2, 0), (2, 1)] {
            let json = make_json(bn, fi);
            buf.push(&json, json.as_bytes().to_vec());
        }

        assert_eq!(buf.entries_after(1, 1).len(), 3); // (1,2), (2,0), (2,1)
        assert_eq!(buf.entries_after(2, 0).len(), 1); // (2,1)
        assert_eq!(buf.entries_after(2, 1).len(), 0);
        assert_eq!(buf.entries_after(0, 0).len(), 5);
    }

    #[test]
    fn empty_buffer_returns_empty() {
        let buf = FlashblocksRingBuffer::new(16);
        assert_eq!(buf.entries_after(0, 0).len(), 0);
    }

    #[test]
    fn unparseable_json_stored_with_max_position() {
        let mut buf = FlashblocksRingBuffer::new(10);
        buf.push("not valid json", b"not valid json".to_vec());
        let entry = buf.entries.back().unwrap();
        assert_eq!(entry.block_number, u64::MAX);
        assert_eq!(entry.flashblock_index, u64::MAX);
        // Entry at (MAX, MAX) is after (MAX-1, 0), so it IS included in replay.
        assert_eq!(buf.entries_after(u64::MAX - 1, 0).len(), 1);
    }

    #[test]
    fn extract_position_valid() {
        assert_eq!(
            extract_position(r#"{"index":3,"metadata":{"block_number":10}}"#),
            Some((10, 3))
        );
    }

    #[test]
    fn extract_position_missing_fields() {
        assert_eq!(extract_position(r#"{"index":3}"#), None);
        assert_eq!(extract_position(r#"{"metadata":{"block_number":10}}"#), None);
        assert_eq!(extract_position("not json"), None);
    }
}
