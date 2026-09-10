use std::ops::{Bound, Range, RangeBounds};

/// Converts provider query bounds into half-open block ranges.
#[derive(Debug, Clone, Copy)]
pub struct ProviderRange;

impl ProviderRange {
    /// Converts a [`RangeBounds`] into a concrete [`Range`]
    pub fn from_bounds<R: RangeBounds<u64>>(bounds: R) -> Range<u64> {
        let start = match bounds.start_bound() {
            Bound::Included(&v) => v,
            Bound::Excluded(&v) => v + 1,
            Bound::Unbounded => 0,
        };

        let end = match bounds.end_bound() {
            Bound::Included(&v) => v + 1,
            Bound::Excluded(&v) => v,
            Bound::Unbounded => u64::MAX,
        };

        start..end
    }
}
