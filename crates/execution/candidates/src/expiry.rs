//! Block windows derived from a duration at the active block cadence.

/// An inclusive range of blocks, `first_block ..= first_block + max_blocks`.
///
/// Durations convert to blocks rounding up, so a partial block still counts: a 7 s window at
/// 2 s blocks spans 4 blocks. Callers pass the active full-block interval (it changes at Denim),
/// never the flashblock interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockWindow {
    /// The earliest block in the window.
    pub first_block: u64,
    /// How many blocks past `first_block` the window extends.
    pub max_blocks: u64,
}

impl BlockWindow {
    /// Returns the window of `max_secs` seconds of blocks starting at `first_block`. A zero
    /// interval is treated as one millisecond rather than dividing by zero.
    pub const fn from_secs(first_block: u64, max_secs: u64, block_interval_millis: u64) -> Self {
        let interval = if block_interval_millis == 0 { 1 } else { block_interval_millis };
        Self { first_block, max_blocks: max_secs.saturating_mul(1_000).div_ceil(interval) }
    }

    /// Returns the last block in the window (inclusive).
    pub const fn last_block(&self) -> u64 {
        self.first_block.saturating_add(self.max_blocks)
    }

    /// Returns `true` when `block` is inside the window (both ends inclusive).
    pub const fn contains(&self, block: u64) -> bool {
        block >= self.first_block && block <= self.last_block()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_rounds_partial_blocks_up() {
        assert_eq!(BlockWindow::from_secs(10, 60, 2_000).last_block(), 40);
        assert_eq!(BlockWindow::from_secs(10, 7, 2_000).last_block(), 14);
        assert_eq!(BlockWindow::from_secs(10, 1, 200).last_block(), 15);
        let window = BlockWindow::from_secs(10, 7, 2_000);
        assert!(window.contains(10) && window.contains(14));
        assert!(!window.contains(9) && !window.contains(15));
    }
}
