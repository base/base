//! Block times on the millisecond clock that EIP-8130 expiry deadlines use.

/// A block time in Unix milliseconds: the header timestamp times 1000 plus the
/// block's `BaseTime` millisecond part.
///
/// A transaction's `valid_before` is checked against this time, while actor
/// expiries and pending unlocks are checked against the header second. Keeping
/// deadlines in this type stops a value on one clock from being compared with a
/// value on the other: second-based rules enter only through
/// [`Self::last_of_second`] and [`Self::before_second`].
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

    /// The last block time inside the header second `secs`: the deadline of a
    /// rule that holds while `block.timestamp <= secs`.
    #[must_use]
    pub const fn last_of_second(secs: u64) -> Self {
        Self(secs.saturating_mul(1_000).saturating_add(999))
    }

    /// The last block time before the header second `secs` starts: the deadline
    /// of a rule that holds while `block.timestamp < secs`.
    #[must_use]
    pub const fn before_second(secs: u64) -> Self {
        Self(secs.saturating_mul(1_000).saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_rules_map_to_their_last_valid_millisecond() {
        assert_eq!(UnixMillis::last_of_second(1_000), UnixMillis::from_millis(1_000_999));
        assert_eq!(UnixMillis::before_second(1_000), UnixMillis::from_millis(999_999));
        assert_eq!(UnixMillis::last_of_second(u64::MAX), UnixMillis::MAX);
        assert_eq!(UnixMillis::before_second(0), UnixMillis::from_millis(0));
    }
}
