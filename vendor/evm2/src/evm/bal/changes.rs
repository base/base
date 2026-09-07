//! BAL change types and the per-item change list.

use alloc::vec::Vec;

use alloy_eip7928::{BalanceChange, CodeChange as AlloyCodeChange, NonceChange, StorageChange};
use alloy_primitives::{B256, U256};

use super::BlockAccessIndex;
use crate::bytecode::{Bytecode, BytecodeDecodeError};

/// A single change of one state item at a [`BlockAccessIndex`].
///
/// Implemented on the EIP-7928 change types ([`NonceChange`], [`BalanceChange`],
/// [`StorageChange`]) and the internal [`BalCodeChange`] so [`BalChanges`] can share the
/// access and push logic across all of them.
pub trait BalChange {
    /// The post-state value carried by the change.
    type Value: PartialEq + Clone;

    /// Create a new change.
    fn new(index: BlockAccessIndex, value: Self::Value) -> Self;

    /// The [`BlockAccessIndex`] at which the change happened.
    fn block_access_index(&self) -> BlockAccessIndex;

    /// The post-state value of the change.
    fn value(&self) -> &Self::Value;

    /// Replace the post-state value of the change.
    fn set_value(&mut self, value: Self::Value);
}

impl BalChange for NonceChange {
    type Value = u64;

    fn new(index: BlockAccessIndex, value: Self::Value) -> Self {
        Self { block_access_index: index, new_nonce: value }
    }

    fn block_access_index(&self) -> BlockAccessIndex {
        self.block_access_index
    }

    fn value(&self) -> &Self::Value {
        &self.new_nonce
    }

    fn set_value(&mut self, value: Self::Value) {
        self.new_nonce = value;
    }
}

impl BalChange for BalanceChange {
    type Value = U256;

    fn new(index: BlockAccessIndex, value: Self::Value) -> Self {
        Self { block_access_index: index, post_balance: value }
    }

    fn block_access_index(&self) -> BlockAccessIndex {
        self.block_access_index
    }

    fn value(&self) -> &Self::Value {
        &self.post_balance
    }

    fn set_value(&mut self, value: Self::Value) {
        self.post_balance = value;
    }
}

impl BalChange for StorageChange {
    type Value = U256;

    fn new(index: BlockAccessIndex, value: Self::Value) -> Self {
        Self { block_access_index: index, new_value: value }
    }

    fn block_access_index(&self) -> BlockAccessIndex {
        self.block_access_index
    }

    fn value(&self) -> &Self::Value {
        &self.new_value
    }

    fn set_value(&mut self, value: Self::Value) {
        self.new_value = value;
    }
}

/// A code change carrying the decoded [`Bytecode`] and its cached hash.
///
/// The EIP-7928 [`AlloyCodeChange`] only stores raw bytes; keeping the decoded bytecode and
/// hash lets BAL reads populate account info without re-decoding and re-hashing, and keeps
/// bytecode validation at import time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BalCodeChange {
    /// The [`BlockAccessIndex`] at which the code changed.
    pub block_access_index: BlockAccessIndex,
    /// The post-state code hash and decoded bytecode.
    pub code: (B256, Bytecode),
}

impl BalCodeChange {
    /// Create a new code change.
    pub const fn new(block_access_index: BlockAccessIndex, code: (B256, Bytecode)) -> Self {
        Self { block_access_index, code }
    }
}

impl BalChange for BalCodeChange {
    type Value = (B256, Bytecode);

    fn new(index: BlockAccessIndex, value: Self::Value) -> Self {
        Self { block_access_index: index, code: value }
    }

    fn block_access_index(&self) -> BlockAccessIndex {
        self.block_access_index
    }

    fn value(&self) -> &Self::Value {
        &self.code
    }

    fn set_value(&mut self, value: Self::Value) {
        self.code = value;
    }
}

impl TryFrom<&AlloyCodeChange> for BalCodeChange {
    type Error = BytecodeDecodeError;

    fn try_from(change: &AlloyCodeChange) -> Result<Self, Self::Error> {
        let bytecode = Bytecode::new_raw_checked(change.new_code.clone())?;
        let hash = bytecode.hash_slow();
        Ok(Self { block_access_index: change.block_access_index, code: (hash, bytecode) })
    }
}

impl From<BalCodeChange> for AlloyCodeChange {
    fn from(change: BalCodeChange) -> Self {
        Self::new(change.block_access_index, change.code.1.original_bytes())
    }
}

/// Chronological change list for one state item.
///
/// If empty it means that this item was read from database.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BalChanges<T> {
    /// List of changes, sorted by [`BlockAccessIndex`].
    pub changes: Vec<T>,
}

impl<T: BalChange> From<Vec<T>> for BalChanges<T> {
    fn from(changes: Vec<T>) -> Self {
        Self { changes }
    }
}

impl<T: BalChange> BalChanges<T> {
    /// Create a new BalChanges.
    pub fn new(mut changes: Vec<T>) -> Self {
        changes.sort_by_key(|change| change.block_access_index());
        Self { changes }
    }

    /// Linear search is used for small number of changes. It is faster than binary search.
    #[inline(never)]
    pub fn get_linear_search(&self, bal_index: BlockAccessIndex) -> Option<&T::Value> {
        // find the first change at or after bal_index; the value before it is the one visible.
        let i = self
            .changes
            .iter()
            .position(|change| change.block_access_index() >= bal_index)
            .unwrap_or(self.changes.len());
        // only if i is not zero, we return the previous value.
        (i != 0).then(|| self.changes[i - 1].value())
    }

