//! Block times on the millisecond clock that Base transaction expiry deadlines use.

/// A block time in Unix milliseconds: the header timestamp times 1000 plus the
/// block's `BaseTime` millisecond part.
///
/// A transaction's `valid_before` is checked against this time. Keeping
/// deadlines in this type stops a millisecond value from being compared with a
/// header second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixMillis(u64);

impl UnixMillis {
    /// The latest representable time, used as the deadline of an unbounded
    /// predicate.
    pub const MAX: Self = Self(u64::MAX);

    /// Wraps a time already in Unix milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }
}
