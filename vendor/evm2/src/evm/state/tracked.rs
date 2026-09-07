//! Tracked overlay values.

/// A value tracked together with the value it had at an aggregation boundary.
///
/// `Tracked` is used by [`State`](super::State) to keep a transaction overlay over the backing
/// database, by [`PendingState`](super::PendingState) to describe transaction transitions, and by
/// [`BlockStateAccumulator`](super::BlockStateAccumulator) to describe block transitions.
/// `original` is the value at the start of the current transaction or block, while `current` is the
/// value after all observed mutations in that boundary. When a transaction is accepted, `current`
/// becomes the next transaction's `original` without writing anything to the backing database.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Tracked<T> {
    /// Value at the start of the aggregation boundary.
    pub original: T,
    /// Value after observed mutations in the aggregation boundary.
    pub current: T,
}

impl<T> Tracked<T> {
    /// Creates a tracked value from distinct original and current values.
    #[inline]
    pub const fn from_parts(original: T, current: T) -> Self {
        Self { original, current }
    }

    /// Creates a tracked value whose original and current values are equal.
    #[inline]
    pub fn new(value: T) -> Self
    where
        T: Clone,
    {
        Self { original: value.clone(), current: value }
    }

    /// Updates the current value.
    #[inline]
    pub fn set_current(&mut self, current: T) {
        self.current = current;
    }

    /// Splits this tracked value into original and current values.
    #[inline]
    pub fn into_parts(self) -> (T, T) {
        (self.original, self.current)
    }
}

impl<T: PartialEq> Tracked<T> {
    /// Returns whether the current value differs from the original value.
    #[inline]
    pub fn is_changed(&self) -> bool {
        self.original != self.current
    }
}