    /// Get value from BAL.
    pub fn get(&self, bal_index: BlockAccessIndex) -> Option<&T::Value> {
        if self.changes.len() < 5 {
            return self.get_linear_search(bal_index);
        }
        // else do binary search.
        let i = match self
            .changes
            .binary_search_by_key(&bal_index, |change| change.block_access_index())
        {
            Ok(i) => i,
            Err(i) => i,
        };
        // only if i is not zero, we return the previous value.
        (i != 0).then(|| self.changes[i - 1].value())
    }

    /// Extend the builder with another builder.
    pub fn extend(&mut self, other: Self) {
        self.changes.extend(other.changes);
    }

    /// Returns true if the builder is empty.
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Force insert a value into the BalChanges.
    ///
    /// Check if last index is same as the index to insert.
    /// If it is, we override the value.
    /// If it is not, we push the value to the end of the vector.
    ///
    /// No checks for original value is done. This is useful when we know that value is different.
    #[inline]
    pub fn force_update(&mut self, index: BlockAccessIndex, value: T::Value) {
        if let Some(last) = self.changes.last_mut()
            && index == last.block_access_index()
        {
            last.set_value(value);
            return;
        }
        self.changes.push(T::new(index, value));
    }

    /// Insert a value into the builder.
    ///
    /// If [`BlockAccessIndex`] is same as last it will override the value.
    pub fn update(&mut self, index: BlockAccessIndex, original_value: &T::Value, value: T::Value) {
        self.update_with_key(index, original_value, value, |i| i);
    }

    /// Insert a value into the builder.
    ///
    /// If [`BlockAccessIndex`] is same as last it will override the value.
    ///
    /// Assumes that index is always greater than last one and that changes are updated in proper
    /// order.
    #[inline]
    pub fn update_with_key<K: PartialEq, F>(
        &mut self,
        index: BlockAccessIndex,
        original_subvalue: &K,
        value: T::Value,
        f: F,
    ) where
        F: Fn(&T::Value) -> &K,
    {
        // A no-op update at an index that already contains a change must preserve the recorded
        // value. The original subvalue is the value before this update, not necessarily the value
        // before the index, so using it as the baseline below could incorrectly remove the change.
        if self.changes.last().is_some_and(|last| last.block_access_index() == index)
            && original_subvalue == f(&value)
        {
            return;
        }

        // if index is different, we push the new value.
        if let Some(last) = self.changes.last_mut()
            && last.block_access_index() != index
        {
            // we push the new value only if it is changed.
            if f(last.value()) != f(&value) {
                self.changes.push(T::new(index, value));
            }
            return;
        }

        // extract previous (Can be original_subvalue or previous value) and last value.
        let (previous, last) = match self.changes.as_mut_slice() {
            [.., previous, last] => (f(previous.value()), last),
            [last] => (original_subvalue, last),
            [] => {
                // if changes are empty check if original value is same as newly set value.
                if original_subvalue != f(&value) {
                    self.changes.push(T::new(index, value));
                }
                return;
            }
        };

        // if previous value is same, we pop the last value.
        if previous == f(&value) {
            self.changes.pop();
            return;
        }

        // if it is different, we update the last value.
        last.set_value(value);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    const fn idx(index: u64) -> BlockAccessIndex {
        BlockAccessIndex::new(index)
    }

    #[test]
    fn test_get() {
        let bal_changes = BalChanges::new(vec![
            NonceChange::new(idx(0), 1),
            NonceChange::new(idx(1), 2),
            NonceChange::new(idx(2), 3),
        ]);
        assert_eq!(bal_changes.get(idx(0)), None);
        assert_eq!(bal_changes.get(idx(1)), Some(&1));
        assert_eq!(bal_changes.get(idx(2)), Some(&2));
        assert_eq!(bal_changes.get(idx(3)), Some(&3));
        assert_eq!(bal_changes.get(idx(4)), Some(&3));
    }

    fn get_binary_search(threshold: u64) {
        // Construct test data up to (threshold - 1), skipping one key to simulate a gap.
        let entries: Vec<_> = (0..threshold - 1)
            .map(|i| NonceChange::new(idx(i), i + 1))
            .chain(core::iter::once(NonceChange::new(idx(threshold), threshold + 1)))
            .collect();

        let bal_changes = BalChanges::new(entries);

        // Case 1: lookup before any entries
        assert_eq!(bal_changes.get(idx(0)), None);

        // Case 2: lookups for existing keys before the gap
        for i in 1..threshold - 1 {
            assert_eq!(bal_changes.get(idx(i)), Some(&i));
        }

        // Case 3: lookup at the skipped key — should return the previous value
        assert_eq!(bal_changes.get(idx(threshold)), Some(&(threshold - 1)));

        // Case 4: lookup after the skipped key — should return the next valid value
        assert_eq!(bal_changes.get(idx(threshold + 1)), Some(&(threshold + 1)));
    }

    #[test]
    fn test_get_binary_search() {
        get_binary_search(4);
        get_binary_search(5);
        get_binary_search(6);
        get_binary_search(7);
    }

    #[test]
    fn update_preserves_existing_change_for_noop_at_same_index() {
        let mut changes = BalChanges::<NonceChange>::default();

        changes.update(idx(1), &0, 1);
        changes.update(idx(1), &1, 1);

        assert_eq!(changes.changes, vec![NonceChange::new(idx(1), 1)]);
    }

    #[test]
    fn update_overwrites_existing_change_at_same_index() {
        let mut changes = BalChanges::<NonceChange>::default();

        changes.update(idx(1), &0, 1);
        changes.update(idx(1), &1, 2);

        assert_eq!(changes.changes, vec![NonceChange::new(idx(1), 2)]);
    }
}
